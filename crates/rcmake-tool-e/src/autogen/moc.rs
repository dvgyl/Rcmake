use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use rcmake_base::BString;
use rcmake_host::{FileTime, path::from_native_cmake as from_native};

use super::{
    Job, JobKind, Jobs, Result, ToolCtx,
    cache::{ParseCache, settings_hash},
    depfile,
    info::{Autogen, Moc, ScanRow},
    output_path,
    parse::{split_include, uses_moc_depfiles},
    required_time, resolve, run_generator, run_tool, timestamp, touch, warn, write_changed,
};

const PLACEHOLDER: &[u8] = b"enum some_compilers { need_more_than_nothing };\n";

pub(super) fn settings_digest(moc: &Moc, version: (u32, u32)) -> String {
    let executable = from_native(&moc.executable);
    let filters = moc
        .filters
        .iter()
        .filter(|_| !uses_moc_depfiles(version))
        .map(|(keyword, _)| keyword);
    settings_hash(
        std::iter::once(&executable)
            .chain(&moc.definitions)
            .chain(&moc.includes)
            .chain(&moc.options)
            .chain(&moc.predefs_command)
            .chain(filters)
            .chain(&moc.macros)
            .map(|value| value.as_slice()),
    )
}

pub(super) fn plan(
    cx: &ToolCtx<'_>,
    info: &Autogen,
    cache: &ParseCache,
    jobs: &mut Jobs,
) -> Result<()> {
    let Some(moc) = &info.moc else { return Ok(()) };
    let mut included_headers = BTreeSet::new();
    for row in &info.rows {
        if row.kind == JobKind::Source
            && row.moc
            && !moc.skip.contains(&row.path)
            && let Some(scan) = cache.get(&row.path)
        {
            source_jobs(cx, info, moc, row, &scan.moc, jobs, &mut included_headers)?;
        }
    }
    for row in &info.rows {
        if row.kind == JobKind::Source
            || !row.moc
            || moc.skip.contains(&row.path)
            || included_headers.contains(&row.path)
        {
            continue;
        }
        let Some(scan) = cache.get(&row.path) else {
            continue;
        };
        let has_macro = !scan.moc.macro_name.is_empty();
        if row.kind == JobKind::Module && info.qt_version < (6, 13) {
            if has_macro {
                return Err(format!(
                    "The C++ module unit \"{}\" contains {}. AUTOMOC on C++ module units requires Qt 6.13 or later.",
                    row.path.display(),
                    scan.moc.macro_name
                ));
            }
            continue;
        }
        if row.kind == JobKind::Header && !has_macro {
            continue;
        }
        let name = row
            .output
            .as_ref()
            .ok_or("The moc scan row has no output path.")?;
        let dir = if info.common.multi || row.kind == JobKind::Module {
            &info.common.include
        } else {
            &info.common.build
        };
        let output = output_path(dir, name)?;
        let aggregate = (row.kind == JobKind::Header).then(|| name.clone());
        jobs.add(output, row.kind, &row.path, aggregate, None)?;
    }
    Ok(())
}

fn source_jobs(
    cx: &ToolCtx<'_>,
    info: &Autogen,
    moc: &Moc,
    row: &ScanRow,
    scan: &super::cache::MocScan,
    jobs: &mut Jobs,
    included_headers: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let source_base = stem(&from_native(&row.path));
    let has_macro = !scan.macro_name.is_empty();
    let own_dot = scan
        .dot
        .iter()
        .any(|include| include_stem(include, b"", b".moc") == Some(source_base.as_slice()));
    let own_underscore = moc.relaxed
        && scan
            .underscore
            .iter()
            .any(|include| include_stem(include, b"moc_", b".cpp") == Some(source_base.as_slice()));
    if has_macro && !(own_dot || own_underscore) {
        return Err(format!(
            "The source \"{}\" contains {}, but does not include \"{}.moc\".",
            row.path.display(),
            scan.macro_name,
            source_base
        ));
    }

    for (include, is_dot) in scan
        .underscore
        .iter()
        .map(|name| (name, false))
        .chain(scan.dot.iter().map(|name| (name, true)))
    {
        let include_base = if is_dot {
            include_stem(include, b"", b".moc")
        } else {
            include_stem(include, b"moc_", b".cpp")
        };
        let Some(include_base) = include_base else {
            return Err(format!("The moc include \"{include}\" is invalid."));
        };
        let output = output_path(&info.common.include, include)?;
        let own = include_base == source_base.as_slice();
        if is_dot && !moc.relaxed && !own {
            return Err(format!(
                "The source \"{}\" includes \"{include}\" for a different source. Include \"{}.moc\".",
                row.path.display(),
                source_base
            ));
        }
        let header = if !is_dot || (moc.relaxed && (!own || !has_macro)) {
            Some(find_header(
                cx,
                info,
                moc,
                &row.path,
                include,
                include_base,
            )?)
        } else {
            None
        };
        let relaxed_source = !is_dot && moc.relaxed && own && !own_dot && has_macro;
        if relaxed_source {
            warn(
                cx,
                "Moc",
                &format!(
                    "AUTOMOC_RELAXED_MODE uses source \"{}\" for \"{include}\". Include \"{}.moc\" for regular mode.",
                    row.path.display(),
                    source_base
                ),
            );
        }
        if let Some(header) = header.filter(|_| !relaxed_source) {
            if moc.skip.contains(&header) {
                continue;
            }
            if is_dot {
                warn(
                    cx,
                    "Moc",
                    &format!(
                        "AUTOMOC_RELAXED_MODE uses header \"{}\" for \"{include}\". Include \"moc_{}.cpp\" for regular mode.",
                        header.display(),
                        BString::from(include_base)
                    ),
                );
            }
            jobs.add(output, JobKind::Header, &header, None, Some(&row.path))?;
            included_headers.insert(header);
        } else {
            if !has_macro {
                warn(
                    cx,
                    "Moc",
                    &format!(
                        "The source \"{}\" includes \"{include}\" without a configured Qt macro.",
                        row.path.display()
                    ),
                );
            }
            jobs.add(output, JobKind::Source, &row.path, None, Some(&row.path))?;
        }
    }
    Ok(())
}

fn stem(path: &[u8]) -> BString {
    let (_, name) = split_include(path);
    let end = name
        .iter()
        .rposition(|&byte| byte == b'.')
        .unwrap_or(name.len());
    BString::from(name.get(..end).unwrap_or_default())
}

fn include_stem<'a>(include: &'a [u8], prefix: &[u8], suffix: &[u8]) -> Option<&'a [u8]> {
    let (_, name) = split_include(include);
    name.strip_prefix(prefix)?
        .strip_suffix(suffix)
        .filter(|stem| !stem.is_empty())
}

fn find_header(
    cx: &ToolCtx<'_>,
    info: &Autogen,
    moc: &Moc,
    source: &Path,
    include: &[u8],
    stem: &[u8],
) -> Result<PathBuf> {
    let (prefix, _) = split_include(include);
    let roots = source
        .parent()
        .into_iter()
        .chain(moc.include_paths.iter().map(PathBuf::as_path));
    for dir in roots {
        let dir = resolve(dir, prefix);
        for extension in &info.header_extensions {
            let candidate = resolve(&dir, &[stem, b".", extension.as_slice()].concat());
            if timestamp(cx, &candidate).is_some() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "The source \"{}\" includes \"{}\", but the corresponding header was not found.",
        source.display(),
        BString::from(include)
    ))
}

pub(super) fn generate(
    cx: &ToolCtx<'_>,
    info: &Autogen,
    moc: &Moc,
    changed: bool,
    jobs: &Jobs,
    cache: &mut ParseCache,
    merged: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let predefs = predefs(cx, info, moc, changed)?;
    let tool_time = required_time(cx, &moc.executable)?;
    let has_depfiles = uses_moc_depfiles(info.qt_version);
    let mut aggregate = Vec::new();
    let mut aggregate_updated = false;

    for (output, job) in &jobs.outputs {
        if job.kind == JobKind::Uic {
            continue;
        }
        let scan = &mut cache.get_or_default(&job.input).moc;
        if job.kind == JobKind::Module && scan.macro_name.is_empty() {
            write_changed(cx, output, PLACEHOLDER)?;
            continue;
        }
        let sidecar = super::suffix_path(output, ".d");
        let mut sidecar_invalid = false;
        let mut dependencies = std::mem::take(&mut scan.dependencies);
        if has_depfiles && timestamp(cx, &sidecar).is_some() {
            match depfile::read_moc(cx, &sidecar) {
                Ok(paths) => dependencies = paths,
                Err(_) => sidecar_invalid = true,
            }
        }

        let source_time = required_time(cx, &job.input)?;
        let generated_time = timestamp(cx, output);
        let must_generate = changed
            || sidecar_invalid
            || generated_time.is_none_or(|time| {
                time < source_time
                    || time < tool_time
                    || predefs.is_some_and(|predefs| time < predefs)
            })
            || dependencies.iter().any(|name| {
                let path = resolve_dependency(cx, info, moc, &job.input, name);
                path.as_deref()
                    .and_then(|path| timestamp(cx, path))
                    .is_none_or(|time| generated_time.is_some_and(|output| output < time))
            });
        if must_generate {
            let args = arguments(moc, output, job, has_depfiles);
            let args = response_file(cx, info, &moc.executable, output, args)?;
            let result = run_generator(
                cx,
                &info.common,
                &moc.executable,
                args,
                &job.input,
                output,
                has_depfiles.then_some(sidecar.as_path()),
            );
            if let Err(error) = result {
                return Err(super::with_includers(error, &job.includers));
            }
            if has_depfiles {
                dependencies = if timestamp(cx, &sidecar).is_some() {
                    match depfile::read_moc(cx, &sidecar) {
                        Ok(paths) => paths,
                        Err(error) => {
                            let _ = super::remove(cx, output);
                            let _ = super::remove(cx, &sidecar);
                            return Err(error);
                        }
                    }
                } else {
                    Vec::new()
                };
            }
        }
        merged.insert(job.input.clone());
        for dependency in &dependencies {
            if let Some(path) = resolve_dependency(cx, info, moc, &job.input, dependency) {
                merged.insert(path);
            }
        }
        scan.dependencies = dependencies;
        if let Some(name) = &job.aggregate {
            aggregate.push((&job.input, name));
            aggregate_updated |= must_generate;
        }
    }
    aggregate.sort_unstable_by(|(a, _), (b, _)| a.as_os_str().cmp(b.as_os_str()));
    let mut content = b"// This file is autogenerated. Changes will be overwritten.\n".to_vec();
    if aggregate.is_empty() {
        content.extend_from_slice(
            b"// No files found that require moc or the moc files are included\n",
        );
        content.extend_from_slice(PLACEHOLDER);
    } else {
        let (opening, closing) = if info.common.multi {
            (b"#include <".as_slice(), b">\n".as_slice())
        } else {
            (b"#include \"".as_slice(), b"\"\n".as_slice())
        };
        for (_, name) in aggregate {
            content.extend_from_slice(opening);
            content.extend_from_slice(name);
            content.extend_from_slice(closing);
            if content.len() as u64 > super::DATA_LIMIT {
                return Err("The moc compilation file exceeds the size limit.".into());
            }
        }
    }
    if !write_changed(cx, &moc.compilation, &content)? && aggregate_updated {
        touch(cx, &moc.compilation)?;
    }
    Ok(())
}

fn resolve_dependency(
    cx: &ToolCtx<'_>,
    info: &Autogen,
    moc: &Moc,
    input: &Path,
    name: &[u8],
) -> Option<PathBuf> {
    if uses_moc_depfiles(info.qt_version) {
        let path = resolve(&info.common.build, name);
        return timestamp(cx, &path).map(|_| path);
    }
    input
        .parent()
        .into_iter()
        .chain(moc.include_paths.iter().map(PathBuf::as_path))
        .map(|folder| resolve(folder, name))
        .find(|path| timestamp(cx, path).is_some())
}

fn arguments(moc: &Moc, output: &Path, job: &Job, depfiles: bool) -> Vec<BString> {
    let mut args = moc.definitions.clone();
    args.extend(moc.includes.iter().cloned());
    if let Some(path) = &moc.predefs_file {
        args.extend([BString::from("--include"), from_native(path)]);
    }
    if moc.path_prefix
        && job.kind == JobKind::Header
        && let Some(relative) = moc
            .include_paths
            .iter()
            .find_map(|include| job.input.strip_prefix(include).ok())
        && let Some(prefix) = relative.parent()
    {
        let prefix = if prefix.as_os_str().is_empty() {
            BString::from("./")
        } else {
            from_native(prefix)
        };
        args.extend([BString::from("-p"), prefix]);
    }
    args.extend(moc.options.iter().cloned());
    if depfiles {
        args.push("--output-dep-file".into());
    }
    args.extend(["-o".into(), from_native(output), from_native(&job.input)]);
    args
}

fn predefs(cx: &ToolCtx<'_>, info: &Autogen, moc: &Moc, changed: bool) -> Result<Option<FileTime>> {
    let Some(output) = &moc.predefs_file else {
        return Ok(None);
    };
    let Some((compiler, options)) = moc.predefs_command.split_first() else {
        return Ok(timestamp(cx, output));
    };
    let compiler = resolve(&info.common.binary, compiler);
    let compiler_time = required_time(cx, &compiler)?;
    if !changed && let Some(time) = timestamp(cx, output).filter(|&time| time >= compiler_time) {
        return Ok(Some(time));
    }
    let mut args = options.to_vec();
    args.extend(moc.definitions.iter().cloned());
    args.extend(moc.includes.iter().cloned());
    let args = response_file(cx, info, &compiler, output, args)?;
    let result = run_tool(cx, &compiler, args, &info.common.build, info.common.verbose)?;
    if !write_changed(cx, output, &result.stdout)? {
        touch(cx, output)?;
    }
    required_time(cx, output).map(Some)
}

#[cfg(windows)]
fn response_file(
    cx: &ToolCtx<'_>,
    info: &Autogen,
    executable: &Path,
    output: &Path,
    args: Vec<BString>,
) -> Result<Vec<BString>> {
    let length =
        from_native(executable).len() + 1 + args.iter().map(|arg| arg.len() + 1).sum::<usize>();
    if length < info.command_limit {
        return Ok(args);
    }
    let response = super::suffix_path(output, ".rsp");
    let mut contents = args.join(&b'\n');
    contents.push(b'\n');
    super::write_file(cx, &response, &contents)?;
    Ok(vec![super::info::prefixed(b"@", &from_native(&response))])
}

#[cfg(not(windows))]
fn response_file(
    _: &ToolCtx<'_>,
    _: &Autogen,
    _: &Path,
    _: &Path,
    args: Vec<BString>,
) -> Result<Vec<BString>> {
    Ok(args)
}
