//! Scan a Fortran source for Ninja.

mod format;
mod scan;

use std::io::ErrorKind;
use std::path::PathBuf;

use rcmake_base::{BString, Limits};
use rcmake_fortran::SourceForm;

use crate::{ToolCtx, depfile, dyndep, error, path, read_all};

pub(crate) use format::{
    compiler, logical_name, physical_name, read_compiler, read_ddi, relative_to_top,
};
pub(crate) use scan::{ScanSettings, scan_source};

const COMMAND: &str = "-E cmake_ninja_depends";

#[derive(Default)]
struct Request<'a> {
    tdi: &'a [u8],
    src: &'a [u8],
    src_orig: &'a [u8],
    out: &'a [u8],
    dep: &'a [u8],
    obj: &'a [u8],
    ddi: &'a [u8],
    lang: &'a [u8],
}

impl<'a> Request<'a> {
    fn parse(args: &'a [BString]) -> Result<Self, BString> {
        let mut request = Self::default();
        for argument in args {
            let Some((key, value)) = crate::split_once(argument, b'=') else {
                return Err(unknown(argument));
            };
            match key {
                b"--tdi" => request.tdi = value,
                b"--src" => request.src = value,
                b"--src-orig" => request.src_orig = value,
                b"--out" => request.out = value,
                b"--dep" => request.dep = value,
                b"--obj" => request.obj = value,
                b"--ddi" => request.ddi = value,
                b"--lang" => request.lang = value,
                b"--pp" => {
                    request.src = value;
                    request.out = value;
                }
                _ => return Err(unknown(argument)),
            }
        }
        for (value, flag) in [
            (request.tdi, "--tdi="),
            (request.src, "--src="),
            (request.out, "--out="),
            (request.dep, "--dep="),
            (request.obj, "--obj="),
            (request.ddi, "--ddi="),
            (request.lang, "--lang="),
        ] {
            if value.is_empty() {
                return Err(format!("{COMMAND} requires value for {flag}").into());
            }
        }
        if request.lang != b"Fortran" {
            return Err(format!(
                "{COMMAND} does not understand the {} language",
                BString::from(request.lang)
            )
            .into());
        }
        Ok(request)
    }
}

fn unknown(argument: &[u8]) -> BString {
    format!("{COMMAND} unknown argument: {}", BString::from(argument)).into()
}

/// Run the Fortran Ninja source scanner and return its exit code.
pub(crate) fn run(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    match Request::parse(args).and_then(|request| execute(&request, cx)) {
        Ok(()) => 0,
        Err(message) => error(cx, &[b"CMake Error: ", &message, b"\n"]),
    }
}

fn execute(request: &Request<'_>, cx: &ToolCtx<'_>) -> Result<(), BString> {
    let document = cx
        .host
        .read(&path(cx, request.tdi), read_all())
        .map_err(|error| {
            let file = BString::from(request.tdi);
            if error.kind == ErrorKind::NotFound {
                format!("{COMMAND} failed to parse {file}File not found: {file}")
            } else {
                format!("{COMMAND} failed to read {file}: {error}")
            }
        })?;
    let metadata = format::read_metadata(&document).map_err(|error| {
        format!(
            "{COMMAND} failed to parse {}{error}",
            BString::from(request.tdi)
        )
    })?;
    let source = path(cx, request.src);
    let original = (!request.src_orig.is_empty()).then(|| path(cx, request.src_orig));
    let include_dirs: Vec<PathBuf> = original
        .iter()
        .filter_map(|name| name.parent().map(PathBuf::from))
        .chain(metadata.include_dirs.iter().map(|dir| path(cx, dir)))
        .collect();
    let settings = ScanSettings {
        dir: cx.cwd,
        include_dirs: &include_dirs,
        form: SourceForm::from_path(request.src)
            .or_else(|| SourceForm::from_path(request.src_orig)),
        defines: &[],
        building_intrinsic_modules: metadata.building_intrinsic_modules,
    };
    let result = scan_source(
        cx.host,
        &source,
        &settings,
        &mut Limits::default().max_file_read,
    )
    .map_err(|error| {
        let file = BString::from(request.src);
        if error.kind == ErrorKind::NotFound {
            format!("{COMMAND} failed to open {file}")
        } else {
            format!("{COMMAND} failed to scan {file}: {error}")
        }
    })?;

    let ddi = format::write_ddi(request, &result, &metadata)?;
    let mut dep = Vec::new();
    append_depfile_path(&mut dep, request.out)?;
    dep.push(b':');
    for include in result.resolved_includes {
        dep.extend_from_slice(b" \\\n ");
        append_depfile_path(&mut dep, &include)?;
    }
    dep.push(b'\n');
    dyndep::write_output(cx, &path(cx, request.dep), &dep, true)?;
    dyndep::write_output(cx, &path(cx, request.ddi), &ddi, true)
}

fn append_depfile_path(output: &mut Vec<u8>, path: &[u8]) -> Result<(), BString> {
    if path.iter().any(|byte| matches!(byte, 0 | b'\n' | b'\r')) {
        return Err("Invalid path in a Fortran dependency file.".into());
    }
    depfile::escape(path, output);
    Ok(())
}

#[cfg(test)]
mod tests;
