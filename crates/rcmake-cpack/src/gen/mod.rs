//! Generator registry and the External and macOS generators.

pub mod bundle;
pub mod dmg;
pub(crate) mod external;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, CopyOptions, FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest,
};

use crate::archive_gen::{ArchiveGenerator, ArchiveKind};
use crate::config::absolute;
use crate::stage::clear_output_file;
use crate::{CpackError, PackageContext, PackageGenerator, error};

/// The generator named `name` (a `CPACK_GENERATOR` entry), or `None` when Rcmake does
/// not have it.
#[must_use]
pub fn create(name: &[u8]) -> Option<Box<dyn PackageGenerator>> {
    if let Some(kind) = ArchiveKind::ALL
        .into_iter()
        .find(|kind| kind.name().as_bytes() == name)
    {
        return Some(Box::new(ArchiveGenerator { kind }));
    }
    match name {
        b"External" => Some(Box::new(external::ExternalGenerator)),
        b"DragNDrop" => Some(Box::new(dmg::DragNDropGenerator)),
        b"Bundle" => Some(Box::new(bundle::BundleGenerator)),
        _ => None,
    }
}

fn option<'a>(cx: &PackageContext<'a>, name: &[u8]) -> Option<&'a [u8]> {
    cx.vars
        .get(name)
        .map(|value| value.as_slice())
        .filter(|value| !value.is_empty())
}

fn copy_asset(host: &dyn Host, source: &[u8], dest: &Path) -> Result<(), CpackError> {
    let source = absolute(&host.initial_cwd(), source);
    if host.metadata(&source, Follow::Yes)?.kind != FileKind::File {
        return Err(error(format!(
            "Expected a regular file at {}",
            source.display()
        )));
    }
    if source != dest {
        clear_output_file(host, dest)?;
        host.copy_file(
            &source,
            dest,
            CopyOptions {
                preserve_permissions: true,
                preserve_mtime: true,
            },
        )?;
    }
    Ok(())
}

fn run_tool(
    host: &dyn Host,
    cwd: &Path,
    program: &[u8],
    args: Vec<BString>,
) -> Result<(), CpackError> {
    let output = host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: program.into(),
                args,
            }],
            cwd: cwd.to_owned(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::SharedWithStdout,
            timeout: Some(Duration::from_secs(300)),
            tee: false,
        },
        &mut |_, _| {},
    )?;
    match output.results.as_slice() {
        [ProcessResult::Exited(0)] => Ok(()),
        [ProcessResult::SpawnFailed(err)] => Err(error(format!(
            "Cannot start {}: {err}",
            BString::from(program)
        ))),
        results => Err(error(format!(
            "{} failed ({results:?})\n{}",
            BString::from(program),
            BString::from(output.stdout)
        ))),
    }
}

struct ScratchDir<'a> {
    host: &'a dyn Host,
    path: PathBuf,
    cleanup: bool,
}

impl<'a> ScratchDir<'a> {
    fn new(host: &'a dyn Host, root: &Path) -> Result<Self, CpackError> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(".rcmake-dmg-{}-{id}", host.system().pid));
            match host.metadata(&path, Follow::No) {
                Err(err) if err.kind == ErrorKind::NotFound => {
                    host.create_dir_all(&path)?;
                    return Ok(Self {
                        host,
                        path,
                        cleanup: true,
                    });
                }
                Err(err) => return Err(err.into()),
                Ok(_) => {}
            }
        }
        Err(error("Cannot create a temporary disk image directory"))
    }
}

impl Drop for ScratchDir<'_> {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = self.host.remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use rcmake_base::hash::HashAlgo;
    use rcmake_host::path::{from_native, to_native};
    use rcmake_host::{CommandSpec, Host, ProcessResult, ReadRange, RunOutput, WriteMode};

    pub(super) fn invoke(host: Arc<dyn Host>, generator: &str, options: &str) -> i32 {
        let root = host.initial_cwd();
        let source = root.join("payload");
        host.create_dir_all(&source).unwrap();
        host.write(&source.join("readme.txt"), b"payload", WriteMode::Truncate)
            .unwrap();
        let config = root.join("CPackConfig.cmake");
        let source = from_native(&source);
        host.write(
            &config,
            format!(
                r#"set(CPACK_GENERATOR {generator})
set(CPACK_PACKAGE_NAME sample)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME "Sample Package")
set(CPACK_INSTALLED_DIRECTORIES [=[{source};/]=])
set(CPACK_PACKAGE_CHECKSUM "MD5;SHA256")
{options}
"#,
            )
            .as_bytes(),
            WriteMode::Truncate,
        )
        .unwrap();

        crate::main(
            &["cpack".into(), "--config".into(), from_native(&config)],
            host,
        )
    }

    pub(super) fn read(host: &dyn Host, path: &Path) -> Vec<u8> {
        host.read(
            path,
            ReadRange::All {
                limit: 16 * 1024 * 1024,
            },
        )
        .unwrap()
    }

    pub(super) fn check_checksums(host: &dyn Host) {
        let root = host.initial_cwd();
        let bytes = read(host, &root.join("Sample Package.dmg"));
        for (extension, algorithm) in [("md5", HashAlgo::Md5), ("sha256", HashAlgo::Sha256)] {
            let expected = format!("{}  Sample Package.dmg\n", algorithm.hex_digest(&bytes));
            assert_eq!(
                read(host, &root.join(format!("Sample Package.dmg.{extension}"))),
                expected.as_bytes()
            );
        }
    }

    pub(super) fn fake_image(host: &dyn Host, command: &CommandSpec) {
        let output = to_native(command.args.last().unwrap());
        host.write(&output, b"disk image", WriteMode::CreateNew)
            .unwrap();
    }

    pub(super) fn success() -> RunOutput {
        RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        }
    }
}
