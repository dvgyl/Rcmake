use rcmake_base::BString;
use rcmake_host::{Host, ReadRange, Stream, WriteMode, discover_cmake_root};

use crate::{options, resolve_cli_path};

const NATIVE_USAGE: &[u8] = include_bytes!("../help/usage.txt");
const NATIVE_MARKER: &[u8] = b"\nRcmake extensions\n";

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

enum Request<'a> {
    Usage,
    Topic(&'static str, String, &'static str),
    Keyword(&'a BString),
    List(&'static str),
    Full,
}

fn argument<'a>(
    args: &mut std::iter::Peekable<std::slice::Iter<'a, BString>>,
) -> Option<&'a BString> {
    args.next_if(|arg| !arg.starts_with(b"-") && !matches!(arg.as_slice(), b"/V" | b"/?"))
}

pub(crate) fn run(
    argv0: Option<&BString>,
    args: &[BString],
    persona: crate::persona::Persona,
    host: &dyn Host,
) -> Option<i32> {
    let mut args = args.iter().peekable();
    let mut requests = Vec::new();
    while let Some(flag) = args.next() {
        if flag == b"--" {
            break;
        }
        let request = if matches!(
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
    if requests.is_empty() {
        return None;
    }
    let reference = if requests
        .iter()
        .any(|(request, _)| !matches!(request, Request::Usage))
    {
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
        if matches!(request, Request::Usage) {
            separate(host, &mut stdout);
            let (compat, native) = usage();
            host.write_out(Stream::Stdout, compat);
            options::print_help_generators(host);
            host.write_out(Stream::Stdout, b"\n");
            if persona == crate::persona::Persona::Native && !crate::persona::oracle_mode(host) {
                host.write_out(Stream::Stdout, native);
            }
            continue;
        }
        let content = match request.text(&reference) {
            Ok(bytes) => std::borrow::Cow::Borrowed(bytes),
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

fn usage() -> (&'static [u8], &'static [u8]) {
    let split = NATIVE_USAGE
        .windows(NATIVE_MARKER.len())
        .position(|bytes| bytes == NATIVE_MARKER)
        .unwrap_or(NATIVE_USAGE.len());
    NATIVE_USAGE.split_at(split)
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
                    "property" | "variable" => lookup(reference, kind, &name)
                        .or_else(|| lookup(reference, kind, &generalize(&name))),
                    _ => lookup(reference, kind, &name),
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
            Self::Usage => Ok(usage().0),
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
        return lookup(reference, "manual", &format!("{name}.{section}"));
    }
    lookup(reference, "manual", name).or_else(|| {
        (0..=9).find_map(|section| lookup(reference, "manual", &format!("{name}.{section}")))
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

fn lookup<'a>(mut reference: &'a [u8], kind: &str, name: &str) -> Option<&'a [u8]> {
    while let Some(end) = reference.iter().position(|&byte| byte == 0) {
        let (key, rest) = reference.split_at(end);
        let rest = rest.get(1..)?;
        let (len, rest) = rest.split_first_chunk::<4>()?;
        let (text, next) = rest.split_at_checked(u32::from_le_bytes(*len) as usize)?;
        if key
            .strip_prefix(kind.as_bytes())
            .and_then(|key| key.strip_prefix(b"\t"))
            == Some(name.as_bytes())
        {
            return Some(text);
        }
        reference = next;
    }
    None
}
