use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::BString;

use super::{ITEM_LIMIT, JobKind, Result, cache::Scan, info::Moc};

pub(super) fn scan(
    contents: &[u8],
    moc: Option<&Moc>,
    uic: bool,
    kind: JobKind,
    qt_version: (u32, u32),
) -> Result<Scan> {
    let contents = contents.split(|&byte| byte == 0).next().unwrap_or_default();
    let mut scan = Scan::default();
    let mut underscore = BTreeSet::new();
    let mut dot = BTreeSet::new();
    let mut ui = BTreeSet::new();

    // Qt checks macro names in metadata order and keeps the first matching name.
    let macro_names = moc.map(|moc| {
        moc.macros
            .iter()
            .enumerate()
            .rev()
            .map(|(index, name)| (name.as_slice(), index))
            .collect::<BTreeMap<_, _>>()
    });
    let mut macro_index: Option<usize> = None;
    for (index, raw_line) in contents.split_inclusive(|&byte| byte == b'\n').enumerate() {
        let line = horizontal(raw_line);
        if let Some(names) = &macro_names
            && index > 0
        {
            let candidate = horizontal(line.strip_prefix(b"{").unwrap_or(line));
            let length = candidate
                .iter()
                .take_while(|&&byte| byte.is_ascii_alphanumeric() || byte == b'_')
                .count();
            if let Some(name) = candidate.get(..length).filter(|_| length < candidate.len())
                && let Some(&index) = names.get(name)
            {
                macro_index = Some(macro_index.map_or(index, |current| current.min(index)));
            }
        }

        let Some(include) = include(line) else {
            continue;
        };
        let (_, name) = split_include(include);
        if moc.is_some() && kind == JobKind::Source {
            if name
                .strip_prefix(b"moc_")
                .and_then(|name| name.strip_suffix(b".cpp"))
                .is_some_and(|stem| !stem.is_empty())
            {
                underscore.insert(BString::from(include));
            } else if name.len() > 4 && name.ends_with(b".moc") {
                dot.insert(BString::from(include));
            }
        }
        if uic
            && name
                .strip_prefix(b"ui_")
                .and_then(|name| name.strip_suffix(b".h"))
                .is_some_and(|stem| !stem.is_empty())
        {
            ui.insert(BString::from(include));
        }
        if underscore.len() + dot.len() + ui.len() > ITEM_LIMIT {
            return Err("The C++ source has too many generated includes.".into());
        }
    }

    if let Some(moc) = moc {
        if let Some(name) = macro_index.and_then(|index| moc.macros.get(index)) {
            scan.moc.macro_name = name.clone();
        }
        if !uses_moc_depfiles(qt_version) {
            scan.moc.dependencies = plugin_metadata(contents, moc)?;
        }
    }
    scan.moc.underscore = underscore.into_iter().collect();
    scan.moc.dot = dot.into_iter().collect();
    scan.uic.includes = ui.into_iter().collect();
    Ok(scan)
}

pub(super) fn uses_moc_depfiles(qt_version: (u32, u32)) -> bool {
    qt_version >= (5, 15)
}

fn plugin_metadata(contents: &[u8], moc: &Moc) -> Result<Vec<BString>> {
    if !moc
        .filters
        .iter()
        .any(|(keyword, _)| keyword == b"Q_PLUGIN_METADATA")
    {
        return Ok(Vec::new());
    }
    let mut dependencies = BTreeSet::new();
    let mut search = contents;
    while let Some((_, after_newline)) = super::cache::split(search, b'\n') {
        let line = horizontal(after_newline);
        search = line;
        let Some(arguments) = line
            .strip_prefix(b"Q_PLUGIN_METADATA")
            .map(horizontal)
            .and_then(|line| line.strip_prefix(b"("))
        else {
            continue;
        };
        let (arguments, rest) = super::cache::split(arguments, b')').unwrap_or((arguments, b""));
        // The remainder can contain another macro call. Advance past this one
        // so an unterminated call cannot cause repeated scans of the same suffix.
        search = rest;
        if let Some(path) = plugin_dependency(arguments) {
            dependencies.insert(path);
            if dependencies.len() > ITEM_LIMIT {
                return Err("The C++ source has too many moc dependencies.".into());
            }
        }
    }
    Ok(dependencies.into_iter().collect())
}

fn plugin_dependency(arguments: &[u8]) -> Option<BString> {
    let (index, _) = arguments
        .windows(4)
        .enumerate()
        .rev()
        .find(|(index, tag)| {
            *tag == b"FILE"
                && arguments
                    .get(index + 4..)
                    .map(horizontal)
                    .is_some_and(|suffix| suffix.starts_with(b"\""))
        })?;
    let path = horizontal(arguments.get(index + 4..)?).strip_prefix(b"\"")?;
    let (path, _) = super::cache::split(path, b'\"')?;
    if path.is_empty() {
        return None;
    }
    Some(
        path.iter()
            .map(|&byte| {
                if byte == b'\r' || byte == b'\n' {
                    b' '
                } else {
                    byte
                }
            })
            .collect::<Vec<_>>()
            .into(),
    )
}

pub(super) fn split_include(include: &[u8]) -> (&[u8], &[u8]) {
    include
        .iter()
        .rposition(|&byte| byte == b'/')
        .and_then(|index| Some((include.get(..index)?, include.get(index + 1..)?)))
        .unwrap_or((b"", include))
}

fn include(line: &[u8]) -> Option<&[u8]> {
    let line = horizontal(line.strip_prefix(b"#")?).strip_prefix(b"include")?;
    if !matches!(line.first(), Some(b' ' | b'\t')) {
        return None;
    }
    let line = horizontal(line);
    let path = match line {
        [b'\"' | b'<', path @ ..] => path,
        _ => return None,
    };
    let end = path.iter().position(|&byte| matches!(byte, b'\"' | b'>'))?;
    let path = path.get(..end)?;
    (!path.contains(&b' ')).then_some(path)
}

fn horizontal(line: &[u8]) -> &[u8] {
    let count = line
        .iter()
        .take_while(|&&byte| matches!(byte, b' ' | b'\t'))
        .count();
    line.get(count..).unwrap_or_default()
}
