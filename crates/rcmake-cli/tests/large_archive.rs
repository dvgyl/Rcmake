//! Exercise extraction above the default archive size limit.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, Follow, Host, Input, NativeHost, Output, ProcessResult, ReadRange, RunOutput,
    RunRequest, WriteMode,
};

const SIZE: u64 = 513 * 1024 * 1024;

struct Temp<'a>(&'a NativeHost, PathBuf);

impl Drop for Temp<'_> {
    fn drop(&mut self) {
        let _ = self.0.remove_dir_all(&self.1);
    }
}

fn run(
    host: &NativeHost,
    cwd: &Path,
    program: &str,
    args: &[&str],
) -> rcmake_host::HostResult<RunOutput> {
    host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: program.into(),
                args: args.iter().copied().map(BString::from).collect(),
            }],
            cwd: cwd.into(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(180)),
            tee: false,
            default_error_mode: false,
        },
        &mut |_, _| {},
    )
}

fn checked(output: RunOutput) -> RunOutput {
    assert_eq!(
        output.results,
        [ProcessResult::Exited(0)],
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn extracts_large_gzip_tar_and_zip_with_bounded_memory() -> Result<(), Box<dyn std::error::Error>> {
    let host = NativeHost::new()?;
    let temp = checked(run(
        &host,
        Path::new("/tmp"),
        "mktemp",
        &["-d", "/tmp/rcmake-large-archive.XXXXXXXX"],
    )?);
    let root = Temp(&host, std::str::from_utf8(&temp.stdout)?.trim().into());
    checked(run(
        &host,
        &root.1,
        "python3",
        &[
            "-c",
            r#"
import tarfile
import zipfile

size = 513 * 1024 * 1024
with open('input', 'wb') as out:
    out.write(b'head')
    out.seek(size - 4)
    out.write(b'tail')
with open('small', 'wb') as out:
    out.write(b'small')
for filename, mode in [('large.tgz', 'w:gz'), ('large.tar', 'w:')]:
    with tarfile.open(filename, mode, compresslevel=1) if mode == 'w:gz' else tarfile.open(filename, mode) as archive:
        archive.add('input', arcname='large')
        archive.add('small')
with zipfile.ZipFile('large.zip', 'w', compression=zipfile.ZIP_STORED) as archive:
    archive.write('input', arcname='large')
    archive.write('small')
"#,
        ],
    )?);
    let binary = env!("CARGO_BIN_EXE_rcmake");
    let listed = checked(run(
        &host,
        &root.1,
        binary,
        &["-E", "tar", "tf", "large.tgz"],
    )?);
    assert_eq!(listed.stdout, b"large\nsmall\n");
    checked(run(
        &host,
        &root.1,
        binary,
        &["-E", "tar", "xf", "large.tgz", "small"],
    )?);
    assert!(host.metadata(&root.1.join("large"), Follow::No).is_err());
    let script = root.1.join("extract.cmake");
    host.write(
        &script,
        br#"
file(ARCHIVE_EXTRACT INPUT large.tgz LIST_ONLY)
file(ARCHIVE_EXTRACT INPUT large.tgz DESTINATION gzip)
"#,
        WriteMode::CreateNew,
    )?;
    let listed = extract(&host, &root.1, &["-P", "extract.cmake"])?;
    assert_eq!(listed.stdout, b"large\nsmall\n");
    assert_payload(&host, &root.1.join("gzip/large"))?;

    for archive in ["large.tar", "large.zip"] {
        extract(&host, &root.1, &["-E", "tar", "xf", archive])?;
        let path = root.1.join("large");
        assert_payload(&host, &path)?;
        host.remove_file(&path)?;
    }
    Ok(())
}

fn extract(
    host: &NativeHost,
    root: &Path,
    args: &[&str],
) -> Result<RunOutput, Box<dyn std::error::Error>> {
    let binary = env!("CARGO_BIN_EXE_rcmake");
    #[cfg(target_os = "macos")]
    {
        let command: Vec<_> = ["-l", binary]
            .into_iter()
            .chain(args.iter().copied())
            .collect();
        let result = checked(run(host, root, "/usr/bin/time", &command)?);
        let usage = std::str::from_utf8(&result.stderr)?;
        let rss: u64 = usage
            .lines()
            .find(|line| line.contains("maximum resident set size"))
            .and_then(|line| line.split_whitespace().next())
            .ok_or("Missing peak resident memory measurement.")?
            .parse()?;
        assert!(
            rss < 192 * 1024 * 1024,
            "{args:?} used {rss} bytes of resident memory."
        );
        Ok(result)
    }
    #[cfg(not(target_os = "macos"))]
    Ok(checked(run(host, root, binary, args)?))
}

fn assert_payload(host: &NativeHost, path: &Path) -> rcmake_host::HostResult<()> {
    assert_eq!(host.metadata(path, Follow::Yes)?.len, SIZE);
    for (offset, expected) in [(0, b"head"), (SIZE - 4, b"tail")] {
        assert_eq!(
            host.read(
                path,
                ReadRange::Slice {
                    offset,
                    len: Some(4)
                }
            )?,
            expected
        );
    }
    Ok(())
}
