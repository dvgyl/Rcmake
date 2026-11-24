use std::{path::Path, sync::Arc};

use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{Follow, Host, ProcessResult, PureHost, RunOutput, Stream, path};

const FILE: &str = "/work/libsample.dylib";
const SCRIPT: &str = "/work/install.cmake";

#[test]
fn file_rpath_edits_macho_through_the_host() -> std::result::Result<(), Box<dyn std::error::Error>>
{
    let host = Arc::new(PureHost::default());
    let mut interp = Interp::new(
        InterpConfig {
            host: host.clone(),
            mode: RunMode::Script,
            cmake_root: "/cmake".into(),
            cmake_command: "/cmake/bin/cmake".into(),
            cwd: "/work".into(),
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        super::super::register,
    );
    host.seed_file(
        Path::new(FILE),
        &macho(&["/build lib", "@loader_path/shared"]),
    )?;
    host.set_command_handler(|host, request, _| {
        let [command] = request.pipeline.as_slice() else {
            panic!("Expected one tool command.");
        };
        let (file, flags) = command.args.split_last().expect("Missing file argument.");
        assert_eq!(path::to_native(file), Path::new(FILE));
        assert_eq!(command.program, "/tools/install_name_tool");
        assert_eq!(
            flags,
            [
                "-delete_rpath",
                "/build lib",
                "-add_rpath",
                "@loader_path/install"
            ]
        );
        host.seed_file(
            Path::new(FILE),
            &macho(&["@loader_path/shared", "@loader_path/install"]),
        )?;
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    host.seed_file(
        Path::new(SCRIPT),
        br#"
file(RPATH_CHANGE FILE libsample.dylib
     OLD_RPATH "/build lib;@loader_path/shared"
     NEW_RPATH "@loader_path/install;@loader_path/shared"
     INSTALL_NAME_TOOL /tools/install_name_tool)
file(RPATH_CHECK FILE libsample.dylib RPATH "@loader_path/install;@loader_path/shared")
    "#,
    )?;
    assert_eq!(
        interp.run_script(Path::new(SCRIPT)).exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert!(host.metadata(Path::new(FILE), Follow::Yes).is_ok());
    host.set_command_handler(|host, request, _| {
        let [command] = request.pipeline.as_slice() else {
            panic!("Expected one tool command.");
        };
        let (file, flags) = command.args.split_last().expect("Missing file argument.");
        assert_eq!(path::to_native(file), Path::new(FILE));
        assert_eq!(command.program, "install_name_tool");
        assert_eq!(flags, ["-delete_rpath", "@loader_path/install"]);
        host.seed_file(Path::new(FILE), &macho(&["@loader_path/shared"]))?;
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    host.seed_file(
        Path::new(SCRIPT),
        br#"
file(RPATH_REMOVE FILE libsample.dylib OLD_RPATH "@loader_path/install")
file(RPATH_CHECK FILE libsample.dylib RPATH "@loader_path/shared")
    "#,
    )?;
    assert_eq!(
        interp.run_script(Path::new(SCRIPT)).exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert!(host.metadata(Path::new(FILE), Follow::Yes).is_ok());
    host.seed_file(
        Path::new(SCRIPT),
        b"file(RPATH_CHECK FILE libsample.dylib RPATH /bad)",
    )?;
    assert_eq!(interp.run_script(Path::new(SCRIPT)).exit_code, 0);
    assert!(host.metadata(Path::new(FILE), Follow::Yes).is_err());
    Ok(())
}

fn macho(paths: &[&str]) -> Vec<u8> {
    let mut commands = Vec::new();
    for path in paths {
        let size = (12 + path.len() + 1).next_multiple_of(8);
        commands.extend(super::LC_RPATH.to_le_bytes().into_iter().take(4));
        commands.extend((size as u32).to_le_bytes());
        commands.extend(12_u32.to_le_bytes());
        commands.extend(path.bytes());
        commands.resize(commands.len() + size - 12 - path.len(), 0);
    }
    let mut file = 0xfeed_facf_u32.to_le_bytes().to_vec();
    file.extend([0; 12]);
    file.extend((paths.len() as u32).to_le_bytes());
    file.extend((commands.len() as u32).to_le_bytes());
    file.extend([0; 8]);
    file.extend(commands);
    file
}
