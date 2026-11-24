//! Run the Native persona's explain subcommand.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::tz::TimeZone;
use rcmake_api::{
    ApiError, CacheDefine, CompletionState, ConfigureRequest, ExplainQuery, ExplainRenderOptions,
    ExplainReport, LedgerLevel, ProcessPolicy, Services, WriteMode, render_explain_json,
    render_explain_terminal, run_configure,
};
use rcmake_base::BString;
use rcmake_diag::DiagOptions;
use rcmake_host::{
    CopyOptions, DirEntry, FileKind, FileLockGuard, FileTime, Follow, GracefulTimeout, Host,
    HostResult, Metadata, ReadRange, RunOutput, RunRequest, Stream, SystemInfo,
    WriteMode as HostWriteMode, path,
};
use rcmake_model::GeneratorKind;

use crate::{cache, configure::initialize_diagnostics, resolve_cli_path};

const HELP: &str = "\
Usage: rcmake explain <NAME> [-B <build-dir>] [--target | --var]
                      [--config <CFG>] [--filter <VAL>] [--json]
                      [--verbose] [--color=auto|always|never]

Show where a target gets its build settings or where a variable gets its value.

  -B <build-dir>  Read CMakeCache.txt from this directory. Default: current directory.
  --target        Look up a target or alias.
  --var           Look up a variable.
  --config <CFG>  Show one target configuration.
  --filter <VAL>  Show the exact target value or compiler flag.
  --json          Write one JSON report to standard output.
  --verbose       Show full values and call stacks in terminal output.
  --color=<mode>  Set terminal color. Default: auto.
  -h, --help      Show this help.

Examples
  rcmake explain app
  rcmake explain app --filter -DFOO=1
  rcmake explain CMAKE_CXX_FLAGS --json

Exit status
  0  Found the item.
  1  The query failed, found no item, or matched both a target and a variable.
  2  Invalid arguments or an unavailable configured build directory.
";

const CONFIGURE_FIRST: &str = "Run configure first, or pass -B <build-dir>.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    Auto,
    Target,
    Variable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Color {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    name: BString,
    build: Option<BString>,
    kind: Kind,
    config: Option<BString>,
    filter: Option<BString>,
    json: bool,
    verbose: bool,
    color: Color,
}

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Help,
    Query(Options),
}

/// Run explain after the Native dispatcher consumes the subcommand name.
///
/// Pass the executable name separately so module discovery can use it.
pub(crate) fn run(argv0: Option<&BString>, args: &[BString], host: Arc<dyn Host>) -> i32 {
    let options = match parse(args) {
        Ok(Invocation::Help) => {
            host.write_out(Stream::Stdout, HELP.as_bytes());
            return 0;
        }
        Ok(Invocation::Query(options)) => options,
        Err(message) => {
            return error(
                host.as_ref(),
                &format!("{message} Run rcmake explain --help."),
                2,
            );
        }
    };
    let request = match request(&options, argv0, &host) {
        Ok(request) => request,
        Err(message) => return error(host.as_ref(), &message, 2),
    };
    let query = match options.kind {
        Kind::Auto => ExplainQuery::Auto(options.name.clone()),
        Kind::Target => ExplainQuery::Target(options.name.clone()),
        Kind::Variable => ExplainQuery::Variable(options.name.clone()),
    };
    let result = match run_configure(request) {
        Ok(result) => result,
        Err(err) => {
            let code = if matches!(err, ApiError::InvalidConfig { .. }) {
                2
            } else {
                1
            };
            return error(host.as_ref(), &err.to_string(), code);
        }
    };
    if let CompletionState::Partial {
        denied_processes,
        omitted_denials,
        cancelled,
        ..
    } = &result.completion
    {
        let count = denied_processes.len().saturating_add(*omitted_denials);
        let reason = if *cancelled {
            "Configure was cancelled.".to_owned()
        } else if count != 0 {
            format!("Rcmake denied {count} subprocess requests.")
        } else {
            "Configure stopped before it completed.".to_owned()
        };
        host.write_out(
            Stream::Stderr,
            format!("Partial configure. {reason}\n").as_bytes(),
        );
    }
    let report = result.explain_filtered(&query, options.filter.clone());
    if matches!(report, ExplainReport::Variable(_))
        && (options.filter.is_some() || options.config.is_some())
    {
        return error(
            host.as_ref(),
            "--config and --filter apply to targets. Use --target to select a target.",
            2,
        );
    }
    write_report(host.as_ref(), &options, &report)
}

fn request(
    options: &Options,
    argv0: Option<&BString>,
    host: &Arc<dyn Host>,
) -> Result<ConfigureRequest, String> {
    let build = options.build.as_ref().map_or_else(
        || host.initial_cwd(),
        |dir| resolve_cli_path(host.as_ref(), dir),
    );
    let mut saved = cache::load(host.as_ref(), &build).map_err(|err| {
        format!(
            "Cannot read {}. {err}",
            build.join("CMakeCache.txt").display()
        )
    })?;
    let source = saved
        .get(b"CMAKE_HOME_DIRECTORY")
        .filter(|entry| !entry.value.is_empty())
        .map(|entry| path::to_native(&entry.value));
    let missing = || {
        format!(
            "No configured build directory at {}. {CONFIGURE_FIRST}",
            build.display()
        )
    };
    let Some(source) = source.filter(|source| source.is_absolute()) else {
        return Err(missing());
    };
    let generator = saved
        .get(b"CMAKE_GENERATOR")
        .filter(|entry| !entry.value.is_empty())
        .ok_or_else(missing)?;
    let kind = GeneratorKind::from_name(&generator.value).ok_or_else(|| {
        format!(
            "Rcmake does not support the cached generator {}.",
            generator.value
        )
    })?;
    let lists = source.join("CMakeLists.txt");
    if !host
        .metadata(&lists, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return Err(format!(
            "Cannot read {}. {CONFIGURE_FIRST}",
            lists.display()
        ));
    }
    let (exe, cmake_root) = rcmake_host::discover_cmake_root(host.as_ref(), argv0);
    let cmake_root = cmake_root.ok_or_else(|| {
        "Cannot find the CMake modules. Set RCMAKE_VENDORED_DATA_ROOT to the CMake data directory."
            .to_owned()
    })?;
    let diagnostics = initialize_diagnostics(host.as_ref(), &mut saved, &[], &[], true);
    let configure_host = Arc::new(ReportHost {
        base: Arc::clone(host),
        exe,
    });
    let mut request = ConfigureRequest::new(source, build, cmake_root, configure_host)
        .generator(kind)
        .write_mode(WriteMode::Overlay)
        .process_policy(ProcessPolicy::Deny)
        .services(Services {
            provenance: LedgerLevel::Full { per_name_cap: 64 },
            ..Services::default()
        })
        .diag(DiagOptions {
            initial_state: diagnostics,
            ..DiagOptions::default()
        });
    request.defines = cache::sorted(&saved)
        .into_iter()
        .map(|(name, entry)| CacheDefine {
            name: name.clone(),
            ty: Some(entry.ty),
            value: entry.value.clone(),
        })
        .collect();
    Ok(request)
}

fn write_report(host: &dyn Host, options: &Options, report: &ExplainReport) -> i32 {
    let config = options.config.as_ref().map(|value| value.as_slice());
    if options.json {
        let mut json = render_explain_json(report, config);
        json.push(b'\n');
        host.write_out(Stream::Stdout, &json);
    } else {
        let rendered = render_explain_terminal(report, render_options(host, options));
        host.write_out(Stream::Stdout, rendered.as_bytes());
    }
    match report {
        ExplainReport::Target(_) | ExplainReport::Variable(_) => 0,
        ExplainReport::Ambiguous { .. }
        | ExplainReport::NotFound { .. }
        | ExplainReport::InvalidDirectory { .. } => 1,
    }
}

fn render_options<'a>(host: &dyn Host, options: &'a Options) -> ExplainRenderOptions<'a> {
    let mut render = ExplainRenderOptions::for_host(host, Stream::Stdout);
    render.config = options.config.as_ref().map(|value| value.as_slice());
    render.verbose = options.verbose;
    match options.color {
        Color::Auto => {}
        Color::Always => render.color = true,
        Color::Never => {
            render.color = false;
            render.hyperlinks = false;
        }
    }
    render
}

fn error(host: &dyn Host, message: &str, code: i32) -> i32 {
    host.write_out(
        Stream::Stderr,
        format!("rcmake explain: {message}\n").as_bytes(),
    );
    code
}

fn parse(args: &[BString]) -> Result<Invocation, String> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let raw = arg.as_slice();
        if raw == b"--" {
            if let Some(name) = args.next() {
                set_name(&mut options, name)?;
            }
            if args.next().is_some() {
                return Err("Specify one name.".into());
            }
            break;
        }
        let mut parts = raw.splitn(2, |byte| *byte == b'=');
        let flag = parts.next().unwrap_or(raw);
        let attached = parts.next();
        match (flag, attached) {
            (b"--help" | b"-h", None) => return Ok(Invocation::Help),
            (b"--target", None) => set_kind(&mut options, Kind::Target)?,
            (b"--var", None) => set_kind(&mut options, Kind::Variable)?,
            (b"--json", None) => options.json = true,
            (b"--verbose", None) => options.verbose = true,
            (b"--config", _) => options.config = Some(value(&mut args, attached, "--config")?),
            (b"--filter", _) => options.filter = Some(value(&mut args, attached, "--filter")?),
            (b"--color", _) => {
                options.color = match value(&mut args, attached, "--color")?.as_slice() {
                    b"auto" => Color::Auto,
                    b"always" => Color::Always,
                    b"never" => Color::Never,
                    _ => return Err("Use --color=auto, --color=always, or --color=never.".into()),
                };
            }
            (b"-B", _) => options.build = Some(value(&mut args, attached, "-B")?),
            _ => {
                if let Some(dir) = raw.strip_prefix(b"-B") {
                    options.build = Some(dir.into());
                } else if raw.starts_with(b"-") {
                    return Err(format!("Unknown option {arg:?}."));
                } else {
                    set_name(&mut options, arg)?;
                }
            }
        }
    }
    if options.name.is_empty() {
        return Err("Specify the name of a target or variable.".into());
    }
    if options.kind == Kind::Variable && (options.filter.is_some() || options.config.is_some()) {
        return Err(
            "--config and --filter apply to targets. Omit --var to use either option.".into(),
        );
    }
    Ok(Invocation::Query(options))
}

fn value<'a>(
    args: &mut impl Iterator<Item = &'a BString>,
    attached: Option<&[u8]>,
    flag: &str,
) -> Result<BString, String> {
    let value = attached.or_else(|| {
        args.next()
            .map(|value| value.as_slice())
            .filter(|arg| !arg.starts_with(b"--") && !matches!(*arg, b"-B" | b"-h"))
    });
    value
        .filter(|value| !value.is_empty())
        .map(BString::from)
        .ok_or_else(|| format!("Supply a value for {flag}."))
}

fn set_name(options: &mut Options, name: &BString) -> Result<(), String> {
    if name.is_empty() || !options.name.is_empty() {
        return Err("Specify one nonempty name.".into());
    }
    options.name = name.clone();
    Ok(())
}

fn set_kind(options: &mut Options, kind: Kind) -> Result<(), String> {
    if options.kind != Kind::Auto && options.kind != kind {
        return Err("Use either --target or --var.".into());
    }
    options.kind = kind;
    Ok(())
}

// Keep configure messages on stderr even when CMake writes them to stdout.
struct ReportHost {
    base: Arc<dyn Host>,
    exe: PathBuf,
}

impl Host for ReportHost {
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        self.base.metadata(p, follow)
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.base.read(p, range)
    }

    fn read_chunks(&self, p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        self.base.read_chunks(p, consume)
    }

    fn write(&self, p: &Path, data: &[u8], mode: HostWriteMode) -> HostResult<()> {
        self.base.write(p, data, mode)
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        self.base.read_dir(p)
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        self.base.create_dir_all(p)
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        self.base.remove_file(p)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        self.base.remove_dir(p)
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        self.base.remove_dir_all(p)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.base.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.base.rename_noreplace(from, to)
    }

    fn lock_file(&self, p: &Path, timeout: Option<Duration>) -> HostResult<Box<dyn FileLockGuard>> {
        self.base.lock_file(p, timeout)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        self.base.copy_file(from, to, opts)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.base.symlink(target, link)
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        self.base.hard_link(src, dst)
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        self.base.read_link(p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        self.base.canonicalize(p)
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        self.base.set_permissions(p, mode)
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.base.set_times(p, atime, mtime)
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        self.base.env_get(key)
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        self.base.env_set(key, value);
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.base.env_all()
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.base.run(req, on_output)
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.base.run_with_timeout_signal(req, timeout, on_output)
    }

    fn run_bounded(&self, req: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        self.base.run_bounded(req, limit)
    }

    fn write_out(&self, _stream: Stream, bytes: &[u8]) {
        self.base.write_out(Stream::Stderr, bytes);
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        self.base.read_stdin(limit)
    }

    fn is_terminal(&self, _stream: Stream) -> bool {
        self.base.is_terminal(Stream::Stderr)
    }

    fn now(&self) -> FileTime {
        self.base.now()
    }

    fn monotonic(&self) -> Duration {
        self.base.monotonic()
    }

    fn sleep(&self, duration: Duration) {
        self.base.sleep(duration);
    }

    fn time_zone(&self) -> TimeZone {
        self.base.time_zone()
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        Ok(self.exe.clone())
    }

    fn initial_cwd(&self) -> PathBuf {
        self.base.initial_cwd()
    }

    fn system(&self) -> SystemInfo {
        self.base.system()
    }
}

#[cfg(test)]
mod tests {
    use rcmake_api::{ExplainItemKind, ExplainSuggestion};
    use rcmake_base::ByteSlice;
    use rcmake_host::PureHost;

    use super::*;

    fn parse_args(args: &[&str]) -> Result<Invocation, String> {
        parse(&args.iter().copied().map(BString::from).collect::<Vec<_>>())
    }

    #[test]
    fn defaults_to_auto_in_the_current_directory() {
        assert_eq!(
            parse_args(&["app"]),
            Ok(Invocation::Query(Options {
                name: "app".into(),
                ..Options::default()
            }))
        );
    }

    #[test]
    fn parses_target_configuration_filter_and_output_flags() {
        assert_eq!(
            parse_args(&[
                "--target",
                "app",
                "-Bout",
                "--config=Debug",
                "--filter",
                "-DFOO=1",
                "--json",
                "--verbose",
                "--color=always",
            ]),
            Ok(Invocation::Query(Options {
                name: "app".into(),
                build: Some("out".into()),
                kind: Kind::Target,
                config: Some("Debug".into()),
                filter: Some("-DFOO=1".into()),
                json: true,
                verbose: true,
                color: Color::Always,
            }))
        );
        assert_eq!(
            parse_args(&["--var", "-B", "build", "--", "--flag"]),
            Ok(Invocation::Query(Options {
                name: "--flag".into(),
                build: Some("build".into()),
                kind: Kind::Variable,
                ..Options::default()
            }))
        );
    }

    #[test]
    fn rejects_invalid_argument_combinations() {
        for args in [
            &[][..],
            &[""][..],
            &["app", "util"][..],
            &["app", "--unknown"][..],
            &["app", "--target", "--var"][..],
            &["app", "--var", "--filter=FOO=1"][..],
            &["app", "-B"][..],
            &["app", "--config", "--json"][..],
            &["app", "--filter="][..],
            &["app", "--color=other"][..],
            &["app", "--json=yes"][..],
        ] {
            assert!(parse_args(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn help_runs_without_a_configured_directory() {
        assert_eq!(parse_args(&["--help"]), Ok(Invocation::Help));
        let host = Arc::new(PureHost::default());
        assert_eq!(run(None, &["-h".into()], host.clone()), 0);
        let output = host.stream_bytes(Stream::Stdout);
        for example in [
            "rcmake explain app",
            "rcmake explain app --filter -DFOO=1",
            "rcmake explain CMAKE_CXX_FLAGS --json",
        ] {
            assert!(output.contains_str(example));
        }
        assert!(host.stream_bytes(Stream::Stderr).is_empty());
    }

    #[test]
    fn missing_build_directory_has_a_usage_status_and_action() {
        let host = Arc::new(PureHost::default());
        assert_eq!(run(None, &["app".into(), "--json".into()], host.clone()), 2);
        assert!(host.stream_bytes(Stream::Stdout).is_empty());
        assert!(
            host.stream_bytes(Stream::Stderr)
                .contains_str(CONFIGURE_FIRST)
        );
    }

    #[test]
    fn configured_cache_drives_the_query_and_keeps_json_stdout_clean() -> Result<(), String> {
        let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
        let host = Arc::new(PureHost::new(root.to_owned(), root.join("bin/rcmake")));
        let source = root.join("src");
        host.seed_file(
            &source.join("CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
set(CMAKE_SYSTEM Generic)
set(CMAKE_SYSTEM_LOADED 1)
set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)
set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)
project(ExplainTest LANGUAGES NONE)
add_library(app INTERFACE)
set(app variable)
message(STATUS "Configure message")
execute_process(COMMAND skipped RESULT_VARIABLE status)
"#,
        )
        .map_err(|error| error.to_string())?;
        host.seed_file(&root.join("data/Modules/CMakeDetermineSystem.cmake"), b"")
            .map_err(|error| error.to_string())?;
        let source = path::from_native_cmake(&source);
        let contents = format!(
            "CMAKE_HOME_DIRECTORY:INTERNAL={source}\nCMAKE_GENERATOR:INTERNAL=Ninja\n             CMAKE_CXX_FLAGS:STRING=-O2\n"
        );
        host.seed_file(&root.join("out/CMakeCache.txt"), contents.as_bytes())
            .map_err(|error| error.to_string())?;
        host.env_set(b"RCMAKE_VENDORED_DATA_ROOT", Some(b"data"));
        let shared: Arc<dyn Host> = host.clone();
        let options = Options {
            build: Some("out".into()),
            ..Options::default()
        };
        let req = request(&options, None, &shared)?;
        assert_eq!(req.generator, Some(GeneratorKind::Ninja));
        assert_eq!(req.source_dir, root.join("src"));
        assert_eq!(req.build_dir, root.join("out"));
        assert_eq!(req.write_mode, WriteMode::Overlay);
        assert_eq!(req.process_policy, ProcessPolicy::Deny);
        assert!(
            req.defines
                .iter()
                .any(|def| def.name == b"CMAKE_CXX_FLAGS" && def.value == b"-O2")
        );
        assert_eq!(
            run(
                None,
                &["app".into(), "-B".into(), "out".into(), "--json".into()],
                shared
            ),
            1
        );
        let json = host.stream_bytes(Stream::Stdout);
        assert!(json.starts_with(b"{"));
        assert!(json.contains_str("\"kind\":\"ambiguous\""));
        assert_eq!(json.split(|&byte| byte == b'\n').count(), 2);
        let stderr = host.stream_bytes(Stream::Stderr);
        assert!(stderr.contains_str("Configure message"));
        assert!(stderr.contains_str("Partial configure."));
        Ok(())
    }

    #[test]
    fn ambiguity_and_suggestions_use_the_report_status_and_format() {
        let ambiguous = ExplainReport::Ambiguous {
            name: "app".into(),
            kinds: [ExplainItemKind::Target, ExplainItemKind::Variable],
        };
        let missing = ExplainReport::NotFound {
            name: "ap".into(),
            suggestions: vec![ExplainSuggestion {
                name: "app".into(),
                kind: ExplainItemKind::Target,
            }],
        };
        let host = PureHost::default();
        assert_eq!(write_report(&host, &Options::default(), &ambiguous), 1);
        assert_eq!(write_report(&host, &Options::default(), &missing), 1);
        let output = host.stream_bytes(Stream::Stdout);
        assert!(output.contains_str("Use --target or --var."));
        assert!(output.contains_str("--target app"));

        let host = PureHost::default();
        host.env_set(b"CLICOLOR_FORCE", Some(b"1"));
        let options = Options {
            json: true,
            color: Color::Always,
            ..Options::default()
        };
        assert_eq!(write_report(&host, &options, &ambiguous), 1);
        let json = host.stream_bytes(Stream::Stdout);
        assert!(json.contains_str("\"kind\":\"ambiguous\""));
        assert!(json.contains_str("\"schema\":\"rcmake-explain-v1\""));
        assert_eq!(json.split(|&byte| byte == b'\n').count(), 2);
        assert!(!json.contains(&0x1b));
    }

    #[test]
    fn explicit_color_mode_precedes_environment() {
        let host = PureHost::default();
        let mut options = Options::default();
        assert!(!render_options(&host, &options).color);
        host.env_set(b"CLICOLOR_FORCE", Some(b"0"));
        host.env_set(b"NO_COLOR", Some(b"1"));
        assert!(!render_options(&host, &options).color);
        options.color = Color::Always;
        assert!(render_options(&host, &options).color);
        host.env_set(b"CLICOLOR_FORCE", Some(b"1"));
        options.color = Color::Auto;
        assert!(render_options(&host, &options).color);
        options.color = Color::Never;
        assert!(!render_options(&host, &options).color);
    }

    #[test]
    fn configure_stdout_goes_to_stderr() {
        let base = Arc::new(PureHost::default());
        let host = ReportHost {
            base: base.clone(),
            exe: PathBuf::from("/bin/rcmake"),
        };
        host.write_out(Stream::Stdout, b"configure message\n");
        host.write_out(Stream::Stderr, b"configure diagnostic\n");
        assert!(base.stream_bytes(Stream::Stdout).is_empty());
        assert_eq!(
            base.stream_bytes(Stream::Stderr),
            b"configure message\nconfigure diagnostic\n"
        );
    }
}
