use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::BString;
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

fn parse_entry(line: &[u8]) -> Option<(&[u8], CacheType, &[u8])> {
    if line.starts_with(b"#") || line.starts_with(b"//") {
        return None;
    }
    let (name, typed_value) = if let Some(quoted) = line.strip_prefix(b"\"") {
        let end = quoted.iter().position(|b| *b == b'"')?;
        let (name, tail) = quoted.split_at(end);
        (name, tail.strip_prefix(b"\":")?)
    } else {
        let colon = line.iter().position(|b| *b == b':')?;
        (line.get(..colon)?, line.get(colon + 1..)?)
    };
    let eq = typed_value.iter().position(|b| *b == b'=')?;
    let ty = CacheType::parse(typed_value.get(..eq)?)?;
    let value = typed_value.get(eq + 1..)?.trim_ascii_end();
    let value = value
        .strip_prefix(b"'")
        .and_then(|v| v.strip_suffix(b"'"))
        .unwrap_or(value);
    Some((name, ty, value))
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
