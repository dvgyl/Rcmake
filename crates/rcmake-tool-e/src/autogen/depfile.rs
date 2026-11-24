use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use rcmake_base::BString;
use rcmake_host::path::from_native_cmake as from_native;

use super::{DATA_LIMIT, Result, ToolCtx, info::Autogen, read, write_changed};
use crate::depfile::{escape, parse};

pub(super) fn read_moc(cx: &ToolCtx<'_>, path: &Path) -> Result<Vec<BString>> {
    let data = read(cx, path, DATA_LIMIT)?;
    let rules =
        parse(&data).ok_or_else(|| format!("Invalid moc depfile \"{}\".", path.display()))?;
    Ok(rules
        .into_iter()
        .flat_map(|rule| rule.dependencies)
        .map(BString::from)
        .collect())
}

pub(super) fn write(
    cx: &ToolCtx<'_>,
    info: &Autogen,
    mut dependencies: BTreeSet<PathBuf>,
) -> Result<()> {
    let Some((path, rule)) = &info.depfile else {
        return Ok(());
    };
    dependencies.remove(&info.cmake);
    let mut contents = Vec::new();
    escape(rule, &mut contents);
    contents.extend_from_slice(b":");
    for file in dependencies.iter().chain(std::iter::once(&info.cmake)) {
        let file = from_native(file);
        if file.contains(&b'\n') || file.contains(&b'\r') {
            return Err("A dependency path contains a line break.".into());
        }
        contents.extend_from_slice(b" \\\n\t");
        escape(&file, &mut contents);
        if contents.len() as u64 > DATA_LIMIT {
            return Err("The autogen depfile exceeds the size limit.".into());
        }
    }
    contents.push(b'\n');
    write_changed(cx, path, &contents)?;
    Ok(())
}
