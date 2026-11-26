//! Update a Make stamp when a Fortran module changes.

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Stream, path::from_native};

use crate::{ToolCtx, dyndep, error, path, read_all, write};

/// Copy one Fortran module file to its stamp and return the exit code.
pub(crate) fn run(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let [module, stamp, tail @ ..] = args else {
        return 1;
    };
    let compiler = tail.first().map_or(b"".as_slice(), |name| name.as_slice());
    let module_path = path(cx, module);
    let Some(filename) = module_path.file_name() else {
        return error(cx, &[b"Invalid Fortran module path.\n"]);
    };
    let original = from_native(std::path::Path::new(filename));
    let (lower, upper) = module_file_names(&original);
    let module_arg = rcmake_host::path::to_native(module);
    let source =
        [upper, lower].map(|name| module_arg.with_file_name(rcmake_host::path::to_native(&name)));
    let Some(file) = source.iter().find(|file| {
        cx.host
            .metadata(&cx.cwd.join(file), Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
    }) else {
        let [upper, lower] = source.each_ref().map(|file| from_native(file));
        return error(
            cx,
            &[
                b"Error copying Fortran module \"",
                module,
                b"\".  Tried \"",
                &upper,
                b"\" and \"",
                &lower,
                b"\".\n",
            ],
        );
    };
    let stamp_path = path(cx, stamp);
    let copy = || {
        let data = cx.host.read(&cx.cwd.join(file), read_all()).ok()?;
        if let Ok(old) = cx.host.read(&stamp_path, read_all()) {
            if let Some(content) = comparable(&data, compiler, &data) {
                if comparable(&old, compiler, &data) == Some(content) {
                    return Some(());
                }
            } else {
                write(
                    cx,
                    Stream::Stderr,
                    &[
                        compiler,
                        b" fortran module ",
                        &from_native(file),
                        b" has unexpected format.\n",
                    ],
                );
            }
        }
        cx.host.create_dir_all(stamp_path.parent()?).ok()?;
        dyndep::write_output(cx, &stamp_path, &data, false).ok()
    };
    if copy().is_some() {
        return 0;
    }
    error(
        cx,
        &[
            b"Error copying Fortran module from \"",
            &from_native(file),
            b"\" to \"",
            stamp,
            b"\".\n",
        ],
    )
}

pub(crate) fn module_file_names(filename: &[u8]) -> (BString, BString) {
    for suffix in [b".mod".as_slice(), b".smod", b".sub"] {
        if let Some(stem) = filename.strip_suffix(suffix) {
            return (
                filename.into(),
                [stem.to_ascii_uppercase().as_slice(), suffix]
                    .concat()
                    .into(),
            );
        }
    }
    (
        [filename, b".mod"].concat().into(),
        [filename.to_ascii_uppercase().as_slice(), b".mod"]
            .concat()
            .into(),
    )
}

fn comparable<'a>(data: &'a [u8], compiler: &[u8], module: &[u8]) -> Option<&'a [u8]> {
    match compiler {
        b"GNU" if !module.starts_with(b"\x1f\x8b") => {
            let end = data.iter().position(|byte| *byte == b'\n')?;
            data.get(end + 1..)
        }
        b"Intel" | b"IntelLLVM" => {
            let body = data.get(1..)?;
            let end = body.windows(2).position(|pair| pair == b"\n\0")?;
            body.get(end + 2..)
        }
        _ => Some(data),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use rcmake_host::{Host, HostResult, PureHost};

    use super::*;

    #[test]
    fn copies_uppercase_modules_and_skips_old_gnu_timestamps() -> HostResult<()> {
        let cwd = PathBuf::from(if cfg!(windows) { "C:/out" } else { "/out" });
        let host = PureHost::new(cwd.clone(), cwd.join("cmake"));
        let module = cwd.join("EXAMPLE.mod");
        let stamp = cwd.join("stamps/example.mod.stamp");
        let cx = ToolCtx::for_test(&host, &cwd);
        let args = ["example", "stamps/example.mod.stamp", "GNU"].map(BString::from);
        host.seed_file(&module, b"Old timestamp\nModule interface\n")?;
        assert_eq!(run(&args, &cx), 0);
        let before = host.metadata(&stamp, Follow::Yes)?.mtime;
        host.sleep(Duration::from_secs(2));
        host.seed_file(&module, b"New timestamp\nModule interface\n")?;
        assert_eq!(run(&args, &cx), 0);
        assert_eq!(host.metadata(&stamp, Follow::Yes)?.mtime, before);
        host.seed_file(&module, b"New timestamp\nNew module interface\n")?;
        assert_eq!(run(&args, &cx), 0);
        assert_eq!(
            host.read(&stamp, read_all())?,
            b"New timestamp\nNew module interface\n"
        );
        Ok(())
    }

    #[test]
    fn compares_intel_version_bytes_and_gnu_gzip_bytes() {
        assert_eq!(
            comparable(b"xdate\n\0interface", b"IntelLLVM", b"xdate\n\0interface"),
            Some(b"interface".as_slice()),
        );
        assert_eq!(
            comparable(b"\x1f\x8b\nmod", b"GNU", b"\x1f\x8b\nmod"),
            Some(b"\x1f\x8b\nmod".as_slice()),
        );
    }
}
