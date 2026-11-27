//! `compile_commands.json` reader with target inference from object paths.
//!
//! A `command` string is split only with a dialect that toolchain evidence supports.
//! Without that evidence, or when a response file cannot be read, the command stays
//! opaque and only its raw text is compared.

use std::collections::BTreeMap;
use std::path::Path;

use rcmake_base::{BString, cmdline::Arguments};
use rcmake_json::Value;

use crate::reader::{ArtifactReader, ReadError};
use crate::snapshot::{Attribution, CompileSettings, IncludeDir, TargetSnapshot};

const MAX_ENTRIES: usize = 200_000;
const MAX_TOKENS: usize = 65_536;
const MAX_RESPONSE_FILE: u64 = 1 << 20;
const MAX_RESPONSE_TOTAL: u64 = 16 << 20;

/// Command-line dialect of a compiler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// POSIX shell quoting and GNU-style flags.
    Gnu,
    /// Windows quoting and MSVC-style flags.
    Msvc,
}

/// A compiler that toolchain evidence names.
#[derive(Clone, Debug)]
pub(crate) struct Compiler {
    pub(crate) language: BString,
    pub(crate) path: BString,
    pub(crate) dialect: Option<Dialect>,
}

/// Result of reading the compile database.
#[derive(Debug, Default)]
pub(crate) struct CompileDatabase {
    pub(crate) targets: BTreeMap<(BString, BString), TargetSnapshot>,
    pub(crate) opaque: usize,
    pub(crate) partial: Option<BString>,
}

fn basename(path: &[u8]) -> &[u8] {
    path.rsplit(|byte| *byte == b'/' || *byte == b'\\')
        .next()
        .unwrap_or(path)
}

fn stem(path: &[u8]) -> BString {
    let name = basename(path);
    let name = name
        .strip_suffix(b".exe")
        .or_else(|| name.strip_suffix(b".EXE"))
        .unwrap_or(name);
    BString::from(name)
}

struct Entry<'a> {
    directory: BString,
    file: BString,
    output: Option<BString>,
    arguments: Option<Vec<BString>>,
    command: Option<&'a [u8]>,
}

fn parse_entry(value: &Value) -> Option<Entry<'_>> {
    let field = |name| value.get(name).and_then(Value::as_bytes);
    Some(Entry {
        directory: field("directory").map(BString::from).unwrap_or_default(),
        file: BString::from(field("file")?),
        output: field("output").map(BString::from),
        arguments: value
            .get("arguments")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_bytes)
                    .take(MAX_TOKENS)
                    .map(BString::from)
                    .collect()
            }),
        command: field("command"),
    })
}

fn absolute(directory: &[u8], path: &[u8]) -> BString {
    if path.starts_with(b"/") || path.get(1) == Some(&b':') || directory.is_empty() {
        return rcmake_base::path_lex::Lexical::new(path).normalize();
    }
    let joined = [directory, b"/", path].concat();
    rcmake_base::path_lex::Lexical::new(&joined).normalize()
}

/// Locate the compiler token. Tokens before it belong to a launcher.
fn locate<'c>(tokens: &[BString], compilers: &'c [Compiler]) -> Option<(usize, &'c Compiler)> {
    tokens
        .iter()
        .enumerate()
        .take(8)
        .find_map(|(index, token)| {
            compilers
                .iter()
                .find(|compiler| token == &compiler.path || stem(token) == stem(&compiler.path))
                .map(|compiler| (index, compiler))
        })
}

struct Expander<'r, 'h> {
    reader: &'r mut ArtifactReader<'h>,
    used: u64,
}

impl Expander<'_, '_> {
    /// Expand `@file` tokens once. Return `None` when a response file is unreadable.
    fn expand(
        &mut self,
        tokens: Vec<BString>,
        directory: &[u8],
        dialect: Dialect,
    ) -> Option<Vec<BString>> {
        let mut out = Vec::with_capacity(tokens.len());
        for token in tokens {
            let Some(file) = token.strip_prefix(b"@") else {
                out.push(token);
                continue;
            };
            let path = absolute(directory, file);
            let relative = self
                .reader
                .relative(Path::new(&String::from_utf8_lossy(&path).into_owned()))?;
            if self.used >= MAX_RESPONSE_TOTAL {
                return None;
            }
            let limit = MAX_RESPONSE_FILE.min(MAX_RESPONSE_TOTAL - self.used);
            let bytes = self.reader.read_limited(&relative, limit).ok()?;
            self.used += bytes.len() as u64;
            out.extend(Arguments::new(&bytes, dialect == Dialect::Msvc).take(MAX_TOKENS));
            if out.len() > MAX_TOKENS {
                return None;
            }
        }
        Some(out)
    }
}

fn classify(
    tokens: &[BString],
    source: &[u8],
    directory: &[u8],
    dialect: Dialect,
    language: Option<BString>,
) -> (CompileSettings, Option<BString>) {
    let mut settings = CompileSettings {
        language,
        ..CompileSettings::default()
    };
    let mut output = None;
    let mut iter = tokens.iter().peekable();
    let msvc = dialect == Dialect::Msvc;
    while let Some(token) = iter.next() {
        let t = token.as_slice();
        let is_source = absolute(directory, t) == absolute(directory, source);
        if is_source {
            continue;
        }
        match t {
            b"-c" | b"-MD" | b"-MMD" => continue,
            b"/c" | b"-showIncludes" | b"/showIncludes" if msvc => continue,
            b"-o" | b"-MT" | b"-MF" | b"-MQ" => {
                let value = iter.next();
                if t == b"-o" {
                    output = value.cloned();
                }
                continue;
            }
            _ if matches!(t, b"-D" | b"-I" | b"-isystem") || PAIRED.contains(&t) => {
                let value = iter.next().cloned().unwrap_or_default();
                match t {
                    b"-D" => settings.defines.push(value),
                    b"-I" => settings.includes.push(IncludeDir {
                        path: absolute(directory, &value),
                        system: false,
                    }),
                    b"-isystem" => settings.includes.push(IncludeDir {
                        path: absolute(directory, &value),
                        system: true,
                    }),
                    _ => settings.fragments.push([t, b" ", &value].concat().into()),
                }
                continue;
            }
            _ => {}
        }
        if let Some(rest) = t
            .strip_prefix(b"-o")
            .filter(|_| !msvc)
            .filter(|rest| !rest.is_empty())
        {
            output = Some(rest.into());
        } else if let Some(rest) = t
            .strip_prefix(b"-D")
            .or_else(|| t.strip_prefix(b"/D").filter(|_| msvc))
        {
            settings.defines.push(rest.into());
        } else if let Some(rest) = t
            .strip_prefix(b"-I")
            .or_else(|| t.strip_prefix(b"/I").filter(|_| msvc))
        {
            settings.includes.push(IncludeDir {
                path: absolute(directory, rest),
                system: false,
            });
        } else if let Some(rest) = t.strip_prefix(b"-isystem") {
            settings.includes.push(IncludeDir {
                path: absolute(directory, rest),
                system: true,
            });
        } else if let Some(rest) = t
            .strip_prefix(b"-std=")
            .or_else(|| t.strip_prefix(b"/std:").filter(|_| msvc))
        {
            settings.standard = Some(rest.into());
        } else if let Some(rest) = t.strip_prefix(b"/Fo").filter(|_| msvc) {
            output = Some(rest.into());
        } else if dependency_output(t, msvc) {
        } else {
            settings.fragments.push(token.clone());
        }
    }
    (settings, output)
}

/// Flags whose value is a separate token. Both readers join them into one token, so
/// that `-arch arm64` compares the same from a fragment list and from a command.
pub(crate) const PAIRED: [&[u8]; 7] = [
    b"-Xclang",
    b"-include",
    b"-imacros",
    b"-arch",
    b"-target",
    b"-iquote",
    b"-idirafter",
];

/// Join each [`PAIRED`] flag with the token after it.
pub(crate) fn group_pairs(tokens: impl IntoIterator<Item = BString>) -> Vec<BString> {
    let mut out = Vec::new();
    let mut tokens = tokens.into_iter();
    while let Some(token) = tokens.next() {
        if PAIRED.contains(&token.as_slice())
            && let Some(value) = tokens.next()
        {
            out.push([token.as_slice(), b" ", value.as_slice()].concat().into());
        } else {
            out.push(token);
        }
    }
    out
}

/// Return whether `token` names a debug database or dependency file that does not
/// change the compile result.
fn dependency_output(token: &[u8], msvc: bool) -> bool {
    if msvc {
        token.starts_with(b"/Fd") || token.starts_with(b"-Fd") || token == b"/FS"
    } else {
        token.starts_with(b"-MF") || token.starts_with(b"-MT") || token.starts_with(b"-MQ")
    }
}

/// Infer `(config, target)` from an object path such as `CMakeFiles/app.dir/Debug/main.o`.
pub(crate) fn infer_target(
    output: &[u8],
    configs: &[BString],
) -> Option<(Option<BString>, BString)> {
    let parts: Vec<&[u8]> = output
        .split(|byte| *byte == b'/' || *byte == b'\\')
        .collect();
    let at = parts.iter().position(|part| *part == b"CMakeFiles")?;
    let name = parts.get(at + 1)?.strip_suffix(b".dir")?;
    let config = parts
        .get(at + 2)
        .filter(|segment| {
            configs.len() > 1 && configs.iter().any(|config| config.as_slice() == **segment)
        })
        .map(|segment| BString::from(*segment));
    Some((config, name.into()))
}

/// Read `compile_commands.json` from the build root.
pub(crate) fn load(
    reader: &mut ArtifactReader<'_>,
    compilers: &[Compiler],
    configs: &[BString],
) -> Result<CompileDatabase, ReadError> {
    let value = reader.json(Path::new("compile_commands.json"))?;
    let entries = value.as_array().ok_or(ReadError::Json(
        "The compile database is not an array.".into(),
    ))?;
    let mut database = CompileDatabase::default();
    if entries.len() > MAX_ENTRIES {
        database.partial =
            Some(format!("compile_commands.json has more than {MAX_ENTRIES} entries.").into());
    }
    let default_config = configs.first().cloned().unwrap_or_default();
    let mut expander = Expander { reader, used: 0 };
    for entry in entries.iter().take(MAX_ENTRIES).filter_map(parse_entry) {
        let (tokens, known) = match (&entry.arguments, entry.command) {
            (Some(arguments), _) => (Some(arguments.clone()), None),
            (None, Some(command)) => {
                let probe: Vec<BString> = Arguments::new(command, false).take(8).collect();
                let windows: Vec<BString> = Arguments::new(command, true).take(8).collect();
                let found = locate(&probe, compilers).or_else(|| locate(&windows, compilers));
                match found.and_then(|(_, compiler)| compiler.dialect) {
                    Some(dialect) => (
                        Some(
                            Arguments::new(command, dialect == Dialect::Msvc)
                                .take(MAX_TOKENS + 1)
                                .collect(),
                        ),
                        Some(dialect),
                    ),
                    None => (None, None),
                }
            }
            (None, None) => continue,
        };
        let mut settings = None;
        let mut output = entry.output.clone();
        if let Some(tokens) = tokens.filter(|tokens| tokens.len() <= MAX_TOKENS) {
            let located = locate(&tokens, compilers);
            let dialect = known
                .or_else(|| located.and_then(|(_, compiler)| compiler.dialect))
                .or(entry.arguments.as_ref().map(|_| Dialect::Gnu));
            let start = located.map_or(1, |(index, _)| index + 1);
            let language = located.map(|(_, compiler)| compiler.language.clone());
            if let Some(dialect) = dialect {
                let rest = tokens.get(start..).unwrap_or_default().to_vec();
                if let Some(rest) = expander.expand(rest, &entry.directory, dialect) {
                    let (classified, from_flags) =
                        classify(&rest, &entry.file, &entry.directory, dialect, language);
                    output = output.or(from_flags);
                    settings = Some(classified);
                }
            }
        }
        let settings = settings.unwrap_or_else(|| {
            database.opaque += 1;
            CompileSettings {
                fragments: vec![entry.command.map_or_else(
                    || {
                        entry
                            .arguments
                            .as_ref()
                            .map(|args| args.join(&b' '))
                            .map(BString::from)
                            .unwrap_or_default()
                    },
                    BString::from,
                )],
                opaque: true,
                ..CompileSettings::default()
            }
        });
        let object = output
            .as_ref()
            .map(|output| absolute(&entry.directory, output));
        let inferred = object
            .as_deref()
            .and_then(|object| infer_target(object, configs));
        let (config, name) = match inferred {
            Some((config, name)) => (config.unwrap_or_else(|| default_config.clone()), name),
            None => {
                let dir = String::from_utf8_lossy(&entry.directory).into_owned();
                (
                    default_config.clone(),
                    format!("<unattributed:{dir}>").into(),
                )
            }
        };
        let target = database
            .targets
            .entry((config, name))
            .or_insert_with(|| TargetSnapshot {
                kind: None,
                attribution: Attribution::Inferred,
                sources: BTreeMap::new(),
                link: None,
                dependencies: std::collections::BTreeSet::new(),
                artifacts: Vec::new(),
                backtrace: None,
            });
        target
            .sources
            .insert(absolute(&entry.directory, &entry.file), settings);
    }
    Ok(database)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gnu() -> Vec<Compiler> {
        vec![Compiler {
            language: "CXX".into(),
            path: "/usr/bin/c++".into(),
            dialect: Some(Dialect::Gnu),
        }]
    }

    #[test]
    fn infers_targets_and_multi_config_names() {
        let configs = ["Debug".into(), "Release".into()];
        assert_eq!(
            infer_target(b"/b/CMakeFiles/app.dir/Debug/main.cpp.o", &configs),
            Some((Some("Debug".into()), "app".into()))
        );
        assert_eq!(
            infer_target(b"/b/sub/CMakeFiles/lib.dir/a.c.o", &[BString::from("")]),
            Some((None, "lib".into()))
        );
        assert_eq!(infer_target(b"/b/main.o", &configs), None);
    }

    #[test]
    fn launcher_is_skipped_and_flags_are_classified() {
        let tokens: Vec<BString> = [
            "/usr/bin/ccache",
            "/usr/bin/c++",
            "-DFOO=1",
            "-I/src/inc",
            "-isystem",
            "/sys",
            "-std=c++20",
            "-O2",
            "-include",
            "pch.h",
            "-o",
            "CMakeFiles/app.dir/main.cpp.o",
            "-c",
            "/src/main.cpp",
            "-MD",
            "-MF",
            "dep.d",
        ]
        .map(BString::from)
        .to_vec();
        let compilers = gnu();
        let (index, compiler) = locate(&tokens, &compilers).expect("compiler");
        assert_eq!(index, 1);
        let rest = tokens.get(index + 1..).unwrap_or_default();
        let (settings, output) = classify(
            rest,
            b"/src/main.cpp",
            b"/b",
            Dialect::Gnu,
            Some(compiler.language.clone()),
        );
        assert_eq!(settings.defines, [BString::from("FOO=1")]);
        assert_eq!(settings.includes.len(), 2);
        assert!(settings.includes[1].system);
        assert_eq!(settings.standard, Some("c++20".into()));
        assert_eq!(
            settings.fragments,
            [BString::from("-O2"), BString::from("-include pch.h")]
        );
        assert_eq!(output, Some("CMakeFiles/app.dir/main.cpp.o".into()));
    }

    #[test]
    fn msvc_absolute_posix_like_argument_keeps_its_meaning() {
        let tokens: Vec<BString> = ["/DWIN=1", "/Iinc", "/usr/share/x", "/Fooutput.obj"]
            .map(BString::from)
            .to_vec();
        let (settings, output) = classify(&tokens, b"main.c", b"C:/b", Dialect::Msvc, None);
        assert_eq!(settings.defines, [BString::from("WIN=1")]);
        assert_eq!(settings.fragments, [BString::from("/usr/share/x")]);
        assert_eq!(output, Some("output.obj".into()));
    }
}
