//! Compare staged Rcmake tools with the CMake 4.5.1 reference binary or a recorded baseline.

#![forbid(unsafe_code)]
#![allow(clippy::print_stdout, clippy::print_stderr)]
#![allow(
    clippy::disallowed_methods,
    reason = "The harness links zero engine crates."
)]

mod case;
mod run;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path};
use std::process::ExitCode;

use run::Outcome;

type Baseline = BTreeMap<String, Outcome>;

#[derive(Clone, Copy, Default)]
enum Mode {
    #[default]
    Live,
    Record,
    Offline,
}

struct Options {
    suite: String,
    mode: Mode,
    filter: Option<String>,
}

impl Options {
    fn parse() -> Result<Self, String> {
        let mut options = Self {
            suite: "smoke".to_owned(),
            mode: Mode::Live,
            filter: None,
        };
        let mut args = std::env::args_os().skip(1).map(|arg| {
            arg.into_string()
                .map_err(|_| "Options must be valid UTF-8.".to_owned())
        });
        while let Some(arg) = args.next().transpose()? {
            let mut value = || {
                args.next()
                    .transpose()?
                    .filter(|value| !value.starts_with("--"))
                    .ok_or_else(|| format!("expected a value for {arg}"))
            };
            match arg.as_str() {
                "--suite" => options.suite = value()?,
                "--filter" => options.filter = Some(value()?),
                "--record" if !matches!(options.mode, Mode::Offline) => options.mode = Mode::Record,
                "--offline" if !matches!(options.mode, Mode::Record) => {
                    options.mode = Mode::Offline
                }
                "--record" | "--offline" => {
                    return Err("--record and --offline cannot be used together".to_owned());
                }
                _ => return Err(format!("unknown option {arg:?}")),
            }
        }
        Ok(options)
    }
}

fn suites(case_dir: &Path, suite: &str) -> Result<Vec<String>, String> {
    if suite != "all" {
        let mut components = Path::new(suite).components();
        if !matches!(components.next(), Some(Component::Normal(name)) if name == suite)
            || components.next().is_some()
        {
            return Err(format!("invalid suite name {suite:?}"));
        }
        return Ok(vec![suite.to_owned()]);
    }
    let scan = || -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(case_dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "case")
                && let Some(name) = path.file_stem().and_then(|name| name.to_str())
                && !name.starts_with("._")
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    };
    scan().map_err(|error| format!("{}: {error}", case_dir.display()))
}

fn read_baseline(path: &Path, allow_missing: bool) -> Result<Baseline, String> {
    let read = || -> io::Result<Baseline> {
        match run::read_text(path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(error) if allow_missing && error.kind() == io::ErrorKind::NotFound => {
                Ok(Baseline::new())
            }
            Err(error) => Err(error),
        }
    };
    read().map_err(|error| format!("{}: {error}", path.display()))
}

fn write_baseline(dir: &Path, path: &Path, baseline: &Baseline) -> Result<(), String> {
    let write = || -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let mut output = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer_pretty(&mut output, baseline)?;
        writeln!(output)?;
        run::check_size(output.as_file().metadata()?.len())?;
        output.persist(path)?;
        Ok(())
    };
    write().map_err(|error| format!("{}: {error}", path.display()))
}

fn run_suites(options: &Options) -> Result<bool, String> {
    let root = match std::env::var_os("RCMAKE_ROOT") {
        Some(path) => std::path::absolute(path),
        None => std::env::current_dir(),
    }
    .map_err(|error| format!("cannot resolve the Rcmake root: {error}"))?;
    let case_dir = root.join("conformance/cases");
    let baseline_dir = root.join("conformance/baselines");
    let stage = root.join("target/stage/bin");
    let oracle = if matches!(options.mode, Mode::Offline) {
        None
    } else {
        let path = std::env::var_os("RCMAKE_ORACLE_BIN_DIR")
            .ok_or_else(|| "RCMAKE_ORACLE_BIN_DIR is unset. Source env/env.sh.".to_owned())?;
        Some(
            std::path::absolute(path)
                .map_err(|error| format!("cannot resolve the oracle path: {error}"))?,
        )
    };

    let mut total = 0;
    let mut all_passed = true;
    for suite in suites(&case_dir, &options.suite)? {
        let case_path = case_dir.join(format!("{suite}.case"));
        let baseline_path = baseline_dir.join(format!("{suite}.json"));
        let cases = run::read_text(&case_path)
            .map_err(|error| format!("{}: {error}", case_path.display()))?;
        let cases =
            case::parse(&cases).map_err(|error| format!("{}: {error}", case_path.display()))?;
        let mut selected = cases
            .into_iter()
            .filter(|case| {
                options
                    .filter
                    .as_ref()
                    .is_none_or(|filter| case.name.contains(filter))
            })
            .peekable();
        if selected.peek().is_none() {
            continue;
        }
        let mut baseline = match options.mode {
            Mode::Offline => read_baseline(&baseline_path, false)?,
            Mode::Record if options.filter.is_some() => read_baseline(&baseline_path, true)?,
            _ => Baseline::new(),
        };
        let mut count = 0;
        let mut matched = 0;
        for case in selected {
            count += 1;
            let tool = case.tool.name();
            let live = oracle
                .as_ref()
                .map(|dir| run::execute(&dir.join(tool), &case))
                .transpose()?;
            if matches!(options.mode, Mode::Record)
                && let Some(outcome) = live
            {
                baseline.insert(case.name, outcome);
                continue;
            }
            let expected = live
                .as_ref()
                .or_else(|| baseline.get(&case.name))
                .ok_or_else(|| {
                    format!(
                        "{} has no entry for {}. Run --record.",
                        baseline_path.display(),
                        case.name
                    )
                })?;
            let staged_exe = stage.join(tool);
            if !staged_exe.is_file() {
                return Err(format!(
                    "Missing {}. Run scripts/stage.sh.",
                    staged_exe.display()
                ));
            }
            let actual = run::execute(&staged_exe, &case)?;
            if *expected == actual {
                matched += 1;
            } else {
                let label = if live.is_some() { "oracle" } else { "baseline" };
                let metadata = if case.expect_fail_allowed {
                    " (expect_fail_allowed=true)"
                } else {
                    ""
                };
                eprintln!(
                    "Mismatch in {suite}/{}{metadata}\n{}",
                    case.name,
                    expected.diff(&actual, label).map_err(|error| {
                        format!("cannot render the diff for {suite}/{}: {error}", case.name)
                    })?
                );
            }
        }
        total += count;
        if matches!(options.mode, Mode::Record) {
            write_baseline(&baseline_dir, &baseline_path, &baseline)?;
            println!("rcmake-harness {suite}: Recorded {count} cases from CMake 4.5.1");
        } else {
            let expected = if matches!(options.mode, Mode::Live) {
                "CMake 4.5.1"
            } else {
                "the recorded baseline"
            };
            println!("rcmake-harness {suite}: {matched}/{count} cases matched {expected}");
            all_passed &= matched == count;
        }
    }
    if total == 0 {
        return Err("No cases matched the suite and filter.".to_owned());
    }
    Ok(all_passed)
}

fn main() -> ExitCode {
    match Options::parse().and_then(|options| run_suites(&options)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("rcmake-harness: {error}");
            ExitCode::FAILURE
        }
    }
}
