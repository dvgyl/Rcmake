//! Unified multi-call CLI driver for `rcmake` (`cmake`, `ctest`, `cpack` dispatch via `argv[0]`).

#![forbid(unsafe_code)]

mod build;
mod cache;
mod configure;
#[cfg(test)]
mod ctest;
mod exec_log;
mod explain_cmd;
mod find_package;
mod generate;
mod help;
mod install;
mod instrumentation;
mod make_depends;
mod options;
mod persona;
mod presets;
mod profile;
mod resource;
mod sarif;
mod system_information;
mod workflow;

#[cfg(test)]
mod driver_tests;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use instrumentation::{
    CommandExit, InstrumentationStep, instrument_lifecycle, sample_instrumentation,
};
use options::{Options, ParseResult};
use rcmake_base::{
    BString, CMAKE_VERSION_MAJOR, CMAKE_VERSION_MINOR, CMAKE_VERSION_PATCH, CMAKE_VERSION_STR,
    Limits,
};
use rcmake_diag::{
    DiagAction, DiagCategory, DiagOptions, DiagState, Diagnostic, Effect, Location, MessageKind,
    RenderOptions, render,
};
use rcmake_eval::{CacheDefine, Interp, InterpConfig, RunMode};
use rcmake_host::{Host, Stream, WriteMode};
use rcmake_json::Value;
use rcmake_model::CacheType;

/// Active tool persona selected from the `argv[0]` basename.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// `cmake` (or default `rcmake`).
    Cmake,
    /// `ctest`.
    Ctest,
    /// `cpack`.
    Cpack,
}

impl Tool {
    /// Canonical lowercase executable name printed in `--version`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cmake => "cmake",
            Self::Ctest => "ctest",
            Self::Cpack => "cpack",
        }
    }
}

/// Determine the [`Tool`] persona from the `argv[0]` basename (not the resolved symlink target).
#[must_use]
pub fn tool_from_argv0(argv0: &OsStr) -> Tool {
    let base = Path::new(argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cmake");
    if base.eq_ignore_ascii_case("ctest") {
        Tool::Ctest
    } else if base.eq_ignore_ascii_case("cpack") {
        Tool::Cpack
    } else {
        Tool::Cmake
    }
}

/// Run the Rcmake CLI with `argv` (including `argv[0]`) against `host`, returning the process exit code.
#[must_use]
pub fn run(argv: &[BString], host: Arc<dyn Host>) -> i32 {
    let invoked = argv.first().map(|a| rcmake_host::path::to_native(a));
    let name = invoked.as_deref().map(Path::as_os_str).unwrap_or_default();
    let tool = tool_from_argv0(name);
    let persona = if persona::oracle_mode(host.as_ref()) {
        persona::Persona::Compat
    } else {
        persona::persona_from_argv0(name)
    };
    let Some(log) = host
        .env_get(b"RCMAKE_EXEC_LOG")
        .filter(|value| !value.is_empty())
    else {
        return run_tool(argv, tool, persona, host);
    };
    let start = host.monotonic();
    let code = run_tool(argv, tool, persona, Arc::clone(&host));
    exec_log::append_exec_log(
        host.as_ref(),
        &log,
        &exec_log::ExecLogEntry {
            argv,
            tool,
            persona,
            cwd: &host.initial_cwd(),
            pid: host.system().pid,
            parent_pid: None,
            exit_code: code,
            duration: host.monotonic().saturating_sub(start),
        },
    );
    code
}

fn run_tool(argv: &[BString], tool: Tool, persona: persona::Persona, host: Arc<dyn Host>) -> i32 {
    let rest = argv.get(1..).unwrap_or(&[]);
    if let Some(first) = rest.first() {
        let fb = first.as_slice();
        let output = rest.get(1).filter(|arg| !arg.starts_with(b"-"));
        if fb == b"--version" || fb == b"-version" || fb == b"/V" || fb == b"/version" {
            let banner = format!(
                "{} version {CMAKE_VERSION_STR}\nRcmake {}\n",
                tool.name(),
                env!("CARGO_PKG_VERSION")
            );
            return write_version(host.as_ref(), output, banner.as_bytes());
        }
        if let Some(fmt) = fb
            .strip_prefix(b"--version=")
            .or_else(|| fb.strip_prefix(b"-version="))
            .or_else(|| fb.strip_prefix(b"/V="))
            .or_else(|| fb.strip_prefix(b"/version="))
        {
            return run_version_format(tool, fmt, host.as_ref(), output);
        }
    }

    match tool {
        Tool::Cmake => run_cmake(argv, rest, persona, host),
        Tool::Ctest => {
            presets::list_for_tool(host.as_ref(), rcmake_presets::PresetKind::Test, rest)
                .unwrap_or_else(|| run_ctest(argv, rest, host))
        }
        Tool::Cpack => {
            presets::list_for_tool(host.as_ref(), rcmake_presets::PresetKind::Package, rest)
                .unwrap_or_else(|| rcmake_cpack::main(argv, host))
        }
    }
}

fn run_ctest(argv: &[BString], args: &[BString], host: Arc<dyn Host>) -> i32 {
    // CTest ignores positional arguments before its options.
    let first_option = args
        .iter()
        .position(|arg| {
            let (flag, _) = options::split_flag(arg, b'=');
            flag.starts_with(b"-") || matches!(flag, b"/?" | b"/V" | b"/version")
        })
        .unwrap_or(args.len());
    if first_option == 0 {
        return rcmake_ctest::main(argv, host);
    }
    let argv = argv
        .iter()
        .take(1)
        .chain(args.iter().skip(first_option))
        .cloned()
        .collect::<Vec<_>>();
    rcmake_ctest::main(&argv, host)
}

fn write_version(host: &dyn Host, output: Option<&BString>, contents: &[u8]) -> i32 {
    if let Some(output) = output {
        let path = resolve_cli_path(host, output);
        if let Err(err) = host.write(&path, contents, WriteMode::Truncate) {
            emit_cli_error(host, format!("Cannot write {}: {err}", path.display()));
            return 1;
        }
    } else {
        host.write_out(Stream::Stdout, contents);
    }
    0
}

fn run_version_format(tool: Tool, fmt: &[u8], host: &dyn Host, output: Option<&BString>) -> i32 {
    if fmt != b"json-v1" {
        emit_cli_error(
            host,
            format!("Invalid --version format: {}", String::from_utf8_lossy(fmt)),
        );
        return 1;
    }
    let mut prog_ver = BTreeMap::new();
    prog_ver.insert(
        BString::from("major"),
        Value::UInt(u64::from(CMAKE_VERSION_MAJOR)),
    );
    prog_ver.insert(
        BString::from("minor"),
        Value::UInt(u64::from(CMAKE_VERSION_MINOR)),
    );
    prog_ver.insert(
        BString::from("patch"),
        Value::UInt(u64::from(CMAKE_VERSION_PATCH)),
    );
    prog_ver.insert(BString::from("string"), Value::string(CMAKE_VERSION_STR));

    let mut program = BTreeMap::new();
    program.insert(BString::from("name"), Value::string(tool.name()));
    program.insert(BString::from("version"), Value::Object(prog_ver));

    let mut schema_ver = BTreeMap::new();
    schema_ver.insert(BString::from("major"), Value::UInt(1));
    schema_ver.insert(BString::from("minor"), Value::UInt(0));

    let mut root = BTreeMap::new();
    root.insert(BString::from("dependencies"), Value::Array(Vec::new()));
    root.insert(BString::from("program"), Value::Object(program));
    root.insert(BString::from("version"), Value::Object(schema_ver));

    let mut text = rcmake_json::write_styled(&Value::Object(root), "  ");
    text.push(b'\n');
    write_version(host, output, text.as_slice())
}

fn run_cmake(
    argv: &[BString],
    rest: &[BString],
    persona: persona::Persona,
    host: Arc<dyn Host>,
) -> i32 {
    if persona == persona::Persona::Native
        && let Some(args) = rest.strip_prefix(&[BString::from("explain")])
    {
        return explain_cmd::run(argv.first(), args, host);
    }
    if let Some(sub_args) = rest.strip_prefix(&[BString::from("-E")]) {
        if let [command, args @ ..] = sub_args {
            match command.as_slice() {
                b"cmake_depends" if args.len() >= 4 => return make_depends::run(args, host),
                b"cmake_llvm_rc" => return resource::run(args, host.as_ref()),
                b"__instrument" => {
                    return match rcmake_instrument::launcher::run_launcher(host.as_ref(), args) {
                        Ok(code) => code,
                        Err(err) => {
                            emit_cli_error(host.as_ref(), err.to_string());
                            1
                        }
                    };
                }
                b"server" => {
                    emit_cli_error(
                        host.as_ref(),
                        "CMake server mode has been removed in favor of the file-api.",
                    );
                    return 1;
                }
                _ => {}
            }
        }
        let cwd = host.initial_cwd();
        let exe = host
            .current_exe()
            .map(|p| rcmake_host::path::from_native(&p))
            .unwrap_or_else(|_| BString::from("cmake"));
        let ctx = rcmake_tool_e::ToolCtx {
            host: host.as_ref(),
            cwd: &cwd,
            exe_display: &exe,
            capabilities: &build_capabilities_json,
        };
        return rcmake_tool_e::run(sub_args, &ctx);
    }

    if let Some(args) = rest.strip_prefix(&[BString::from("--build")]) {
        return build::run(argv, args, host.as_ref());
    }
    if let Some(args) = rest.strip_prefix(&[BString::from("--install")]) {
        return install::run(argv, args, host);
    }
    if let Some(args) = rest.strip_prefix(&[BString::from("--workflow")]) {
        return workflow::run(args, host);
    }

    if let Some(code) = help::run(argv.first(), rest, persona, host.as_ref()) {
        return code;
    }

    if let Some(args) = rest.strip_prefix(&[BString::from("--system-information")]) {
        return system_information::run(argv, args, host.as_ref());
    }

    let mut opts = match options::parse(rest, persona, host.as_ref()) {
        ParseResult::Run(opts) => *opts,
        ParseResult::Exit(code) => return code,
    };
    if opts.find_package && opts.script.is_none() {
        return find_package::run(argv, opts, host);
    }
    if let Some(script) = opts.script.take() {
        let mut interp = new_interp(argv, &opts, RunMode::Script, Arc::clone(&host));
        let profiler = opts
            .profile_output
            .as_ref()
            .map(|_| Arc::new(profile::Recorder::new(host.as_ref())));
        if let Some(profiler) = &profiler {
            profiler.attach(&mut interp);
        }
        for edit in opts.cache_edits {
            if let options::CacheEdit::Define(def) = edit {
                cache::apply(interp.cx().cache(), def);
            }
        }
        let state = configure::initialize_diagnostics(
            host.as_ref(),
            interp.cx().cache(),
            &[],
            &opts.warning_flags,
            false,
        );
        *interp.cx().diag_state_mut() = state;
        let start = host.now();
        let outcome = interp.run_script(&script);
        if let Some((profiler, path)) = profiler.as_ref().zip(opts.profile_output.as_deref())
            && let Err(error) = profiler.write(host.as_ref(), path)
        {
            emit_cli_error(
                host.as_ref(),
                format!("Could not write profiling output: {error}"),
            );
            return 1;
        }
        if let Some(output) = opts.sarif.as_deref() {
            let command = interp.cx().var_or_empty(b"CMAKE_COMMAND");
            let invocation = sarif::Invocation {
                command: &command,
                start,
                exit_code: outcome.exit_code,
                directories: None,
            };
            if !sarif::write(host.as_ref(), output, &interp, Vec::new(), invocation) {
                return 1;
            }
        }
        return outcome.exit_code;
    }
    for (ignore_key, ignore) in [
        (
            "_RCMAKE_IGNORE_LINK_WARNING_AS_ERROR",
            opts.ignore_link_warning_as_error,
        ),
        (
            "_RCMAKE_NO_WARNING_AS_ERROR",
            opts.ignore_compile_warning_as_error,
        ),
    ] {
        if ignore {
            opts.cache_edits
                .push(options::CacheEdit::Define(rcmake_eval::CacheDefine {
                    name: ignore_key.into(),
                    ty: Some(CacheType::Internal),
                    value: "ON".into(),
                }));
        } else if !opts.regenerate_during_build {
            opts.cache_edits
                .push(options::CacheEdit::Remove(ignore_key.into()));
        }
    }
    configure::run(argv, opts, host)
}

fn new_interp(argv: &[BString], opts: &Options, mode: RunMode, host: Arc<dyn Host>) -> Interp {
    let (cmake_command, cmake_root) = rcmake_host::discover_cmake_root(host.as_ref(), argv.first());
    new_interp_with_paths(
        opts,
        mode,
        host,
        cmake_command,
        cmake_root.unwrap_or_default(),
        argv,
        rcmake_eval::Services::default(),
    )
}

fn new_interp_with_paths(
    opts: &Options,
    mode: RunMode,
    host: Arc<dyn Host>,
    cmake_command: PathBuf,
    cmake_root: PathBuf,
    argv: &[BString],
    services: rcmake_eval::Services,
) -> Interp {
    let name = if cfg!(windows) { "cmake.exe" } else { "cmake" };
    let alias = cmake_command.with_file_name(name);
    let cmake_command = if alias != cmake_command
        && host
            .canonicalize(&alias)
            .is_ok_and(|path| path == cmake_command)
    {
        alias
    } else {
        cmake_command
    };
    let cfg = InterpConfig {
        services: services.into(),
        cwd: host.initial_cwd(),
        host,
        mode,
        cmake_root,
        cmake_command,
        limits: Limits::default(),
        diag: DiagOptions {
            initial_state: DiagState::default(),
            sarif: mode == RunMode::Configure || opts.sarif.is_some(),
        },
        defines: Vec::new(),
        script_args: argv.to_vec(),
    };
    let mut interp = Interp::new(cfg, configure::register);
    interp.set_check_system_vars(opts.check_system_vars);
    if opts.debug_trycompile {
        interp.cx().set_var(b"CMAKE_DEBUG_TRY_COMPILE", "ON".into());
    }
    if opts.debug_find {
        interp.cx().set_var(b"CMAKE_FIND_DEBUG_MODE", "ON".into());
    }
    interp.set_log_level(opts.log_level);
    interp.set_log_context(opts.log_context);
    interp.set_trace_config(opts.trace.clone());
    interp.set_find_debug_filters(
        opts.debug_find_variables.clone(),
        opts.debug_find_packages.clone(),
    );
    interp
}

fn parse_define(raw: &[u8]) -> Option<CacheDefine> {
    let eq = raw.iter().position(|&b| b == b'=')?;
    let lhs = raw.get(..eq)?;
    if lhs.is_empty() {
        return None;
    }
    let value = raw.get(eq + 1..)?;
    let trimmed = value
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r'))
        .and_then(|end| value.get(..=end))
        .unwrap_or(value);
    let value = BString::from(
        trimmed
            .strip_prefix(b"'")
            .and_then(|inner| inner.strip_suffix(b"'"))
            .unwrap_or(trimmed),
    );
    if let Some(colon) = lhs.iter().position(|&b| b == b':') {
        let name = BString::from(lhs.get(..colon)?);
        let ty = CacheType::parse(lhs.get(colon + 1..).unwrap_or(&[]));
        Some(CacheDefine { name, ty, value })
    } else {
        Some(CacheDefine {
            name: BString::from(lhs),
            ty: None,
            value,
        })
    }
}

fn apply_warning_flag(flag: &[u8], state: &mut DiagState) -> bool {
    let Some(rest) = flag.strip_prefix(b"-W") else {
        return false;
    };
    let (negated, rest) = match rest.strip_prefix(b"no-") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let (error_level, cat_name) = match rest.strip_prefix(b"error=") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let cat = match cat_name {
        b"dev" | b"author" => DiagCategory::Author,
        b"deprecated" => DiagCategory::Deprecated,
        b"policy" => DiagCategory::Policy,
        b"strict" => DiagCategory::Strict,
        b"experimental" => DiagCategory::Experimental,
        b"non-target-directive" => DiagCategory::NonTargetDirective,
        b"install-absolute-destination" => DiagCategory::InstallAbsoluteDestination,
        b"uninitialized" => DiagCategory::Uninitialized,
        b"unused-cli" => DiagCategory::UnusedCli,
        _ => return false,
    };
    match (negated, error_level) {
        (false, false) => state.promote(cat, DiagAction::Warn, true),
        (true, false) => state.set(cat, DiagAction::Ignore, true),
        (false, true) => state.promote(cat, DiagAction::SendError, true),
        (true, true) => state.demote(cat, DiagAction::Warn, true),
    }
    true
}

fn resolve_cli_path(host: &dyn Host, arg: &[u8]) -> PathBuf {
    let native = rcmake_host::path::to_native(arg);
    let absolute = host.initial_cwd().join(native);
    let mut path = PathBuf::new();
    for component in absolute.components() {
        if component == std::path::Component::ParentDir {
            if path.parent().is_some() {
                path.pop();
            }
        } else {
            path.push(component);
        }
    }
    path
}

fn build_capabilities_json() -> Value {
    let mut version = BTreeMap::new();
    version.insert(BString::from("isDirty"), Value::Bool(false));
    version.insert(
        BString::from("major"),
        Value::UInt(u64::from(CMAKE_VERSION_MAJOR)),
    );
    version.insert(
        BString::from("minor"),
        Value::UInt(u64::from(CMAKE_VERSION_MINOR)),
    );
    version.insert(
        BString::from("patch"),
        Value::UInt(u64::from(CMAKE_VERSION_PATCH)),
    );
    version.insert(BString::from("string"), Value::string(CMAKE_VERSION_STR));
    version.insert(BString::from("suffix"), Value::string(""));

    let mut root = BTreeMap::new();
    root.insert(BString::from("debugger"), Value::Bool(false));
    let requests = [
        ("codemodel", 2, 12),
        ("configureLog", 1, 0),
        ("cache", 2, 0),
        ("cmakeFiles", 1, 1),
        ("toolchains", 1, 1),
    ]
    .map(|(kind, major, minor)| {
        object([
            ("kind", Value::string(kind)),
            (
                "version",
                Value::Array(vec![object([
                    ("major", Value::UInt(major)),
                    ("minor", Value::UInt(minor)),
                ])]),
            ),
        ])
    });
    root.insert(
        "fileApi".into(),
        object([("requests", Value::Array(requests.into()))]),
    );
    let generators: Vec<_> = rcmake_model::GeneratorKind::available()
        .map(|generator| {
            object([
                ("name", Value::string(generator.name())),
                ("extraGenerators", Value::Array(Vec::new())),
                (
                    "platformSupport",
                    Value::Bool(generator.supports_platform()),
                ),
                (
                    "toolsetSupport",
                    Value::Bool(generator.toolset_support() != rcmake_model::ToolsetSupport::None),
                ),
            ])
        })
        .collect();
    root.insert("generators".into(), Value::Array(generators));
    root.insert(
        "instrumentation".into(),
        object([
            ("apiVersion", Value::UInt(rcmake_instrument::API_VERSION)),
            (
                "dataVersion",
                object([
                    ("major", Value::UInt(rcmake_instrument::DATA_VERSION.major)),
                    ("minor", Value::UInt(rcmake_instrument::DATA_VERSION.minor)),
                ]),
            ),
        ]),
    );
    root.insert(BString::from("serverMode"), Value::Bool(false));
    root.insert(BString::from("tls"), Value::Bool(false));
    root.insert(BString::from("version"), Value::Object(version));
    Value::Object(root)
}

fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn emit_cli_error(host: &dyn Host, body: impl Into<BString>) {
    let diag = Diagnostic {
        annotations: None,
        kind: MessageKind::Error,
        effect: Effect::Fatal,
        category: None,
        policy: None,
        location: Location::None,
        body: body.into(),
    };
    let rendered = render(&diag, &RenderOptions::default());
    host.write_out(Stream::Stderr, rendered.as_slice());
}

fn missing_cmake_root(host: &dyn Host, exe: &Path, configure: bool) -> i32 {
    let root = exe
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("/"))
        .join("share/cmake-4.5");
    host.write_out(
        Stream::Stderr,
        format!(
            "CMake Error: Could not find CMAKE_ROOT !!!\nCMake has most likely not been installed correctly.\nModules directory not found in\n{}\n",
            rcmake_host::path::from_native_cmake(&root)
        ).as_bytes(),
    );
    if configure {
        host.write_out(
            Stream::Stderr,
            b"CMake Error: Error executing cmake::LoadCache(). Aborting.\n\n",
        );
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_definitions_remove_suffix_space_and_single_quotes() {
        for (argument, expected) in [
            (b"KEY='a,b,c'  ".as_slice(), b"a,b,c".as_slice()),
            (b"KEY:STRING='a\t' \r", b"a\t"),
            (b"KEY=''", b""),
            (b"KEY='", b"'"),
            (b"KEY= \t\r", b" \t\r"),
            (b"KEY=\"a  b\"", b"\"a  b\""),
            (b"KEY= a=b  c \t", b" a=b  c"),
        ] {
            let definition = parse_define(argument).expect("valid definition");
            assert_eq!(definition.value.as_slice(), expected);
        }
    }

    #[test]
    fn argv0_dispatch_and_version_banner() {
        assert_eq!(tool_from_argv0(OsStr::new("/tmp/bin/cmake")), Tool::Cmake);
        assert_eq!(tool_from_argv0(OsStr::new("ctest")), Tool::Ctest);
        assert_eq!(tool_from_argv0(OsStr::new("/opt/cpack")), Tool::Cpack);
        assert_eq!(tool_from_argv0(OsStr::new("rcmake")), Tool::Cmake);

        let host = crate::driver_tests::project_host();
        let code = run(
            &[BString::from("cmake"), BString::from("--version")],
            host.clone(),
        );
        assert_eq!(code, 0);
        let out = host.stream_bytes(Stream::Stdout);
        assert_eq!(out, b"cmake version 4.5.1\nRcmake 0.1.0\n");
    }

    #[test]
    fn capabilities_include_instrumentation_versions() {
        let host = crate::driver_tests::project_host();
        let args = ["cmake", "-E", "capabilities"].map(BString::from);
        assert_eq!(run(&args, host.clone()), 0);
        let field = br#""instrumentation":{"apiVersion":1,"dataVersion":{"major":1,"minor":2}}"#;
        let output = host.stream_bytes(Stream::Stdout);
        assert!(output.windows(field.len()).any(|value| value == field));
    }

    #[test]
    fn script_mode_executes_with_defines() {
        let host = crate::driver_tests::project_host();
        host.seed_file(
            Path::new("/work/hello.cmake"),
            b"message(STATUS \"hi ${WHO}\")\n",
        )
        .unwrap();
        let code = run(
            &[
                BString::from("cmake"),
                BString::from("-DWHO=Rcmake"),
                BString::from("-P"),
                BString::from("hello.cmake"),
            ],
            host.clone(),
        );
        assert_eq!(code, 0);
        assert_eq!(
            host.stream_bytes(Stream::Stdout),
            BString::from("-- hi Rcmake\n")
        );
    }

    #[test]
    fn malformed_define_is_rejected() {
        let host = crate::driver_tests::project_host();
        let code = run(
            &[
                BString::from("cmake"),
                BString::from("-DFOO"),
                BString::from("-P"),
                BString::from("hello.cmake"),
            ],
            host.clone(),
        );
        assert_eq!(code, 1);
        assert!(
            host.stream_bytes(Stream::Stderr)
                .starts_with(b"CMake Error: Parse error in command line argument: FOO\n")
        );
    }

    #[test]
    fn configure_reuses_and_refreshes_cache() {
        let host = crate::driver_tests::project_host();
        host.seed_file(
            Path::new("/work/CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 3.10)\nproject(P NONE)\nmessage(STATUS \"value=${VALUE}\")\n",
        )
        .unwrap();
        let invoke = |args: &[&str]| {
            let argv: Vec<_> = std::iter::once("cmake")
                .chain(args.iter().copied())
                .map(BString::from)
                .collect();
            run(&argv, host.clone())
        };
        assert_eq!(
            invoke(&["-S", ".", "-B", "build", "-DVALUE:STRING=initial"]),
            0
        );
        assert_eq!(invoke(&["build"]), 0);
        assert_eq!(invoke(&["--fresh", "build"]), 0);
        let output = host.stream_bytes(Stream::Stdout);
        assert_eq!(
            output
                .split(|byte| *byte == b'\n')
                .filter(|line| line.starts_with(b"-- value="))
                .collect::<Vec<_>>(),
            [
                b"-- value=initial".as_slice(),
                b"-- value=initial",
                b"-- value="
            ]
        );
    }

    #[test]
    fn view_mode_uses_explicit_directories_and_implicit_preload() {
        let host = crate::driver_tests::project_host();
        host.seed_file(
            Path::new("/work/src/PreLoad.cmake"),
            b"message(STATUS \"${CMAKE_SOURCE_DIR} -> ${CMAKE_BINARY_DIR} ${LOCAL}\")\nset(LOCAL set)\n",
        )
        .unwrap();
        let args = [
            "cmake",
            "-N",
            "-C",
            "src/PreLoad.cmake",
            "-S",
            "src",
            "-B",
            "build",
        ]
        .map(BString::from);
        assert_eq!(run(&args, host.clone()), 0);
        assert_eq!(
            host.stream_bytes(Stream::Stdout),
            BString::from(
                "loading initial cache file src/PreLoad.cmake\n-- /work/src -> /work/build \n-- /work/src -> /work/build \n"
            )
        );
        assert!(!configure::is_dir(host.as_ref(), Path::new("/work/build")));
    }
}
