//! Tiny dependency-free launcher shim for Windows release packaging.
//!
//! Copies of this executable are shipped as `cmake.exe`, `ctest.exe`, `cpack.exe`,
//! `ccmake.exe`, and `rcmake-lsp.exe` alongside the single full `rcmake.exe` engine
//! binary so release archives do not duplicate the main executable six times.

#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods, clippy::exit)]

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

const ALLOWED_TOOLS: [&str; 5] = ["cmake", "ctest", "cpack", "ccmake", "rcmake-lsp"];
const SHIM_TOOL_FLAG_PREFIX: &str = "--rcmake-internal-shim-tool=";
const SHIM_ARGV0_FLAG: &str = "--rcmake-internal-shim-argv0";

fn allowed_tool(path: &Path) -> Option<&'static str> {
    if !path
        .extension()
        .is_none_or(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    ALLOWED_TOOLS
        .into_iter()
        .find(|allowed| stem.eq_ignore_ascii_case(allowed))
}

fn sibling_engine(exe: &Path) -> Option<PathBuf> {
    let engine_name = if cfg!(windows) {
        "rcmake.exe"
    } else {
        "rcmake"
    };
    Some(exe.parent()?.join(engine_name))
}

fn write_stderr(message: &str) {
    let _ = std::io::stderr().write_all(message.as_bytes());
}

fn exit_code_from_status(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return 128_i32.saturating_add(signal);
        }
    }
    1
}

fn main() {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            write_stderr(&format!(
                "rcmake-shim: failed to locate current executable: {err}\n"
            ));
            std::process::exit(1);
        }
    };

    let Some(tool) = allowed_tool(&exe) else {
        let name = exe
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_else(|| exe.to_string_lossy());
        write_stderr(&format!(
            "rcmake-shim: unsupported launcher name '{name}'; expected cmake, ctest, cpack, ccmake, or rcmake-lsp\n"
        ));
        std::process::exit(1);
    };

    let Some(engine) = sibling_engine(&exe) else {
        write_stderr(&format!(
            "{tool}: failed to determine parent directory for '{}'\n",
            exe.display()
        ));
        std::process::exit(1);
    };

    if !std::fs::metadata(&engine).is_ok_and(|meta| meta.is_file()) {
        write_stderr(&format!(
            "{tool}: sibling engine '{}' was not found next to '{}'\n",
            engine.display(),
            exe.display()
        ));
        std::process::exit(1);
    }

    let mut raw_args = std::env::args_os();
    let orig_argv0: OsString = raw_args
        .next()
        .unwrap_or_else(|| exe.as_os_str().to_os_string());

    let mut tool_arg = String::with_capacity(SHIM_TOOL_FLAG_PREFIX.len() + tool.len());
    tool_arg.push_str(SHIM_TOOL_FLAG_PREFIX);
    tool_arg.push_str(tool);

    let mut command = Command::new(&engine);
    command
        .arg(tool_arg)
        .arg(SHIM_ARGV0_FLAG)
        .arg(orig_argv0)
        .arg("--")
        .args(raw_args);

    match command.status() {
        Ok(status) => std::process::exit(exit_code_from_status(status)),
        Err(err) => {
            write_stderr(&format!(
                "{tool}: failed to execute '{}': {err}\n",
                engine.display()
            ));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_accepts_expected_names_case_insensitively() {
        for (input, expected) in [
            ("cmake", Some("cmake")),
            ("cmake.exe", Some("cmake")),
            ("CMAKE.EXE", Some("cmake")),
            ("/opt/rcmake/bin/CTest.exe", Some("ctest")),
            ("cpack", Some("cpack")),
            ("CPACK.exe", Some("cpack")),
            ("ccmake", Some("ccmake")),
            ("CCMake.EXE", Some("ccmake")),
            ("/opt/rcmake/bin/ccmake.exe", Some("ccmake")),
            ("rcmake-lsp", Some("rcmake-lsp")),
            ("RCMAKE-LSP.EXE", Some("rcmake-lsp")),
        ] {
            assert_eq!(allowed_tool(Path::new(input)), expected, "{input}");
        }
    }

    #[test]
    fn allowlist_rejects_other_names_and_extensions() {
        for input in [
            "rcmake",
            "rcmake.exe",
            "rcmake-shim",
            "rcmake-shim.exe",
            "rcmake-verify.exe",
            "ccmake.com",
            "ccmake-gui.exe",
            "cmake-gui.exe",
            "cmake.bak",
            "cmake.exe.old",
            "evil",
            "",
        ] {
            assert_eq!(allowed_tool(Path::new(input)), None, "{input}");
        }
    }

    #[test]
    fn sibling_engine_uses_parent_directory() {
        let exe = Path::new("/install/bin/cmake.exe");
        let expected = if cfg!(windows) {
            PathBuf::from("/install/bin/rcmake.exe")
        } else {
            PathBuf::from("/install/bin/rcmake")
        };
        assert_eq!(sibling_engine(exe), Some(expected));
    }
}
