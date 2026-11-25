use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::{BString, list};
use rcmake_eval::CacheDefine;
use rcmake_host::{FileKind, Follow, Host, HostResult, ReadRange, Stream, WriteMode};
use rcmake_model::{Cache, CacheEntry, CacheType};

const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) fn load(host: &dyn Host, dir: &Path) -> HostResult<Cache> {
    let path = dir.join("CMakeCache.txt");
    match host.metadata(&path, Follow::Yes) {
        Ok(meta) if meta.kind == FileKind::File => {}
        Ok(_) => {
            return Err(rcmake_host::HostError::unsupported(
                "CMakeCache.txt is not a regular file",
            ));
        }
        Err(err) if err.kind == ErrorKind::NotFound => return Ok(Cache::default()),
        Err(err) => return Err(err),
    }
    let data = host.read(
        &path,
        ReadRange::All {
            limit: MAX_CACHE_BYTES,
        },
    )?;
    if data.len() as u64 > MAX_CACHE_BYTES {
        return Err(rcmake_host::HostError::unsupported(
            "CMakeCache.txt is too large",
        ));
    }
    let mut entries = Cache::default();
    let mut advanced = Vec::new();
    let mut strings = BTreeMap::new();
    let mut help = BString::default();
    for (index, line) in data.split(|b| *b == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(comment) = line.strip_prefix(b"//") {
            if !help.is_empty() {
                help.push(b' ');
            }
            help.extend_from_slice(comment);
            continue;
        }
        let description = std::mem::take(&mut help);
        let Some((name, ty, value)) = parse_entry(line) else {
            if !line.starts_with(b"#") && (!line.trim_ascii().is_empty() || !description.is_empty())
            {
                host.write_out(
                    Stream::Stderr,
                    format!(
                        "CMake Error: Parse error in cache file {} on line {}. Offending entry: {}\n",
                        path.display(), index + 1, String::from_utf8_lossy(line)
                    ).as_bytes(),
                );
            }
            continue;
        };
        if ty == CacheType::Internal {
            if let Some(key) = name.strip_suffix(b"-ADVANCED") {
                if rcmake_base::truth::is_on(value) {
                    advanced.push(BString::from(key));
                }
                continue;
            }
            if let Some(key) = name.strip_suffix(b"-STRINGS") {
                strings.insert(BString::from(key), BString::from(value));
                continue;
            }
        }
        insert(&mut entries, name, value.into(), Some(ty));
        if let Some(entry) = entries.get_mut(name) {
            entry.help = description;
        }
    }
    for name in advanced {
        if let Some(entry) = entries.get_mut(&name) {
            entry.advanced = true;
        }
    }
    for (name, value) in strings {
        if let Some(entry) = entries.get_mut(&name) {
            entry.props.set("STRINGS".into(), value);
        }
    }
    Ok(entries)
}

/// Parse one `CMakeCache.txt` record as `cmCacheManager::LoadCache` does.
///
/// An untyped record gets `UNINITIALIZED`. An unknown type text gets
/// `STRING`, as in `cmState::StringToCacheEntryType`.
fn parse_entry(line: &[u8]) -> Option<(&[u8], CacheType, &[u8])> {
    if line.starts_with(b"#") || line.starts_with(b"//") {
        return None;
    }
    let entry = parse_definition(line)?;
    let ty = entry.ty.map_or(CacheType::Uninitialized, |text| {
        CacheType::parse(text).unwrap_or(CacheType::String)
    });
    Some((entry.name, ty, entry.value))
}

/// A cache definition split as `cmState::ParseCacheEntry` splits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Definition<'a> {
    /// Entry name, without the enclosing double quotes of a quoted name.
    pub(crate) name: &'a [u8],
    /// Type text between `:` and `=`, or `None` for `NAME=VALUE`.
    pub(crate) ty: Option<&'a [u8]>,
    /// Value after the trim and quote rules.
    pub(crate) value: &'a [u8],
}

/// Split `NAME:TYPE=VALUE`, `"NAME":TYPE=VALUE`, `NAME=VALUE`, or
/// `"NAME"=VALUE`.
///
/// A quoted name can contain `:` and `=`. An unquoted typed name ends at
/// its first `:` and cannot contain `=`. An unquoted untyped name ends at the
/// first `=`. The value loses trailing spaces, tabs, and carriage returns,
/// unless it contains only those bytes. Then one enclosing pair of single
/// quotes is removed.
pub(crate) fn parse_definition(entry: &[u8]) -> Option<Definition<'_>> {
    parse_typed(entry).or_else(|| parse_untyped(entry))
}

fn parse_typed(entry: &[u8]) -> Option<Definition<'_>> {
    let quoted = quoted_name(entry).and_then(|(name, rest)| {
        let typed = rest.strip_prefix(b":")?;
        typed.contains(&b'=').then_some((name, typed))
    });
    let (name, typed) = match quoted {
        Some(found) => found,
        None => {
            let end = entry.iter().position(|b| matches!(b, b'=' | b':'))?;
            if entry.get(end) != Some(&b':') {
                return None;
            }
            (entry.get(..end)?, entry.get(end + 1..)?)
        }
    };
    let eq = typed.iter().position(|b| *b == b'=')?;
    Some(Definition {
        name,
        ty: Some(typed.get(..eq)?),
        value: clean_value(typed.get(eq + 1..)?),
    })
}

fn parse_untyped(entry: &[u8]) -> Option<Definition<'_>> {
    let (name, value) =
        match quoted_name(entry).and_then(|(name, rest)| Some((name, rest.strip_prefix(b"=")?))) {
            Some(found) => found,
            None => {
                let eq = entry.iter().position(|b| *b == b'=')?;
                (entry.get(..eq)?, entry.get(eq + 1..)?)
            }
        };
    Some(Definition {
        name,
        ty: None,
        value: clean_value(value),
    })
}

/// Split `"NAME"REST` into `NAME` and `REST`.
fn quoted_name(entry: &[u8]) -> Option<(&[u8], &[u8])> {
    let inner = entry.strip_prefix(b"\"")?;
    let end = inner.iter().position(|b| *b == b'"')?;
    Some((inner.get(..end)?, inner.get(end + 1..)?))
}

fn clean_value(value: &[u8]) -> &[u8] {
    let trimmed = value
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r'))
        .and_then(|last| value.get(..=last))
        .unwrap_or(value);
    match trimmed {
        [b'\'', inner @ .., b'\''] => inner,
        _ => trimmed,
    }
}

/// Normalize a command-line or preset definition before it is stored.
///
/// `cmCacheManager::AddCacheEntry` converts a `PATH` or `FILEPATH` value
/// with `cmSystemTools::ConvertToUnixSlashes`. Call this when the edit
/// runs, so that `~` uses the `HOME` of the preset environment.
pub(crate) fn normalize_definition(host: &dyn Host, mut def: CacheDefine) -> CacheDefine {
    def.value = typed_value(host, def.ty, &def.value);
    def
}

/// Return `value` as CMake stores it for an entry of type `ty`.
pub(crate) fn typed_value(host: &dyn Host, ty: Option<CacheType>, value: &[u8]) -> BString {
    if !matches!(ty, Some(CacheType::Path | CacheType::Filepath)) {
        return value.into();
    }
    if !value.contains(&b';') {
        return unix_path(host, value);
    }
    // A list value loses its empty elements, as `cmList` does.
    list::join(
        list::split(value, list::Empty::Drop)
            .iter()
            .map(|element| unix_path(host, element)),
    )
}

/// `cmSystemTools::ConvertToUnixSlashes` for one path.
///
/// Backslashes become slashes. Repeated slashes collapse, except exactly two
/// leading slashes. A leading `~` alone or before `/` becomes `HOME`. A
/// trailing slash goes, except for `/`, `c:/`, and `//`. The `~user` form
/// needs an account lookup that `Host` does not offer, so it stays as is.
fn unix_path(host: &dyn Host, path: &[u8]) -> BString {
    let converted: Vec<u8> = path
        .iter()
        .map(|&byte| if byte == b'\\' { b'/' } else { byte })
        .collect();
    let keep_double = converted.starts_with(b"//") && converted.get(2) != Some(&b'/');
    let mut out = Vec::with_capacity(converted.len());
    for (index, &byte) in converted.iter().enumerate() {
        if byte == b'/' && out.last() == Some(&b'/') && !(keep_double && index == 1) {
            continue;
        }
        out.push(byte);
    }
    if out.first() == Some(&b'~')
        && matches!(out.get(1), None | Some(b'/'))
        && let Some(home) = host.env_get(b"HOME")
    {
        out.splice(0..1, home.iter().copied());
    }
    let size = out.len();
    if size > 1
        && out.last() == Some(&b'/')
        && !(size == 3 && out.get(1) == Some(&b':'))
        && out.get(size - 2) != Some(&b'/')
    {
        out.pop();
    }
    out.into()
}

pub(crate) fn apply(cache: &mut Cache, def: CacheDefine) {
    insert(cache, &def.name, def.value, def.ty);
    if let Some(entry) = cache.get_mut(&def.name)
        && entry.help.is_empty()
    {
        entry.help = command_line_help(&def.name).into();
    }
}

fn command_line_help(name: &[u8]) -> &'static str {
    if name == b"CMAKE_COMPILE_WARNING_AS_ERROR" {
        return "Specify whether to treat warnings on compile as errors. This variable is used to initialize the COMPILE_WARNING_AS_ERROR property on all the targets.";
    }
    if let Some(name) = name.strip_prefix(b"CMAKE_") {
        let has_language = |suffix: &[u8]| {
            name.strip_suffix(suffix).is_some_and(|language| {
                !language.is_empty()
                    && language
                        .iter()
                        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            })
        };
        if has_language(b"_CLANG_TIDY") {
            return "Default value for the <LANG>_CLANG_TIDY target property; names a clang-tidy command (with arguments) run alongside compilation. Applies only when <LANG> is C, CXX, OBJC, or OBJCXX.";
        }
        if has_language(b"_CPPCHECK") {
            return "Default value for the <LANG>_CPPCHECK target property; names a cppcheck command (with arguments) run alongside compilation. Applies only when <LANG> is C or CXX.";
        }
    }
    "No help, variable specified on the command line."
}

pub(crate) fn insert(cache: &mut Cache, name: &[u8], value: BString, ty: Option<CacheType>) {
    if let Some(entry) = cache.get_mut(name) {
        entry.value = value;
        if let Some(ty) = ty {
            entry.ty = ty;
        }
    } else {
        cache.insert(
            name.into(),
            CacheEntry {
                value,
                ty: ty.unwrap_or(CacheType::Uninitialized),
                help: BString::default(),
                advanced: false,
                props: Default::default(),
            },
        );
    }
}

pub(crate) fn save(host: &dyn Host, dir: &Path, cache: &Cache) -> HostResult<()> {
    let mut data = BString::from("# This is the CMakeCache file.\n\n");
    for (name, entry) in sorted(cache) {
        if name.iter().any(|byte| matches!(byte, b'\n' | b'\r' | b'"')) {
            continue;
        }
        for line in entry
            .help
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
        {
            data.extend_from_slice(b"//");
            data.extend_from_slice(line);
            data.push(b'\n');
        }
        write_entry(&mut data, name, entry.ty, &entry.value);
        if entry.value.contains(&b'\n') {
            let warning = format!("Value of {name} contained a newline");
            host.write_out(
                Stream::Stderr,
                format!("CMake Warning:\n  {warning}; truncating\n\n\n").as_bytes(),
            );
            data.extend_from_slice(
                format!("# WARNING: {warning} and was\n#  truncated. Original value:\n").as_bytes(),
            );
            let mut lines = entry.value.split(|byte| *byte == b'\n');
            if let Some(first) = lines.next().filter(|line| !line.is_empty()) {
                data.extend_from_slice(b"# ");
                data.extend_from_slice(first);
                data.push(b'\n');
            }
            for line in lines {
                data.extend_from_slice(b"# \\n");
                data.extend_from_slice(line);
                data.push(b'\n');
            }
        }
        if entry.advanced {
            write_entry(
                &mut data,
                &[name.as_slice(), b"-ADVANCED"].concat(),
                CacheType::Internal,
                b"1",
            );
        }
        if let Some(strings) = entry.props.get(b"STRINGS") {
            write_entry(
                &mut data,
                &[name.as_slice(), b"-STRINGS"].concat(),
                CacheType::Internal,
                strings,
            );
        }
        data.push(b'\n');
    }
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let time = host.now();
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".CMakeCache.txt.{}-{}-{}-{sequence}",
        std::process::id(),
        time.secs,
        time.nanos
    ));
    host.write(&tmp, &data, WriteMode::CreateNew)?;
    let result = host.rename(&tmp, &dir.join("CMakeCache.txt"));
    if result.is_err() {
        let _ = host.remove_file(&tmp);
    }
    result
}

fn write_entry(data: &mut BString, name: &[u8], ty: CacheType, value: &[u8]) {
    let quote_name = name.contains(&b':') || name.starts_with(b"#") || name.starts_with(b"//");
    if quote_name {
        data.push(b'"');
    }
    data.extend_from_slice(name);
    if quote_name {
        data.push(b'"');
    }
    data.push(b':');
    data.extend_from_slice(ty.as_str().as_bytes());
    data.push(b'=');
    let value = value.split(|b| *b == b'\n').next().unwrap_or_default();
    let quote_value = value != value.trim_ascii_end() || value.starts_with(b"'");
    if quote_value {
        data.push(b'\'');
    }
    data.extend_from_slice(value);
    if quote_value {
        data.push(b'\'');
    }
    data.push(b'\n');
}

pub(crate) fn sorted(cache: &Cache) -> BTreeMap<&BString, &CacheEntry> {
    cache.iter().collect()
}

#[cfg(test)]
mod tests {
    use rcmake_model::CacheType;

    use rcmake_base::BString;
    use rcmake_host::{Host, PureHost};

    use super::{parse_entry, typed_value};

    #[test]
    fn cache_records_use_the_command_line_entry_rules() {
        let cases: [(&[u8], &[u8], CacheType, &[u8]); 7] = [
            (
                b"\"K:EY=1\":STRING='  sp  '",
                b"K:EY=1",
                CacheType::String,
                b"  sp  ",
            ),
            (
                b"A=B:STRING=v",
                b"A",
                CacheType::Uninitialized,
                b"B:STRING=v",
            ),
            (b"FOO=bar", b"FOO", CacheType::Uninitialized, b"bar"),
            (b"X:BOGUS=1", b"X", CacheType::String, b"1"),
            (b"SP:STRING=  x  \t", b"SP", CacheType::String, b"  x"),
            (b"Q:STRING=' q '", b"Q", CacheType::String, b" q "),
            (b"W:STRING=   ", b"W", CacheType::String, b"   "),
        ];
        for (line, name, ty, value) in cases {
            assert_eq!(parse_entry(line), Some((name, ty, value)));
        }
        assert_eq!(parse_entry(b"# comment=1"), None);
        assert_eq!(parse_entry(b"NOVALUE"), None);
    }

    #[test]
    fn path_values_convert_like_cmake() {
        let host = PureHost::default();
        host.env_set(b"HOME", Some(b"/hh"));
        let path = |ty, value: &str| typed_value(&host, ty, value.as_bytes());
        let typed = Some(CacheType::Path);
        for (value, expected) in [
            ("~/q", "/hh/q"),
            ("~", "/hh"),
            ("~\\x", "/hh/x"),
            ("~x", "~x"),
            ("a\\\\b//c/", "a/b/c"),
            ("//server//share/", "//server/share"),
            ("///x", "/x"),
            ("/", "/"),
            ("c:/", "c:/"),
            ("~/a;;~/b", "/hh/a;/hh/b"),
            ("[x;~/b];~/c", "[x;~/b];/hh/c"),
            ("", ""),
        ] {
            assert_eq!(path(typed, value), BString::from(expected), "{value}");
        }
        assert_eq!(
            path(Some(CacheType::Filepath), "~/f"),
            BString::from("/hh/f")
        );
        for ty in [
            None,
            Some(CacheType::String),
            Some(CacheType::Uninitialized),
        ] {
            assert_eq!(path(ty, "~/a;;b\\"), BString::from("~/a;;b\\"));
        }
    }
}
