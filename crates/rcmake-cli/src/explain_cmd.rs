//! Run the Native persona's explain subcommand.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use jiff::tz::TimeZone;
use rcmake_api::{
    ApiError, CompletionState, ConfigureRequest, ExplainFilter, ExplainQuery, ExplainRenderOptions,
    ExplainReport, LedgerLevel, ProcessPolicy, Services, WriteMode, render_explain_json,
    render_explain_terminal, run_configure,
};
use rcmake_base::BString;
use rcmake_diag::DiagOptions;
use rcmake_eval::FindTraceLevel;
use rcmake_host::{FileKind, FileTime, Follow, Host, HostResult, Stream, SystemInfo, path};
use rcmake_model::GeneratorKind;

use crate::{cache, configure::initialize_diagnostics, resolve_cli_path};

const HELP: &str = "\
Usage: rcmake explain <name> [-B <build-dir>] [--target | --var]
                      [--config <cfg>] [--filter <val>] [--json]
                      [--verbose] [--color=auto|always|never]
       rcmake explain <target> --flag|--define|--include|--dep <val> [options]
       rcmake explain --find <package> | why-not <package> [options]
       rcmake explain --eval <expression> [options]

Show where a target gets its build settings or where a variable gets its value.

  -B <build-dir>   Read CMakeCache.txt from this directory. Default: current directory.
  --target         Look up a target or alias.
  --var            Look up a variable.
  --config <cfg>   Select a target configuration. Implies --target.
  --filter <val>   Select an exact target value or compiler flag. Implies --target.
  --flag <flag>    Show where a compile or link flag comes from. Implies --target.
                   Use --flag=<flag> for a flag that starts with --.
  --define <name>  Show where a preprocessor definition comes from. Use NAME or NAME=VALUE.
  --include <dir>  Show where an include directory comes from.
  --dep <target>   Show link and order-only paths from the target to <target>.
                   The search stops at 16 paths, depth 32, or 20000 edges, and
                   then reports that it is incomplete.
  --json           Write one JSON report to standard output.
  --verbose        Show full values and call stacks in terminal output.
  --color=<mode>   Set terminal color. Default: auto.
  -h, --help       Show this help. Use with --find or --eval for their help.

The filter options --filter, --flag, --define, --include, and --dep exclude each other.

Examples
  rcmake explain app
  rcmake explain app --filter -DFOO=1
  rcmake explain app --define USE_SSL=1
  rcmake explain app --dep zlib
  rcmake explain CMAKE_CXX_FLAGS --json

Exit status
  0  Found the item. A target selection can be empty.
     Check the configuration and entry lists in JSON or the selection message
     in terminal output.
  1  The query failed, found no item, matched both a target and a variable,
     named an unavailable configuration, or a typed filter matched nothing.
  2  Invalid arguments or an unavailable configured build directory.
";

const CONFIGURE_FIRST: &str = "Run configure first, or pass -B <build-dir>.";

mod args;
mod eval;
mod find;

use args::{Color, Invocation, Kind, Options, parse};

/// Run explain after the Native dispatcher consumes the subcommand name.
///
/// Pass the executable name separately so module discovery can use it.
pub(crate) fn run(argv0: Option<&BString>, args: &[BString], host: Arc<dyn Host>) -> i32 {
    let options = match parse(args) {
        Ok(Invocation::Help) => {
            host.write_out(Stream::Stdout, HELP.as_bytes());
            return 0;
        }
        Ok(Invocation::FindHelp) => {
            host.write_out(Stream::Stdout, find::HELP.as_bytes());
            return 0;
        }
        Ok(Invocation::EvalHelp) => {
            host.write_out(Stream::Stdout, eval::HELP.as_bytes());
            return 0;
        }
        Ok(Invocation::Find(options)) => return find::run(argv0, &options, &host),
        Ok(Invocation::Eval(options)) => return eval::run(argv0, &options, &host),
        Ok(Invocation::Query(options)) => options,
        Err(message) => {
            return error(
                host.as_ref(),
                &format!("{message} Run rcmake explain --help."),
                2,
            );
        }
    };
    let request = match request(options.build.as_ref(), argv0, &host, FindTraceLevel::Off) {
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
    report_partial(host.as_ref(), &result, options.verbose);
    let mut filter = options.typed.clone();
    if let Some(ExplainFilter::Include(dir)) = &mut filter
        && !path::to_native(dir).is_absolute()
    {
        let root = &result.model.dir(rcmake_model::DirId::ROOT).source_dir;
        *dir =
            rcmake_base::path_lex::Lexical::new(&[root.as_slice(), b"/", dir.as_slice()].concat())
                .normalize();
    }
    let typed = filter.is_some() && options.filter.is_none();
    let report = result.explain_with(&query, filter);
    let code = write_report(host.as_ref(), &options, &report);
    if code == 0 && typed && typed_filter_empty(&report, options.config.as_ref()) {
        return 1;
    }
    code
}

fn typed_filter_empty(report: &ExplainReport, config: Option<&BString>) -> bool {
    let ExplainReport::Target(target) = report else {
        return false;
    };
    if let Some(dependency) = &target.dependency {
        return dependency
            .paths_for(config.map(|name| name.as_slice()))
            .next()
            .is_none();
    }
    !target
        .configurations
        .iter()
        .filter(|entry| config.is_none_or(|name| name.eq_ignore_ascii_case(&entry.name)))
        .any(|entry| !entry.entries.is_empty())
}

/// Return the bundled CMake module directory, for attributing denied subprocesses.
pub(super) fn module_root(host: &dyn Host, argv0: Option<&BString>) -> Option<BString> {
    rcmake_host::discover_cmake_root(host, argv0)
        .1
        .map(|root| path::from_native_cmake(&root))
}

/// Print the partial-configure notice for an overlay result.
pub(super) fn report_partial(host: &dyn Host, result: &rcmake_api::ConfigureResult, verbose: bool) {
    if let CompletionState::Partial {
        denied_processes,
        omitted_denials,
        module_denials,
        project_denial_sites,
        cancelled,
        ..
    } = &result.completion
    {
        let count = denied_processes.len().saturating_add(*omitted_denials);
        let unconfirmed = count.saturating_sub(*module_denials);
        if *cancelled {
            host.write_out(
                Stream::Stderr,
                b"Partial configure. Configure was cancelled.\n",
            );
        } else if unconfirmed > 0 {
            let source_root = &result.model.dir(rcmake_model::DirId::ROOT).source_dir;
            let sites = format_denial_sites(source_root, project_denial_sites);
            let plural = if unconfirmed == 1 { "" } else { "s" };
            let message = if sites.is_empty() {
                format!(
                    "Partial configure. rcmake denied {unconfirmed} subprocess request{plural}. Values that depend on them are provisional.\n"
                )
            } else {
                format!(
                    "Partial configure. rcmake denied {unconfirmed} subprocess request{plural} ({sites}). Values that depend on them are provisional.\n"
                )
            };
            host.write_out(Stream::Stderr, message.as_bytes());
        } else if count > 0 {
            if verbose {
                let message = cached_probes_note(count);
                host.write_out(Stream::Stderr, message.as_bytes());
            }
        } else {
            host.write_out(
                Stream::Stderr,
                b"Partial configure. Configure stopped before it completed.\n",
            );
        }
    }
}

fn cached_probes_note(count: usize) -> String {
    let (plural, verb) = if count == 1 {
        ("", "was")
    } else {
        ("s", "were")
    };
    format!(
        "note: {count} cached CMake probe{plural} {verb} skipped; their values come from CMakeCache.txt.\n"
    )
}

/// Build the overlay re-configure request for a configured build directory.
pub(super) fn request(
    build_arg: Option<&BString>,
    argv0: Option<&BString>,
    host: &Arc<dyn Host>,
    find_trace: FindTraceLevel,
) -> Result<ConfigureRequest, String> {
    let build = build_arg.map_or_else(
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
        let hint = if build_arg.is_none()
            && host
                .metadata(&build.join("CMakeCache.txt"), Follow::Yes)
                .is_err_and(|err| err.kind == std::io::ErrorKind::NotFound)
            && host
                .metadata(&build.join("build/CMakeCache.txt"), Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            "\nHint: found ./build/CMakeCache.txt. Try `rcmake explain ... -B build`."
        } else {
            ""
        };
        format!(
            "No configured build directory at {}. {CONFIGURE_FIRST}{hint}",
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
            "rcmake does not support the cached generator {}.",
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
    Ok(
        ConfigureRequest::new(source, build, cmake_root, configure_host)
            .initial_cache(saved)
            .generator(kind)
            .write_mode(WriteMode::Overlay)
            .process_policy(ProcessPolicy::Deny)
            .services(Services {
                provenance: LedgerLevel::Full { per_name_cap: 64 },
                find_trace,
                ..Services::default()
            })
            .diag(DiagOptions {
                initial_state: diagnostics,
                ..DiagOptions::default()
            }),
    )
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
        ExplainReport::Target(target) => {
            if let Some(requested) = config
                && !target.has_configuration(requested)
            {
                if !options.json {
                    let requested = String::from_utf8_lossy(requested);
                    let note = match target.configurations.as_slice() {
                        [only] if !only.name.is_empty() => format!(
                            "This build directory is configured for {} only. Reconfigure with -DCMAKE_BUILD_TYPE={requested}, or use a multi-config generator.\n",
                            only.name
                        ),
                        [_] => format!(
                            "This build directory is configured for (default) only. Reconfigure with -DCMAKE_BUILD_TYPE={requested}, or use a multi-config generator.\n"
                        ),
                        [] => format!(
                            "Configuration {requested} is unavailable because no configurations were resolved.\n"
                        ),
                        many => {
                            let names = many
                                .iter()
                                .map(|cfg| {
                                    if cfg.name.is_empty() {
                                        "(default)".to_owned()
                                    } else {
                                        cfg.name.to_string()
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("Configuration {requested} is not one of: {names}.\n")
                        }
                    };
                    host.write_out(Stream::Stderr, note.as_bytes());
                }
                return 1;
            }
            0
        }
        ExplainReport::Variable(_) => 0,
        ExplainReport::Ambiguous { .. }
        | ExplainReport::NotFound { .. }
        | ExplainReport::InvalidDirectory { .. } => 1,
    }
}

fn format_denial_sites(source_root: &[u8], sites: &[(BString, u32)]) -> String {
    use rcmake_base::path_lex::Lexical;
    let root = (!source_root.is_empty()).then(|| Lexical::new(source_root).normalize());
    let mut rendered = Vec::new();
    for (file, line) in sites {
        let normalized = Lexical::new(file).normalize();
        let display = root
            .as_ref()
            .and_then(|root| Lexical::new(&normalized).relative(Lexical::new(root)))
            .filter(|rel| rel != ".." && !rel.starts_with(b"../"))
            .unwrap_or(normalized);
        let site = if *line != 0 {
            format!("{}:{line}", String::from_utf8_lossy(&display))
        } else {
            String::from_utf8_lossy(&display).into_owned()
        };
        if !rendered.contains(&site) {
            rendered.push(site);
            if rendered.len() == 3 {
                break;
            }
        }
    }
    rendered.join(", ")
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

pub(super) fn error(host: &dyn Host, message: &str, code: i32) -> i32 {
    host.write_out(
        Stream::Stderr,
        format!("rcmake explain: {message}\n").as_bytes(),
    );
    code
}

// Keep configure messages on stderr even when CMake writes them to stdout.
struct ReportHost {
    base: Arc<dyn Host>,
    exe: PathBuf,
}

impl Host for ReportHost {
    rcmake_host::forward_host!(base, fs, env, process);

    fn registry_host_view(&self) -> Option<rcmake_host::RegistryView> {
        self.base.registry_host_view()
    }

    fn registry_value(
        &self,
        root: rcmake_host::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: rcmake_host::RegistryView,
    ) -> HostResult<Option<BString>> {
        self.base.registry_value(root, subkey, name, view)
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

    fn terminal_width(&self, _stream: Stream) -> Option<usize> {
        self.base.terminal_width(Stream::Stderr)
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

    fn effective_uid(&self) -> Option<u32> {
        self.base.effective_uid()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
                typed: Some(ExplainFilter::Any("-DFOO=1".into())),
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
        let cases: &[&[&str]] = &[
            &[],
            &[""],
            &["app", "util"],
            &["app", "--unknown"],
            &["app", "--target", "--var"],
            &["app", "--var", "--filter=FOO=1"],
            &["app", "-B"],
            &["app", "--config", "--json"],
            &["app", "--filter="],
            &["app", "--color=other"],
            &["app", "--json=yes"],
        ];
        for args in cases {
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
    fn dependency_exit_status_follows_the_requested_configuration() -> Result<(), String> {
        let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
        let host = Arc::new(PureHost::new(root.to_owned(), root.join("bin/rcmake")));
        host.seed_file(
            &root.join("src/CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 4.5)\nproject(Deps LANGUAGES NONE)\nset(CMAKE_CONFIGURATION_TYPES Debug Release)\nadd_library(dbg INTERFACE)\nadd_library(top INTERFACE)\ntarget_link_libraries(top INTERFACE $<$<CONFIG:Debug>:dbg>)\n",
        )
        .map_err(|error| error.to_string())?;
        let mut request = ConfigureRequest::new("src", "build", "cmake", host);
        request.generator = Some(GeneratorKind::NinjaMultiConfig);
        let result = run_configure(request).map_err(|error| error.to_string())?;
        let report = result.explain_with(
            &ExplainQuery::Target("top".into()),
            Some(ExplainFilter::Dependency("dbg".into())),
        );
        assert!(!typed_filter_empty(&report, None));
        assert!(!typed_filter_empty(&report, Some(&"debug".into())));
        assert!(typed_filter_empty(&report, Some(&"Release".into())));
        Ok(())
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
        let lists = br#"
cmake_minimum_required(VERSION 4.5)
set(CMAKE_SYSTEM Generic)
set(CMAKE_SYSTEM_LOADED 1)
set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)
set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)
project(ExplainTest LANGUAGES NONE)
add_library(app INTERFACE)
set(app variable)
set(removed active)
unset(removed)
add_subdirectory(child)
if(DEFINED preload_only OR NOT preload_cache STREQUAL "kept")
  message(FATAL_ERROR "PreLoad variables were not isolated.")
endif()
get_property(mode_options CACHE MODE PROPERTY STRINGS)
if(NOT mode_options STREQUAL "fast;safe")
  message(FATAL_ERROR "Missing saved MODE options.")
endif()
message(STATUS "Configure message")
execute_process(COMMAND skipped RESULT_VARIABLE status)
"#;
        for (path, text) in [
            ("src/CMakeLists.txt", lists.as_slice()),
            ("src/PreLoad.cmake", b"set(preload_only hidden)\nset(preload_cache kept CACHE STRING Preload)\n"),
            ("out/PreLoad.cmake", b"if(DEFINED preload_only)\nmessage(FATAL_ERROR \"Leaked preload variable.\")\nendif()\n"),
            ("src/child/CMakeLists.txt", b"add_library(child INTERFACE)\n"),
        ] {
            host.seed_file(&root.join(path), text)
                .map_err(|error| error.to_string())?;
        }
        host.seed_file(&root.join("data/Modules/CMakeDetermineSystem.cmake"), b"")
            .map_err(|error| error.to_string())?;
        let source = path::from_native_cmake(&source);
        let contents = format!(
            "CMAKE_HOME_DIRECTORY:INTERNAL={source}\nCMAKE_GENERATOR:INTERNAL=Ninja\nCMAKE_CXX_FLAGS:STRING=-O2\n//Build mode\nMODE:STRING=fast\nMODE-ADVANCED:INTERNAL=1\nMODE-STRINGS:INTERNAL=fast;safe\n"
        );
        host.seed_file(&root.join("out/CMakeCache.txt"), contents.as_bytes())
            .map_err(|error| error.to_string())?;
        host.env_set(b"RCMAKE_VENDORED_DATA_ROOT", Some(b"data"));
        let shared: Arc<dyn Host> = host.clone();
        let options = Options {
            build: Some("out".into()),
            ..Options::default()
        };
        let req = request(options.build.as_ref(), None, &shared, FindTraceLevel::Off)?;
        assert_eq!(req.generator, Some(GeneratorKind::Ninja));
        assert_eq!(req.source_dir, root.join("src"));
        assert_eq!(req.build_dir, root.join("out"));
        assert_eq!(req.write_mode, WriteMode::Overlay);
        assert_eq!(req.process_policy, ProcessPolicy::Deny);
        assert!(
            req.initial_cache
                .as_ref()
                .and_then(|cache| cache.get(b"CMAKE_CXX_FLAGS"))
                .is_some_and(|entry| entry.value == b"-O2")
        );
        let result = run_configure(req.collect_index(true)).map_err(|error| error.to_string())?;
        let index = result.index.as_ref().ok_or("Missing index")?;
        let offset = lists
            .windows(b"child)".len())
            .position(|window| window == b"child)")
            .ok_or("Missing child listfile reference")?;
        let site = rcmake_api::SourceLocation {
            path: path::from_native_cmake(&root.join("src/CMakeLists.txt")),
            line: 0,
            byte_offset: u32::try_from(offset).ok(),
            source_id: None,
            range: None,
        };
        assert_eq!(index.resolve_file_edges(None, &site).count(), 1);
        let ExplainReport::Variable(removed) =
            result.explain(&ExplainQuery::Variable("removed".into()))
        else {
            return Err("Missing unset variable history".into());
        };
        assert!(!removed.normal_binding);
        assert!(removed.writes.last().is_some_and(|write| {
            write.command.as_ref().is_some_and(|name| name == b"unset") && write.value.is_none()
        }));

        assert_eq!(
            run(
                None,
                &["app".into(), "-B".into(), "out".into(), "--json".into()],
                shared.clone()
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
        assert_eq!(
            run(
                None,
                &[
                    "app".into(),
                    "-B".into(),
                    "out".into(),
                    "--filter=FOO".into()
                ],
                shared,
            ),
            0
        );
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

    #[test]
    fn unavailable_config_on_single_config_build_returns_one() {
        let report = ExplainReport::Target(Box::new(rcmake_api::ExplainTarget {
            name: "app".into(),
            target: "app".into(),
            kind: rcmake_model::TargetKind::Executable,
            definition: rcmake_diag::Backtrace::default(),
            alias_definition: None,
            sources: Vec::new(),
            roots: rcmake_api::ExplainRoots::default(),
            filter: None,
            typed_filter: None,
            dependency: None,
            configurations: vec![rcmake_api::ExplainConfig {
                name: "Release".into(),
                ..rcmake_api::ExplainConfig::default()
            }],
            properties: Vec::new(),
            property_history_incomplete: Some(false),
            provisional: false,
        }));
        let host = PureHost::default();
        let options = Options {
            config: Some("Debug".into()),
            ..Options::default()
        };
        assert_eq!(write_report(&host, &options, &report), 1);
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(stderr.contains("configured for Release only"), "{stderr}");
        assert!(stderr.contains("-DCMAKE_BUILD_TYPE=Debug"), "{stderr}");
    }

    #[test]
    fn explain_keeps_partial_notice_for_unconfirmed_try_compile_and_project_denials()
    -> Result<(), String> {
        let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
        let host = Arc::new(PureHost::new(root.to_owned(), root.join("bin/rcmake")));
        let source = root.join("src");
        let lists = concat!(
            "cmake_minimum_required(VERSION 4.5)\n",
            "project(ExplainPartial LANGUAGES NONE)\n",
            "try_compile(HAVE_FOO \"${CMAKE_BINARY_DIR}/tc\" \"${CMAKE_SOURCE_DIR}/probe.c\")\n",
            "add_library(app INTERFACE)\n",
        );
        let determine_system = concat!(
            "execute_process(COMMAND uname -m RESULT_VARIABLE uname_rc)\n",
            "set(CMAKE_SYSTEM_NAME PureOS)\n",
            "set(CMAKE_SYSTEM PureOS)\n",
            "set(CMAKE_SYSTEM_LOADED 1)\n",
            "set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)\n",
            "set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)\n",
            "file(WRITE \"${CMAKE_PLATFORM_INFO_DIR}/CMakeSystem.cmake\" \"set(CMAKE_SYSTEM PureOS)\\n\")\n",
        );
        for (path, text) in [
            ("src/CMakeLists.txt", lists.as_bytes()),
            ("src/probe.c", b"int main(void) { return 0; }\n".as_slice()),
            (
                "data/Modules/CMakeDetermineSystem.cmake",
                determine_system.as_bytes(),
            ),
        ] {
            host.seed_file(&root.join(path), text)
                .map_err(|error| error.to_string())?;
        }
        let source_cmake = path::from_native_cmake(&source);
        let cache = format!(
            "CMAKE_HOME_DIRECTORY:INTERNAL={source_cmake}\n\
             CMAKE_GENERATOR:INTERNAL=Unix Makefiles\n\
             CMAKE_MAKE_PROGRAM:FILEPATH=/usr/bin/make\n\
             CMAKE_PLATFORM_INFO_INITIALIZED:INTERNAL=1\n\
             HAVE_FOO:INTERNAL=TRUE\n"
        );
        host.seed_file(&root.join("out/CMakeCache.txt"), cache.as_bytes())
            .map_err(|error| error.to_string())?;
        host.env_set(b"RCMAKE_VENDORED_DATA_ROOT", Some(b"data"));
        let shared: Arc<dyn Host> = host.clone();
        assert_eq!(
            run(None, &["app".into(), "-B".into(), "out".into()], shared),
            0
        );
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(stderr.contains("Partial configure."), "{stderr}");
        assert!(stderr.contains("denied 2 subprocess requests"), "{stderr}");
        Ok(())
    }

    #[test]
    fn cached_probes_note_uses_grammatical_number() {
        assert_eq!(
            cached_probes_note(1),
            "note: 1 cached CMake probe was skipped; their values come from CMakeCache.txt.\n"
        );
        assert_eq!(
            cached_probes_note(2),
            "note: 2 cached CMake probes were skipped; their values come from CMakeCache.txt.\n"
        );
    }
}
