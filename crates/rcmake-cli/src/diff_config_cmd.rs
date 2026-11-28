//! Native `rcmake diff-config` subcommand.

#![forbid(unsafe_code)]

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_api::Freshness;
use rcmake_base::BString;
use rcmake_buildinfo::{
    BuildSnapshot, CacheSet, DiffOptions, PackageObservation, QUERY_FILES, SaveOptions, Section,
    SnapshotError, TraceCall, TraceInput, compare, load, load_snapshot_file, render_json,
    render_text, save_snapshot_file, validate,
};
use rcmake_host::{FileKind, Follow, Host, Stream, WriteMode};
use rcmake_presets::{EnvSnapshot, PresetKind};

use crate::configure::find_hints;
use crate::resolve_cli_path;

const HELP: &str = "\
Usage: rcmake diff-config <a-dir-or-snapshot> <b-dir-or-snapshot> [options]
       rcmake diff-config --preset <a> --preset <b> [-S <source-dir>] [options]
       rcmake diff-config --save <file> (<build-dir> | --preset <name>) [options]
       rcmake diff-config --prepare <build-dir>...

Compare the build settings of two configured build directories or portable snapshots.
Build directories can come from cmake or rcmake. rcmake reads them and never runs cmake.

  --save <file>         Save a normalized portable JSON snapshot of one build
                        directory (or --preset) to <file>, then exit.
  --preset <name>       Use the binary directory of a configure preset.
                        Give it twice, mix it with one build directory or snapshot,
                        or pair it with --save.
  -S <source-dir>       Read presets from this directory. Default: current directory.
  --config <a>[:<b>]    Compare these configurations.
  --target <name>       Compare only this target. Repeat to add more.
  --only <sections>     Compare only these sections, separated by commas:
                        targets, compile, link, toolchain, packages, cache.
  --cache <set>         Cache entries to compare or save: key, all, none. Default: key.
  --no-normalize        Keep absolute paths. Do not replace build, source, and home
                        roots. Cannot be used with --save or snapshot files.
  --prepare             Create File API query files in each directory, then exit
                        (or prepare before --save).
  --json                Write one JSON report to standard output.
  --color=<mode>        Set terminal color. Default: auto.
  -h, --help            Show this help.

Exact data comes from a File API reply. Without one, compile data comes from
compile_commands.json with targets inferred from object paths, and link data
is unavailable.

Examples
  rcmake diff-config build-main build-pr
  rcmake diff-config --save baseline.json build-main
  rcmake diff-config baseline.json build-pr
  rcmake diff-config --preset debug --preset release --only compile
  rcmake diff-config build-gcc build-clang --json > diff.json
  rcmake diff-config --prepare build-a build-b

Exit status
  0  No differences, and every requested section had complete evidence (or --save /
     --prepare succeeded).
  1  Differences found.
  2  The evidence is incomplete for a requested section, so parity is not
     proved. Also invalid arguments, an unknown target or configuration, or an
     unreadable build directory or snapshot file. Use --only to limit sections.
";

#[derive(Debug, PartialEq, Eq)]
enum Operand {
    Path(BString),
    Preset(BString),
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    operands: Vec<Operand>,
    save: Option<BString>,
    source: Option<BString>,
    config: Option<(BString, BString)>,
    targets: Vec<BString>,
    only: Option<Vec<Section>>,
    cache: CacheSet,
    normalize: bool,
    prepare: bool,
    json: bool,
    color: Color,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Color {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Help,
    Run(Options),
}

fn value<'a>(
    args: &mut impl Iterator<Item = &'a BString>,
    attached: Option<&[u8]>,
    flag: &str,
) -> Result<BString, String> {
    attached
        .map(BString::from)
        .or_else(|| args.next().filter(|arg| !arg.starts_with(b"-")).cloned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Supply a value for {flag}."))
}

fn parse(args: &[BString]) -> Result<Invocation, String> {
    let mut options = Options {
        normalize: true,
        ..Options::default()
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let raw = arg.as_slice();
        if raw == b"--" {
            options.operands.extend(iter.cloned().map(Operand::Path));
            break;
        }
        let (flag, attached) = if raw.starts_with(b"--") {
            let mut parts = raw.splitn(2, |byte| *byte == b'=');
            (parts.next().unwrap_or(raw), parts.next())
        } else if raw.len() > 2 && raw.starts_with(b"-S") {
            (b"-S".as_slice(), raw.get(2..))
        } else {
            (raw, None)
        };
        match flag {
            b"-h" | b"--help" => return Ok(Invocation::Help),
            b"--save" => {
                options.save = Some(value(&mut iter, attached, "--save")?);
            }
            b"--preset" => options
                .operands
                .push(Operand::Preset(value(&mut iter, attached, "--preset")?)),
            b"-S" => options.source = Some(value(&mut iter, attached, "-S")?),
            b"--config" => {
                let text = value(&mut iter, attached, "--config")?;
                let pair: (BString, BString) = match text.iter().position(|byte| *byte == b':') {
                    Some(split) => (
                        text.get(..split).unwrap_or_default().into(),
                        text.get(split + 1..).unwrap_or_default().into(),
                    ),
                    None => (text.clone(), text),
                };
                if pair.0.is_empty() || pair.1.is_empty() {
                    return Err("Give a configuration on each side of : in --config.".into());
                }
                options.config = Some(pair);
            }
            b"--target" => options
                .targets
                .push(value(&mut iter, attached, "--target")?),
            b"--only" => {
                let text = value(&mut iter, attached, "--only")?;
                if text.split(|byte| *byte == b',').any(<[u8]>::is_empty) {
                    return Err(
                        "Give section names separated by commas, with no empty name, to --only."
                            .into(),
                    );
                }
                let sections = text
                    .split(|byte| *byte == b',')
                    .map(|part| {
                        Section::parse(part).ok_or_else(|| {
                            format!("Unknown section {}. Use targets, compile, link, toolchain, packages, or cache.", String::from_utf8_lossy(part))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                options.only = Some(sections);
            }
            b"--cache" => {
                options.cache = match value(&mut iter, attached, "--cache")?.as_slice() {
                    b"key" => CacheSet::Key,
                    b"all" => CacheSet::All,
                    b"none" => CacheSet::None,
                    _ => return Err("Use --cache key, --cache all, or --cache none.".into()),
                };
            }
            b"--no-normalize" | b"--prepare" | b"--json" if attached.is_some() => {
                return Err(format!("{} takes no value.", String::from_utf8_lossy(flag)));
            }
            b"--no-normalize" => options.normalize = false,
            b"--prepare" => options.prepare = true,
            b"--json" => options.json = true,
            b"--color" => {
                options.color = match value(&mut iter, attached, "--color")?.as_slice() {
                    b"auto" => Color::Auto,
                    b"always" => Color::Always,
                    b"never" => Color::Never,
                    _ => return Err("Use --color=auto, --color=always, or --color=never.".into()),
                };
            }
            _ if raw.starts_with(b"-") => return Err(format!("Unknown option {arg:?}.")),
            _ => options.operands.push(Operand::Path(arg.clone())),
        }
    }
    if options.save.is_some() {
        if !options.normalize {
            return Err(
                "--no-normalize cannot be used with --save because snapshots are normalized when saved."
                    .into(),
            );
        }
        if options.config.is_some() {
            return Err("--config cannot be used with --save.".into());
        }
        if !options.targets.is_empty() {
            return Err("--target cannot be used with --save.".into());
        }
        if options.only.is_some() {
            return Err("--only cannot be used with --save.".into());
        }
        if options.json {
            return Err("--json cannot be used with --save.".into());
        }
        if options.operands.len() != 1 {
            return Err("Give one build directory or one --preset to --save.".into());
        }
    } else if options.prepare {
        if options.operands.is_empty()
            || options
                .operands
                .iter()
                .any(|operand| matches!(operand, Operand::Preset(_)))
        {
            return Err("Give one or more build directories to --prepare.".into());
        }
    } else if options.operands.len() != 2 {
        return Err("Give two build directories or snapshots, two presets, or one of each.".into());
    }
    Ok(Invocation::Run(options))
}

fn error(host: &dyn Host, message: &str) -> i32 {
    host.write_out(
        Stream::Stderr,
        format!("rcmake diff-config: {message}\n").as_bytes(),
    );
    2
}

fn preset_dir(host: &dyn Host, source: &Path, name: &[u8]) -> Result<PathBuf, String> {
    let presets = rcmake_presets::load(source, host).map_err(|err| err.message)?;
    let resolved = rcmake_presets::resolve(
        &presets,
        PresetKind::Configure,
        name,
        &EnvSnapshot::from_host(host),
    )
    .map_err(|err| err.message)?;
    let dir = resolved.binary_dir.ok_or_else(|| {
        format!(
            "Configure preset {} has no binaryDir.",
            String::from_utf8_lossy(name)
        )
    })?;
    let dir = rcmake_host::path::to_native(&dir);
    Ok(if dir.is_absolute() {
        dir
    } else {
        source.join(dir)
    })
}

const QUERY_PARTS: [&str; 4] = [".cmake", "api", "v1", "query"];

/// Check that each query directory under `root` is a real directory. Return the
/// query directory. With `create`, make the missing directories one at a time.
fn query_dir(host: &dyn Host, root: &Path, create: bool) -> Result<PathBuf, String> {
    let mut current = root.to_path_buf();
    for part in QUERY_PARTS {
        current.push(part);
        match host.metadata(&current, Follow::No) {
            Ok(meta) if meta.kind == FileKind::Dir => {}
            Ok(_) => {
                return Err(format!(
                    "{} must be a directory, not a link or file.",
                    current.display()
                ));
            }
            Err(err) if err.kind == ErrorKind::NotFound && create => {
                host.create_dir_all(&current)
                    .map_err(|err| err.to_string())?;
                let meta = host
                    .metadata(&current, Follow::No)
                    .map_err(|err| err.to_string())?;
                if meta.kind != FileKind::Dir {
                    return Err(format!(
                        "{} changed while rcmake created it.",
                        current.display()
                    ));
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(current)
}

/// Create the shared stateless query files. Existing links and non-directories stop it.
///
/// The host creates files by path, so a parent directory can change between a check
/// and a create. rcmake checks the parents again before and after each file and
/// removes a file that was created through a changed path.
fn prepare(host: &dyn Host, dir: &Path) -> Result<Vec<PathBuf>, String> {
    let meta = host
        .metadata(dir, Follow::Yes)
        .map_err(|_| format!("{} does not exist.", dir.display()))?;
    if meta.kind != FileKind::Dir {
        return Err(format!("{} is not a directory.", dir.display()));
    }
    let root = host.canonicalize(dir).map_err(|err| err.to_string())?;
    let current = query_dir(host, &root, true)?;
    let mut created = Vec::new();
    for file in QUERY_FILES {
        let path = current.join(file);
        match host.metadata(&path, Follow::No) {
            Ok(meta) if meta.kind == FileKind::File => {}
            Ok(_) => return Err(format!("{} must be a regular file.", path.display())),
            Err(err) if err.kind == ErrorKind::NotFound => {
                query_dir(host, &root, false)?;
                host.write(&path, b"", WriteMode::CreateNew)
                    .map_err(|err| err.to_string())?;
                let moved = query_dir(host, &root, false).is_err()
                    || !host
                        .canonicalize(&path)
                        .is_ok_and(|canonical| canonical == path);
                if moved {
                    let _ = host.remove_file(&path);
                    return Err(format!(
                        "{} changed while rcmake created query files.",
                        current.display()
                    ));
                }
                created.push(path);
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(created)
}

/// Load the native find trace of `dir` with the shared typed parser and freshness check.
fn trace_input(host: &dyn Host, dir: &Path) -> TraceInput {
    let Some(saved) = find_hints::load_saved(host, dir) else {
        return TraceInput::Missing;
    };
    match saved.freshness {
        Freshness::Past(reason) => TraceInput::Past(reason.into()),
        Freshness::Current => {
            let store = &saved.document.store;
            let complete = saved.document.completed && store.all_calls_kept();
            let calls = store
                .calls
                .iter()
                .map(|call| TraceCall {
                    name: call.name.clone(),
                    observation: PackageObservation {
                        package_call: call.package_call,
                        outcome: call.outcome.label().into(),
                        version: call.version.clone(),
                        selected: call.selected.as_ref().map(|selected| selected.file.clone()),
                    },
                })
                .collect();
            TraceInput::Current { calls, complete }
        }
    }
}

/// Quote a path for a shell command line in a message.
fn quoted(dir: &[u8]) -> String {
    let mut out = String::from("\"");
    for character in String::from_utf8_lossy(dir).chars() {
        if matches!(character, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
    out
}

struct LoadedOperand {
    snapshot: BuildSnapshot,
    from_file: bool,
}

fn load_operand(host: &dyn Host, path: &Path) -> Result<LoadedOperand, String> {
    match host.metadata(path, Follow::Yes) {
        Ok(meta) if meta.kind == FileKind::File => match load_snapshot_file(host, path) {
            Ok(snapshot) => Ok(LoadedOperand {
                snapshot,
                from_file: true,
            }),
            Err(SnapshotError::NotASnapshot) => Err(format!(
                "{} is a file, not a build directory or an rcmake-config-snapshot file.",
                path.display()
            )),
            Err(err) => Err(format!("{}: {err}", path.display())),
        },
        Ok(meta) if meta.kind == FileKind::Dir => load(host, path, trace_input(host, path))
            .map(|snapshot| LoadedOperand {
                snapshot,
                from_file: false,
            })
            .map_err(|err| err.to_string()),
        Ok(_) => Err(format!(
            "{} is not a build directory or a snapshot file.",
            path.display()
        )),
        Err(_) => Err(format!(
            "{} is not a configured build directory or snapshot file.",
            path.display()
        )),
    }
}

fn resolve_operand(host: &dyn Host, source: &Path, operand: &Operand) -> Result<PathBuf, String> {
    match operand {
        Operand::Preset(name) => preset_dir(host, source, name),
        Operand::Path(raw) => Ok(resolve_cli_path(host, raw)),
    }
}

/// Execute `rcmake diff-config`.
pub(crate) fn run(_argv0: Option<&BString>, args: &[BString], host: Arc<dyn Host>) -> i32 {
    let host = host.as_ref();
    let options = match parse(args) {
        Ok(Invocation::Help) => {
            host.write_out(Stream::Stdout, HELP.as_bytes());
            return 0;
        }
        Ok(Invocation::Run(options)) => options,
        Err(message) => return error(host, &format!("{message} Run rcmake diff-config --help.")),
    };
    let source = options
        .source
        .as_ref()
        .map_or_else(|| host.initial_cwd(), |dir| resolve_cli_path(host, dir));
    if let Some(save_arg) = &options.save {
        let [operand] = options.operands.as_slice() else {
            return error(host, "Give one build directory or one --preset to --save.");
        };
        let dir = match resolve_operand(host, &source, operand) {
            Ok(dir) => dir,
            Err(message) => return error(host, &message),
        };
        if options.prepare {
            match prepare(host, &dir) {
                Ok(created) => {
                    for file in created {
                        host.write_out(
                            Stream::Stdout,
                            format!("Created {}\n", file.display()).as_bytes(),
                        );
                    }
                }
                Err(message) => return error(host, &message),
            }
        }
        let snapshot = match load(host, &dir, trace_input(host, &dir)) {
            Ok(snapshot) => snapshot,
            Err(err) => return error(host, &err.to_string()),
        };
        let output = resolve_cli_path(host, save_arg);
        let save_opts = SaveOptions {
            cache: options.cache,
        };
        if let Err(err) = save_snapshot_file(host, &output, &snapshot, &save_opts) {
            return error(host, &err.to_string());
        }
        host.write_out(
            Stream::Stdout,
            format!("Saved snapshot to {}\n", output.display()).as_bytes(),
        );
        return 0;
    }
    if options.prepare {
        let dirs: Vec<&BString> = options
            .operands
            .iter()
            .filter_map(|operand| match operand {
                Operand::Path(dir) => Some(dir),
                Operand::Preset(_) => None,
            })
            .collect();
        for dir in &dirs {
            let path = resolve_cli_path(host, dir);
            match prepare(host, &path) {
                Ok(created) => {
                    for file in created {
                        host.write_out(
                            Stream::Stdout,
                            format!("Created {}\n", file.display()).as_bytes(),
                        );
                    }
                }
                Err(message) => return error(host, &message),
            }
        }
        let commands: Vec<String> = dirs
            .iter()
            .map(|dir| format!("rcmake -B {}", quoted(dir)))
            .collect();
        host.write_out(
            Stream::Stdout,
            format!(
                "Configure each directory again, for example: {}\nFor a cmake older than 3.14, configure with -DCMAKE_EXPORT_COMPILE_COMMANDS=ON instead.\n",
                commands.join("   ")
            )
            .as_bytes(),
        );
        return 0;
    }
    let mut loaded = Vec::new();
    for operand in &options.operands {
        let path = match resolve_operand(host, &source, operand) {
            Ok(path) => path,
            Err(message) => return error(host, &message),
        };
        match load_operand(host, &path) {
            Ok(item) => loaded.push(item),
            Err(message) => return error(host, &message),
        }
    }
    let [a_item, b_item] = loaded.as_slice() else {
        return error(host, "Give two build directories or snapshots.");
    };
    if !options.normalize && (a_item.from_file || b_item.from_file) {
        return error(
            host,
            "--no-normalize cannot be used with a snapshot file because snapshots are normalized when saved.",
        );
    }
    let (a, b) = (&a_item.snapshot, &b_item.snapshot);
    let sections: std::collections::BTreeSet<Section> = options.only.map_or_else(
        || Section::ALL.into_iter().collect(),
        |sections| sections.into_iter().collect(),
    );
    // Only the target, compile, and link sections read the File API reply.
    let needs_file_api = sections
        .iter()
        .any(|section| matches!(section, Section::Targets | Section::Compile | Section::Link));
    for item in [a_item, b_item] {
        if !item.from_file
            && needs_file_api
            && !item.snapshot.evidence(Section::Compile).complete()
            && !options.json
        {
            host.write_out(
                Stream::Stderr,
                format!(
                    "note: {} has no current File API reply. For exact data, run: rcmake diff-config --prepare {}\n      then configure it again with cmake or rcmake.\n",
                    String::from_utf8_lossy(&item.snapshot.path),
                    String::from_utf8_lossy(&item.snapshot.path)
                )
                .as_bytes(),
            );
        }
    }
    let diff_options = DiffOptions {
        sections,
        targets: options.targets,
        cache: options.cache,
        normalize: options.normalize,
        config_pair: options.config,
    };
    if let Err(message) = validate(a, b, &diff_options) {
        return error(host, &message);
    }
    let diff = compare(a, b, &diff_options);
    if options.json {
        host.write_out(Stream::Stdout, &render_json(a, b, &diff));
    } else {
        let color = match options.color {
            Color::Auto => rcmake_api::ExplainRenderOptions::for_host(host, Stream::Stdout).color,
            Color::Always => true,
            Color::Never => false,
        };
        host.write_out(Stream::Stdout, render_text(a, b, &diff, color).as_bytes());
    }
    diff.exit_code()
}

#[cfg(test)]
mod tests {
    use rcmake_base::ByteSlice;
    use rcmake_host::PureHost;

    use super::*;

    fn parse_args(args: &[&str]) -> Result<Invocation, String> {
        parse(&args.iter().copied().map(BString::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_pairs_sections_and_config_pairs() {
        let Ok(Invocation::Run(options)) = parse_args(&[
            "a",
            "b",
            "--only=compile,link",
            "--config",
            "Debug:Release",
            "--cache",
            "all",
        ]) else {
            panic!("parse");
        };
        assert_eq!(options.only, Some(vec![Section::Compile, Section::Link]));
        assert_eq!(options.config, Some(("Debug".into(), "Release".into())));
        assert_eq!(options.cache, CacheSet::All);
        for bad in [
            &["a"][..],
            &["a", "b", "c"],
            &["a", "b", "--only", "nope"],
            &["--prepare"],
            &["a", "b", "--cache", "x"],
            &["a", "b", "--only", ","],
            &["a", "b", "--only=compile,"],
            &["a", "b", "--config", ":Release"],
            &["a", "b", "--config=Debug:"],
        ] {
            assert!(parse_args(bad).is_err(), "{bad:?}");
        }
        assert!(matches!(
            parse_args(&["--preset", "x", "dir"]),
            Ok(Invocation::Run(_))
        ));
        assert_eq!(parse_args(&["-h"]), Ok(Invocation::Help));
        for flag in ["--json=1", "--prepare=1", "--no-normalize=1"] {
            let name = flag.split('=').next().unwrap_or_default();
            assert_eq!(
                parse_args(&["a", "b", flag]),
                Err(format!("{name} takes no value.")),
            );
        }
    }

    #[test]
    fn prepare_suggests_rcmake_with_quoted_directories() {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/my b/CMakeCache.txt"), b"")
            .expect("seed");
        assert_eq!(
            run(None, &["--prepare".into(), "/my b".into()], host.clone()),
            0
        );
        assert!(
            host.stream_bytes(Stream::Stdout)
                .contains_str("for example: rcmake -B \"/my b\"")
        );
    }

    #[test]
    fn file_api_note_appears_only_for_sections_that_read_it() {
        let host = Arc::new(PureHost::default());
        let cache = "CMAKE_HOME_DIRECTORY:INTERNAL=/src\nCMAKE_BUILD_TYPE:STRING=Debug\n";
        for dir in ["/a", "/b"] {
            host.seed_file(&Path::new(dir).join("CMakeCache.txt"), cache.as_bytes())
                .expect("seed");
        }
        let args = |only: &str| {
            ["/a", "/b", "--only", only]
                .into_iter()
                .map(BString::from)
                .collect::<Vec<_>>()
        };
        run(None, &args("cache"), host.clone());
        assert!(host.stream_bytes(Stream::Stderr).is_empty());
        run(None, &args("compile"), host.clone());
        assert!(
            host.stream_bytes(Stream::Stderr)
                .contains_str("has no current File API reply")
        );
    }

    #[test]
    fn prepare_creates_exactly_four_empty_files_once() {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/b/CMakeCache.txt"), b"")
            .expect("seed");
        assert_eq!(
            run(None, &["--prepare".into(), "/b".into()], host.clone()),
            0
        );
        for file in QUERY_FILES {
            let path = Path::new("/b/.cmake/api/v1/query").join(file);
            let meta = host.metadata(&path, Follow::No).expect("query file");
            assert_eq!((meta.kind, meta.len), (FileKind::File, 0));
        }
        let entries = host
            .read_dir(Path::new("/b/.cmake/api/v1/query"))
            .expect("dir");
        assert_eq!(entries.len(), 4);
        let before = host.stream_bytes(Stream::Stdout).len();
        assert_eq!(
            run(None, &["--prepare".into(), "/b".into()], host.clone()),
            0
        );
        let after = host.stream_bytes(Stream::Stdout);
        assert!(
            !after
                .get(before..)
                .unwrap_or_default()
                .contains_str("Created")
        );
    }

    #[test]
    fn prepare_rejects_a_linked_query_directory() {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/elsewhere/x"), b"")
            .expect("seed");
        host.seed_file(Path::new("/b/CMakeCache.txt"), b"")
            .expect("seed");
        host.create_dir_all(Path::new("/b/.cmake/api"))
            .expect("dir");
        host.symlink(Path::new("/elsewhere"), Path::new("/b/.cmake/api/v1"))
            .expect("link");
        assert_eq!(
            run(None, &["--prepare".into(), "/b".into()], host.clone()),
            2
        );
        assert!(
            host.metadata(Path::new("/elsewhere/query"), Follow::No)
                .is_err()
        );
    }

    #[test]
    fn exit_codes_follow_diff_and_incomplete_evidence() {
        let host = Arc::new(PureHost::default());
        let cache = "CMAKE_HOME_DIRECTORY:INTERNAL=/src\nCMAKE_BUILD_TYPE:STRING=Debug\n";
        host.seed_file(Path::new("/a/CMakeCache.txt"), cache.as_bytes())
            .expect("seed");
        host.seed_file(Path::new("/b/CMakeCache.txt"), cache.as_bytes())
            .expect("seed");
        host.seed_file(
            Path::new("/c/CMakeCache.txt"),
            b"CMAKE_BUILD_TYPE:STRING=Release\n",
        )
        .expect("seed");
        let run_args = |args: &[&str]| {
            run(
                None,
                &args.iter().copied().map(BString::from).collect::<Vec<_>>(),
                host.clone(),
            )
        };
        assert_eq!(run_args(&["/a", "/b", "--only", "cache"]), 0);
        assert_eq!(run_args(&["/a", "/c", "--only", "cache"]), 1);
        assert_eq!(run_args(&["/a", "/b"]), 2);
        assert_eq!(run_args(&["/a", "/missing"]), 2);
        assert_eq!(run_args(&["/a"]), 2);
        assert_eq!(
            run_args(&["/a", "/b", "--only", "cache", "--config", "Release"]),
            2,
            "a Debug tree has no Release configuration"
        );
        assert_eq!(
            run_args(&["/a", "/b", "--only", "cache", "--color=always"]),
            0
        );
        assert!(
            host.stream_bytes(Stream::Stdout)
                .contains_str("\x1b[32mNo differences.\x1b[0m")
        );
    }

    #[test]
    fn lenient_cache_reader_matches_the_cli_cache_loader() {
        let host = PureHost::default();
        let text = "# header\n//Build type\nCMAKE_BUILD_TYPE:STRING=Release\n//Opt\n\"QUOTED:NAME\":BOOL=ON\nFoo_DIR:PATH=Foo_DIR-NOTFOUND\nSPACED:STRING='  a b  '\nX-ADVANCED:INTERNAL=1\n";
        host.seed_file(Path::new("/c/CMakeCache.txt"), text.as_bytes())
            .expect("seed");
        let cli = crate::cache::load(&host, Path::new("/c")).expect("load");
        let lenient = rcmake_buildinfo::parse_cache(text.as_bytes());
        for (name, record) in &lenient {
            if name.ends_with(b"-ADVANCED") {
                continue;
            }
            let entry = cli.get(name).unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(entry.value, record.value, "{name}");
            assert_eq!(
                Some(entry.ty),
                rcmake_model::CacheType::parse(&record.ty),
                "{name}"
            );
        }
    }

    #[test]
    fn parses_save_mode_and_rejects_conflicting_flags() {
        assert!(matches!(
            parse_args(&["--save", "out.json", "build-a"]),
            Ok(Invocation::Run(_))
        ));
        assert!(matches!(
            parse_args(&["--save=out.json", "--preset", "debug", "-S", "/src"]),
            Ok(Invocation::Run(_))
        ));
        assert!(matches!(
            parse_args(&[
                "--save",
                "out.json",
                "--prepare",
                "--cache",
                "all",
                "build-a"
            ]),
            Ok(Invocation::Run(_))
        ));
        for bad in [
            &["--save", "out.json"][..],
            &["--save", "out.json", "a", "b"],
            &["--save", "out.json", "a", "--no-normalize"],
            &["--save", "out.json", "a", "--json"],
            &["--save", "out.json", "a", "--only", "cache"],
            &["--save", "out.json", "a", "--target", "app"],
            &["--save", "out.json", "a", "--config", "Debug"],
        ] {
            assert!(parse_args(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn save_and_compare_snapshot_vs_snapshot_and_snapshot_vs_live() {
        let host = Arc::new(PureHost::default());
        host.env_set(b"HOME", Some(b"/Users/alice"));
        let cache_a = "\
CMAKE_HOME_DIRECTORY:INTERNAL=/Users/alice/proj\n\
CMAKE_BUILD_TYPE:STRING=Debug\n\
API_KEY:STRING=secret-token\n\
MY_OPT:BOOL=ON\n";
        let cache_b = "\
CMAKE_HOME_DIRECTORY:INTERNAL=/Users/alice/proj\n\
CMAKE_BUILD_TYPE:STRING=Debug\n\
API_KEY:STRING=different-secret\n\
MY_OPT:BOOL=ON\n";
        host.seed_file(
            Path::new("/Users/alice/build-a/CMakeCache.txt"),
            cache_a.as_bytes(),
        )
        .expect("seed a");
        host.seed_file(
            Path::new("/Users/alice/build-b/CMakeCache.txt"),
            cache_b.as_bytes(),
        )
        .expect("seed b");

        let run_args = |args: &[&str]| {
            run(
                None,
                &args.iter().copied().map(BString::from).collect::<Vec<_>>(),
                host.clone(),
            )
        };

        // Save snapshot without .json extension to verify marker-based detection (not extension).
        assert_eq!(
            run_args(&["--save", "/snaps/baseline", "/Users/alice/build-a"]),
            0
        );
        assert_eq!(
            run_args(&["--save", "/snaps/second.snap", "/Users/alice/build-b"]),
            0
        );
        let saved_bytes = host
            .read(
                Path::new("/snaps/baseline"),
                rcmake_host::ReadRange::All { limit: 1 << 20 },
            )
            .expect("read snapshot");
        assert!(
            !saved_bytes.contains_str("secret-token"),
            "default --cache key must not persist API_KEY"
        );
        assert!(
            !saved_bytes.contains_str("/Users/alice"),
            "saved snapshot must not leak home directory"
        );

        // snapshot vs snapshot == 0 on complete section (--only cache).
        assert_eq!(
            run_args(&["/snaps/baseline", "/snaps/second.snap", "--only", "cache"]),
            0
        );
        // snapshot vs live build dir == 0 on complete section.
        assert_eq!(
            run_args(&["/snaps/baseline", "/Users/alice/build-a", "--only", "cache"]),
            0
        );
        // live build dir vs snapshot == 0 on complete section.
        assert_eq!(
            run_args(&["/Users/alice/build-a", "/snaps/baseline", "--only", "cache"]),
            0
        );
        // Unavailable sections (no File API) preserved from save time -> exit 2!
        assert_eq!(run_args(&["/snaps/baseline", "/snaps/second.snap"]), 2);

        // --no-normalize combined with a snapshot operand -> exit 2 usage error!
        assert_eq!(
            run_args(&[
                "/snaps/baseline",
                "/Users/alice/build-a",
                "--only",
                "cache",
                "--no-normalize"
            ]),
            2
        );
        assert_eq!(
            run_args(&[
                "/Users/alice/build-a",
                "/snaps/baseline",
                "--only",
                "cache",
                "--no-normalize"
            ]),
            2
        );
        assert_eq!(
            run_args(&[
                "--save",
                "/snaps/nope",
                "/Users/alice/build-a",
                "--no-normalize"
            ]),
            2
        );

        // Non-snapshot .json file is rejected cleanly with exit 2.
        host.seed_file(Path::new("/snaps/not_a_snapshot.json"), br#"{"hello":1}"#)
            .expect("seed json");
        assert_eq!(
            run_args(&[
                "/snaps/not_a_snapshot.json",
                "/Users/alice/build-a",
                "--only",
                "cache"
            ]),
            2
        );
    }

    #[test]
    fn save_supports_preset_prepare_and_cache_all() {
        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/src/CMakePresets.json"),
            br#"{
                "version": 3,
                "configurePresets": [
                    {"name": "dev", "binaryDir": "/builds/dev"}
                ]
            }"#,
        )
        .expect("seed presets");
        host.seed_file(
            Path::new("/builds/dev/CMakeCache.txt"),
            b"CMAKE_HOME_DIRECTORY:INTERNAL=/src\nCMAKE_BUILD_TYPE:STRING=Debug\nAPI_KEY:STRING=kept-on-all\n",
        )
        .expect("seed cache");

        let run_args = |args: &[&str]| {
            run(
                None,
                &args.iter().copied().map(BString::from).collect::<Vec<_>>(),
                host.clone(),
            )
        };

        assert_eq!(
            run_args(&[
                "--save",
                "/out/dev.json",
                "--preset",
                "dev",
                "-S",
                "/src",
                "--prepare",
                "--cache",
                "all"
            ]),
            0
        );
        // --prepare created the 4 File API query files.
        for file in QUERY_FILES {
            assert!(
                host.metadata(
                    &Path::new("/builds/dev/.cmake/api/v1/query").join(file),
                    Follow::No
                )
                .is_ok()
            );
        }
        // --cache all kept API_KEY.
        let bytes = host
            .read(
                Path::new("/out/dev.json"),
                rcmake_host::ReadRange::All { limit: 1 << 20 },
            )
            .expect("read");
        assert!(bytes.contains_str("kept-on-all"));
        assert_eq!(
            run_args(&[
                "/out/dev.json",
                "--preset",
                "dev",
                "-S",
                "/src",
                "--only",
                "cache",
                "--cache",
                "all"
            ]),
            0
        );
    }
}
