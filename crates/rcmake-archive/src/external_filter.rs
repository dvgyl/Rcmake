use std::path::Path;
use std::time::Duration;

use rcmake_host::{CommandSpec, Host, Input, Output, ProcessResult, RunRequest, path};

use crate::{ArchiveError, size_limit};

pub(super) fn detect(prefix: &[u8]) -> Option<&'static str> {
    prefix.starts_with(b"GRZipII\0").then_some("grzip")
}

pub(super) fn decompress(
    host: &dyn Host,
    archive: &Path,
    name: &str,
) -> Result<Vec<u8>, ArchiveError> {
    let executable = path::executable_on_absolute_path(host, name)
        .ok_or_else(|| ArchiveError::new("Unrecognized archive format"))?;
    let output = host.run_bounded(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: path::from_native(&executable),
                args: vec!["-d".into()],
            }],
            cwd: host.initial_cwd(),
            env: host.env_all(),
            stdin: Input::File(archive.to_path_buf()),
            stdout: Output::Capture,
            stderr: Output::Null,
            timeout: Some(Duration::from_secs(30)),
            tee: false,
        },
        usize::try_from(size_limit()).unwrap_or(usize::MAX),
    )?;
    match output.results.as_slice() {
        [ProcessResult::Exited(0)] => Ok(output.stdout),
        [ProcessResult::Exited(code)] => Err(ArchiveError::new(format!(
            "Child process exited with status {code}"
        ))),
        [ProcessResult::TimedOut] => Err(ArchiveError::new("Archive filter timed out")),
        _ => Err(ArchiveError::new("Archive filter failed")),
    }
}
