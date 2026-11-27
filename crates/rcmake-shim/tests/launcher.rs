//! End-to-end tests exercising the real compiled `rcmake-shim` launcher alongside `rcmake`.

#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods)]

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const SHIM_BIN: &str = env!("CARGO_BIN_EXE_rcmake-shim");
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    }
}

fn engine_binary() -> PathBuf {
    let candidate = Path::new(SHIM_BIN).with_file_name(exe_name("rcmake"));
    assert!(
        candidate.is_file(),
        "Expected compiled rcmake engine at {} (build it with `cargo build -p rcmake-cli --bin rcmake` or run `cargo test --workspace`).",
        candidate.display()
    );
    candidate
}

struct Stage {
    root: PathBuf,
    bin_dir: PathBuf,
    work_dir: PathBuf,
}

impl Stage {
    fn new() -> io::Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "rcmake-shim-int-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let bin_dir = root.join("bin");
        let work_dir = root.join("work");
        fs::create_dir_all(&bin_dir)?;
        fs::create_dir_all(&work_dir)?;
        Ok(Self {
            root,
            bin_dir,
            work_dir,
        })
    }

    fn install_engine(&self) -> io::Result<PathBuf> {
        let dest = self.bin_dir.join(exe_name("rcmake"));
        copy_executable(&engine_binary(), &dest)?;
        Ok(dest)
    }

    fn install_shim(&self, name: &str) -> io::Result<PathBuf> {
        let dest = self.bin_dir.join(exe_name(name));
        copy_executable(Path::new(SHIM_BIN), &dest)?;
        Ok(dest)
    }

    fn install_minimal_cmake_root(&self) -> io::Result<PathBuf> {
        let modules = self.root.join("share").join("cmake-4.5").join("Modules");
        fs::create_dir_all(&modules)?;
        for name in [
            "CMakeDetermineSystem.cmake",
            "CMakeSystemSpecificInitialize.cmake",
            "CMakeSystemSpecificInformation.cmake",
        ] {
            fs::write(modules.join(name), b"")?;
        }
        Ok(self.root.join("share").join("cmake-4.5"))
    }

    fn command(&self, exe: &Path) -> Command {
        let mut cmd = Command::new(exe);
        cmd.current_dir(&self.work_dir)
            .env_remove("RCMAKE_ORACLE_MODE")
            .env_remove("RCMAKE_VENDORED_DATA_ROOT")
            .env_remove("RCMAKE_EXEC_LOG");
        cmd
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn copy_executable(src: &Path, dst: &Path) -> io::Result<()> {
    fs::copy(src, dst)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(src)?.permissions().mode();
        fs::set_permissions(dst, fs::Permissions::from_mode(mode | 0o111))?;
    }
    Ok(())
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "status={}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn invalid_shim_basename_and_missing_engine_are_rejected() -> io::Result<()> {
    let stage = Stage::new()?;
    let bogus = stage.install_shim("evil-launcher")?;
    let out = stage.command(&bogus).arg("--version").output()?;
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unsupported launcher name"),
        "expected unsupported launcher error, got: {stderr}"
    );

    // Direct invocation of rcmake-shim itself must also be rejected.
    let out = stage
        .command(Path::new(SHIM_BIN))
        .arg("--version")
        .output()?;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unsupported launcher name"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Valid shim name without sibling rcmake engine must fail cleanly with exit 1.
    let cmake = stage.install_shim("cmake")?;
    let out = stage.command(&cmake).arg("--version").output()?;
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sibling engine"),
        "expected missing sibling engine error, got: {stderr}"
    );
    Ok(())
}

#[test]
fn version_banners_and_compat_persona_for_all_tools() -> io::Result<()> {
    let stage = Stage::new()?;
    stage.install_engine()?;
    let cmake = stage.install_shim("cmake")?;
    let ctest = stage.install_shim("ctest")?;
    let cpack = stage.install_shim("cpack")?;

    for (exe, expected_prefix) in [
        (&cmake, "cmake version "),
        (&ctest, "ctest version "),
        (&cpack, "cpack version "),
    ] {
        let out = stage.command(exe).arg("--version").output()?;
        assert_ok(&out);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.starts_with(expected_prefix) && stdout.to_ascii_lowercase().contains("rcmake"),
            "exe={} stdout={stdout}",
            exe.display()
        );
    }

    // Shimmed cmake runs in Compat mode: native-only flags are rejected.
    let out = stage
        .command(&cmake)
        .args(["--diagnostics=rich", "-S", ".", "-B", "out"])
        .output()?;
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Unknown argument --diagnostics=rich"),
        "shimmed cmake must run in Compat mode: {stderr}"
    );

    // Passing a partial internal shim flag to direct rcmake is NOT stripped.
    let rcmake = stage.bin_dir.join(exe_name("rcmake"));
    let out = stage
        .command(&rcmake)
        .arg("--rcmake-internal-shim-tool=cmake")
        .output()?;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--rcmake-internal-shim-tool=cmake"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // RCMAKE_ORACLE_MODE=1 forces Compat mode on direct rcmake too.
    let out = stage
        .command(&rcmake)
        .env("RCMAKE_ORACLE_MODE", "1")
        .args(["--diagnostics=rich", "-S", ".", "-B", "out"])
        .output()?;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Unknown argument --diagnostics=rich"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

#[test]
fn verbatim_arguments_exit_codes_and_argv0_preservation() -> io::Result<()> {
    let stage = Stage::new()?;
    stage.install_engine()?;
    let cmake = stage.install_shim("cmake")?;

    let script = stage.work_dir.join("args.cmake");
    fs::write(
        &script,
        r#"file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/argv0.txt" "${CMAKE_ARGV0}")
math(EXPR last "${CMAKE_ARGC} - 1")
set(out "")
foreach(i RANGE 0 ${last})
  string(APPEND out "${i}=<${CMAKE_ARGV${i}}>\n")
endforeach()
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/argv.txt" "${out}")
"#,
    )?;

    let weird_args = [
        "hello world",
        "café-日本語-🦀",
        "",
        "has \"double\" quotes",
        r"trailing\",
        r#"mixed \"quotes\" and trailing\\\"#,
        "--",
        "--rcmake-internal-shim-tool=ctest",
        "--rcmake-internal-shim-argv0",
        "spoofed",
    ];
    let mut cmd = stage.command(&cmake);
    cmd.arg("-P").arg(&script).arg("--");
    for arg in weird_args {
        cmd.arg(arg);
    }
    let out = cmd.output()?;
    assert_ok(&out);

    let argv0 = fs::read_to_string(stage.work_dir.join("argv0.txt"))?;
    let argv0_name = Path::new(&argv0)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("");
    assert!(
        argv0_name.eq_ignore_ascii_case(&exe_name("cmake")),
        "expected CMAKE_ARGV0 to preserve cmake shim name, got: {argv0:?}"
    );

    let recorded = fs::read_to_string(stage.work_dir.join("argv.txt"))?;
    // Index 0 = cmake shim, 1 = -P, 2 = script, 3 = --, 4.. = weird_args verbatim.
    for (idx, expected) in weird_args.iter().enumerate() {
        let needle = format!("{}=<{expected}>\n", idx + 4);
        assert!(
            recorded.contains(&needle),
            "missing verbatim arg {idx} ({expected:?}) in:\n{recorded}"
        );
    }

    // Exit code propagation (0, 1, 2, 42).
    let exit_script = stage.work_dir.join("exit.cmake");
    for code in [0, 1, 2, 42] {
        fs::write(&exit_script, format!("cmake_language(EXIT {code})\n"))?;
        let out = stage.command(&cmake).arg("-P").arg(&exit_script).output()?;
        assert_eq!(out.status.code(), Some(code), "exit code {code}");
    }
    Ok(())
}

#[test]
fn stdin_stdout_pipes_cmake_command_and_recursive_invocation() -> io::Result<()> {
    let stage = Stage::new()?;
    let rcmake = stage.install_engine()?;
    let cmake = stage.install_shim("cmake")?;
    let _ctest = stage.install_shim("ctest")?;
    let _cpack = stage.install_shim("cpack")?;
    stage.install_minimal_cmake_root()?;

    // Verify stdin/stdout piping via `-E cat -`.
    let payload = b"line one\nunicode \xc3\xa9\nfinal line\n";
    let mut child = stage
        .command(&cmake)
        .args(["-E", "cat", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload)?;
    }
    let out = child.wait_with_output()?;
    assert_ok(&out);
    assert_eq!(out.stdout, payload);

    // Verify CMAKE_COMMAND points to sibling cmake shim (NOT rcmake) and supports recursive -P.
    let child_script = stage.work_dir.join("child.cmake");
    fs::write(
        &child_script,
        "message(STATUS \"RECURSIVE_CMD=${CMAKE_COMMAND}\")\n",
    )?;
    let parent_script = stage.work_dir.join("parent.cmake");
    fs::write(
        &parent_script,
        r#"file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/vars.txt"
  "CMD=${CMAKE_COMMAND}\nCTEST=${CMAKE_CTEST_COMMAND}\nCPACK=${CMAKE_CPACK_COMMAND}\n")
execute_process(
  COMMAND "${CMAKE_COMMAND}" -P "${CMAKE_CURRENT_BINARY_DIR}/child.cmake"
  RESULT_VARIABLE rc
  OUTPUT_VARIABLE sub_out
  ERROR_VARIABLE sub_err
)
if(NOT rc EQUAL 0)
  message(FATAL_ERROR "recursive cmake -P failed (${rc}): ${sub_err}")
endif()
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/sub_out.txt" "${sub_out}")
"#,
    )?;

    let out = stage
        .command(&cmake)
        .arg("-P")
        .arg(&parent_script)
        .output()?;
    assert_ok(&out);

    let vars = fs::read_to_string(stage.work_dir.join("vars.txt"))?;
    let expected_bin = fs::canonicalize(&stage.bin_dir)?;
    let expected_cmake = expected_bin.join(exe_name("cmake"));
    let expected_ctest = expected_bin.join(exe_name("ctest"));
    let expected_cpack = expected_bin.join(exe_name("cpack"));

    let mut cmd_val = None;
    let mut ctest_val = None;
    let mut cpack_val = None;
    for line in vars.lines() {
        if let Some(v) = line.strip_prefix("CMD=") {
            cmd_val = Some(PathBuf::from(v));
        } else if let Some(v) = line.strip_prefix("CTEST=") {
            ctest_val = Some(PathBuf::from(v));
        } else if let Some(v) = line.strip_prefix("CPACK=") {
            cpack_val = Some(PathBuf::from(v));
        }
    }
    assert_eq!(
        fs::canonicalize(cmd_val.expect("CMD"))?,
        expected_cmake,
        "CMAKE_COMMAND should point to sibling cmake shim:\n{vars}"
    );
    assert_eq!(
        fs::canonicalize(ctest_val.expect("CTEST"))?,
        expected_ctest,
        "CMAKE_CTEST_COMMAND should point to sibling ctest shim:\n{vars}"
    );
    assert_eq!(
        fs::canonicalize(cpack_val.expect("CPACK"))?,
        expected_cpack,
        "CMAKE_CPACK_COMMAND should point to sibling cpack shim:\n{vars}"
    );

    let sub_out = fs::read_to_string(stage.work_dir.join("sub_out.txt"))?;
    assert!(
        sub_out.contains(&exe_name("cmake")),
        "expected recursive invocation to report cmake shim: {sub_out}"
    );

    // Direct native rcmake STILL keeps CMAKE_COMMAND pointing to rcmake.
    let native_script = stage.work_dir.join("native.cmake");
    fs::write(
        &native_script,
        "file(WRITE \"${CMAKE_CURRENT_BINARY_DIR}/native_cmd.txt\" \"${CMAKE_COMMAND}\")\n",
    )?;
    let out = stage
        .command(&rcmake)
        .arg("-P")
        .arg(&native_script)
        .output()?;
    assert_ok(&out);
    let native_cmd = PathBuf::from(fs::read_to_string(stage.work_dir.join("native_cmd.txt"))?);
    assert_eq!(
        fs::canonicalize(native_cmd)?,
        fs::canonicalize(&rcmake)?,
        "native rcmake must keep CMAKE_COMMAND pointing to rcmake"
    );
    Ok(())
}

#[test]
fn rcmake_lsp_shim_handles_jsonrpc_and_ignores_oracle_mode() -> io::Result<()> {
    let stage = Stage::new()?;
    stage.install_engine()?;
    let lsp = stage.install_shim("rcmake-lsp")?;

    let frame = |body: &str| format!("Content-Length: {}\r\n\r\n{body}", body.len());
    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#,
        r#"{"jsonrpc":"2.0","method":"exit"}"#,
    ]
    .into_iter()
    .map(frame)
    .collect::<String>();

    for oracle_mode in [false, true] {
        let mut cmd = stage.command(&lsp);
        if oracle_mode {
            cmd.env("RCMAKE_ORACLE_MODE", "1");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(requests.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        assert_ok(&out);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("\"capabilities\"") && stdout.contains("\"id\":1"),
            "oracle_mode={oracle_mode} missing initialize reply: {stdout}"
        );
        assert!(
            stdout.contains("\"id\":2") && stdout.contains("\"result\":null"),
            "oracle_mode={oracle_mode} missing shutdown reply: {stdout}"
        );
    }

    // Verify LSP startup CLI flags (`--version`) pass cleanly after `--`.
    let out = stage.command(&lsp).arg("--version").output()?;
    assert_ok(&out);
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("rcmake-lsp "),
        "stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    Ok(())
}
