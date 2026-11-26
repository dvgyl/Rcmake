use std::collections::BTreeSet;
use std::io::ErrorKind;

use rcmake_base::{BString, truth};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, RunMode};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, ReadRange, path};
use rcmake_model::{CacheEntry, CacheType};

use super::values;

enum Destination<'a> {
    Variables {
        prefix: &'a [u8],
        keys: BTreeSet<&'a [u8]>,
    },
    Cache {
        exclude: BTreeSet<&'a [u8]>,
        include_internals: BTreeSet<&'a [u8]>,
    },
}

pub(super) fn load(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [directory, options @ ..] = args.as_slice() else {
        return Err(CmdError::status("called with wrong number of arguments."));
    };
    let destination = match options {
        [b"READ_WITH_PREFIX", prefix, keys @ ..] => Destination::Variables {
            prefix,
            keys: keys.iter().copied().collect(),
        },
        [b"READ_WITH_PREFIX"] => {
            return Err(CmdError::status(
                "READ_WITH_PREFIX form must specify a prefix.",
            ));
        }
        _ if cx.mode() == RunMode::Script => {
            return Err(CmdError::status(
                "Only load_cache(READ_WITH_PREFIX) may be used in script mode",
            ));
        }
        _ => legacy_options(options),
    };
    let filename = path::to_native(directory).join("CMakeCache.txt");
    let path = cx.host().initial_cwd().join(&filename);
    let description = path::from_native(&filename);
    let cannot_load = || CmdError::status(format!("Cannot load cache file from {description}"));
    match cx.host().metadata(&path, Follow::Yes) {
        Ok(meta) if meta.kind == FileKind::File => {}
        Ok(_) => return Err(cannot_load()),
        Err(error)
            if matches!(destination, Destination::Cache { .. })
                && matches!(error.kind, ErrorKind::NotFound | ErrorKind::NotADirectory) =>
        {
            return Ok(Flow::Normal);
        }
        Err(_) => return Err(cannot_load()),
    }
    let contents = cx
        .host()
        .read(
            &path,
            ReadRange::All {
                limit: cx.limits().max_file_read,
            },
        )
        .map_err(|_| cannot_load())?;
    match destination {
        Destination::Variables { prefix, keys } => {
            for entry in contents
                .split(|byte| *byte == b'\n')
                .filter_map(parse_entry)
                .filter(|entry| keys.contains(entry.key))
            {
                cx.set_var(&[prefix, entry.key].concat(), entry.value.into());
            }
        }
        Destination::Cache {
            exclude,
            include_internals,
        } => {
            import_cache(cx, &description, &contents, &exclude, &include_internals);
        }
    }
    Ok(Flow::Normal)
}

fn legacy_options<'a>(options: &[&'a [u8]]) -> Destination<'a> {
    let mut exclude = BTreeSet::new();
    let mut include_internals = BTreeSet::new();
    let mut names = None;
    for &option in options {
        match option {
            b"EXCLUDE" => names = Some(&mut exclude),
            b"INCLUDE_INTERNALS" => names = Some(&mut include_internals),
            name => {
                if let Some(names) = &mut names {
                    names.insert(name);
                }
            }
        }
    }
    Destination::Cache {
        exclude,
        include_internals,
    }
}

fn import_cache(
    cx: &mut Cx<'_>,
    filename: &BString,
    contents: &[u8],
    exclude: &BTreeSet<&[u8]>,
    include_internals: &BTreeSet<&[u8]>,
) {
    for (index, line) in contents.split(|byte| *byte == b'\n').enumerate() {
        let Some(entry) = parse_entry(line) else {
            if !ignored(line) {
                cx.issue(
                    MessageKind::Error,
                    Effect::SetError,
                    None,
                    format!(
                        "Parse error in cache file {filename} on line {}. Offending entry: {}",
                        index + 1,
                        BString::from(line),
                    )
                    .into(),
                );
            }
            continue;
        };
        if exclude.contains(entry.key)
            || entry.kind == CacheType::Internal && !include_internals.contains(entry.key)
        {
            continue;
        }
        if entry.kind == CacheType::Internal {
            if let Some(key) = entry.key.strip_suffix(b"-ADVANCED") {
                if let Some(owner) = cx.cache().get_mut(key) {
                    owner.advanced = truth::is_on(entry.value);
                }
                continue;
            }
            if let Some(key) = entry.key.strip_suffix(b"-STRINGS") {
                if let Some(mut owner) = cx.cache().get(key).cloned() {
                    owner.props.set("STRINGS".into(), entry.value.into());
                    if !cx.insert_cache_entry_unrecorded(key.into(), owner) {
                        return;
                    }
                }
                continue;
            }
        }
        let key = BString::from(entry.key);
        let help = format!(
            "DO NOT EDIT, {key} loaded from external file.  To change this value edit this file: {filename}"
        );
        // A script chooses the imported file, so each entry passes the cache byte limit.
        if !cx.insert_cache_entry_unrecorded(key, internal(entry.value, help.into())) {
            return;
        }
    }
    for (key, help) in [
        (
            "CMAKE_CACHE_MAJOR_VERSION",
            "Major version of cmake used to create the current loaded cache",
        ),
        (
            "CMAKE_CACHE_MINOR_VERSION",
            "Minor version of cmake used to create the current loaded cache",
        ),
    ] {
        if !cx.cache().contains(key.as_bytes())
            && !cx.insert_cache_entry_unrecorded(key.into(), internal(b"0", help.into()))
        {
            return;
        }
    }
}

fn internal(value: &[u8], help: BString) -> CacheEntry {
    CacheEntry {
        value: value.into(),
        ty: CacheType::Internal,
        help,
        advanced: false,
        props: Default::default(),
    }
}

struct Entry<'a> {
    key: &'a [u8],
    kind: CacheType,
    value: &'a [u8],
}

fn ignored(line: &[u8]) -> bool {
    let line = line.trim_ascii_start();
    line.is_empty() || line.starts_with(b"#") || line.starts_with(b"//")
}

fn parse_entry(line: &[u8]) -> Option<Entry<'_>> {
    if ignored(line) {
        return None;
    }
    let (key, tail) = if let Some(quoted) = line.strip_prefix(b"\"") {
        let end = quoted.iter().position(|&byte| byte == b'"')?;
        let (key, tail) = quoted.split_at(end);
        (key, tail.strip_prefix(b"\"")?)
    } else {
        let end = line.iter().position(|&byte| matches!(byte, b':' | b'='))?;
        line.split_at(end)
    };
    let eq = tail.iter().position(|&byte| byte == b'=')?;
    let kind = tail
        .get(..eq)?
        .strip_prefix(b":")
        .and_then(CacheType::parse)
        .unwrap_or(CacheType::Uninitialized);
    let value = tail.get(eq + 1..)?.trim_ascii_end();
    let value = value
        .strip_prefix(b"'")
        .and_then(|value| value.strip_suffix(b"'"))
        .unwrap_or(value);
    Some(Entry { key, kind, value })
}
