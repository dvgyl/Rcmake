use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_host::{
    CommandSpec, FileKind, FileTime, Follow, Host, Input, Output, ProcessResult, ReadRange,
    RunRequest, Stream, path,
};

use super::{Dashboard, definition};
use crate::cli::Invocation;
use crate::dart::{self, Configuration, Tag};
use crate::junit::{escape, write_file};

const MAX_TREE_DEPTH: usize = 128;
const MAX_TREE_ENTRIES: usize = 1_000_000;
const MAX_INPUT_BYTES: u64 = 32 * 1024 * 1024;
const COVERAGE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

pub(super) fn run(
    host: &Arc<dyn Host>,
    invocation: &Invocation,
    dashboard: &Dashboard,
) -> Result<i32, String> {
    super::missing_configuration(host.as_ref(), &dashboard.build);
    let setting = |name, key| {
        definition(invocation, name).map_or_else(
            || dashboard.value(host.as_ref(), invocation, key),
            |value| Ok(BString::from(value)),
        )
    };
    let command = setting(b"CTEST_COVERAGE_COMMAND", b"CoverageCommand")?;
    let flags = setting(b"CTEST_COVERAGE_EXTRA_FLAGS", b"CoverageExtraFlags")?;
    let flags = if flags.is_empty() {
        Vec::new()
    } else {
        crate::script::split_dashboard_command(host, &flags)?
    };
    collect(
        host.as_ref(),
        &dashboard.config,
        &Options {
            source: &dashboard.source,
            build: &dashboard.build,
            command: &command,
            flags: &flags,
            labels: &[],
            append: false,
            log: !invocation.options.quiet,
        },
    )
}

pub(crate) struct Options<'a> {
    pub(crate) source: &'a Path,
    pub(crate) build: &'a Path,
    pub(crate) command: &'a [u8],
    pub(crate) flags: &'a [BString],
    pub(crate) labels: &'a [BString],
    pub(crate) append: bool,
    pub(crate) log: bool,
}

type SourceLabels = BTreeMap<PathBuf, BTreeSet<BString>>;
type Coverage = BTreeMap<PathBuf, BTreeMap<u32, u64>>;

pub(crate) fn collect(
    host: &dyn Host,
    config: &Configuration,
    options: &Options<'_>,
) -> Result<i32, String> {
    let start = host.now();
    let started = host.monotonic();
    if options.log {
        host.write_out(Stream::Stdout, b"Performing coverage\n");
    }
    let mut remaining = MAX_INPUT_BYTES;
    let labels = source_labels(host, options.build, &mut remaining)?;
    let mut coverage = Coverage::new();
    let mut failed = false;
    let mut saw_data = false;
    for file in data_files(host, options.build)? {
        if host.monotonic().saturating_sub(started) >= COVERAGE_TIMEOUT {
            return Err("Coverage collection timed out.".into());
        }
        if file
            .extension()
            .is_some_and(|extension| extension == "gcov")
        {
            let data = read(host, &file, &mut remaining)?;
            parse_gcov(host, options, options.build, &data, &mut coverage);
            saw_data = true;
        } else if !options.command.is_empty() {
            match gcov(host, options, &file, &mut remaining, &mut coverage) {
                Ok(()) => saw_data = true,
                Err(error) => {
                    host.write_out(Stream::Stderr, format!("{error}\n").as_bytes());
                    failed = true;
                }
            }
        }
    }
    if !saw_data {
        if options.log && !failed {
            host.write_out(
                Stream::Stderr,
                b" Cannot find any coverage files. Ignoring Coverage request.\n",
            );
        }
        return Ok(if failed { -1 } else { 0 });
    }
    coverage.retain(|file, _| {
        options.labels.is_empty()
            || labels
                .get(file)
                .is_some_and(|set| options.labels.iter().any(|label| set.contains(label)))
    });
    let tag = Tag::read(host, options.build).map_err(|error| error.to_string())?;
    write_report(host, config, options, &tag, start, &labels, coverage)?;
    Ok(if failed { -1 } else { 0 })
}

fn data_files(host: &dyn Host, build: &Path) -> Result<Vec<PathBuf>, String> {
    let mut pending = vec![(build.to_path_buf(), 0)];
    let mut files = Vec::new();
    let mut visited = 0;
    let testing = build.join("Testing");
    while let Some((dir, depth)) = pending.pop() {
        for entry in host.read_dir(&dir).map_err(|error| error.to_string())? {
            visited += 1;
            if visited > MAX_TREE_ENTRIES {
                return Err("Coverage directory entry limit exceeded.".into());
            }
            let file = dir.join(path::to_native(&entry.name));
            match entry.kind {
                FileKind::Dir
                    if file != testing && !matches!(entry.name.as_slice(), b".git" | b".svn") =>
                {
                    if depth == MAX_TREE_DEPTH {
                        return Err("Coverage directory depth limit exceeded.".into());
                    }
                    pending.push((file, depth + 1));
                }
                FileKind::File
                    if entry.name.ends_with(b".gcda")
                        || entry.name.ends_with(b".da")
                        || entry.name.ends_with(b".gcov") =>
                {
                    files.push(file);
                }
                _ => {}
            }
        }
    }
    files.sort_unstable();
    Ok(files)
}

fn gcov(
    host: &dyn Host,
    options: &Options<'_>,
    file: &Path,
    remaining: &mut u64,
    coverage: &mut Coverage,
) -> Result<(), String> {
    let object_dir = file.parent().unwrap_or(options.build);
    let nonce = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
    let scratch = options.build.join("Testing/Temporary").join(format!(
        "Coverage-{}-{}-{nonce}",
        host.system().pid,
        host.now().secs
    ));
    host.create_dir_all(&scratch)
        .map_err(|error| error.to_string())?;
    let result = (|| {
        let mut args = options.flags.to_vec();
        args.extend([
            "-o".into(),
            path::from_native(object_dir),
            path::from_native(file),
        ]);
        let program = path::to_native(options.command);
        let program = if program.is_relative() && program.components().count() > 1 {
            path::from_native(&options.build.join(program))
        } else {
            options.command.into()
        };
        let output = host
            .run_bounded(
                &RunRequest {
                    pipeline: vec![CommandSpec { program, args }],
                    cwd: scratch.clone(),
                    env: host.env_all(),
                    stdin: Input::Null,
                    stdout: Output::Capture,
                    stderr: Output::Capture,
                    timeout: Some(Duration::from_secs(60)),
                    tee: false,
                },
                1024 * 1024,
            )
            .map_err(|error| error.to_string())?;
        if !matches!(output.results.as_slice(), [ProcessResult::Exited(0)]) {
            let details = BString::from(output.stderr);
            return Err(format!(
                "Coverage command failed for {}. {details}",
                file.display()
            ));
        }
        for entry in host.read_dir(&scratch).map_err(|error| error.to_string())? {
            if entry.kind == FileKind::File && entry.name.ends_with(b".gcov") {
                let data = read(host, &scratch.join(path::to_native(&entry.name)), remaining)?;
                parse_gcov(host, options, object_dir, &data, coverage);
            }
        }
        Ok(())
    })();
    let cleanup = host.remove_dir_all(&scratch);
    result?;
    cleanup.map_err(|error| error.to_string())
}

fn parse_gcov(
    host: &dyn Host,
    options: &Options<'_>,
    object_dir: &Path,
    data: &[u8],
    coverage: &mut Coverage,
) {
    let mut source = None;
    for line in data.split(|&byte| byte == b'\n') {
        let Some((count, rest)) = dart::split(line, b':') else {
            continue;
        };
        let Some((number, text)) = dart::split(rest, b':') else {
            continue;
        };
        let number = std::str::from_utf8(number.trim_ascii())
            .ok()
            .and_then(|number| number.parse::<u32>().ok());
        let Some(number) = number else {
            continue;
        };
        if number == 0 {
            if let Some(file) = text.trim_ascii_end().strip_prefix(b"Source:") {
                source = coverage_source(host, options, object_dir, file);
            }
            continue;
        }
        let Some(file) = &source else {
            continue;
        };
        let count = count.trim_ascii();
        let hits = match count {
            b"#####" | b"=====" => Some(0),
            _ => std::str::from_utf8(count.strip_suffix(b"*").unwrap_or(count))
                .ok()
                .and_then(|count| count.parse::<u64>().ok()),
        };
        if let Some(hits) = hits {
            let total = coverage
                .entry(file.clone())
                .or_default()
                .entry(number)
                .or_default();
            *total = total.saturating_add(hits);
        }
    }
}

fn coverage_source(
    host: &dyn Host,
    options: &Options<'_>,
    object_dir: &Path,
    value: &[u8],
) -> Option<PathBuf> {
    let file = path::to_native(value);
    let path = [options.build, options.source, object_dir]
        .into_iter()
        .map(|dir| normalize(&dir.join(&file)))
        .find(|file| {
            host.metadata(file, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
        })?;
    for (root, other) in [
        (options.source, options.build),
        (options.build, options.source),
    ] {
        if let Ok(relative) = path.strip_prefix(root) {
            if [path.clone(), other.join(relative)].iter().any(|file| {
                file.ancestors()
                    .skip(1)
                    .take_while(|dir| dir.starts_with(root) || dir.starts_with(other))
                    .any(|dir| {
                        host.metadata(&dir.join(".NoDartCoverage"), Follow::Yes)
                            .is_ok()
                    })
            }) {
                return None;
            }
            return Some(path);
        }
    }
    None
}

fn normalize(file: &Path) -> PathBuf {
    path::to_native(&Lexical::new(&path::from_native_cmake(file)).normalize())
}

fn read(host: &dyn Host, file: &Path, remaining: &mut u64) -> Result<Vec<u8>, String> {
    let content = host
        .read(file, ReadRange::All { limit: *remaining })
        .map_err(|error| error.to_string())?;
    *remaining = remaining.saturating_sub(content.len() as u64);
    Ok(content)
}

fn read_optional(host: &dyn Host, file: &Path, remaining: &mut u64) -> Result<Vec<u8>, String> {
    match host.metadata(file, Follow::Yes) {
        Ok(meta) if meta.kind == FileKind::File => read(host, file, remaining),
        Ok(_) => Ok(Vec::new()),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn source_labels(
    host: &dyn Host,
    build: &Path,
    remaining: &mut u64,
) -> Result<SourceLabels, String> {
    let targets = read_optional(
        host,
        &build.join("CMakeFiles/TargetDirectories.txt"),
        remaining,
    )?;
    let mut sources = SourceLabels::new();
    for dir in targets
        .split(|&byte| byte == b'\n')
        .map(<[u8]>::trim_ascii)
        .filter(|dir| !dir.is_empty())
    {
        let file = build.join(path::to_native(dir)).join("Labels.txt");
        let content = read_optional(host, &file, remaining)?;
        let mut target_labels = Vec::new();
        let mut source = None;
        for line in content.split(|&byte| byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            if let Some(label) = line.strip_prefix(b" ") {
                if let Some(source) = &source {
                    sources
                        .entry(PathBuf::clone(source))
                        .or_default()
                        .insert(label.into());
                } else {
                    target_labels.push(BString::from(label));
                }
            } else {
                let file = normalize(&build.join(path::to_native(line)));
                sources
                    .entry(file.clone())
                    .or_default()
                    .extend(target_labels.iter().cloned());
                source = Some(file);
            }
        }
    }
    Ok(sources)
}

fn write_report(
    host: &dyn Host,
    config: &Configuration,
    options: &Options<'_>,
    tag: &Tag,
    start: FileTime,
    labels: &SourceLabels,
    coverage: Coverage,
) -> Result<(), String> {
    let site = escape(&config.site, true);
    let build = escape(&config.build_name, true);
    let stamp = escape(format!("{}-{}", tag.stamp(), tag.track).as_bytes(), true);
    let version = rcmake_base::CMAKE_VERSION_STR;
    let append = if options.append {
        " Append=\"true\""
    } else {
        ""
    };
    let change_id = if config.change_id.is_empty() {
        String::new()
    } else {
        format!(" ChangeId=\"{}\"", escape(&config.change_id, true))
    };
    let date = |time| crate::reporting::format_date(host, time, true, "%b %d %H:%M GMT");
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Site BuildName=\"{build}\" BuildStamp=\"{stamp}\" Name=\"{site}\" Generator=\"ctest-{version}\"{append}{change_id}>\n"
    );
    dart::write_subprojects(&mut xml, &config.labels_for_subprojects);
    let _ = writeln!(
        xml,
        "<Coverage>\n<StartDateTime>{}</StartDateTime>\n<StartTime>{}</StartTime>",
        date(start),
        start.secs
    );
    let mut tested = 0;
    let mut untested = 0;
    for (file, lines) in coverage {
        let file_tested = lines.values().filter(|&&hits| hits > 0).count();
        let file_untested = lines.len() - file_tested;
        tested += file_tested;
        untested += file_untested;
        let name = escape(
            &path::from_native(Path::new(file.file_name().unwrap_or_default())),
            true,
        );
        let relative = [options.build, options.source]
            .into_iter()
            .filter_map(|dir| file.strip_prefix(dir).ok())
            .min_by_key(|path| path.as_os_str().len());
        let short = relative.map_or_else(
            || path::from_native_cmake(&file),
            |path| path::from_native_cmake(&Path::new(".").join(path)),
        );
        let full = escape(&short, true);
        let percent = percent(file_tested, lines.len());
        let metric = (file_tested + 10) as f64 / (lines.len() + 10) as f64;
        let _ = writeln!(
            xml,
            "<File Name=\"{name}\" FullPath=\"{full}\" Covered=\"true\"><LOCTested>{file_tested}</LOCTested><LOCUnTested>{file_untested}</LOCUnTested><BranchesTested>0</BranchesTested><BranchesUnTested>0</BranchesUnTested><PercentCoverage>{percent:.2}</PercentCoverage><CoverageMetric>{metric:.2}</CoverageMetric>"
        );
        if let Some(labels) = labels.get(&file).filter(|labels| !labels.is_empty()) {
            xml.push_str("<Labels>\n");
            for label in labels {
                let _ = writeln!(xml, "<Label>{}</Label>", escape(label, false));
            }
            xml.push_str("</Labels>\n");
        }
        xml.push_str("</File>\n");
    }
    let lines = tested + untested;
    let percent = percent(tested, lines);
    let end = host.now();
    let minutes = end.secs.saturating_sub(start.secs).max(0) / 60;
    let _ = writeln!(
        xml,
        "<LOCTested>{tested}</LOCTested>\n<LOCUntested>{untested}</LOCUntested>\n<LOC>{lines}</LOC>\n<PercentCoverage>{percent:.2}</PercentCoverage>\n<EndDateTime>{}</EndDateTime>\n<EndTime>{}</EndTime>\n<ElapsedMinutes>{minutes}</ElapsedMinutes>\n</Coverage>\n</Site>",
        date(end),
        end.secs
    );
    let file = options
        .build
        .join("Testing")
        .join(tag.stamp())
        .join("Coverage.xml");
    write_file(host, &file, xml.as_bytes()).map_err(|error| error.to_string())?;
    host.write_out(
        Stream::Stdout,
        format!("\n\tCovered LOC:         {tested}\n\tNot covered LOC:     {untested}\n\tTotal LOC:           {lines}\n\tPercentage Coverage: {percent:.2}%\n").as_bytes(),
    );
    Ok(())
}

fn percent(tested: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        100.0 * tested as f64 / total as f64
    }
}
