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
    let mut properties = Vec::new();
    let mut help = BString::default();
    for (index, line) in data.split(|b| *b == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // `cmCacheManager::LoadCache` joins the help lines without a separator,
        // and `//\n` starts a new line of the help text.
        if let Some(comment) = line.strip_prefix(b"//") {
            if let Some(rest) = comment.strip_prefix(b"\\n") {
                help.push(b'\n');
                help.extend_from_slice(rest);
            } else {
                help.extend_from_slice(comment);
            }
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
        if ty == CacheType::Internal
            && let Some((key, property)) = [b"ADVANCED".as_slice(), b"MODIFIED", b"STRINGS"]
                .into_iter()
                .find_map(|property| {
                    let key = name.strip_suffix(property)?.strip_suffix(b"-")?;
                    Some((key, property))
                })
        {
            properties.push((BString::from(key), property, BString::from(value)));
            continue;
        }
        insert(&mut entries, name, value.into(), Some(ty));
        if let Some(entry) = entries.get_mut(name) {
            entry.help = description;
        }
    }
    for (name, property, value) in properties {
        if let Some(entry) = entries.get_mut(&name) {
            // The stored value keeps an explicit `ADVANCED=0` from `mark_as_advanced(CLEAR)`.
            if property == b"ADVANCED" {
                entry.advanced = rcmake_base::truth::is_on(&value);
            }
            entry.props.set(property.into(), value);
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
        && (entry.help.is_empty() || def.name == b"CMAKE_INSTALL_PREFIX")
    {
        entry.help = command_line_help(&def.name).into();
    }
}

fn command_line_help(name: &[u8]) -> &'static str {
    if name == b"CMAKE_INSTALL_PREFIX" {
        return "Install directory prepended to all install() destinations. Defaults to c:/Program Files/${PROJECT_NAME} on Windows and /usr/local on UNIX; may be overridden at install time via cmake --install --prefix.";
    }
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

pub(crate) fn insert_internal(cache: &mut Cache, name: &[u8], value: BString, help: &str) {
    insert(cache, name, value, Some(CacheType::Internal));
    if let Some(entry) = cache.get_mut(name) {
        entry.help = help.into();
    }
}

/// The fixed text at the top of `CMakeCache.txt` after the two location lines.
const HEADER_TAIL: &str = "# You can edit this file to change values found and used by cmake.
# If you do not want to change any of the values, simply exit the editor.
# If you do want to change a value, simply edit, save, and exit the editor.
# The syntax for the file is as follows:
# KEY:TYPE=VALUE
# KEY is the name of a variable in the cache.
# TYPE is a hint to GUIs for the type of VALUE, DO NOT EDIT TYPE!.
# VALUE is the current value for the KEY.

########################
# EXTERNAL cache entries
########################

";

const INTERNAL_BANNER: &str = "
########################
# INTERNAL cache entries
########################

";

/// Write `CMakeCache.txt` in `dir` with the `cmCacheManager::SaveCache` layout.
///
/// The file has a header, the sorted non-`INTERNAL` entries (each with its help
/// text and a blank line), and then the internal section. The internal section
/// has, for each sorted entry, its `ADVANCED`, `MODIFIED`, and `STRINGS`
/// property entries, and then the entry itself if it is `INTERNAL`.
pub(crate) fn save(host: &dyn Host, dir: &Path, cache: &Cache) -> HostResult<()> {
    let entries: Vec<_> = sorted(cache)
        .into_iter()
        .filter(|(name, entry)| {
            entry
                .props
                .get(rcmake_eval::Cx::CACHE_PLACEHOLDER)
                .is_none()
                && !name.iter().any(|byte| matches!(byte, b'\n' | b'\r' | b'"'))
        })
        .collect();
    let mut data = BString::from("# This is the CMakeCache file.\n# For build in directory: ");
    data.extend_from_slice(&cache_file_dir(dir));
    data.extend_from_slice(b"\n# It was generated by CMake: ");
    if let Some(command) = cache.get(b"CMAKE_COMMAND") {
        data.extend_from_slice(&command.value);
    }
    data.push(b'\n');
    data.extend_from_slice(HEADER_TAIL.as_bytes());
    for (name, entry) in &entries {
        if entry.ty == CacheType::Internal {
            continue;
        }
        write_help(&mut data, &entry.help);
        write_value_entry(host, &mut data, name, entry.ty, &entry.value);
        data.push(b'\n');
    }
    data.extend_from_slice(INTERNAL_BANNER.as_bytes());
    for (name, entry) in &entries {
        let advanced = advanced_value(entry);
        for (property, value) in [
            (b"ADVANCED".as_slice(), advanced),
            (
                b"MODIFIED",
                entry.props.get(b"MODIFIED").map(|v| v.as_slice()),
            ),
            (
                b"STRINGS",
                entry.props.get(b"STRINGS").map(|v| v.as_slice()),
            ),
        ] {
            let Some(value) = value else {
                continue;
            };
            let property = String::from_utf8_lossy(property);
            write_help(
                &mut data,
                format!("{property} property for variable: {name}").as_bytes(),
            );
            let key = [name.as_slice(), b"-", property.as_bytes()].concat();
            write_value_entry(host, &mut data, &key, CacheType::Internal, value);
        }
        if entry.ty == CacheType::Internal {
            write_help(&mut data, &entry.help);
            write_value_entry(host, &mut data, name, entry.ty, &entry.value);
        }
    }
    data.push(b'\n');
    let dest = dir.join("CMakeCache.txt");
    if let Ok(existing) = host.read(
        &dest,
        ReadRange::All {
            limit: MAX_CACHE_BYTES,
        },
    ) && existing == data.as_slice()
    {
        return Ok(());
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
    let result = host.rename(&tmp, &dest);
    if result.is_err() {
        let _ = host.remove_file(&tmp);
    }
    result
}

/// The `ADVANCED` property value to write for `entry`.
///
/// A stored value that agrees with `entry.advanced` stays as it is, so an
/// explicit `0` survives a save and a load. Otherwise the flag gives `1`, or `0`
/// when a stored true value was cleared.
fn advanced_value(entry: &CacheEntry) -> Option<&[u8]> {
    let stored = entry.props.get(b"ADVANCED").map(|value| value.as_slice());
    match (entry.advanced, stored) {
        (advanced, Some(value)) if rcmake_base::truth::is_on(value) == advanced => Some(value),
        (true, _) => Some(b"1"),
        (false, Some(_)) => Some(b"0"),
        (false, None) => None,
    }
}

/// `CMAKE_CACHEFILE_DIR` for `dir`: forward slashes and a lowercase drive letter.
fn cache_file_dir(dir: &Path) -> BString {
    let mut text = rcmake_host::path::from_native_cmake(dir);
    if text.get(1) == Some(&b':')
        && let Some(drive) = text.first_mut()
    {
        drive.make_ascii_lowercase();
    }
    text
}

/// Write `help` as `//` comment lines, as `cmCacheManager::OutputHelpString` does.
fn write_help(data: &mut BString, help: &[u8]) {
    write_comment(data, b"//", help, true);
}

/// Write `text` as comment lines that start with `prefix`.
///
/// This is the rule of `cmCacheManager::OutputHelpString` and
/// `OutputWarningComment`. A line ends at a newline, or, if `wrap`, at a space
/// when the line has 60 bytes or more. That space starts the next line. A line
/// that follows a newline starts with `\n` after the prefix. Empty text writes
/// nothing.
fn write_comment(data: &mut BString, prefix: &[u8], text: &[u8], wrap: bool) {
    let mut start = 0;
    for index in 1..=text.len() {
        let next = text.get(index);
        let wrap_here = wrap && index - start >= 60 && next == Some(&b' ');
        if index == text.len() || next == Some(&b'\n') || wrap_here {
            data.extend_from_slice(prefix);
            let mut line = text.get(start..index).unwrap_or_default();
            if let Some(rest) = line.strip_prefix(b"\n") {
                data.extend_from_slice(b"\\n");
                line = rest;
            }
            data.extend_from_slice(line);
            data.push(b'\n');
            start = index;
        }
    }
}

/// Write one entry and, for a value with a newline, the truncation warning
/// (`cmCacheManager::OutputNewlineTruncationWarning`).
fn write_value_entry(
    host: &dyn Host,
    data: &mut BString,
    name: &[u8],
    ty: CacheType,
    value: &[u8],
) {
    write_entry(data, name, ty, value);
    if !value.contains(&b'\n') {
        return;
    }
    let name = BString::from(name);
    host.write_out(
        Stream::Stderr,
        format!("CMake Warning:\n  Value of {name} contained a newline; truncating\n\n\n")
            .as_bytes(),
    );
    let comment =
        format!("WARNING: Value of {name} contained a newline and was truncated. Original value:");
    write_comment(data, b"# ", comment.as_bytes(), true);
    write_comment(data, b"# ", value, false);
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

    use std::path::Path;

    use rcmake_host::ReadRange;
    use rcmake_model::{Cache, CacheEntry};

    use super::{load, parse_entry, save, typed_value};

    fn entry(ty: CacheType, value: &str, help: &str) -> CacheEntry {
        CacheEntry {
            value: value.into(),
            ty,
            help: help.into(),
            advanced: false,
            props: Default::default(),
        }
    }

    #[test]
    fn save_uses_the_cmake_cache_layout_and_load_reads_it_back() {
        let mut cache = Cache::default();
        let long = "Choose the type of build, options are: None Debug Release RelWithDebInfo MinSizeRel and more words here.";
        let mut choice = entry(CacheType::String, "v", long);
        choice.props.set("STRINGS".into(), "a;b".into());
        cache.insert("LONGHELP".into(), choice);
        let mut multi = entry(CacheType::String, "v", "line one\nline two");
        multi.advanced = true;
        cache.insert("MULTI".into(), multi);
        cache.insert("EMPTYHELP".into(), entry(CacheType::String, "v", ""));
        cache.insert(
            "CMAKE_COMMAND".into(),
            entry(
                CacheType::Internal,
                "/bin/cmake",
                "Path to CMake executable.",
            ),
        );
        cache.insert(
            "INT".into(),
            entry(CacheType::Internal, "v", "internal help"),
        );
        let host = PureHost::default();
        host.create_dir_all(Path::new("/b")).unwrap_or_default();
        save(&host, Path::new("/b"), &cache).unwrap_or_else(|error| panic!("{error}"));
        let text = host
            .read(
                Path::new("/b/CMakeCache.txt"),
                ReadRange::All { limit: 1 << 20 },
            )
            .unwrap_or_default();
        let expected = "# This is the CMakeCache file.
# For build in directory: /b
# It was generated by CMake: /bin/cmake
# You can edit this file to change values found and used by cmake.
# If you do not want to change any of the values, simply exit the editor.
# If you do want to change a value, simply edit, save, and exit the editor.
# The syntax for the file is as follows:
# KEY:TYPE=VALUE
# KEY is the name of a variable in the cache.
# TYPE is a hint to GUIs for the type of VALUE, DO NOT EDIT TYPE!.
# VALUE is the current value for the KEY.

########################
# EXTERNAL cache entries
########################

EMPTYHELP:STRING=v

//Choose the type of build, options are: None Debug Release RelWithDebInfo
// MinSizeRel and more words here.
LONGHELP:STRING=v

//line one
//\\nline two
MULTI:STRING=v


########################
# INTERNAL cache entries
########################

//Path to CMake executable.
CMAKE_COMMAND:INTERNAL=/bin/cmake
//internal help
INT:INTERNAL=v
//STRINGS property for variable: LONGHELP
LONGHELP-STRINGS:INTERNAL=a;b
//ADVANCED property for variable: MULTI
MULTI-ADVANCED:INTERNAL=1

";
        assert_eq!(String::from_utf8_lossy(&text), expected);

        let loaded = load(&host, Path::new("/b")).unwrap_or_default();
        let help = |name: &str| {
            loaded
                .get(name.as_bytes())
                .map(|entry| entry.help.clone())
                .unwrap_or_default()
        };
        assert_eq!(help("LONGHELP"), BString::from(long));
        assert_eq!(help("MULTI"), BString::from("line one\nline two"));
        assert!(loaded.get(b"MULTI").is_some_and(|entry| entry.advanced));
        assert_eq!(
            loaded
                .get(b"LONGHELP")
                .and_then(|entry| entry.props.get(b"STRINGS").cloned()),
            Some(BString::from("a;b"))
        );
        assert!(loaded.get(b"LONGHELP-STRINGS").is_none());
    }

    #[test]
    fn explicit_advanced_zero_survives_save_and_load() {
        let host = PureHost::default();
        host.create_dir_all(Path::new("/b")).unwrap_or_default();
        host.write(
            Path::new("/b/CMakeCache.txt"),
            b"V:STRING=x\nV-ADVANCED:INTERNAL=0\nW:STRING=y\nW-ADVANCED:INTERNAL=ON\n",
            rcmake_host::WriteMode::Truncate,
        )
        .unwrap_or_default();
        let cache = load(&host, Path::new("/b")).unwrap_or_default();
        assert!(cache.get(b"V").is_some_and(|entry| !entry.advanced));
        assert!(cache.get(b"W").is_some_and(|entry| entry.advanced));
        save(&host, Path::new("/b"), &cache).unwrap_or_else(|error| panic!("{error}"));
        let text = host
            .read(
                Path::new("/b/CMakeCache.txt"),
                ReadRange::All { limit: 1 << 20 },
            )
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&text);
        assert!(text.contains("\nV-ADVANCED:INTERNAL=0\n"), "{text}");
        assert!(text.contains("\nW-ADVANCED:INTERNAL=ON\n"), "{text}");
        let reloaded = load(&host, Path::new("/b")).unwrap_or_default();
        assert!(reloaded.get(b"V").is_some_and(|entry| !entry.advanced));
        assert!(
            reloaded
                .get(b"V")
                .and_then(|entry| entry.props.get(b"ADVANCED").cloned())
                == Some(BString::from("0"))
        );
    }

    #[test]
    fn cache_records_use_the_command_line_entry_rules() {
        type EntryCase<'a> = (&'a [u8], &'a [u8], CacheType, &'a [u8]);
        let cases: [EntryCase<'_>; 7] = [
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

    #[test]
    fn save_is_copy_if_different_preserving_mtime() {
        let host = PureHost::default();
        host.create_dir_all(Path::new("/b")).unwrap_or_default();
        let mut cache = Cache::default();
        cache.insert("FOO".into(), entry(CacheType::String, "one", "help"));
        save(&host, Path::new("/b"), &cache).unwrap_or_else(|error| panic!("{error}"));
        let first_mtime = host
            .metadata(Path::new("/b/CMakeCache.txt"), rcmake_host::Follow::No)
            .expect("first metadata")
            .mtime;

        host.sleep(std::time::Duration::from_secs(5));
        save(&host, Path::new("/b"), &cache).unwrap_or_else(|error| panic!("{error}"));
        let second_mtime = host
            .metadata(Path::new("/b/CMakeCache.txt"), rcmake_host::Follow::No)
            .expect("second metadata")
            .mtime;
        assert_eq!(
            first_mtime, second_mtime,
            "unchanged CMakeCache.txt save must preserve mtime"
        );

        cache.insert("FOO".into(), entry(CacheType::String, "two", "help"));
        save(&host, Path::new("/b"), &cache).unwrap_or_else(|error| panic!("{error}"));
        let third_mtime = host
            .metadata(Path::new("/b/CMakeCache.txt"), rcmake_host::Follow::No)
            .expect("third metadata")
            .mtime;
        assert_ne!(
            first_mtime, third_mtime,
            "modified CMakeCache.txt save must update mtime"
        );
    }
}
