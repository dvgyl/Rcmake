use std::path::PathBuf;

use rcmake_base::{BString, suggest::levenshtein};
use rcmake_eval::{CacheDefine, LogLevel, TraceConfig};
use rcmake_host::{Host, Stream};
use rcmake_model::GeneratorKind;

use crate::presets::Listing;
use crate::resolve_cli_path;

mod cache;
mod debug;
mod diagnostics;
mod generator;
mod presets;
mod trace;

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
    pub directories: Vec<DirectoryArg>,
    pub cache_edits: Vec<CacheEdit>,
    pub check_system_vars: bool,
    pub warning_flags: Vec<BString>,
    pub ignore_link_warning_as_error: bool,
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
    b"--link-no-warning-as-error",
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

fn parse_args(args: &[BString], host: &dyn Host) -> Result<Option<Options>, String> {
    let mut opts = Options::default();
    let mut iter = args.iter().peekable();
    let mut profile_format = None;
    while let Some(raw) = iter.next() {
        if raw.as_slice() == b"--" {
            break;
        }
        let mut arg = Argument::new(raw, &mut iter);
        if opts.script.is_some() && !script_mode_flag(arg.flag) {
            continue;
        }
        if arg.flag == b"--print-config-dir" {
            print_config_dir(host);
            return Ok(None);
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
        return Err(unknown_option(raw));
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
                resolve_cli_path(host, raw),
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
            b"-P" => {
                let Some(script) = arg.value() else {
                    crate::emit_cli_error(host, "No script specified for argument -P");
                    return Err(String::new());
                };
                opts.script = Some(resolve_cli_path(host, script));
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
    let path = resolve_cli_path(
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

pub(crate) fn take_value<'a, T: AsRef<[u8]> + ?Sized + 'a>(
    iter: &mut std::iter::Peekable<impl Iterator<Item = &'a T>>,
) -> Option<&'a [u8]> {
    iter.next_if(|arg| !arg.as_ref().starts_with(b"-"))
        .map(AsRef::as_ref)
}

fn unknown_option(arg: &[u8]) -> String {
    unknown_mode_option(arg, LONG_OPTIONS)
}

pub(crate) fn unknown_mode_option(arg: &[u8], flags: &[&[u8]]) -> String {
    const MAX_DISTANCE: usize = 2;
    let mut message = format!("Unknown argument {}", String::from_utf8_lossy(arg));
    if let Some((flag, _)) = flags
        .iter()
        .filter(|flag| arg.len().abs_diff(flag.len()) <= MAX_DISTANCE)
        .map(|flag| (*flag, levenshtein(arg, flag)))
        .filter(|&(_, distance)| distance <= MAX_DISTANCE)
        .min_by_key(|&(_, distance)| distance)
    {
        message.push_str(&format!(
            ". Did you mean: {}?",
            String::from_utf8_lossy(flag)
        ));
    }
    message
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

#[cfg(test)]
mod tests {
    use super::unknown_mode_option;

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
    }
}
