//! `compile_commands.json` reader with target inference from object paths.
//!
//! A `command` string is split with the quoting rules of the platform that wrote
//! the database. Flags are classified only with a compiler frontend that toolchain
//! evidence supports. Without that evidence, past a limit, with a response file that
//! cannot be read, or with a malformed row, the command stays opaque and only its raw
//! text or its ordered tokens are compared.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use rcmake_base::{BString, cmdline::Arguments, path_lex::Lexical};
use rcmake_json::Value;

use crate::reader::{ArtifactReader, ReadError};
use crate::snapshot::{Attribution, CompileSettings, IncludeDir, TargetSnapshot};

const MAX_ENTRIES: usize = 200_000;
const MAX_TOKENS: usize = 65_536;
const MAX_RESPONSE_FILE: u64 = 1 << 20;
const MAX_RESPONSE_TOTAL: u64 = 16 << 20;
/// Deepest chain of response files inside response files that the reader expands.
const MAX_RESPONSE_DEPTH: usize = 4;

/// Flag style of a compiler frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Frontend {
    /// GNU-style flags such as `-I` and `-o`.
    Gnu,
    /// MSVC-style flags such as `/I` and `/Fo`.
    Msvc,
}

/// Quoting rules of a command line. They depend on the platform, not on the compiler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quoting {
    Posix,
    Windows,
}

/// A compiler that toolchain evidence names.
#[derive(Clone, Debug)]
pub(crate) struct Compiler {
    pub(crate) language: BString,
    pub(crate) path: BString,
    pub(crate) frontend: Option<Frontend>,
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

/// Arguments of one row: the ordered tokens, or why the row is uncertain.
enum RowArguments {
    Tokens(Vec<BString>),
    /// The array has more tokens than the limit.
    TooMany,
    /// An array item is not a string.
    Malformed,
}

struct Entry<'a> {
    directory: BString,
    file: BString,
    output: Option<BString>,
    arguments: Option<RowArguments>,
    command: Option<&'a [u8]>,
}

fn parse_entry(value: &Value) -> Option<Entry<'_>> {
    let field = |name| value.get(name).and_then(Value::as_bytes);
    let arguments = match value.get("arguments") {
        None => None,
        Some(Value::Array(items)) if items.len() > MAX_TOKENS => Some(RowArguments::TooMany),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|item| item.as_bytes().map(BString::from))
                .collect::<Option<Vec<_>>>()
                .map_or(RowArguments::Malformed, RowArguments::Tokens),
        ),
        Some(_) => Some(RowArguments::Malformed),
    };
    Some(Entry {
        directory: field("directory").map(BString::from).unwrap_or_default(),
        file: BString::from(field("file")?),
        output: field("output").map(BString::from),
        arguments,
        command: field("command"),
    })
}

/// Return whether `path` uses a Windows drive or UNC root, with either separator.
fn windows_path(path: &[u8]) -> bool {
    matches!(path, [letter, b':', ..] if letter.is_ascii_alphabetic())
        || path.starts_with(b"\\\\")
        || path.starts_with(b"//")
}

fn forward(path: &[u8]) -> BString {
    path.iter()
        .map(|&byte| if byte == b'\\' { b'/' } else { byte })
        .collect::<Vec<u8>>()
        .into()
}

/// Make `path` absolute against `directory` with the path rules of the platform
/// that wrote the row. A Windows path keeps its drive or UNC root.
fn absolute(directory: &[u8], path: &[u8]) -> BString {
    if windows_path(directory) || windows_path(path) {
        let (directory, path) = (forward(directory), forward(path));
        if Lexical::windows(&path).absolute() || directory.is_empty() {
            return Lexical::windows(&path).normalize();
        }
        let joined = Lexical::windows(&directory).append(&path);
        return Lexical::windows(&joined).normalize();
    }
    if path.starts_with(b"/") || directory.is_empty() {
        return Lexical::new(path).normalize();
    }
    let joined = [directory, b"/", path].concat();
    Lexical::new(&joined).normalize()
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
    /// Expand `@file` tokens. A response file inside a response file is expanded
    /// up to [`MAX_RESPONSE_DEPTH`] levels. Return `None` when a response file is
    /// unreadable, a link, too deep, or when the result has more than [`MAX_TOKENS`]
    /// tokens.
    fn expand(
        &mut self,
        tokens: Vec<BString>,
        directory: &[u8],
        frontend: Frontend,
        depth: usize,
    ) -> Option<Vec<BString>> {
        let mut out = Vec::with_capacity(tokens.len().min(MAX_TOKENS));
        for token in tokens {
            let Some(file) = token.strip_prefix(b"@") else {
                out.push(token);
                if out.len() > MAX_TOKENS {
                    return None;
                }
                continue;
            };
            if depth >= MAX_RESPONSE_DEPTH {
                return None;
            }
            let path = rcmake_host::path::to_native(&absolute(directory, file));
            let relative = self.reader.artifact_relative(&path)?;
            if self.used >= MAX_RESPONSE_TOTAL {
                return None;
            }
            let limit = MAX_RESPONSE_FILE.min(MAX_RESPONSE_TOTAL - self.used);
            let bytes = self.reader.read_limited(&relative, limit).ok()?;
            self.used += bytes.len() as u64;
            // Read one token past the room that is left, so a cut expansion is visible.
            let room = MAX_TOKENS.saturating_sub(out.len()).saturating_add(1);
            let inner: Vec<BString> = Arguments::new(&bytes, frontend == Frontend::Msvc)
                .take(room)
                .collect();
            if inner.len() >= room {
                return None;
            }
            let inner = self.expand(inner, directory, frontend, depth + 1)?;
            out.extend(inner);
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
    frontend: Frontend,
    language: Option<BString>,
) -> (CompileSettings, Option<BString>) {
    let mut settings = CompileSettings {
        language,
        ..CompileSettings::default()
    };
    let mut output = None;
    let mut iter = tokens.iter().peekable();
    let msvc = frontend == Frontend::Msvc;
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

/// Counts of rows that stay opaque or incomplete, for the partial note.
#[derive(Default)]
struct RowNotes {
    too_many: usize,
    malformed: usize,
    skipped: usize,
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
    let mut notes = Vec::new();
    if entries.len() > MAX_ENTRIES {
        notes.push(format!(
            "compile_commands.json has more than {MAX_ENTRIES} entries."
        ));
    }
    let default_config = configs.first().cloned().unwrap_or_default();
    let mut expander = Expander { reader, used: 0 };
    let mut shared: HashSet<Arc<CompileSettings>> = HashSet::new();
    let mut rows = RowNotes::default();
    for value in entries.iter().take(MAX_ENTRIES) {
        let Some(entry) = parse_entry(value) else {
            rows.skipped += 1;
            continue;
        };
        let quoting = if windows_path(&entry.directory) {
            Quoting::Windows
        } else {
            Quoting::Posix
        };
        let (tokens, from_command) = match (&entry.arguments, entry.command) {
            (Some(RowArguments::Tokens(arguments)), _) => (Some(arguments.clone()), false),
            (Some(RowArguments::TooMany), _) => {
                rows.too_many += 1;
                (None, false)
            }
            (Some(RowArguments::Malformed), _) => {
                rows.malformed += 1;
                (None, false)
            }
            (None, Some(command)) => {
                let split: Vec<BString> = Arguments::new(command, quoting == Quoting::Windows)
                    .take(MAX_TOKENS + 1)
                    .collect();
                if split.len() > MAX_TOKENS {
                    rows.too_many += 1;
                    (None, true)
                } else {
                    (Some(split), true)
                }
            }
            (None, None) => {
                rows.skipped += 1;
                continue;
            }
        };
        let mut settings = None;
        let mut output = entry.output.clone();
        if let Some(tokens) = &tokens {
            let located = locate(tokens, compilers);
            // A command string needs frontend evidence. An argument array names its
            // tokens, so GNU flags are the documented default.
            let frontend = located
                .and_then(|(_, compiler)| compiler.frontend)
                .or((!from_command).then_some(Frontend::Gnu));
            let compiler_at = located.map_or(0, |(index, _)| index);
            let language = located.map(|(_, compiler)| compiler.language.clone());
            if let Some(frontend) = frontend {
                let rest = tokens.get(compiler_at + 1..).unwrap_or_default().to_vec();
                if let Some(rest) = expander.expand(rest, &entry.directory, frontend, 0) {
                    let (mut classified, from_flags) =
                        classify(&rest, &entry.file, &entry.directory, frontend, language);
                    classified.compiler = tokens.get(compiler_at).cloned();
                    classified.launcher = tokens.get(..compiler_at).unwrap_or_default().to_vec();
                    output = output.or(from_flags);
                    settings = Some(classified);
                }
            }
        }
        let settings = settings.unwrap_or_else(|| {
            database.opaque += 1;
            let fragments = match (&entry.arguments, entry.command) {
                (Some(RowArguments::Tokens(arguments)), _) => arguments.clone(),
                (_, Some(command)) => vec![BString::from(command)],
                _ => Vec::new(),
            };
            CompileSettings {
                fragments,
                opaque: true,
                ..CompileSettings::default()
            }
        });
        let settings = match shared.get(&settings) {
            Some(existing) => Arc::clone(existing),
            None => {
                let settings = Arc::new(settings);
                shared.insert(Arc::clone(&settings));
                settings
            }
        };
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
            .entry(absolute(&entry.directory, &entry.file))
            .or_default()
            .push(settings);
    }
    if rows.too_many > 0 {
        notes.push(format!(
            "{} rows had more than {MAX_TOKENS} arguments and are compared as text.",
            rows.too_many
        ));
    }
    if rows.malformed > 0 {
        notes.push(format!(
            "{} rows had malformed arguments and are compared as text.",
            rows.malformed
        ));
    }
    if rows.skipped > 0 {
        notes.push(format!(
            "{} rows had no file or no command and were skipped.",
            rows.skipped
        ));
    }
    if !notes.is_empty() {
        database.partial = Some(notes.join(" ").into());
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
            frontend: Some(Frontend::Gnu),
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
            Frontend::Gnu,
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
        let (settings, output) = classify(&tokens, b"main.c", b"C:/b", Frontend::Msvc, None);
        assert_eq!(settings.defines, [BString::from("WIN=1")]);
        assert_eq!(settings.fragments, [BString::from("/usr/share/x")]);
        assert_eq!(output, Some("output.obj".into()));
    }

    fn database(rows: &str, files: &[(&str, &[u8])]) -> CompileDatabase {
        let host = rcmake_host::PureHost::default();
        host.seed_file(Path::new("/b/compile_commands.json"), rows.as_bytes())
            .expect("seed");
        for (path, bytes) in files {
            host.seed_file(Path::new(path), bytes).expect("seed");
        }
        let mut reader = ArtifactReader::new(&host, Path::new("/b"), crate::ReadLimits::default())
            .expect("root");
        load(&mut reader, &gnu(), &[BString::default()]).expect("database")
    }

    fn sources(database: &CompileDatabase) -> Vec<Arc<CompileSettings>> {
        database
            .targets
            .values()
            .flat_map(|target| target.sources.values().flatten().cloned())
            .collect()
    }

    #[test]
    fn rows_for_one_source_keep_each_invocation_and_the_compiler_identity() {
        let rows = r#"[
          {"directory":"/b","file":"/src/a.c","arguments":["ccache","/usr/bin/c++","-arch","x86_64","-o","CMakeFiles/app.dir/a.c.o","-c","/src/a.c"]},
          {"directory":"/b","file":"/src/a.c","arguments":["ccache","/usr/bin/c++","-arch","arm64","-o","CMakeFiles/app.dir/a.c.o.arm64","-c","/src/a.c"]}
        ]"#;
        let database = database(rows, &[]);
        let target = &database.targets[&(BString::default(), BString::from("app"))];
        let invocations = &target.sources[b"/src/a.c".as_slice()];
        assert_eq!(invocations.len(), 2);
        assert_eq!(invocations[0].fragments, [BString::from("-arch x86_64")]);
        assert_eq!(invocations[1].fragments, [BString::from("-arch arm64")]);
        assert_eq!(invocations[0].compiler, Some("/usr/bin/c++".into()));
        assert_eq!(invocations[0].launcher, [BString::from("ccache")]);
    }

    #[test]
    fn oversized_and_malformed_rows_stay_opaque_and_partial() {
        let many: Vec<String> = (0..=MAX_TOKENS)
            .map(|index| format!("\"-D{index}\""))
            .collect();
        let rows = format!(
            r#"[{{"directory":"/b","file":"/src/a.c","arguments":["/usr/bin/c++",{}]}},
               {{"directory":"/b","file":"/src/b.c","arguments":["/usr/bin/c++",1,"-c","/src/b.c"]}},
               {{"directory":"/b"}}]"#,
            many.join(",")
        );
        let database = database(&rows, &[]);
        assert_eq!(database.opaque, 2);
        let partial = database.partial.expect("partial").to_string();
        assert!(partial.contains("more than 65536 arguments"), "{partial}");
        assert!(partial.contains("malformed"), "{partial}");
        assert!(partial.contains("skipped"), "{partial}");
    }

    #[test]
    fn response_files_detect_cut_nested_and_linked_expansions() {
        let full: Vec<String> = (0..=MAX_TOKENS).map(|index| format!("-D{index}")).collect();
        let full = full.join(" ");
        let row = |file: &str, response: &str| {
            format!(
                r#"{{"directory":"/b","file":"/src/{file}","arguments":["/usr/bin/c++","@{response}","-o","CMakeFiles/app.dir/{file}.o","-c","/src/{file}"]}}"#
            )
        };
        let rows = format!(
            "[{},{},{}]",
            row("big.c", "big.rsp"),
            row("nested.c", "one.rsp"),
            row("deep.c", "d1.rsp")
        );
        let database = database(
            &rows,
            &[
                ("/b/big.rsp", full.as_bytes()),
                ("/b/one.rsp", b"-DONE @two.rsp"),
                ("/b/two.rsp", b"-DTWO"),
                ("/b/d1.rsp", b"@d2.rsp"),
                ("/b/d2.rsp", b"@d3.rsp"),
                ("/b/d3.rsp", b"@d4.rsp"),
                ("/b/d4.rsp", b"@d5.rsp"),
                ("/b/d5.rsp", b"-DDEEP"),
            ],
        );
        let by_file = |name: &str| {
            let key = BString::from(format!("/src/{name}"));
            database
                .targets
                .values()
                .find_map(|target| target.sources.get(&key))
                .and_then(|invocations| invocations.first().cloned())
                .expect("source")
        };
        assert!(
            by_file("big.c").opaque,
            "one token past the limit is visible"
        );
        assert_eq!(
            by_file("nested.c").defines,
            [BString::from("ONE"), BString::from("TWO")]
        );
        assert!(by_file("deep.c").opaque, "too deep");
    }

    #[test]
    fn linked_response_file_is_rejected() {
        let host = rcmake_host::PureHost::default();
        host.seed_file(Path::new("/b/real.rsp"), b"-DLINK")
            .expect("seed");
        rcmake_host::Host::symlink(&host, Path::new("/b/real.rsp"), Path::new("/b/alias.rsp"))
            .expect("link");
        host.seed_file(
            Path::new("/b/compile_commands.json"),
            br#"[{"directory":"/b","file":"/src/a.c","arguments":["/usr/bin/c++","@alias.rsp","-o","CMakeFiles/app.dir/a.c.o","-c","/src/a.c"]}]"#,
        )
        .expect("seed");
        let mut reader = ArtifactReader::new(&host, Path::new("/b"), crate::ReadLimits::default())
            .expect("root");
        let database = load(&mut reader, &gnu(), &[BString::default()]).expect("database");
        let settings = sources(&database);
        assert!(settings[0].opaque);
    }

    #[test]
    fn windows_paths_keep_drive_and_unc_roots() {
        assert_eq!(
            absolute(b"C:/b", br"\\server\share\x.rsp"),
            BString::from("//server/share/x.rsp")
        );
        assert_eq!(
            absolute(br"C:\b", b"sub/x.rsp"),
            BString::from("C:/b/sub/x.rsp")
        );
        assert_eq!(absolute(b"/b", b"x.rsp"), BString::from("/b/x.rsp"));
    }
}
