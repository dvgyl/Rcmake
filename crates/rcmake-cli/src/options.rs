use std::path::PathBuf;

use rcmake_base::BString;
use rcmake_diag::DiagState;
use rcmake_eval::{CacheDefine, LogLevel, TraceConfig, TraceFormat};
use rcmake_host::{Host, Stream, WriteMode};
use rcmake_model::CacheType;

use crate::presets::Listing;
use crate::{apply_warning_flag, parse_define, resolve_cli_path};

pub(crate) enum CacheEdit {
    Define(CacheDefine),
    Remove(BString),
    Preload(PathBuf),
}

pub(crate) enum DirectoryArg {
    Source(PathBuf),
    Binary(PathBuf),
    Positional(PathBuf),
}

#[derive(Default)]
pub(crate) struct Options {
    pub directories: Vec<DirectoryArg>,
    pub cache_edits: Vec<CacheEdit>,
    pub warning_flags: Vec<BString>,
    pub generator: Option<BString>,
    pub architecture: Option<BString>,
    pub toolset: Option<BString>,
    pub log_level: Option<LogLevel>,
    pub log_context: bool,
    pub debug_trycompile: bool,
    pub sarif: Option<PathBuf>,
    pub script: Option<PathBuf>,
    pub preset: Option<BString>,
    pub presets_file: Option<PathBuf>,
    pub list_presets: Option<Listing>,
    pub view: bool,
    pub fresh: bool,
    pub list_cache: Option<ListCache>,
    pub profile_output: Option<PathBuf>,
    pub graphviz: Option<PathBuf>,
    pub trace: Option<TraceConfig>,
}

#[derive(Default)]
pub(crate) struct ListCache {
    pub advanced: bool,
    pub help: bool,
    pub pattern: Option<BString>,
}

pub(crate) enum ParseResult {
    Run(Box<Options>),
    Exit(i32),
}

const USAGE: &str = "Usage\n\n  cmake [options] <path-to-source>\n  cmake [options] <path-to-existing-build>\n  cmake [options] -S <path-to-source> -B <path-to-build>\n\nSpecify a source directory to (re-)generate a build system for it in the\ncurrent working directory.  Specify an existing build directory to\nre-generate its build system.\n\nRun 'cmake --help' for more information.\n\n";

const LONG_OPTIONS: &[&[u8]] = &[
    b"--build",
    b"--install",
    b"--version",
    b"--help",
    b"--help-variable",
    b"--fresh",
    b"--regenerate-during-build",
    b"--preset",
    b"--presets-file",
    b"--list-presets",
    b"--toolchain",
    b"--install-prefix",
    b"--debugger",
    b"--debugger-pipe",
    b"--debugger-dap-log",
    b"--log-level",
    b"--log-context",
    b"--trace",
    b"--trace-expand",
    b"--trace-format",
    b"--trace-source",
    b"--trace-redirect",
    b"--warn-uninitialized",
    b"--no-warn-unused-cli",
    b"--check-system-vars",
    b"--debug-output",
    b"--debug-trycompile",
    b"--debug-find",
    b"--debug-find-pkg",
    b"--debug-find-var",
    b"--sarif-output",
    b"--graphviz",
    b"--profiling-format",
    b"--profiling-output",
    b"--print-config-dir",
];

pub(crate) fn parse(args: &[BString], host: &dyn Host) -> ParseResult {
    if args.is_empty() {
        out(host, USAGE);
        return ParseResult::Exit(0);
    }
    match parse_args(args, host) {
        Ok(Some(opts)) => ParseResult::Run(Box::new(opts)),
        Ok(None) => ParseResult::Exit(0),
        Err(body) => ParseResult::Exit(cli_usage_error(host, &body)),
    }
}

fn parse_args(args: &[BString], host: &dyn Host) -> Result<Option<Options>, String> {
    let mut opts = Options::default();
    let mut iter = args.iter().map(|arg| arg.as_slice()).peekable();
    let mut profile_format = None;
    while let Some(arg) = iter.next() {
        if arg == b"--" {
            break;
        }
        let (flag, inline) = split_flag(arg, b'=');
        if opts.script.is_some()
            && !matches!(
                flag,
                b"--fresh"
                    | b"--preset"
                    | b"--presets-file"
                    | b"--list-presets"
                    | b"--log-level"
                    | b"--loglevel"
                    | b"--log-context"
            )
        {
            continue;
        }
        if arg.starts_with(b"--") {
            macro_rules! value {
                ($error:expr) => {
                    inline
                        .or_else(|| take_value(&mut iter))
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| $error.to_owned())?
                };
            }
            match flag {
                b"--help" | b"--help-variable" => {
                    let Some(topic) = inline.or_else(|| take_value(&mut iter)) else {
                        out(host, USAGE);
                        print_generators(host, Stream::Stdout);
                        return Ok(None);
                    };
                    return show_help(host, topic, flag == b"--help-variable").map(|()| None);
                }
                b"--regenerate-during-build" if inline.is_none() => {}
                b"--preset" => {
                    opts.preset = Some(value!("No preset specified for --preset").into());
                }
                b"--presets-file" => {
                    opts.presets_file = Some(resolve_cli_path(
                        host,
                        value!("No file specified for --presets-file"),
                    ));
                }
                b"--list-presets" => {
                    let mode = inline.or_else(|| take_value(&mut iter));
                    if let Some(value) = mode.filter(|value| value.starts_with(b"-")) {
                        return Err(unknown_option(value));
                    }
                    if mode == Some(b"") {
                        crate::emit_cli_error(host, "Invalid value used with --list-presets");
                        return Err(String::new());
                    }
                    opts.list_presets = Some(Listing::parse(mode)?);
                }
                b"--fresh" => opts.fresh = true,
                b"--log-context" => opts.log_context = true,
                b"--log-level" | b"--loglevel" => {
                    let invalid = format!(
                        "Invalid level specified for {}",
                        String::from_utf8_lossy(flag)
                    );
                    opts.log_level = Some(LogLevel::parse(value!(invalid)).ok_or(invalid)?);
                }
                b"--sarif-output" => {
                    opts.sarif = Some(resolve_cli_path(
                        host,
                        value!("No file specified for --sarif-output"),
                    ));
                }
                b"--toolchain" | b"--install-prefix" => {
                    let (name, message) = if flag == b"--toolchain" {
                        ("CMAKE_TOOLCHAIN_FILE", "No file specified for --toolchain")
                    } else {
                        (
                            "CMAKE_INSTALL_PREFIX",
                            "No install directory specified for --install-prefix",
                        )
                    };
                    opts.cache_edits.push(CacheEdit::Define(CacheDefine {
                        name: name.into(),
                        ty: Some(if flag == b"--toolchain" {
                            CacheType::Filepath
                        } else {
                            CacheType::Path
                        }),
                        value: value!(message).into(),
                    }));
                }
                b"--debugger" | b"--debugger-pipe" | b"--debugger-dap-log" => {
                    let name = String::from_utf8_lossy(flag);
                    if flag != b"--debugger" {
                        let label = if flag == b"--debugger-pipe" {
                            "path"
                        } else {
                            "file"
                        };
                        let _ = value!(format!("No {label} specified for {name}"));
                    }
                    return Err(format!("CMake was not built with support for {name}"));
                }
                b"--warn-uninitialized" => {
                    out(host, "Warn about uninitialized values.\n");
                    opts.warning_flags.push("-Wuninitialized".into());
                }
                b"--no-warn-unused-cli" => {
                    out(
                        host,
                        "Not searching for unused variables given on the command line.\n",
                    );
                    opts.warning_flags.push("-Wno-unused-cli".into());
                }
                b"--check-system-vars" => {
                    out(host, "Find problems with variable usage in system files.\n")
                }
                b"--debug-output" => out(host, "Running with debug output on.\n"),
                b"--debug-trycompile" => {
                    out(host, "debug trycompile on\n");
                    opts.debug_trycompile = true;
                }
                b"--debug-find" => {
                    out(
                        host,
                        "Running with debug output on for the 'find' commands.\n",
                    );
                    opts.cache_edits.push(CacheEdit::Define(CacheDefine {
                        name: "CMAKE_FIND_DEBUG_MODE".into(),
                        ty: Some(CacheType::Bool),
                        value: "ON".into(),
                    }));
                }
                b"--debug-find-pkg" | b"--debug-find-var" => {
                    let list = value!("No names specified for debug find");
                    let category = if flag == b"--debug-find-pkg" {
                        "package(s)"
                    } else {
                        "variable(s)"
                    };
                    let names = String::from_utf8_lossy(list).replace(',', " ");
                    out(
                        host,
                        &format!(
                            "Running with debug output on for the 'find' commands for {category} {names}.\n"
                        ),
                    );
                }
                b"--graphviz" => {
                    opts.graphviz = Some(resolve_cli_path(
                        host,
                        value!("No file specified for --graphviz"),
                    ));
                }
                b"--trace" => {
                    out(host, "Put cmake in trace mode.\n");
                    opts.trace.get_or_insert_with(TraceConfig::default);
                }
                b"--trace-expand" => {
                    out(
                        host,
                        "Put cmake in trace mode, but with variables expanded.\n",
                    );
                    let trace = opts.trace.get_or_insert_with(TraceConfig::default);
                    trace.expand = true;
                }
                b"--trace-format" => {
                    out(
                        host,
                        "Put cmake in trace mode and sets the trace output format.\n",
                    );
                    let fmt = value!("No format specified for --trace-format");
                    let format = match fmt {
                        b"human" => TraceFormat::Human,
                        b"json-v1" => TraceFormat::JsonV1,
                        _ => {
                            return Err("Invalid format specified for --trace-format. Valid formats are human, json-v1.".into());
                        }
                    };
                    let trace = opts.trace.get_or_insert_with(TraceConfig::default);
                    trace.format = format;
                }
                b"--trace-source" => {
                    let src = value!("No file specified for --trace-source");
                    let trace = opts.trace.get_or_insert_with(TraceConfig::default);
                    trace.sources.push(src.into());
                }
                b"--trace-redirect" => {
                    let path = value!("No file specified for --trace-redirect");
                    let absolute = resolve_cli_path(host, path);
                    if let Err(err) = host.write(&absolute, b"", WriteMode::Truncate) {
                        crate::emit_cli_error(
                            host,
                            format!(
                                "Error opening trace file {}: {err}",
                                String::from_utf8_lossy(path)
                            ),
                        );
                        return Err(String::new());
                    }
                    out(
                        host,
                        &format!(
                            "Put cmake in trace mode and redirect trace output to a file instead of stderr.\nTrace will be written to {}\n",
                            absolute.display()
                        ),
                    );
                    let trace = opts.trace.get_or_insert_with(TraceConfig::default);
                    trace.redirect = Some(absolute);
                }
                b"--profiling-format" => {
                    profile_format =
                        Some(value!("Invalid format specified for --profiling-format"));
                }
                b"--profiling-output" => {
                    opts.profile_output = Some(resolve_cli_path(
                        host,
                        value!("No file specified for --profiling-output"),
                    ));
                }
                b"--print-config-dir" => {
                    print_config_dir(host);
                    return Ok(None);
                }
                _ => return Err(unknown_option(arg)),
            }
            continue;
        }
        if arg.is_empty() {
            warn(
                host,
                "Ignoring empty string (\"\") provided on the command line.",
            );
            continue;
        }
        if !arg.starts_with(b"-") {
            opts.directories
                .push(DirectoryArg::Positional(resolve_cli_path(host, arg)));
            continue;
        }
        match arg {
            b"-h" | b"-help" => {
                out(host, USAGE);
                print_generators(host, Stream::Stdout);
                return Ok(None);
            }
            b"-i" => {
                host.write_out(
                    Stream::Stderr,
                    b"The \"cmake -i\" wizard mode is no longer supported.\n",
                );
                return Err(String::new());
            }
            b"-N" => opts.view = true,
            b"-L" | b"-LA" | b"-LH" | b"-LAH" | b"-LR" | b"-LRA" | b"-LRH" | b"-LRAH" => {
                let pattern = if arg.contains(&b'R') {
                    Some(
                        take_value(&mut iter)
                            .ok_or("No regex specified for -LR")?
                            .into(),
                    )
                } else {
                    None
                };
                opts.list_cache = Some(ListCache {
                    advanced: arg.contains(&b'A'),
                    help: arg.contains(&b'H'),
                    pattern,
                });
            }
            _ => {
                let (flag, tail) = arg.split_at(arg.len().min(2));
                let value = |iter: &mut std::iter::Peekable<_>| {
                    if tail.is_empty() {
                        take_value(iter)
                    } else {
                        Some(tail)
                    }
                };
                match flag {
                    b"-S" | b"-H" | b"-B" => {
                        let (is_build, label) = if flag == b"-B" {
                            (true, "build")
                        } else {
                            (false, "source")
                        };
                        let path = resolve_cli_path(
                            host,
                            value(&mut iter).filter(|v| !v.is_empty()).ok_or_else(|| {
                                format!(
                                    "No {label} directory specified for {}",
                                    String::from_utf8_lossy(flag)
                                )
                            })?,
                        );
                        opts.directories.push(if is_build {
                            DirectoryArg::Binary(path)
                        } else {
                            DirectoryArg::Source(path)
                        });
                    }
                    b"-C" => {
                        let path = if tail.is_empty() {
                            take_value(&mut iter).ok_or("-C must be followed by a file name.")?
                        } else {
                            tail.strip_prefix(b"=").unwrap_or(tail).trim_ascii_start()
                        };
                        if path.is_empty() {
                            return Err("No file name specified for -C".into());
                        }
                        opts.cache_edits
                            .push(CacheEdit::Preload(rcmake_host::path::to_native(path)));
                    }
                    b"-D" => {
                        let raw = value(&mut iter).ok_or("-D must be followed with VAR=VALUE.")?;
                        let Some(mut def) = parse_define(raw) else {
                            host.write_out(Stream::Stderr, format!("CMake Error: Parse error in command line argument: {}\n Should be: VAR:type=value\n\n", String::from_utf8_lossy(raw)).as_bytes());
                            return Err(cli_usage_error_only(host));
                        };
                        if let Some(home) = host.env_get(b"HOME") {
                            if def.value.as_slice() == b"~" {
                                def.value = home;
                            } else if let Some(relative) = def.value.strip_prefix(b"~/") {
                                def.value = [home.as_slice(), b"/", relative].concat().into();
                            }
                        }
                        opts.cache_edits.push(CacheEdit::Define(def));
                    }
                    b"-U" => {
                        opts.cache_edits.push(CacheEdit::Remove(
                            value(&mut iter)
                                .ok_or("-U must be followed with VAR.")?
                                .into(),
                        ));
                    }
                    b"-W" => {
                        let raw =
                            value(&mut iter).ok_or("-W must be followed with [no-]<name>.")?;
                        let warning = [b"-W", raw].concat();
                        if !apply_warning_flag(&warning, &mut DiagState::default()) {
                            let name = raw.strip_prefix(b"no-").unwrap_or(raw);
                            let name = name.strip_prefix(b"error=").unwrap_or(name);
                            return Err(if name.is_empty() {
                                "No warning name provided.".into()
                            } else {
                                format!(
                                    "The warning category \"{}\" is not known.",
                                    String::from_utf8_lossy(name)
                                )
                            });
                        }
                        opts.warning_flags.push(warning.into());
                    }
                    b"-G" => {
                        let maybe = value(&mut iter).filter(|v| !v.is_empty());
                        if maybe.is_none_or(|g| !is_generator(g)) {
                            generator_error(host, maybe);
                            return Err(String::new());
                        }
                        opts.generator = maybe.map(BString::from);
                    }
                    b"-A" | b"-T" => {
                        let (slot, missing) = if flag == b"-A" {
                            (&mut opts.architecture, "No platform specified for -A")
                        } else {
                            (&mut opts.toolset, "No toolset specified for -T")
                        };
                        *slot = Some(value(&mut iter).ok_or(missing)?.into());
                    }
                    b"-P" => {
                        let Some(script) = value(&mut iter) else {
                            crate::emit_cli_error(host, "No script specified for argument -P");
                            return Err(String::new());
                        };
                        opts.script = Some(resolve_cli_path(host, script));
                    }
                    _ => return Err(unknown_option(arg)),
                }
            }
        }
    }
    if opts.script.is_some() {
        if opts.fresh {
            crate::emit_cli_error(host, "--fresh allowed only when configuring a project");
            return Err(String::new());
        }
        if opts.preset.is_some() || opts.list_presets.is_some() || opts.presets_file.is_some() {
            host.write_out(
                Stream::Stderr,
                b"CMake Error:\n  Presets are not supported in CMake script mode.\n",
            );
            return Err(String::new());
        }
    }
    if profile_format.is_some() || opts.profile_output.is_some() {
        let error = match (profile_format, &opts.profile_output) {
            (Some(b"google-trace"), Some(path)) => host
                .write(path, b"[\n]\n", WriteMode::Truncate)
                .err()
                .map(|_| {
                    format!(
                        "Could not start profiling: Unable to open: {}",
                        path.display()
                    )
                }),
            (Some(b"google-trace"), None) => {
                Some("--profiling-format specified but no --profiling-output!".into())
            }
            _ => Some("Invalid format specified for --profiling-format".into()),
        };
        if let Some(error) = error {
            crate::emit_cli_error(host, error);
            return Err(String::new());
        }
    }
    Ok(Some(opts))
}

fn cli_usage_error_only(host: &dyn Host) -> String {
    host.write_out(
        Stream::Stderr,
        b"CMake Error: Run 'cmake --help' for all supported options.\n",
    );
    String::new()
}

pub(crate) fn split_flag(arg: &[u8], separator: u8) -> (&[u8], Option<&[u8]>) {
    if let Some(i) = arg.iter().position(|&b| b == separator) {
        (arg.get(..i).unwrap_or(arg), arg.get(i + 1..))
    } else {
        (arg, None)
    }
}

pub(crate) fn take_value<'a>(
    iter: &mut std::iter::Peekable<impl Iterator<Item = &'a [u8]>>,
) -> Option<&'a [u8]> {
    iter.next_if(|arg| !arg.starts_with(b"-"))
}

fn unknown_option(arg: &[u8]) -> String {
    unknown_mode_option(arg, LONG_OPTIONS)
}

pub(crate) fn unknown_mode_option(arg: &[u8], flags: &[&[u8]]) -> String {
    let mut message = format!("Unknown argument {}", String::from_utf8_lossy(arg));
    if let Some((flag, distance)) = flags
        .iter()
        .map(|flag| (*flag, edit_distance(arg, flag)))
        .min_by_key(|&(_, distance)| distance)
        && distance <= 2
    {
        message.push_str(&format!(
            ". Did you mean: {}?",
            String::from_utf8_lossy(flag)
        ));
    }
    message
}

fn edit_distance(a: &[u8], b: &[u8]) -> usize {
    if a.len().abs_diff(b.len()) > 2 {
        return 3;
    }
    let mut row: Vec<_> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let Some((first, rest)) = row.split_first_mut() else {
            return 3;
        };
        let mut diagonal = *first;
        *first = i + 1;
        let mut left = *first;
        for (cell, y) in rest.iter_mut().zip(b) {
            let above = *cell;
            *cell = (above + 1)
                .min(left + 1)
                .min(diagonal + usize::from(x != y));
            diagonal = above;
            left = *cell;
        }
    }
    row.last().copied().unwrap_or(3)
}

pub(crate) fn cli_usage_error(host: &dyn Host, body: &str) -> i32 {
    if !body.is_empty() {
        crate::emit_cli_error(host, body);
        cli_usage_error_only(host);
    }
    1
}

pub(crate) fn out(host: &dyn Host, text: &str) {
    host.write_out(Stream::Stdout, text.as_bytes());
}

pub(crate) fn warn(host: &dyn Host, text: &str) {
    host.write_out(
        Stream::Stderr,
        format!("CMake Warning:\n  {text}\n\n\n").as_bytes(),
    );
}

const GENERATORS: &[(&str, &str)] = &[
    ("Unix Makefiles", "Generates standard UNIX makefiles."),
    ("Ninja", "Generates build.ninja files."),
    (
        "Ninja Multi-Config",
        "Generates build-<Config>.ninja files.",
    ),
    ("Xcode", "Generate Xcode project files."),
    (
        "Visual Studio 17 2022",
        "Generates Visual Studio 2022 project files.",
    ),
    (
        "Visual Studio 18 2026",
        "Generates Visual Studio 2026 project files.",
    ),
];

fn generators() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    GENERATORS.iter().filter(|(name, _)| match *name {
        "Xcode" => cfg!(target_os = "macos"),
        name if name.starts_with("Visual Studio") => cfg!(windows),
        _ => true,
    })
}

pub(crate) fn is_generator(name: &[u8]) -> bool {
    generators().any(|(g, _)| name == g.as_bytes())
}

pub(crate) fn default_generator(host: &dyn Host) -> BString {
    host.env_get(b"CMAKE_GENERATOR")
        .filter(|v| is_generator(v))
        .unwrap_or_else(|| "Unix Makefiles".into())
}

fn print_generators(host: &dyn Host, stream: Stream) {
    let default = default_generator(host);
    if stream == Stream::Stdout
        && host
            .env_get(b"CMAKE_GENERATOR")
            .is_some_and(|g| !is_generator(&g))
    {
        invalid_generator_env(host);
    }
    let mut text = String::from("Generators\n");
    for (name, desc) in generators() {
        let marker = if name.as_bytes() == default.as_slice() {
            '*'
        } else {
            ' '
        };
        text.push_str(&format!("{marker} {name:<28} = {desc}\n"));
    }
    host.write_out(stream, text.as_bytes());
}

pub(crate) fn generator_error(host: &dyn Host, generator: Option<&[u8]>) {
    if host.env_get(b"CMAKE_GENERATOR").is_none() {
        for var in [
            "CMAKE_GENERATOR_INSTANCE",
            "CMAKE_GENERATOR_PLATFORM",
            "CMAKE_GENERATOR_TOOLSET",
        ] {
            if host.env_get(var.as_bytes()).is_some() {
                host.write_out(Stream::Stderr, format!("Warning: Environment variable {var} will be ignored, because CMAKE_GENERATOR is not set.\n").as_bytes());
            }
        }
    }
    if let Some(generator) = generator {
        crate::emit_cli_error(
            host,
            format!(
                "Could not create named generator {}",
                String::from_utf8_lossy(generator)
            ),
        );
    } else {
        crate::emit_cli_error(host, "No generator specified for -G");
        if host
            .env_get(b"CMAKE_GENERATOR")
            .is_some_and(|g| !is_generator(&g))
        {
            invalid_generator_env(host);
        }
    }
    host.write_out(Stream::Stderr, b"\n");
    print_generators(host, Stream::Stderr);
}

fn invalid_generator_env(host: &dyn Host) {
    crate::emit_cli_error(
        host,
        "CMAKE_GENERATOR was set but the specified generator doesn't exist. Using CMake default.",
    );
}

fn print_config_dir(host: &dyn Host) {
    let from_env = host.env_get(b"CMAKE_CONFIG_DIR");
    if let Some(dir) = from_env {
        out(host, &format!("{}\n", String::from_utf8_lossy(&dir)));
        return;
    }
    let home = host.env_get(b"HOME").unwrap_or_default();
    let home = String::from_utf8_lossy(&home);
    let dir = if cfg!(target_os = "macos") {
        format!("{home}/Library/Application\\ Support/CMake")
    } else if cfg!(windows) {
        format!("{home}/AppData/Local/CMake")
    } else {
        host.env_get(b"XDG_CONFIG_HOME")
            .filter(|p| !p.is_empty())
            .map_or_else(
                || format!("{home}/.config/cmake"),
                |dir| format!("{}/cmake", String::from_utf8_lossy(&dir)),
            )
    };
    out(host, &format!("{dir}\n"));
}

fn show_help(_host: &dyn Host, topic: &[u8], variable: bool) -> Result<(), String> {
    let flag = if variable {
        "--help-variable"
    } else {
        "--help"
    };
    Err(format!(
        "Argument \"{}\" to {flag} is not a defined variable.",
        String::from_utf8_lossy(topic)
    ))
}
