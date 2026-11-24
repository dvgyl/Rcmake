use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::path::from_native_cmake as from_native;

use super::{
    JobKind, Jobs, Result, ToolCtx,
    cache::{ParseCache, settings_hash},
    info::{Autogen, Uic, prefixed},
    output_path,
    parse::split_include,
    required_time, resolve, run_generator, timestamp,
};

pub(super) fn settings_digest(uic: &Uic) -> Option<String> {
    let executable = from_native(uic.executable.as_ref()?);
    let file_paths: Vec<_> = uic
        .file_options
        .keys()
        .map(|file| from_native(file))
        .collect();
    Some(settings_hash(
        std::iter::once(&executable)
            .chain(&uic.options)
            .chain(
                file_paths
                    .iter()
                    .zip(uic.file_options.values())
                    .flat_map(|(path, options)| std::iter::once(path).chain(options)),
            )
            .map(|value| value.as_slice()),
    ))
}

pub(super) fn plan(
    cx: &ToolCtx<'_>,
    info: &Autogen,
    cache: &ParseCache,
    jobs: &mut Jobs,
) -> Result<()> {
    let Some(uic) = &info.uic else { return Ok(()) };
    for row in &info.rows {
        let Some(scan) = cache.get(&row.path) else {
            continue;
        };
        for name in &scan.uic.includes {
            let output = output_path(&info.common.include, name)?;
            let input = find_ui(cx, uic, &row.path, name)?;
            if !uic.skip.contains(&input) {
                jobs.add(output, JobKind::Uic, &input, None, Some(&row.path))?;
            }
        }
    }
    Ok(())
}

fn find_ui(cx: &ToolCtx<'_>, uic: &Uic, source: &Path, include: &[u8]) -> Result<PathBuf> {
    let (prefix, file) = split_include(include);
    let stem = file
        .strip_prefix(b"ui_")
        .and_then(|name| name.strip_suffix(b".h"))
        .filter(|stem| !stem.is_empty())
        .ok_or_else(|| {
            format!(
                "Invalid generated UI include \"{}\".",
                BString::from(include)
            )
        })?;
    let file = prefixed(stem, b".ui");
    let local = source.parent().unwrap_or(cx.cwd);
    let candidates = [
        resolve(&resolve(local, prefix), &file),
        resolve(local, &file),
    ]
    .into_iter()
    .chain(uic.search_paths.iter().map(|path| resolve(path, &file)))
    .chain(
        uic.search_paths
            .iter()
            .map(|path| resolve(&resolve(path, prefix), &file)),
    );
    for path in candidates {
        if timestamp(cx, &path).is_some() {
            return Ok(path);
        }
    }
    Err(format!(
        "The source \"{}\" includes \"{}\", but the UI file \"{file}\" was not found.",
        source.display(),
        BString::from(include)
    ))
}

pub(super) fn generate(cx: &ToolCtx<'_>, info: &Autogen, changed: bool, jobs: &Jobs) -> Result<()> {
    let Some(uic) = &info.uic else { return Ok(()) };
    for (output, job) in &jobs.outputs {
        if job.kind != JobKind::Uic {
            continue;
        }
        let executable = uic.executable.as_ref().ok_or_else(|| {
            format!(
                "The UI file \"{}\" requires uic, but QT_UIC_EXECUTABLE is empty.",
                job.input.display()
            )
        })?;
        let input_time = required_time(cx, &job.input)?;
        let tool_time = required_time(cx, executable)?;
        if changed || timestamp(cx, output).is_none_or(|time| time < input_time || time < tool_time)
        {
            let mut args = uic.options.clone();
            if let Some(options) = uic.file_options.get(&job.input) {
                merge_options(&mut args, options, info.qt_version.0 >= 5);
            }
            args.extend(["-o".into(), from_native(output), from_native(&job.input)]);
            if let Err(error) =
                run_generator(cx, &info.common, executable, args, &job.input, output, None)
            {
                return Err(super::with_includers(error, &job.includers));
            }
        }
    }
    Ok(())
}

fn merge_options(base: &mut Vec<BString>, new: &[BString], qt5: bool) {
    if base.is_empty() {
        base.extend_from_slice(new);
        return;
    }
    let mut extra = Vec::new();
    let mut iter = new.iter();
    while let Some(option) = iter.next() {
        let Some(index) = base.iter().position(|value| value == option) else {
            extra.push(option.clone());
            continue;
        };
        let Some(name) = option.strip_prefix(b"-") else {
            continue;
        };
        let name = if qt5 {
            name.strip_prefix(b"-").unwrap_or(name)
        } else {
            name
        };
        if matches!(
            name,
            b"tr" | b"translate" | b"postfix" | b"generator" | b"include" | b"g"
        ) && let Some(value) = base.get_mut(index + 1)
            && let Some(replacement) = iter.next()
        {
            *value = replacement.clone();
        }
    }
    base.extend(extra);
}
