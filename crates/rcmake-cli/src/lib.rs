//! Unified multi-call CLI driver for `rcmake` (`cmake`, `ctest`, `cpack` dispatch via `argv[0]`).

#![forbid(unsafe_code)]

mod build;
mod cache;
mod configure;
mod ctest;
mod generate;
mod install;
mod options;
mod presets;
mod resource;

#[cfg(test)]
mod driver_tests;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    let tool = argv
        .first()
        .map(|a| {
            let p = rcmake_host::path::to_native(a.as_slice());
            tool_from_argv0(p.as_os_str())
        })
        .unwrap_or(Tool::Cmake);

    let rest = argv.get(1..).unwrap_or(&[]);
    if let Some(first) = rest.first() {
        let fb = first.as_slice();
        if fb == b"--version" || fb == b"-version" || fb == b"/V" || fb == b"/version" {
            let banner = format!(
                "{} version {CMAKE_VERSION_STR}\n\nRcmake native Rust drop-in replacement for CMake {CMAKE_VERSION_STR}.\n",
                tool.name()
            );
            host.write_out(Stream::Stdout, banner.as_bytes());
            return 0;
        }
        if let Some(fmt) = fb
            .strip_prefix(b"--version=")
            .or_else(|| fb.strip_prefix(b"-version="))
            .or_else(|| fb.strip_prefix(b"/V="))
            .or_else(|| fb.strip_prefix(b"/version="))
        {
            return run_version_format(tool, fmt, host.as_ref());
        }
    }

    if tool == Tool::Cmake {
        return run_cmake(argv, rest, host);
    }

    if tool == Tool::Ctest {
        return ctest::run(rest, host);
    }

    emit_cli_error(
        host.as_ref(),
        format!(
            "{} mode is not yet implemented (scheduled for Milestone 3)",
            tool.name()
        ),
    );
    1
}

fn run_version_format(tool: Tool, fmt: &[u8], host: &dyn Host) -> i32 {
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
    host.write_out(Stream::Stdout, text.as_slice());
    0
}

fn run_cmake(argv: &[BString], rest: &[BString], host: Arc<dyn Host>) -> i32 {
    if let Some(sub_args) = rest.strip_prefix(&[BString::from("-E")]) {
        if let [command, args @ ..] = sub_args {
            match command.as_slice() {
                b"cmake_llvm_rc" => return resource::run(args, host.as_ref()),
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
        return build::run(args, host.as_ref());
    }
    if let Some(args) = rest.strip_prefix(&[BString::from("--install")]) {
        return install::run(argv, args, host);
    }

    let mut opts = match options::parse(rest, host.as_ref()) {
        ParseResult::Run(opts) => *opts,
        ParseResult::Exit(code) => return code,
    };
    if let Some(script) = opts.script.take() {
        let mut interp = new_interp(argv, &opts, RunMode::Script, Arc::clone(&host));
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
        let outcome = interp.run_script(&script);
        if !write_sarif_if_requested(host.as_ref(), opts.sarif.as_deref(), &interp, Vec::new()) {
            return 1;
        }
        return outcome.exit_code;
    }
    configure::run(argv, opts, host)
}

fn new_interp(argv: &[BString], opts: &Options, mode: RunMode, host: Arc<dyn Host>) -> Interp {
    let (cmake_command, cmake_root) = discover_cmake_paths(host.as_ref(), argv.first());
    let cfg = InterpConfig {
        cwd: host.initial_cwd(),
        host,
        mode,
        cmake_root,
        cmake_command,
        limits: Limits::default(),
        diag: DiagOptions {
            initial_state: DiagState::default(),
            sarif: opts.sarif.is_some(),
        },
        defines: Vec::new(),
        script_args: argv.to_vec(),
    };
    let mut interp = Interp::new(cfg, rcmake_commands::register_all);
    if opts.debug_trycompile {
        interp.cx().set_var(b"CMAKE_DEBUG_TRY_COMPILE", "ON".into());
    }
    interp.set_log_level(opts.log_level);
    interp.set_log_context(opts.log_context);
    interp.set_trace_config(opts.trace.clone());
    interp
}

fn sarif_results(sarif: &mut Value) -> Option<&mut Vec<Value>> {
    let Value::Object(root) = sarif else {
        return None;
    };
    let Value::Array(runs) = root.get_mut(b"runs".as_slice())? else {
        return None;
    };
    let Value::Object(run) = runs.first_mut()? else {
        return None;
    };
    let Value::Array(results) = run.get_mut(b"results".as_slice())? else {
        return None;
    };
    Some(results)
}

fn write_sarif_if_requested(
    host: &dyn Host,
    path: Option<&Path>,
    interp: &Interp,
    mut preload_results: Vec<Value>,
) -> bool {
    let Some(sarif_path) = path else {
        return true;
    };
    let mut sarif = interp.diag_engine().sarif_json();
    if !preload_results.is_empty()
        && let Some(results) = sarif_results(&mut sarif)
    {
        preload_results.append(results);
        *results = preload_results;
    }
    let json = rcmake_json::write_styled(&sarif, "  ");
    if let Err(err) = host.write(sarif_path, json.as_slice(), WriteMode::Truncate) {
        emit_cli_error(
            host,
            format!(
                "Failed to write SARIF output to \"{}\": {err}",
                sarif_path.display()
            ),
        );
        return false;
    }
    true
}

fn parse_define(raw: &[u8]) -> Option<CacheDefine> {
    let eq = raw.iter().position(|&b| b == b'=')?;
    let lhs = raw.get(..eq)?;
    if lhs.is_empty() {
        return None;
    }
    let value = BString::from(raw.get(eq + 1..).unwrap_or(&[]));
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

fn discover_cmake_paths(host: &dyn Host, argv0: Option<&BString>) -> (PathBuf, PathBuf) {
    let exe = host
        .current_exe()
        .unwrap_or_else(|_| PathBuf::from("/usr/local/bin/cmake"));
    let argv_path = argv0.map(|arg| resolve_cli_path(host, arg));
    for executable in argv_path.iter().chain(std::iter::once(&exe)) {
        if let Some(bin) = executable.parent()
            && let Some(prefix) = bin.parent()
        {
            let root = prefix.join("share/cmake-4.5");
            if configure::is_dir(host, &root.join("Modules")) {
                return (executable.clone(), root);
            }
        }
    }
    let root = exe.parent().and_then(Path::parent).map_or_else(
        || PathBuf::from("/usr/local/share/cmake-4.5"),
        |prefix| prefix.join("share/cmake-4.5"),
    );
    (exe, root)
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
    let generators = rcmake_model::GeneratorKind::ALL.map(|generator| {
        object([
            ("name", Value::string(generator.name())),
            ("extraGenerators", Value::Array(Vec::new())),
            ("platformSupport", Value::Bool(false)),
            ("toolsetSupport", Value::Bool(false)),
        ])
    });
    root.insert("generators".into(), Value::Array(generators.into()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    #[test]
    fn argv0_dispatch_and_version_banner() {
        assert_eq!(tool_from_argv0(OsStr::new("/tmp/bin/cmake")), Tool::Cmake);
        assert_eq!(tool_from_argv0(OsStr::new("ctest")), Tool::Ctest);
        assert_eq!(tool_from_argv0(OsStr::new("/opt/cpack")), Tool::Cpack);
        assert_eq!(tool_from_argv0(OsStr::new("rcmake")), Tool::Cmake);

        let host = Arc::new(PureHost::default());
        let code = run(
            &[BString::from("cmake"), BString::from("--version")],
            host.clone(),
        );
        assert_eq!(code, 0);
        let out = host.stream_bytes(Stream::Stdout);
        assert!(out.starts_with(b"cmake version 4.5.1\n\n"));
    }

    #[test]
    fn script_mode_executes_with_defines() {
        let host = Arc::new(PureHost::default());
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
        let host = Arc::new(PureHost::default());
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
        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/CMakeLists.txt"),
            b"message(STATUS \"value=${VALUE}\")\n",
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
        let host = Arc::new(PureHost::default());
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
