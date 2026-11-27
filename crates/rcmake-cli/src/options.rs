use std::path::PathBuf;

use rcmake_base::{BString, suggest::levenshtein};
use rcmake_eval::{CacheDefine, LogLevel, TraceConfig};
use rcmake_host::{Host, Stream};
use rcmake_model::GeneratorKind;

use crate::persona::Persona;
use crate::presets::Listing;
use crate::resolve_cli_path;

mod cache;
mod debug;
mod diagnostics;
pub(crate) mod extensions;
mod generator;
mod presets;
mod trace;

#[derive(Clone)]
pub(crate) enum CacheEdit {
    Define(CacheDefine),
    Remove(BString),
    Preload(PathBuf),
}

pub(crate) enum DirectoryArg {
    Source(PathBuf),
    Binary(PathBuf),
    Positional(PathBuf, bool),
}

#[derive(Default)]
pub(crate) struct Options {
    pub persona: Persona,
    /// True only when invoked via a recognized `rcmake-shim` launcher prefix.
    pub shim_handoff: bool,
    pub extensions: extensions::RcmakeExt,
    pub directories: Vec<DirectoryArg>,
    pub cache_edits: Vec<CacheEdit>,
    pub check_system_vars: bool,
    pub warning_flags: Vec<BString>,
    pub ignore_link_warning_as_error: bool,
    /// `--compile-no-warning-as-error`.
    pub ignore_compile_warning_as_error: bool,
    pub regenerate_during_build: bool,
    pub generator: Option<BString>,
    pub architecture: Option<BString>,
    pub toolset: Option<BString>,
    pub log_level: Option<LogLevel>,
    pub log_context: bool,
    pub debug_trycompile: bool,
    pub debug_output: bool,
    pub debug_find: bool,
    pub debug_find_variables: Vec<BString>,
    pub debug_find_packages: Vec<BString>,
    /// `--debugger`.
    pub debugger: bool,
    /// `--debugger-pipe <path>`.
    pub debugger_pipe: Option<PathBuf>,
    /// `--debugger-dap-log <path>`.
    pub debugger_dap_log: Option<PathBuf>,
    /// `-P` was given. Unlike [`Options::script`], the run does not take this out.
    pub script_mode: bool,
    pub sarif: Option<PathBuf>,
    pub script: Option<PathBuf>,
    /// `--find-package`: run `CMakeFindPackageMode.cmake` (find-package tool mode).
    pub find_package: bool,
    pub preset: Option<BString>,
    pub presets_file: Option<PathBuf>,
    pub list_presets: Option<Listing>,
    pub view: bool,
    pub fresh: bool,
    pub list_cache: Option<ListCache>,
    pub profile_output: Option<PathBuf>,
    pub graphviz: Option<PathBuf>,
    pub trace: Option<TraceConfig>,
    pub project_file: Option<BString>,
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

const NATIVE_USAGE: &str = "Usage\n\n  rcmake [options] <path-to-source>\n  rcmake [options] <path-to-existing-build>\n  rcmake [options] -S <path-to-source> -B <path-to-build>\n\nSpecify a source directory to (re-)generate a build system for it in the\ncurrent working directory.  Specify an existing build directory to\nre-generate its build system.\n\nRun 'rcmake --help' for more information.\n\n";

const NATIVE_FLAGS: &[&[u8]] = &[
    b"--hermetic",
    b"--profile",
    b"--diagnostics",
    b"--fetch-lock",
    b"--no-probe-cache",
    b"--export-compile-commands",
];

/// Options that CMake 4.5.1 suggests for an unknown configure argument, in the
/// order that `cmake::SetArgs` declares them. A Windows CMake also has
/// `WINDOWS_SUGGESTIONS`.
const COMPAT_SUGGESTIONS: &[&[u8]] = &[
    b"-S",
    b"-H",
    b"-O",
    b"-B",
    b"--fresh",
    b"-P",
    b"-D",
    b"-C",
    b"-U",
    b"-W",
    b"-A",
    b"-T",
    b"--toolchain",
    b"--install-prefix",
    b"--check-build-system",
    b"--check-stamp-file",
    b"--check-stamp-list",
    b"--regenerate-during-build",
    b"--find-package",
    b"--graphviz",
    b"--debug-trycompile",
    b"--debug-output",
    b"--log-level",
    b"--loglevel",
    b"--log-context",
    b"--project-file",
    b"--debug-find",
    b"--debug-find-pkg",
    b"--debug-find-var",
    b"--trace",
    b"--trace-expand",
    b"--trace-format",
    b"--trace-source",
    b"--trace-redirect",
    b"--warn-uninitialized",
    b"--warn-unused-vars",
    b"--no-warn-unused-cli",
    b"--check-system-vars",
    b"--compile-no-warning-as-error",
    b"--link-no-warning-as-error",
    b"--debugger",
    b"--debugger-pipe",
    b"--debugger-dap-log",
    b"--profiling-format",
    b"--profiling-output",
    b"--preset",
    b"--presets-file",
    b"--list-presets",
];

/// The configure options that only a Windows CMake has.
const WINDOWS_SUGGESTIONS: &[&[u8]] = &[b"--vs-solution-file"];

/// Long options that the Native persona suggests for an unknown argument.
const LONG_OPTIONS: &[&[u8]] = &[
    b"--build",
    b"--install",
    b"--open",
    b"--version",
    b"--help",
    b"--help-variable",
    b"--fresh",
    b"--regenerate-during-build",
    b"--link-no-warning-as-error",
    b"--compile-no-warning-as-error",
    b"--find-package",
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
    b"--warn-unused-vars",
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
    b"--project-file",
];

pub(crate) fn parse(args: &[BString], persona: Persona, host: &dyn Host) -> ParseResult {
    if args.is_empty() {
        out(
            host,
            if persona == Persona::Native {
                NATIVE_USAGE
            } else {
                USAGE
            },
        );
        return ParseResult::Exit(0);
    }
    match parse_args(args, persona, host) {
        Ok(Some(opts)) => ParseResult::Run(Box::new(opts)),
        Ok(None) => ParseResult::Exit(0),
        Err(body) => ParseResult::Exit(cli_usage_error(host, persona, &body)),
    }
}

type Arguments<'a> = std::iter::Peekable<std::slice::Iter<'a, BString>>;

struct Argument<'a, 'rest> {
    original: &'a [u8],
    flag: &'a [u8],
    inline: Option<&'a [u8]>,
    rest: &'rest mut Arguments<'a>,
}

impl<'a, 'rest> Argument<'a, 'rest> {
    fn new(original: &'a [u8], rest: &'rest mut Arguments<'a>) -> Self {
        let (flag, inline) = if let Some(path) = original.strip_prefix(b"--install-prefix ") {
            (
                b"--install-prefix".as_slice(),
                Some(path.trim_ascii_start()),
            )
        } else if original.starts_with(b"--") {
            split_flag(original, b'=')
        } else {
            let (flag, tail) = original.split_at(original.len().min(2));
            (flag, (!tail.is_empty()).then_some(tail))
        };
        Self {
            original,
            flag,
            inline,
            rest,
        }
    }

    fn value(&mut self) -> Option<&'a [u8]> {
        self.inline.or_else(|| take_value(self.rest))
    }

    fn required(&mut self, missing: impl Into<String>) -> Result<&'a [u8], String> {
        self.value()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| missing.into())
    }
}

fn parse_args(
    args: &[BString],
    persona: Persona,
    host: &dyn Host,
) -> Result<Option<Options>, String> {
    let mut opts = Options {
        persona,
        ..Options::default()
    };
    // Any `-P` argument selects script mode. Only script mode stops option
    // processing at `--`, so that the script can take arbitrary arguments.
    let script_role = args.iter().any(|arg| arg.starts_with(b"-P"));
    let mut configure_only_extension = None;
    let mut last_unknown = None;
    let mut iter = args.iter().peekable();
    let mut profile_format = None;
    while let Some(raw) = iter.next() {
        if script_role && raw.as_slice() == b"--" {
            if opts.script.is_none() {
                // CMake also stops here when `-P` comes after `--`: it runs nothing.
                return Ok(None);
            }
            break;
        }
        let mut arg = Argument::new(raw, &mut iter);
        if opts.script.is_some() && !script_mode_flag(arg.flag) {
            continue;
        }
        if arg.flag == b"--find-package" && arg.inline.is_none() {
            opts.find_package = true;
            continue;
        }
        if arg.flag == b"--print-config-dir" {
            print_config_dir(host);
            return Ok(None);
        }
        if persona == Persona::Native
            && extensions::parse_native_extension_flag(arg.original, &mut opts.extensions)
                .map_err(|error| error.to_string())?
        {
            if !extensions::is_script_mode_extension(arg.flag) {
                configure_only_extension.get_or_insert(arg.flag);
            }
            if arg.original == b"--profile"
                && let Some(next) = arg.rest.peek()
            {
                let next = next.as_slice();
                if !next.is_empty() && !next.starts_with(b"-") {
                    opts.extensions.bare_profile_arg = Some(resolve_project_path(host, next));
                }
            }
            continue;
        }
        if parse_project(&mut arg, &mut opts, host)?
            || presets::parse(&mut arg, &mut opts, host)?
            || generator::parse(&mut arg, &mut opts, host)?
            || cache::parse(&mut arg, &mut opts, host)?
            || diagnostics::parse(&mut arg, &mut opts, host)?
            || trace::parse(&mut arg, &mut opts, host)?
            || debug::parse(&mut arg, &mut opts, host, &mut profile_format)?
        {
            continue;
        }
        // CMake reports the last unknown argument.
        last_unknown = Some(raw);
    }
    if opts.script.is_none()
        && let Some(flag) = last_unknown
    {
        return Err(unknown_option(flag, persona));
    }
    if opts.script.is_some()
        && let Some(flag) = configure_only_extension
    {
        return Err(format!(
            "{} allowed only when configuring a project",
            String::from_utf8_lossy(flag)
        ));
    }
    validate_script_mode(&opts, host)?;
    debug::validate_profile(host, &opts, profile_format)?;
    Ok(Some(opts))
}

fn script_mode_flag(flag: &[u8]) -> bool {
    matches!(
        flag,
        b"--fresh"
            | b"--preset"
            | b"--presets-file"
            | b"--list-presets"
            | b"--log-level"
            | b"--loglevel"
            | b"--log-context"
            | b"--debugger"
            | b"--debugger-pipe"
            | b"--debugger-dap-log"
            | b"--diagnostics"
    )
}

fn parse_project(
    arg: &mut Argument<'_, '_>,
    opts: &mut Options,
    host: &dyn Host,
) -> Result<bool, String> {
    match arg.original {
        b"" => warn(
            host,
            "Ignoring empty string (\"\") provided on the command line.",
        ),
        raw if !raw.starts_with(b"-") => {
            opts.directories.push(DirectoryArg::Positional(
                resolve_project_path(host, raw),
                raw.ends_with(b"/"),
            ));
        }
        b"-i" => {
            host.write_out(
                Stream::Stderr,
                b"The \"cmake -i\" wizard mode is no longer supported.\n",
            );
            return Err(String::new());
        }
        b"-N" => opts.view = true,
        raw @ (b"-L" | b"-LA" | b"-LH" | b"-LAH" | b"-LR" | b"-LRA" | b"-LRH" | b"-LRAH") => {
            let pattern = if raw.contains(&b'R') {
                Some(
                    take_value(arg.rest)
                        .ok_or("No regex specified for -LR")?
                        .into(),
                )
            } else {
                None
            };
            opts.list_cache = Some(ListCache {
                advanced: raw.contains(&b'A'),
                help: raw.contains(&b'H'),
                pattern,
            });
        }
        _ => match arg.flag {
            b"--regenerate-during-build" if arg.inline.is_none() => {
                opts.regenerate_during_build = true
            }
            b"--graphviz" => {
                opts.graphviz = Some(resolve_cli_path(
                    host,
                    arg.required("No file specified for --graphviz")?,
                ));
            }
            b"-S" | b"-H" | b"-B" => parse_directory(arg, opts, host)?,
            b"--project-file" => {
                let name = arg.required("No filename specified for --project-file")?;
                if opts.project_file.is_some() {
                    return Err("Multiple --project-file options not allowed".into());
                }
                opts.project_file = Some(name.into());
            }
            b"-P" => {
                let Some(script) = arg.value() else {
                    crate::emit_cli_error(host, "No script specified for argument -P");
                    return Err(String::new());
                };
                opts.script = Some(resolve_cli_path(host, script));
                opts.script_mode = true;
            }
            _ => return Ok(false),
        },
    }
    Ok(true)
}

fn parse_directory(
    arg: &mut Argument<'_, '_>,
    opts: &mut Options,
    host: &dyn Host,
) -> Result<(), String> {
    let is_build = arg.flag == b"-B";
    let label = if is_build { "build" } else { "source" };
    let flag = String::from_utf8_lossy(arg.flag);
    let path = resolve_project_path(
        host,
        arg.required(format!("No {label} directory specified for {flag}"))?,
    );
    if arg.flag == b"-H" {
        host.write_out(
            Stream::Stderr,
            b"The -H option is deprecated.  Use -S instead.\n",
        );
    }
    opts.directories.push(if is_build {
        DirectoryArg::Binary(path)
    } else {
        DirectoryArg::Source(path)
    });
    Ok(())
}

fn resolve_project_path(host: &dyn Host, raw: &[u8]) -> PathBuf {
    use rcmake_base::path_lex::Lexical;
    use rcmake_host::path::{from_native_cmake, to_native};

    let logical = resolve_cli_path(host, raw);
    let relative = to_native(raw);
    if relative.is_absolute()
        || !relative
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        return logical;
    }
    let cwd = host.initial_cwd();
    let Ok(physical_cwd) = host.canonicalize(&cwd) else {
        return logical;
    };
    let physical = physical_cwd.join(relative);
    let physical = to_native(&Lexical::new(&from_native_cmake(&physical)).normalize());
    match host.canonicalize(&physical) {
        Ok(actual) if host.canonicalize(&logical).ok().as_ref() != Some(&actual) => physical,
        _ => logical,
    }
}

fn validate_script_mode(opts: &Options, host: &dyn Host) -> Result<(), String> {
    if opts.script.is_none() {
        return Ok(());
    }
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
    Ok(())
}

pub(crate) fn cli_usage_error_only(host: &dyn Host, persona: Persona) -> String {
    let message = if persona == Persona::Native {
        b"CMake Error: Run 'rcmake --help' for all supported options.\n".as_slice()
    } else {
        b"CMake Error: Run 'cmake --help' for all supported options.\n".as_slice()
    };
    host.write_out(Stream::Stderr, message);
    String::new()
}

pub(crate) fn split_flag(arg: &[u8], separator: u8) -> (&[u8], Option<&[u8]>) {
    if let Some(i) = arg.iter().position(|&b| b == separator) {
        (arg.get(..i).unwrap_or(arg), arg.get(i + 1..))
    } else {
        (arg, None)
    }
}

pub(crate) fn take_value<'a, T: AsRef<[u8]> + ?Sized + 'a>(
    iter: &mut std::iter::Peekable<impl Iterator<Item = &'a T>>,
) -> Option<&'a [u8]> {
    iter.next_if(|arg| !arg.as_ref().starts_with(b"-"))
        .map(AsRef::as_ref)
}

/// The first flag with the smallest edit distance to `arg`, as `cmFindClosestString` selects it.
///
/// The accepted distance grows with the length of `arg`: about one edit per five
/// characters, from one to four edits. A flag equal to `arg` is never a suggestion,
/// so that an error cannot suggest the argument that it rejects.
fn closest_flag<'a>(arg: &[u8], flags: impl IntoIterator<Item = &'a [u8]>) -> Option<&'a [u8]> {
    let max_distance = (arg.len() / 5 + 1).clamp(1, 4);
    flags
        .into_iter()
        .filter(|&flag| flag != arg && arg.len().abs_diff(flag.len()) <= max_distance)
        .map(|flag| (flag, levenshtein(arg, flag)))
        .filter(|&(_, distance)| distance <= max_distance)
        .min_by_key(|&(_, distance)| distance)
        .map(|(flag, _)| flag)
}

fn unknown_option(arg: &[u8], persona: Persona) -> String {
    if persona == Persona::Compat {
        let windows: &[&[u8]] = if cfg!(windows) {
            WINDOWS_SUGGESTIONS
        } else {
            &[]
        };
        return unknown_message(arg, arg, COMPAT_SUGGESTIONS.iter().chain(windows).copied());
    }
    unknown_message(
        arg,
        split_flag(arg, b'=').0,
        LONG_OPTIONS.iter().chain(NATIVE_FLAGS).copied(),
    )
}

pub(crate) fn unknown_mode_option(arg: &[u8], flags: &[&[u8]]) -> String {
    unknown_message(arg, arg, flags.iter().copied())
}

/// `Unknown argument <arg>`, with the flag closest to `probe` as a suggestion.
fn unknown_message<'a>(
    arg: &[u8],
    probe: &[u8],
    flags: impl IntoIterator<Item = &'a [u8]>,
) -> String {
    let mut message = format!("Unknown argument {}", String::from_utf8_lossy(arg));
    if let Some(flag) = closest_flag(probe, flags) {
        message.push_str(&format!(
            ". Did you mean: {}?",
            String::from_utf8_lossy(flag)
        ));
    }
    message
}

pub(crate) fn cli_usage_error(host: &dyn Host, persona: Persona, body: &str) -> i32 {
    if !body.is_empty() {
        crate::emit_cli_error(host, body);
        cli_usage_error_only(host, persona);
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

/// `true` if `name` is a generator that this host can create (`-G` host gate).
///
/// A Visual Studio name passes only on Windows, or in a build with the `vs-cross`
/// feature. Every other known name gets the CMake "Could not create named generator"
/// error.
pub(crate) fn is_generator(name: &[u8]) -> bool {
    GeneratorKind::from_name(name).is_some_and(GeneratorKind::host_available)
}

pub(crate) fn default_generator(host: &dyn Host) -> BString {
    host.env_get(b"CMAKE_GENERATOR")
        .filter(|v| is_generator(v))
        .unwrap_or_else(|| "Unix Makefiles".into())
}

/// Print the generators for this host and mark the default generator.
pub(crate) fn print_generators(host: &dyn Host, stream: Stream) {
    write_generators(host, stream, "Generators\n");
}

pub(crate) fn print_help_generators(host: &dyn Host) {
    write_generators(
        host,
        Stream::Stdout,
        "\nGenerators\n\nThe following generators are available on this platform (* marks default):\n",
    );
}

fn write_generators(host: &dyn Host, stream: Stream, heading: &str) {
    let default = default_generator(host);
    if stream == Stream::Stdout
        && host
            .env_get(b"CMAKE_GENERATOR")
            .is_some_and(|g| !is_generator(&g))
    {
        invalid_generator_env(host);
    }
    let mut text = String::from(heading);
    for kind in GeneratorKind::available() {
        let (name, desc) = (kind.name(), kind.description());
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

#[derive(Clone, Copy)]
enum ConfigPlatform {
    Windows,
    Macos,
    Unix,
}

fn format_config_output_path(dir: &str, platform: ConfigPlatform) -> String {
    match platform {
        ConfigPlatform::Windows => dir.replace('/', "\\"),
        ConfigPlatform::Macos | ConfigPlatform::Unix => dir.replace(' ', "\\ "),
    }
}

/// The CMake config directory (`cmSystemTools::GetCMakeConfigDirectory`).
///
/// On macOS and other Unix hosts, a variable that is set wins even when its value
/// is empty, and CMake does not remove a trailing slash.
fn resolve_config_dir(host: &dyn Host, platform: ConfigPlatform) -> String {
    let env_set = |key: &[u8]| {
        host.env_get(key)
            .map(|value| String::from_utf8_lossy(&value).into_owned())
    };
    let env_nonempty = |key: &[u8]| env_set(key).filter(|value| !value.is_empty());
    if let Some(dir) = env_set(b"CMAKE_CONFIG_DIR") {
        return format_config_output_path(&dir, platform);
    }
    let dir = match platform {
        ConfigPlatform::Windows => {
            if let Some(local) = env_nonempty(b"LOCALAPPDATA") {
                let base = local.trim_end_matches(['\\', '/']);
                format!("{base}\\CMake")
            } else if let Some(profile) =
                env_nonempty(b"USERPROFILE").or_else(|| env_nonempty(b"HOME"))
            {
                let base = profile.trim_end_matches(['\\', '/']);
                format!("{base}\\AppData\\Local\\CMake")
            } else {
                String::new()
            }
        }
        ConfigPlatform::Macos => {
            if let Some(xdg) = env_set(b"XDG_CONFIG_HOME") {
                format!("{xdg}/CMake")
            } else if let Some(home) = env_set(b"HOME") {
                format!("{home}/Library/Application Support/CMake")
            } else {
                String::new()
            }
        }
        ConfigPlatform::Unix => {
            if let Some(xdg) = env_set(b"XDG_CONFIG_HOME") {
                format!("{xdg}/cmake")
            } else if let Some(home) = env_set(b"HOME") {
                format!("{home}/.config/cmake")
            } else {
                String::new()
            }
        }
    };
    format_config_output_path(&dir, platform)
}

fn print_config_dir(host: &dyn Host) {
    let platform = if cfg!(windows) {
        ConfigPlatform::Windows
    } else if cfg!(target_os = "macos") {
        ConfigPlatform::Macos
    } else {
        ConfigPlatform::Unix
    };
    let dir = resolve_config_dir(host, platform);
    out(host, &format!("{dir}\n"));
}

#[cfg(test)]
mod tests {
    use rcmake_base::BString;
    use rcmake_host::{Host, PureHost, Stream};

    use super::{
        ConfigPlatform, Options, ParseResult, Persona, parse, resolve_config_dir,
        unknown_mode_option, unknown_option,
    };

    fn parse_with(persona: Persona, args: &[&str]) -> (PureHost, Result<Options, i32>) {
        let host = PureHost::default();
        let args: Vec<BString> = args.iter().copied().map(BString::from).collect();
        let result = match parse(&args, persona, &host) {
            ParseResult::Run(opts) => Ok(*opts),
            ParseResult::Exit(code) => Err(code),
        };
        (host, result)
    }

    fn run_options(result: Result<Options, i32>) -> Options {
        match result {
            Ok(opts) => opts,
            Err(code) => panic!("the parser exited with {code}"),
        }
    }

    fn stderr(host: &PureHost) -> String {
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned()
    }

    #[test]
    fn unknown_option_suggests_the_first_closest_flag_within_two_edits() {
        assert_eq!(
            unknown_mode_option(b"--xax", &[b"--cat", b"--bar", b"--ax"]),
            "Unknown argument --xax. Did you mean: --ax?"
        );
        assert_eq!(
            unknown_mode_option(b"--xax", &[b"--cat", b"--bar"]),
            "Unknown argument --xax. Did you mean: --cat?"
        );
        assert_eq!(
            unknown_mode_option(b"--xyz", &[b"--cat", b"--bar"]),
            "Unknown argument --xyz"
        );
        assert_eq!(
            unknown_option(b"--opne", Persona::Native),
            "Unknown argument --opne. Did you mean: --open?"
        );
    }

    #[test]
    fn compat_suggestions_use_the_cmake_candidates_and_distance_limit() {
        for (argument, expected) in [
            ("--opne", "Unknown argument --opne"),
            ("--", "Unknown argument --. Did you mean: -S?"),
            ("-X", "Unknown argument -X. Did you mean: -S?"),
            ("--S", "Unknown argument --S. Did you mean: -S?"),
            (
                "--check-stamp-fil",
                "Unknown argument --check-stamp-fil. Did you mean: --check-stamp-file?",
            ),
            ("--sarif-outpt", "Unknown argument --sarif-outpt"),
            ("--help-comand", "Unknown argument --help-comand"),
        ] {
            assert_eq!(
                unknown_option(argument.as_bytes(), Persona::Compat),
                expected
            );
        }
    }

    #[test]
    fn unknown_option_never_suggests_the_rejected_flag() {
        for persona in [Persona::Compat, Persona::Native] {
            for flag in ["--build", "--install", "--version", "--workflow"] {
                assert_eq!(
                    unknown_option(flag.as_bytes(), persona),
                    format!("Unknown argument {flag}")
                );
            }
        }
        assert_eq!(
            unknown_mode_option(b"--ax", &[b"--ax", b"--bx"]),
            "Unknown argument --ax. Did you mean: --bx?"
        );
    }

    #[test]
    fn double_dash_is_unknown_in_configure_mode() {
        let (host, result) = parse_with(Persona::Compat, &["--", "-S", "."]);
        assert_eq!(result.err(), Some(1));
        assert_eq!(
            stderr(&host),
            "CMake Error: Unknown argument --. Did you mean: -S?\nCMake Error: Run 'cmake --help' for all supported options.\n"
        );
    }

    #[test]
    fn double_dash_ends_options_only_in_script_mode() {
        let (host, result) = parse_with(Persona::Compat, &["-P", "x.cmake", "--", "-S", "--bad"]);
        let opts = run_options(result);
        assert!(opts.script.is_some());
        assert!(stderr(&host).is_empty());

        // `--` before `-P` stops processing before the script is known: nothing runs.
        let (host, result) = parse_with(Persona::Compat, &["--", "-P", "x.cmake"]);
        assert_eq!(result.err(), Some(0));
        assert!(stderr(&host).is_empty());
        assert!(host.stream_bytes(Stream::Stdout).is_empty());
    }

    #[test]
    fn script_mode_accepts_diagnostics_and_rejects_configure_extensions() {
        let (_, result) = parse_with(Persona::Native, &["--diagnostics=rich", "-P", "x.cmake"]);
        let opts = run_options(result);
        assert_eq!(
            opts.extensions.diagnostics,
            Some(crate::persona::DiagStyle::Rich)
        );

        for flag in [
            "--profile=/tmp/p",
            "--hermetic",
            "--fetch-lock=frozen",
            "--no-probe-cache",
        ] {
            let (host, result) = parse_with(Persona::Native, &[flag, "-P", "x.cmake"]);
            assert_eq!(result.err(), Some(1), "{flag}");
            let name = flag.split('=').next().unwrap_or(flag);
            assert!(
                stderr(&host).starts_with(&format!(
                    "CMake Error: {name} allowed only when configuring a project\n"
                )),
                "{flag}: {}",
                stderr(&host)
            );
        }
    }

    #[test]
    fn unknown_argument_error_names_the_last_unknown_argument() {
        let (host, result) = parse_with(Persona::Compat, &["--fooo", "--barr"]);
        assert_eq!(result.err(), Some(1));
        assert!(stderr(&host).starts_with("CMake Error: Unknown argument --barr\n"));
    }

    #[test]
    fn deprecated_warning_options_match_cmake_text() {
        let (host, result) = parse_with(
            Persona::Compat,
            &[
                "--warn-uninitialized",
                "--no-warn-unused-cli",
                "--warn-unused-vars",
                "--check-system-vars",
                "-P",
                "x.cmake",
            ],
        );
        let opts = run_options(result);
        assert!(opts.check_system_vars);
        assert_eq!(
            opts.warning_flags,
            [
                BString::from("-Wuninitialized"),
                BString::from("-Wno-unused-cli")
            ]
        );
        assert_eq!(
            stderr(&host),
            "The --warn-uninitialized option is deprecated.  Use -Wuninitialized instead.\nThe --no-warn-unused-cli option is deprecated.  Use -Wno-unused-cli instead.\n"
        );
        assert_eq!(
            host.stream_bytes(Stream::Stdout),
            BString::from(
                "Also check system files when warning about unused and uninitialized variables.\n"
            )
        );
    }

    #[test]
    fn trace_source_prints_the_cmake_banner() {
        let (host, result) = parse_with(
            Persona::Compat,
            &["--trace-source=a.cmake", "-P", "x.cmake"],
        );
        assert!(result.is_ok());
        assert_eq!(
            host.stream_bytes(Stream::Stdout),
            BString::from(
                "Put cmake in trace mode, but output only lines of a specified file. Multiple options are allowed.\n"
            )
        );
    }

    #[test]
    fn config_dir_keeps_set_but_empty_variables() {
        let host = PureHost::default();
        host.env_set(b"XDG_CONFIG_HOME", Some(b"/tmp/test"));
        host.env_set(b"CMAKE_CONFIG_DIR", Some(b""));
        assert_eq!(resolve_config_dir(&host, ConfigPlatform::Macos), "");

        let host = PureHost::default();
        host.env_set(b"HOME", Some(b"/Users/alice"));
        host.env_set(b"XDG_CONFIG_HOME", Some(b""));
        assert_eq!(resolve_config_dir(&host, ConfigPlatform::Macos), "/CMake");
        assert_eq!(resolve_config_dir(&host, ConfigPlatform::Unix), "/cmake");

        let host = PureHost::default();
        host.env_set(b"XDG_CONFIG_HOME", Some(b"/xdg/"));
        assert_eq!(
            resolve_config_dir(&host, ConfigPlatform::Unix),
            "/xdg//cmake"
        );
    }

    #[test]
    fn native_unknown_option_suggests_native_extensions_and_compat_does_not() {
        assert_eq!(
            unknown_option(b"--hermtic", Persona::Native),
            "Unknown argument --hermtic. Did you mean: --hermetic?"
        );
        assert_eq!(
            unknown_option(b"--hermtic=strict", Persona::Native),
            "Unknown argument --hermtic=strict. Did you mean: --hermetic?"
        );
        assert_eq!(
            unknown_option(b"--hermtic", Persona::Compat),
            "Unknown argument --hermtic"
        );
        assert_eq!(
            unknown_option(b"--export-compile-command", Persona::Native),
            "Unknown argument --export-compile-command. Did you mean: --export-compile-commands?"
        );
        assert_eq!(
            unknown_option(b"--export-compile-command", Persona::Compat),
            "Unknown argument --export-compile-command"
        );
    }

    #[test]
    fn debugger_rejects_inline_values_and_script_mode_flag_includes_diagnostics() {
        assert!(super::script_mode_flag(b"--diagnostics"));
        // Configure mode rejects `--debugger=foo` as an unknown argument.
        let (host, result) = parse_with(Persona::Compat, &["--debugger=foo"]);
        assert_eq!(result.err(), Some(1));
        assert!(
            stderr(&host).contains("Unknown argument --debugger=foo"),
            "stderr={}",
            stderr(&host)
        );
        // Script mode does not enable the debugger for `--debugger=foo`.
        let (_, script_dbg) = parse_with(Persona::Compat, &["--debugger=foo", "-P", "x.cmake"]);
        assert!(!run_options(script_dbg).debugger);
        // `--diagnostics` after `-P` is still recognized in Native script mode.
        let (_, post_p) = parse_with(Persona::Native, &["-P", "x.cmake", "--diagnostics=rich"]);
        assert_eq!(
            run_options(post_p).extensions.diagnostics,
            Some(crate::persona::DiagStyle::Rich)
        );
    }

    #[test]
    fn resolves_config_dir_across_windows_macos_and_unix() {
        let host = PureHost::default();
        host.env_set(b"HOME", Some(b"/Users/alice"));
        assert_eq!(
            resolve_config_dir(&host, ConfigPlatform::Macos),
            "/Users/alice/Library/Application\\ Support/CMake"
        );
        assert_eq!(
            resolve_config_dir(&host, ConfigPlatform::Unix),
            "/Users/alice/.config/cmake"
        );

        host.env_set(b"XDG_CONFIG_HOME", Some(b"/xdg/config"));
        assert_eq!(
            resolve_config_dir(&host, ConfigPlatform::Macos),
            "/xdg/config/CMake"
        );
        assert_eq!(
            resolve_config_dir(&host, ConfigPlatform::Unix),
            "/xdg/config/cmake"
        );

        let win = PureHost::default();
        win.env_set(b"USERPROFILE", Some(b"C:\\Users\\alice"));
        assert_eq!(
            resolve_config_dir(&win, ConfigPlatform::Windows),
            "C:\\Users\\alice\\AppData\\Local\\CMake"
        );
        win.env_set(b"LOCALAPPDATA", Some(b"D:/LocalAppData"));
        assert_eq!(
            resolve_config_dir(&win, ConfigPlatform::Windows),
            "D:\\LocalAppData\\CMake"
        );

        win.env_set(b"CMAKE_CONFIG_DIR", Some(b"cmake_config_dir"));
        assert_eq!(
            resolve_config_dir(&win, ConfigPlatform::Windows),
            "cmake_config_dir"
        );
        assert_eq!(
            resolve_config_dir(&win, ConfigPlatform::Macos),
            "cmake_config_dir"
        );
    }
}
