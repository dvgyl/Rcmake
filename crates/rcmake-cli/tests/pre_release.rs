//! Integration tests for CLI exit codes, data discovery, and filesystem safety.

#![allow(clippy::disallowed_methods)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const RCMAKE: &str = env!("CARGO_BIN_EXE_rcmake");
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "rcmake-pre-release-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir)?;
        Ok(Self(dir))
    }

    fn command(&self, exe: impl AsRef<Path>) -> Command {
        let mut command = Command::new(exe.as_ref());
        command.current_dir(&self.0);
        command
    }

    #[cfg(unix)]
    fn shim(&self, name: &str) -> io::Result<PathBuf> {
        let shim = self.0.join(name);
        std::os::unix::fs::symlink(RCMAKE, &shim)?;
        Ok(shim)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn script_exit_preserves_the_platform_exit_code() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let script = fixture.0.join("exit.cmake");
    for code in [0, 1, 255, 256, 300, -1, i32::MAX] {
        fs::write(&script, format!("cmake_language(EXIT {code})\n"))?;
        let output = fixture.command(RCMAKE).arg("-P").arg(&script).output()?;
        let expected = if cfg!(windows) { code } else { code & 0xff };
        assert_eq!(output.status.code(), Some(expected), "EXIT {code}");
        assert!(output.stderr.is_empty(), "EXIT {code}");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn ctest_preserves_script_exit_status() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let ctest = fixture.shim("ctest")?;
    for code in [0, 1, 255, 256, 300, -1] {
        fs::write(
            fixture.0.join("exit.cmake"),
            format!("cmake_language(EXIT {code})\n"),
        )?;
        let output = fixture
            .command(&ctest)
            .args(["-S", "exit.cmake"])
            .output()?;
        assert_eq!(output.status.code(), Some(code & 0xff), "EXIT {code}");
        assert!(output.stderr.is_empty(), "EXIT {code}");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn help_and_version_work_for_each_persona() -> io::Result<()> {
    let fixture = Fixture::new()?;
    for (exe, program) in [
        (PathBuf::from(RCMAKE), "cmake"),
        (fixture.shim("cmake")?, "cmake"),
        (fixture.shim("ctest")?, "ctest"),
        (fixture.shim("cpack")?, "cpack"),
    ] {
        let help = fixture.command(&exe).arg("--help").output()?;
        assert_success(&help);
        assert!(help.stdout.starts_with(b"Usage"));
        let banner = format!("{program} version 4.5.1\n");
        let version = fixture.command(&exe).arg("--version").output()?;
        assert_success(&version);
        assert!(version.stdout.starts_with(banner.as_bytes()));
        let written = fixture
            .command(&exe)
            .args(["--version", "version.txt"])
            .output()?;
        assert_success(&written);
        assert!(written.stdout.is_empty());
        assert!(fs::read(fixture.0.join("version.txt"))?.starts_with(banner.as_bytes()));
        let json = fixture.command(&exe).arg("--version=json-v1").output()?;
        assert_success(&json);
        use rcmake_json::{ReadOptions, Value, parse};
        let Value::Object(json) =
            parse(&json.stdout, &ReadOptions::default()).map_err(io::Error::other)?
        else {
            return Err(io::Error::other("Expected a JSON object"));
        };
        let Some(Value::Object(program_info)) = json.get(b"program".as_slice()) else {
            return Err(io::Error::other("Expected program information"));
        };
        assert_eq!(
            program_info.get(b"name".as_slice()),
            Some(&Value::string(program))
        );
        let Some(Value::Object(version)) = program_info.get(b"version".as_slice()) else {
            return Err(io::Error::other("Expected version information"));
        };
        assert_eq!(
            version.get(b"string".as_slice()),
            Some(&Value::string("4.5.1"))
        );
        assert!(
            !fixture
                .command(&exe)
                .arg("--version=bad.txt")
                .output()?
                .status
                .success()
        );
        assert!(!fixture.0.join("bad.txt").exists());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn output_to_a_closed_pipe_does_not_panic() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let ctest = fixture.shim("ctest")?;
    fs::write(fixture.0.join("large"), vec![b'x'; 1024 * 1024])?;
    fs::write(
        fixture.0.join("CTestTestfile.cmake"),
        b"add_test(sample echo hello)\n",
    )?;
    for (exe, args) in [
        (Path::new(RCMAKE), &["--help"][..]),
        (Path::new(RCMAKE), &["-E", "cat", "large"][..]),
        (ctest.as_path(), &["-N"][..]),
    ] {
        let (reader, writer) = io::pipe()?;
        drop(reader);
        let output = fixture
            .command(exe)
            .args(args)
            .stdout(Stdio::from(writer))
            .output()?;
        assert_success(&output);
        assert!(
            output.stderr.is_empty(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn finds_data_in_a_relocated_install_through_a_shim() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let install = fixture.0.join("moved-install");
    let root = install.join("share/cmake-4.5");
    let modules = root.join("Modules");
    fs::create_dir_all(&modules)?;
    fs::write(modules.join("CMakeDetermineSystem.cmake"), b"")?;
    fs::write(
        modules.join("Relocated.cmake"),
        b"message(STATUS relocated)\n",
    )?;
    let bin = install.join("bin");
    fs::create_dir(&bin)?;
    let exe = bin.join("rcmake");
    fs::copy(RCMAKE, &exe)?;
    let shim = fixture.0.join("cmake");
    std::os::unix::fs::symlink(&exe, &shim)?;
    fs::write(fixture.0.join("read.cmake"), b"include(Relocated)\n")?;
    for path in [exe, shim] {
        let output = fixture
            .command(path)
            .env_remove("RCMAKE_VENDORED_DATA_ROOT")
            .args(["-P", "read.cmake"])
            .output()?;
        assert_success(&output);
        assert_eq!(output.stdout, b"-- relocated\n");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn recursive_removal_does_not_follow_directory_symlinks() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let outside = fixture.0.join("outside");
    let tree = fixture.0.join("tree");
    fs::create_dir(&outside)?;
    fs::create_dir(&tree)?;
    fs::write(outside.join("kept"), b"data")?;
    std::os::unix::fs::symlink(&outside, tree.join("nested"))?;
    std::os::unix::fs::symlink(&outside, fixture.0.join("direct"))?;
    fs::write(
        fixture.0.join("remove.cmake"),
        b"file(REMOVE_RECURSE tree direct/)\n",
    )?;
    let output = fixture
        .command(RCMAKE)
        .args(["-P", "remove.cmake"])
        .output()?;
    assert_success(&output);
    assert!(!tree.exists());
    assert!(fs::symlink_metadata(fixture.0.join("direct")).is_err());
    assert_eq!(fs::read(outside.join("kept"))?, b"data");
    Ok(())
}
