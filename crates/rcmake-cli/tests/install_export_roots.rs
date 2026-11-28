//! Install export validation uses the top-level project directories.

#![cfg(unix)]
#![allow(clippy::expect_used)]

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use rcmake_host::{
    CommandSpec, Host, Input, NativeHost, Output, ProcessResult, RunOutput, RunRequest, WriteMode,
};

struct Fixture {
    host: NativeHost,
    root: PathBuf,
}

impl Fixture {
    fn run(&self, program: &str, arguments: &[&str]) -> RunOutput {
        self.host
            .run_bounded(
                &RunRequest {
                    pipeline: vec![CommandSpec {
                        program: program.into(),
                        args: arguments
                            .iter()
                            .map(|argument| (*argument).into())
                            .collect(),
                    }],
                    cwd: self.root.clone(),
                    env: self.host.env_all(),
                    stdin: Input::Null,
                    stdout: Output::Capture,
                    stderr: Output::Capture,
                    timeout: Some(Duration::from_secs(30)),
                    tee: false,
                    default_error_mode: false,
                },
                1024 * 1024,
            )
            .expect("run configure")
    }

    fn write(&self, path: &str, text: &[u8]) {
        self.host
            .write(&self.root.join(path), text, WriteMode::Truncate)
            .expect("write fixture");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.root);
    }
}

#[test]
fn install_export_uses_root_directories() {
    let host = NativeHost::new().expect("create native host");
    let root = std::env::temp_dir().join(format!("rcmake-export-roots-{}", std::process::id()));
    host.create_dir_all(&root.join("sub"))
        .expect("create project");
    let fixture = Fixture { host, root };
    fixture.write(
        "CMakeLists.txt",
        b"cmake_minimum_required(VERSION 4.5)\nproject(ExportRoots NONE)\nadd_subdirectory(sub)\n",
    );
    fixture.write(
        "sub/CMakeLists.txt",
        br#"
add_library(api INTERFACE)
target_include_directories(api INTERFACE "${export_include}")
install(TARGETS api EXPORT API)
install(EXPORT API DESTINATION lib/cmake/API)
"#,
    );

    let source = fixture.root.to_str().expect("UTF-8 fixture path");
    let program = env!("CARGO_BIN_EXE_rcmake");
    let build = fixture.root.join("build");
    let build = build.to_str().expect("UTF-8 build path");
    let source_input = format!(
        "-Dexport_include={}",
        fixture.root.join("source-sibling").display()
    );
    let output = fixture.run(
        program,
        &["-S", source, "-B", build, "-G", "Ninja", &source_input],
    );
    let error = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.results,
        [ProcessResult::Exited(1)],
        "{program} {error}"
    );
    assert!(
        error.contains("prefixed in the source directory"),
        "{error}"
    );
    assert!(error.contains("sub/CMakeLists.txt"), "{error}");

    let prefix = Path::new(build);
    let install_prefix = format!("-DCMAKE_INSTALL_PREFIX={}", prefix.display());
    let build_input = format!("-Dexport_include={}", prefix.join("include").display());
    let output = fixture.run(
        program,
        &["-S", source, "-B", build, &build_input, &install_prefix],
    );
    assert_eq!(
        output.results,
        [ProcessResult::Exited(0)],
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
