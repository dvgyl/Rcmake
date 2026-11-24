use std::path::Path;
use std::sync::{Arc, Mutex};

use rcmake_host::{Host, PureHost, RunOutput};

use super::*;

fn command(host: &PureHost, name: &[u8], args: &[&str]) -> i32 {
    let ctx = ToolCtx {
        host,
        cwd: Path::new("/work"),
        exe_display: b"cmake",
        capabilities: &|| rcmake_json::Value::Null,
    };
    run(
        name,
        &args.iter().copied().map(BString::from).collect::<Vec<_>>(),
        &ctx,
    )
}

fn exit(status: i32) -> RunOutput {
    RunOutput {
        results: vec![ProcessResult::Exited(status)],
        ..RunOutput::default()
    }
}

#[test]
fn expands_nmake_response_files_and_retains_cmake_response_files() {
    for (name, id) in [(b"vs_link_exe".as_slice(), "1"), (b"vs_link_dll", "2")] {
        let host = PureHost::default();
        host.env_set(b"Path", Some(b"C:/Existing Path"));
        host.seed_file(
            Path::new("/work/link args.rsp"),
            br#"/OUT:"target dir/main.exe" /INCREMENTAL:NO "/DKEY=\"with space\"" "C:\object dir\leaf\\" "my'file.obj" @child.rsp"#,
        )
        .unwrap();
        host.seed_file(Path::new("/work/child.rsp"), b"unused.obj")
            .unwrap();
        host.set_command_handler(move |_, request, _| {
            assert_eq!(request.pipeline[0].program, "C:/VC Tools/link.exe");
            assert_eq!(
                request.pipeline[0].args,
                [
                    "/OUT:target dir/main.exe",
                    "/INCREMENTAL:NO",
                    "/DKEY=\"with space\"",
                    "C:\\object dir\\leaf\\",
                    "my'file.obj",
                    "@child.rsp",
                    "@CMakeFiles/objects.rsp",
                    "@.o/first.rsp",
                    "@.o\\second.rsp",
                    &format!("/MANIFEST:EMBED,ID={id}"),
                    "/MANIFESTINPUT:extra file.manifest",
                ]
            );
            assert!(request.env.iter().any(|(key, value)| key == b"Path" && value == b"C:/Existing Path;C:/SDK Tools"));
            Ok(exit(0))
        }).unwrap();

        assert_eq!(
            command(
                &host,
                name,
                &[
                    "--intdir=obj dir",
                    "--msvc-ver=1944",
                    "--rc=C:/SDK Tools/rc.exe",
                    "--manifests",
                    "extra file.manifest",
                    "--",
                    "C:/VC Tools/link.exe",
                    "@missing.rsp",
                    "@link args.rsp",
                    "@CMakeFiles/objects.rsp",
                    "@.o/first.rsp",
                    "@.o\\second.rsp",
                ]
            ),
            0
        );
        assert_eq!(host.env_get(b"Path"), Some("C:/Existing Path".into()));
    }
}

#[test]
fn retains_manifest_no_and_adds_an_embed_option_for_explicit_input() {
    for explicit in [false, true] {
        let host = PureHost::default();
        host.set_command_handler(move |_, request, _| {
            let args = &request.pipeline[0].args;
            assert_eq!(&args[..3], ["/Fe", "target path/main.exe", "/MANIFEST:NO"]);
            if explicit {
                assert_eq!(
                    &args[3..],
                    ["/MANIFEST:EMBED,ID=1", "/MANIFESTINPUT:user.manifest"]
                );
            } else {
                assert_eq!(args.len(), 3);
            }
            Ok(exit(7))
        })
        .unwrap();
        let mut args = vec!["--intdir=obj"];
        if explicit {
            args.extend(["--manifests", "user.manifest"]);
        }
        if explicit {
            args.push("--");
        }
        args.extend(["link", "/Fe", "target path/main.exe", "/MANIFEST:NO"]);

        assert_eq!(command(&host, b"vs_link_exe", &args), -1);
        assert!(
            host.stream_bytes(Stream::Stdout)
                .contains_str(b"LINK: command \"link /Fe target path/main.exe /MANIFEST:NO")
        );
        assert!(host.stream_bytes(Stream::Stderr).is_empty());
    }
}

#[test]
fn updates_incremental_manifest_only_when_mt_reports_a_change() {
    for (status, expected) in [
        (0, vec!["rc", "link", "mt"]),
        (0xbb, vec!["rc", "link", "mt", "rc", "link"]),
    ] {
        let host = PureHost::default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&calls);
        host.set_command_handler(move |host, request, _| {
            let tool = &request.pipeline[0];
            log.lock().unwrap().push(tool.program.clone());
            match tool.program.as_slice() {
                b"rc" => {
                    assert_eq!(
                        tool.args,
                        ["/fo", "obj dir/manifest.res", "obj dir/manifest.rc"]
                    );
                    assert_eq!(
                        host.read(
                            Path::new("/work/obj dir/manifest.rc"),
                            super::super::read_all()
                        )?,
                        b"#pragma code_page(65001)\n2 24 \"/work/obj dir/embed.manifest\"\n"
                    );
                    host.seed_file(Path::new("/work/obj dir/manifest.res"), b"resource")?;
                }
                b"link" => {
                    assert!(
                        tool.args
                            .iter()
                            .any(|arg| arg == b"/MANIFESTFILE:obj dir/intermediate.manifest")
                    );
                    assert_eq!(tool.args.last().unwrap(), "obj dir/manifest.res");
                    host.seed_file(
                        Path::new("/work/obj dir/intermediate.manifest"),
                        b"manifest",
                    )?;
                }
                b"mt" => {
                    assert_eq!(
                        tool.args,
                        [
                            "/nologo",
                            "/manifest",
                            "obj dir/intermediate.manifest",
                            "/out:obj dir/embed.manifest",
                            "/notify_update"
                        ]
                    );
                    return Ok(exit(status));
                }
                _ => panic!("Unexpected tool."),
            }
            Ok(exit(0))
        })
        .unwrap();
        assert_eq!(
            command(
                &host,
                b"vs_link_dll",
                &[
                    "--intdir=obj dir",
                    "--",
                    "link",
                    "/INCREMENTAL",
                    "/out:demo.dll",
                ]
            ),
            0
        );
        assert_eq!(*calls.lock().unwrap(), expected);
    }
}

#[test]
fn propagates_each_incremental_tool_failure() {
    for (program, exit_code) in [("rc", 12), ("link", 23), ("mt", 34)] {
        let host = PureHost::default();
        host.set_command_handler(move |host, request, _| {
            assert_eq!(request.stderr, Output::SharedWithStdout);
            let tool = &request.pipeline[0];
            if tool.program == b"link" {
                host.seed_file(Path::new("/work/obj/intermediate.manifest"), b"manifest")?;
            }
            if tool.program == program {
                return Ok(RunOutput {
                    stderr: b"tool diagnostic\n".to_vec(),
                    ..exit(exit_code)
                });
            }
            Ok(exit(0))
        })
        .unwrap();
        assert_eq!(
            command(
                &host,
                b"vs_link_exe",
                &[
                    "--intdir=obj",
                    "--",
                    "link",
                    "/INCREMENTAL",
                    "/out:demo.exe",
                ]
            ),
            -1
        );
        let stdout = host.stream_bytes(Stream::Stdout);
        let code = if program == "mt" {
            format!("{exit_code:#x}")
        } else {
            exit_code.to_string()
        };
        assert!(stdout.contains_str(format!("failed (exit code {code})")));
        assert!(stdout.ends_with(b"tool diagnostic\n"));
        assert!(host.stream_bytes(Stream::Stderr).is_empty());
    }
}

#[test]
fn prints_successful_tool_output_only_in_verbose_mode() -> rcmake_host::HostResult<()> {
    for verbose in [false, true] {
        let host = PureHost::default();
        if verbose {
            host.env_set(b"VERBOSE", Some(b""));
        }
        host.set_command_handler(|_, _, _| {
            Ok(RunOutput {
                stdout: b"from stdout\n".to_vec(),
                stderr: b"from stderr\n".to_vec(),
                ..exit(0)
            })
        })?;
        assert_eq!(
            command(
                &host,
                b"vs_link_exe",
                &[
                    "--intdir=obj",
                    "--",
                    "link",
                    "/out:demo.exe",
                    "/MANIFEST:NO"
                ],
            ),
            0
        );
        let output = host.stream_bytes(Stream::Stdout);
        if verbose {
            assert_eq!(
                output,
                b"Visual Studio Non-Incremental Link\nLINK:\nlink /out:demo.exe /MANIFEST:NO\nfrom stdout\nfrom stderr\n",
            );
        } else {
            assert!(output.is_empty());
        }
        assert!(host.stream_bytes(Stream::Stderr).is_empty());
    }
    Ok(())
}

#[test]
fn uses_mt_with_older_linkers() {
    let host = PureHost::default();
    host.seed_file(Path::new("/work/wrapper.rsp"), br#"--msvc-ver=1600 "--intdir=obj dir" --manifests "extra file.manifest" -- link "/out:main file.exe""#).unwrap();
    host.set_command_handler(|host, request, _| {
        let tool = &request.pipeline[0];
        if tool.program == b"link" {
            assert_eq!(
                tool.args,
                [
                    "/out:main file.exe",
                    "/MANIFEST",
                    "/MANIFESTFILE:obj dir/intermediate.manifest"
                ]
            );
            host.seed_file(
                Path::new("/work/obj dir/intermediate.manifest"),
                b"manifest",
            )?;
        } else {
            assert_eq!(tool.program, "mt");
            assert_eq!(
                tool.args,
                [
                    "/nologo",
                    "/manifest",
                    "obj dir/intermediate.manifest",
                    "/manifest",
                    "extra file.manifest",
                    "/outputresource:main file.exe;#1"
                ]
            );
        }
        Ok(exit(0))
    })
    .unwrap();
    assert_eq!(command(&host, b"vs_link_exe", &["@wrapper.rsp"]), 0);
}
