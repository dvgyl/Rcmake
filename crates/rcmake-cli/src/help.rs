use rcmake_base::BString;
use rcmake_host::{Host, ReadRange, Stream, WriteMode, discover_cmake_root};

use crate::{Tool, options, resolve_cli_path};

const COMPAT_USAGE: &[u8] = include_bytes!("../help/usage.txt");
const NATIVE_USAGE: &[u8] = include_bytes!("../help/native_usage.txt");
const CCMAKE_USAGE: &[u8] = include_bytes!("../help/ccmake_usage.txt");

/// Short usage printed by `ccmake` when invoked with no arguments.
pub(crate) const CCMAKE_SHORT_USAGE: &[u8] = b"Usage\n\n  ccmake <path-to-source>\n  ccmake <path-to-existing-build>\n\nSpecify a source directory to (re-)generate a build system for it in the\ncurrent working directory.  Specify an existing build directory to\nre-generate its build system.\n\nRun 'ccmake --help' for more information.\n\n";

const MISSING_MANUAL: &str =
    "is not an available manual.  Use --help-manual-list to see all available manuals.";

const TOPICS: &[(&str, &str)] = &[
    (
        "command",
        "is not a CMake command.  Use --help-command-list to see all commands.",
    ),
    (
        "diagnostic",
        "is not a CMake diagnostic.  Use --help-diagnostic-list to see all diagnostics.",
    ),
    ("manual", MISSING_MANUAL),
    ("module", "is not a CMake module."),
    ("policy", "is not a CMake policy."),
    (
        "property",
        "is not a CMake property.  Use --help-property-list to see all properties.",
    ),
    (
        "variable",
        "is not a defined variable.  Use --help-variable-list to see all defined variables.",
    ),
];

const MANUALS: &[(&str, &str)] = &[
    ("commands", "cmake-commands.7"),
    ("diagnostics", "cmake-diagnostics.7"),
    ("modules", "cmake-modules.7"),
    ("policies", "cmake-policies.7"),
    ("properties", "cmake-properties.7"),
    ("variables", "cmake-variables.7"),
];

/// Options that CMake 4.5.1 no longer supports. CMake prints a warning, stops
/// reading documentation options, and exits with status zero.
const REMOVED_OPTIONS: &[&[u8]] = &[
    b"--copyright",
    b"--help-compatcommands",
    b"--help-custom-modules",
    b"--help-html",
    b"--help-man",
];

enum Request<'a> {
    Usage,
    Version { json: bool },
    Topic(&'static str, String, &'static str),
    Keyword(&'a BString),
    List(&'static str),
    Full,
}

impl Request<'_> {
    /// `true` for a request that reads the reference documentation.
    fn uses_reference(&self) -> bool {
        !matches!(self, Self::Usage | Self::Version { .. })
    }
}

/// The version request that `flag` names, as `cmDocumentation::CheckOptions` accepts it.
fn version_request(flag: &[u8]) -> Option<Request<'static>> {
    let (name, format) = options::split_flag(flag, b'=');
    if !matches!(name, b"--version" | b"-version" | b"/V" | b"/version") {
        return None;
    }
    match format {
        None => Some(Request::Version { json: false }),
        Some(b"json-v1") => Some(Request::Version { json: true }),
        // Another format is an unknown argument for the tool parser.
        Some(_) => None,
    }
}

fn argument<'a>(
    args: &mut std::iter::Peekable<std::slice::Iter<'a, BString>>,
) -> Option<&'a BString> {
    args.next_if(|arg| !arg.starts_with(b"-") && !matches!(arg.as_slice(), b"/V" | b"/?"))
}

/// Print the documentation that `args` requests for `tool`, in command-line order.
///
/// Return `None` if `args` has no documentation request. For `ctest` and `cpack`,
/// return `None` also if no request reads the reference documentation: the tool
/// parser then prints its own usage and version text.
pub(crate) fn run(
    argv0: Option<&BString>,
    args: &[BString],
    tool: Tool,
    persona: crate::persona::Persona,
    host: &dyn Host,
) -> Option<i32> {
    // `cmake` and `ccmake` stop at `--` and `cpack` stops at `-G`. `ctest` reads every argument.
    let exit_option: Option<&[u8]> = match tool {
        Tool::Cmake | Tool::Ccmake => Some(b"--".as_slice()),
        Tool::Ctest => None,
        Tool::Cpack => Some(b"-G".as_slice()),
    };
    let mut args = args.iter().peekable();
    let mut requests = Vec::new();
    let mut removed_option = false;
    while let Some(flag) = args.next() {
        if exit_option == Some(flag.as_slice()) {
            break;
        }
        if REMOVED_OPTIONS.contains(&flag.as_slice()) {
            host.write_out(
                Stream::Stderr,
                format!("Warning: {flag} no longer supported\n").as_bytes(),
            );
            removed_option = true;
            break;
        }
        let request = if let Some(request) = version_request(flag) {
            request
        } else if matches!(
            flag.as_slice(),
            b"--help" | b"-help" | b"-usage" | b"-h" | b"-H" | b"/?"
        ) {
            match argument(&mut args).filter(|arg| !arg.is_empty()) {
                Some(name) => Request::Keyword(name),
                None => Request::Usage,
            }
        } else if let Some(flag) = flag.strip_prefix(b"--help-") {
            let Ok(flag) = std::str::from_utf8(flag) else {
                continue;
            };
            match flag {
                "full" => Request::Full,
                _ => {
                    if let Some(&(kind, error)) = TOPICS.iter().find(|(kind, _)| *kind == flag) {
                        let name =
                            argument(&mut args).map_or_else(String::new, ToString::to_string);
                        Request::Topic(kind, name, error)
                    } else if let Some(&(list, _)) = TOPICS
                        .iter()
                        .find(|(kind, _)| flag.strip_suffix("-list") == Some(*kind))
                    {
                        Request::List(list)
                    } else if let Some(&(_, name)) =
                        MANUALS.iter().find(|(alias, _)| *alias == flag)
                    {
                        Request::Topic("manual", name.to_owned(), MISSING_MANUAL)
                    } else {
                        continue;
                    }
                }
            }
        } else {
            continue;
        };
        let output = if matches!(request, Request::Usage) {
            None
        } else {
            argument(&mut args)
        };
        requests.push((request, output));
    }
    let needs_reference = requests.iter().any(|(request, _)| request.uses_reference());
    if requests.is_empty() && !removed_option {
        return None;
    }
    if !matches!(tool, Tool::Cmake | Tool::Ccmake) && !needs_reference && !removed_option {
        return None;
    }
    let reference = if needs_reference {
        let compressed = discover_cmake_root(host, argv0).1.and_then(|root| {
            host.read(
                &root.join("Help/reference.zst"),
                ReadRange::All {
                    limit: 4 * 1024 * 1024,
                },
            )
            .ok()
        });
        match compressed.and_then(|raw| zstd::bulk::decompress(&raw, 32 * 1024 * 1024).ok()) {
            Some(data) => data,
            None => {
                crate::emit_cli_error(host, "Cannot read CMake help.");
                return Some(1);
            }
        }
    } else {
        Vec::new()
    };
    let mut stdout = false;
    let mut status = 0;
    for (request, output) in requests {
        if matches!(request, Request::Usage) && !matches!(tool, Tool::Cmake | Tool::Ccmake) {
            separate(host, &mut stdout);
            host.write_out(Stream::Stdout, tool_usage(tool));
            continue;
        }
        if matches!(request, Request::Usage) && tool == Tool::Ccmake {
            separate(host, &mut stdout);
            host.write_out(Stream::Stdout, CCMAKE_USAGE);
            options::print_help_generators(host);
            host.write_out(Stream::Stdout, b"\n");
            continue;
        }
        if matches!(request, Request::Usage) {
            separate(host, &mut stdout);
            if persona == crate::persona::Persona::Native {
                let native = String::from_utf8_lossy(COMPAT_USAGE)
                    .replace("  cmake [options]", "  rcmake [options]")
                    .replace("\"cmake -E\"", "\"rcmake -E\"")
                    .replace("\"cmake --build\"", "\"rcmake --build\"")
                    .replace("\"cmake --install\"", "\"rcmake --install\"");
                host.write_out(Stream::Stdout, native.as_bytes());
            } else {
                host.write_out(Stream::Stdout, COMPAT_USAGE);
            }
            options::print_help_generators(host);
            host.write_out(Stream::Stdout, b"\n");
            if persona == crate::persona::Persona::Native {
                host.write_out(Stream::Stdout, NATIVE_USAGE);
            }
            continue;
        }
        let content = match &request {
            Request::Version { json } => {
                Ok(std::borrow::Cow::Owned(crate::version_text(tool, *json)))
            }
            _ => request.text(&reference).map(std::borrow::Cow::Borrowed),
        };
        let content = match content {
            Ok(bytes) => bytes,
            Err(message) => {
                status = 1;
                std::borrow::Cow::Owned(message.into_bytes())
            }
        };
        if let Some(output) = output {
            if host
                .write(
                    &resolve_cli_path(host, output),
                    &content,
                    WriteMode::Truncate,
                )
                .is_err()
            {
                status = 1;
            }
        } else {
            separate(host, &mut stdout);
            host.write_out(Stream::Stdout, &content);
        }
    }
    Some(status)
}

/// The `--help` usage text of `ctest` or `cpack`.
fn tool_usage(tool: Tool) -> &'static [u8] {
    match tool {
        Tool::Cmake => COMPAT_USAGE,
        Tool::Ccmake => CCMAKE_USAGE,
        Tool::Ctest => rcmake_ctest::USAGE,
        Tool::Cpack => rcmake_cpack::USAGE,
    }
}

fn separate(host: &dyn Host, seen: &mut bool) {
    if std::mem::replace(seen, true) {
        host.write_out(Stream::Stdout, b"\n\n");
    }
}

impl Request<'_> {
    fn text<'a>(&self, reference: &'a [u8]) -> Result<&'a [u8], String> {
        match self {
            Self::Topic(kind, name, error) => {
                let name = match *kind {
                    "command" => name.to_ascii_lowercase(),
                    "diagnostic" => name.to_ascii_uppercase(),
                    _ => name.clone(),
                };
                let value = match *kind {
                    "manual" => manual(reference, &name),
                    "property" | "variable" => lookup_any_case(reference, kind, &name)
                        .or_else(|| lookup_any_case(reference, kind, &generalize(&name))),
                    _ => lookup_any_case(reference, kind, &name),
                };
                let flag = if *kind == "diagnostic" {
                    "command"
                } else {
                    kind
                };
                value.ok_or_else(|| format!("Argument \"{name}\" to --help-{flag} {error}\n"))
            }
            Self::Keyword(name) => {
                let name = name.to_string();
                lookup(reference, "keyword", &name)
                    .or_else(|| lookup(reference, "keyword", &generalize(&name)))
                    .or_else(|| lookup(reference, "command", &name.to_ascii_lowercase()))
                    .ok_or_else(|| format!("Argument \"{name}\" to --help did not match any keywords.  Use --help without any arguments to print CMake help information.\n"))
            }
            Self::List(kind) => {
                lookup(reference, &format!("{kind}-list"), "").ok_or_else(missing_help)
            }
            Self::Full => lookup(reference, "full", "").ok_or_else(missing_help),
            Self::Usage | Self::Version { .. } => Ok(COMPAT_USAGE),
        }
    }
}

fn missing_help() -> String {
    "Cannot read CMake help.\n".to_owned()
}

fn manual<'a>(reference: &'a [u8], name: &str) -> Option<&'a [u8]> {
    if let Some((name, section)) = name
        .strip_suffix(')')
        .and_then(|name| name.rsplit_once('('))
    {
        return lookup_any_case(reference, "manual", &format!("{name}.{section}"));
    }
    lookup_any_case(reference, "manual", name).or_else(|| {
        (0..=9)
            .find_map(|section| lookup_any_case(reference, "manual", &format!("{name}.{section}")))
    })
}

fn generalize(name: &str) -> String {
    name.split('_')
        .map(|part| match part {
            "C" | "CXX" | "CUDA" | "Fortran" | "HIP" | "ISPC" | "OBJC" | "OBJCXX" | "Swift" => {
                "<LANG>"
            }
            "DEBUG" | "RELEASE" | "RELWITHDEBINFO" | "MINSIZEREL" => "<CONFIG>",
            _ => part,
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn lookup<'a>(reference: &'a [u8], kind: &str, name: &str) -> Option<&'a [u8]> {
    find(reference, kind, |key| key == name.as_bytes())
}

/// The text for `name`, or else for a name that differs only in ASCII case.
///
/// CMake finds a help file with a file glob. The default macOS and Windows file
/// systems ignore case, so `--help-policy cmp0001` shows `CMP0001`.
fn lookup_any_case<'a>(reference: &'a [u8], kind: &str, name: &str) -> Option<&'a [u8]> {
    lookup(reference, kind, name).or_else(|| {
        find(reference, kind, |key| {
            key.eq_ignore_ascii_case(name.as_bytes())
        })
    })
}

fn find<'a>(
    mut reference: &'a [u8],
    kind: &str,
    matches: impl Fn(&[u8]) -> bool,
) -> Option<&'a [u8]> {
    while let Some(end) = reference.iter().position(|&byte| byte == 0) {
        let (key, rest) = reference.split_at(end);
        let rest = rest.get(1..)?;
        let (len, rest) = rest.split_first_chunk::<4>()?;
        let (text, next) = rest.split_at_checked(u32::from_le_bytes(*len) as usize)?;
        if key
            .strip_prefix(kind.as_bytes())
            .and_then(|key| key.strip_prefix(b"\t"))
            .is_some_and(&matches)
        {
            return Some(text);
        }
        reference = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use rcmake_base::BString;
    use rcmake_host::{PureHost, Stream};

    const VERSION: &str = "cmake version 4.5.1\nrcmake 0.2.0\n";

    /// A host with a small reference database in the default `CMAKE_ROOT`.
    fn host_with_reference(entries: &[(&str, &str)]) -> Arc<PureHost> {
        let host = crate::driver_tests::project_host();
        let mut data = Vec::new();
        for (key, text) in entries {
            data.extend_from_slice(key.as_bytes());
            data.push(0);
            let len = u32::try_from(text.len()).unwrap_or(u32::MAX);
            data.extend_from_slice(&len.to_le_bytes());
            data.extend_from_slice(text.as_bytes());
        }
        let compressed = zstd::bulk::compress(&data, 1).unwrap_or_default();
        host.seed_file(
            Path::new("/usr/local/share/cmake-4.5/Help/reference.zst"),
            &compressed,
        )
        .unwrap_or_else(|error| panic!("cannot seed the reference: {error}"));
        host
    }

    fn invoke(host: &Arc<PureHost>, args: &[&str]) -> (i32, BString, BString) {
        let argv: Vec<BString> = args.iter().copied().map(BString::from).collect();
        let code = crate::run(&argv, host.clone());
        (
            code,
            host.stream_bytes(Stream::Stdout),
            host.stream_bytes(Stream::Stderr),
        )
    }

    #[test]
    fn topic_lookup_ignores_ascii_case() {
        let entries = [
            ("policy\tCMP0001", "policy text\n"),
            ("property\tLOCATION", "property text\n"),
            ("variable\tCMAKE_VERSION", "variable text\n"),
            ("module\tFindZLIB", "module text\n"),
            ("manual\tcmake.1", "manual text\n"),
        ];
        for (args, expected) in [
            (["cmake", "--help-policy", "cmp0001"], "policy text\n"),
            (["cmake", "--help-property", "location"], "property text\n"),
            (
                ["cmake", "--help-variable", "cmake_version"],
                "variable text\n",
            ),
            (["cmake", "--help-module", "findzlib"], "module text\n"),
            (["cmake", "--help-manual", "CMAKE.1"], "manual text\n"),
            (["cmake", "--help-manual", "CMAKE(1)"], "manual text\n"),
        ] {
            let host = host_with_reference(&entries);
            let (code, stdout, stderr) = invoke(&host, &args);
            assert_eq!(code, 0, "{args:?}: {stderr}");
            assert_eq!(stdout, BString::from(expected), "{args:?}");
        }

        // The error message keeps the original spelling.
        let host = host_with_reference(&entries);
        let (code, stdout, _) = invoke(&host, &["cmake", "--help-policy", "cmp9999"]);
        assert_eq!(code, 1);
        assert_eq!(
            stdout,
            BString::from("Argument \"cmp9999\" to --help-policy is not a CMake policy.\n")
        );
    }

    #[test]
    fn version_is_read_anywhere_before_double_dash() {
        let host = host_with_reference(&[]);
        let (code, stdout, stderr) = invoke(&host, &["cmake", "-S", ".", "--version"]);
        assert_eq!(
            (code, stdout, stderr),
            (0, BString::from(VERSION), BString::default())
        );

        let host = host_with_reference(&[]);
        let (code, stdout, _) = invoke(&host, &["cmake", "--version", "--help"]);
        assert_eq!(code, 0);
        let expected = format!("{VERSION}\n\nUsage\n");
        assert!(
            stdout.starts_with(expected.as_bytes()),
            "{}",
            String::from_utf8_lossy(&stdout)
        );

        let host = host_with_reference(&[]);
        let (_, stdout, _) = invoke(&host, &["cmake", "--help", "--version"]);
        assert!(stdout.starts_with(b"Usage\n"));
        assert!(stdout.ends_with(format!("\n\n{VERSION}").as_bytes()));
    }

    #[test]
    fn invalid_version_format_is_an_unknown_argument_for_each_tool() {
        for (args, expected) in [
            (
                ["cmake", "--version=invalid"],
                "CMake Error: Unknown argument --version=invalid\nCMake Error: Run 'cmake --help' for all supported options.\n",
            ),
            (
                ["ctest", "--version=invalid"],
                "CMake Error: Unknown argument: --version=invalid\nCMake Error: Run 'ctest --help' for all supported options.\n",
            ),
            (
                ["cpack", "--version=invalid"],
                "CPack Error: Unknown argument: --version=invalid\n",
            ),
        ] {
            let host = host_with_reference(&[]);
            let (code, stdout, stderr) = invoke(&host, &args);
            assert_eq!(code, 1, "{args:?}");
            assert!(stdout.is_empty(), "{args:?}");
            assert_eq!(stderr, BString::from(expected), "{args:?}");
        }
    }

    #[test]
    fn removed_options_warn_and_exit_with_status_zero() {
        for flag in ["--copyright", "--help-html", "--help-man"] {
            let host = host_with_reference(&[]);
            let (code, stdout, stderr) = invoke(&host, &["cmake", flag]);
            assert_eq!(code, 0, "{flag}");
            assert!(stdout.is_empty(), "{flag}");
            assert_eq!(
                stderr,
                BString::from(format!("Warning: {flag} no longer supported\n"))
            );
        }
        // Requests before the removed option still print.
        let host = host_with_reference(&[]);
        let (code, stdout, _) = invoke(&host, &["cmake", "--version", "--copyright"]);
        assert_eq!((code, stdout), (0, BString::from(VERSION)));
    }

    #[test]
    fn ctest_and_cpack_print_reference_topics() {
        let entries = [
            ("command\tset", "set text\n"),
            ("manual-list\t", "cmake(1)\nctest(1)\n"),
        ];
        let host = host_with_reference(&entries);
        let (code, stdout, _) = invoke(&host, &["ctest", "--help-command", "SET"]);
        assert_eq!((code, stdout), (0, BString::from("set text\n")));

        let host = host_with_reference(&entries);
        let (code, stdout, _) = invoke(&host, &["cpack", "--help-manual-list"]);
        assert_eq!((code, stdout), (0, BString::from("cmake(1)\nctest(1)\n")));

        // `cpack` stops reading documentation options at `-G`.
        let host = host_with_reference(&entries);
        let (code, _, stderr) = invoke(&host, &["cpack", "-G", "TGZ", "--help-manual-list"]);
        assert_eq!(code, 1);
        assert!(stderr.starts_with(b"CPack Error: Unknown argument: --help-manual-list"));

        // Plain usage and version stay with the tool parser.
        let host = host_with_reference(&entries);
        let (code, stdout, _) = invoke(&host, &["ctest", "--help"]);
        assert_eq!((code, stdout), (0, BString::from(rcmake_ctest::USAGE)));

        let host = host_with_reference(&entries);
        let (code, stdout, _) = invoke(&host, &["ctest", "--version", "--help-command", "set"]);
        assert_eq!(code, 0);
        assert_eq!(
            stdout,
            BString::from("ctest version 4.5.1\nrcmake 0.2.0\n\n\nset text\n")
        );
    }

    #[test]
    fn ccmake_prints_usage_version_help_and_topics_without_a_tty() {
        let entries = [("command\tset", "set text\n")];

        let host = host_with_reference(&entries);
        let (code, stdout, stderr) = invoke(&host, &["ccmake"]);
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        assert_eq!(stdout, BString::from(super::CCMAKE_SHORT_USAGE));

        let host = host_with_reference(&entries);
        let (code, stdout, stderr) = invoke(&host, &["ccmake", "--version"]);
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        assert_eq!(
            stdout,
            BString::from("ccmake version 4.5.1\nrcmake 0.2.0\n")
        );

        let host = host_with_reference(&entries);
        let (code, stdout, stderr) = invoke(&host, &["ccmake", "--version=json-v1"]);
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        assert!(
            String::from_utf8_lossy(&stdout).contains("\"name\" : \"ccmake\""),
            "{stdout}"
        );

        let host = host_with_reference(&entries);
        let (code, stdout, stderr) = invoke(&host, &["ccmake", "--help"]);
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        let help = String::from_utf8_lossy(&stdout);
        assert!(
            help.starts_with("Usage\n\n  ccmake <path-to-source>"),
            "{help}"
        );
        assert!(help.contains("Options\n"), "{help}");
        assert!(help.contains("Generators\n"), "{help}");

        let host = host_with_reference(&entries);
        let (code, stdout, _) = invoke(&host, &["ccmake", "--help-command", "set"]);
        assert_eq!((code, stdout), (0, BString::from("set text\n")));
    }
}
