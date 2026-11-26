//! Open a generated IDE project in its associated application (`--open <dir>`).

#![forbid(unsafe_code)]

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest, Stream, path,
};
use rcmake_model::GeneratorKind;

use crate::configure::{is_dir, is_file};
use crate::persona::Persona;
use crate::{build, cache, resolve_cli_path};

fn error(host: &dyn Host, message: &str) -> i32 {
    host.write_out(Stream::Stderr, format!("Error: {message}\n").as_bytes());
    1
}

/// Implement `cmake --open <dir>` / `rcmake --open <dir>`.
pub(crate) fn run(args: &[BString], persona: Persona, host: &dyn Host) -> i32 {
    let mut dir = None;
    if let Some((first, extra)) = args.split_first() {
        if !first.is_empty() {
            dir = Some(first.as_slice());
        }
        for arg in extra {
            host.write_out(
                Stream::Stderr,
                format!("Unknown argument {arg}\n").as_bytes(),
            );
            dir = None;
        }
    }
    let Some(raw_dir) = dir else {
        let program = if persona == Persona::Native {
            "rcmake"
        } else {
            "cmake"
        };
        host.write_out(
            Stream::Stderr,
            format!("Usage: {program} --open <dir>\n").as_bytes(),
        );
        return 1;
    };

    let dir_path = resolve_cli_path(host, raw_dir);
    if !is_dir(host, &dir_path) {
        return error(
            host,
            &format!("{} is not a directory", path::from_native_cmake(&dir_path)),
        );
    }

    let cache_dir = build::cache_dir(host, &dir_path);
    if !host
        .metadata(&cache_dir.join("CMakeCache.txt"), Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return error(host, "not a CMake build directory (missing CMakeCache.txt)");
    }
    let Ok(cache) = cache::load(host, &cache_dir) else {
        return error(host, "not a CMake build directory (missing CMakeCache.txt)");
    };

    let Some(generator) = cache.get(b"CMAKE_GENERATOR") else {
        return error(host, "could not find CMAKE_GENERATOR in Cache");
    };
    let extra = cache
        .get(b"CMAKE_EXTRA_GENERATOR")
        .map(|entry| entry.value.as_slice())
        .unwrap_or_default();
    let full_name = if extra.is_empty() {
        generator.value.clone()
    } else {
        let mut name = BString::from(extra);
        name.extend_from_slice(b" - ");
        name.extend_from_slice(&generator.value);
        name
    };
    let Some(kind) = GeneratorKind::from_name(&full_name).filter(|kind| kind.host_available())
    else {
        return error(
            host,
            &format!("could not create CMAKE_GENERATOR \"{full_name}\""),
        );
    };

    let Some(project_name) = cache.get(b"CMAKE_PROJECT_NAME") else {
        return error(host, "could not find CMAKE_PROJECT_NAME in Cache");
    };

    match kind {
        GeneratorKind::Xcode => open_xcode(host, &dir_path, &project_name.value),
        GeneratorKind::VisualStudio(_) => open_visual_studio(host, &dir_path, &project_name.value),
        GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig | GeneratorKind::UnixMakefiles => {
            if persona == Persona::Native {
                error(
                    host,
                    &format!("generator \"{}\" does not support --open", generator.value),
                )
            } else {
                1
            }
        }
    }
}

fn with_suffix(name: &[u8], suffix: &[u8]) -> BString {
    let mut out = BString::from(name);
    out.extend_from_slice(suffix);
    out
}

fn open_xcode(host: &dyn Host, dir: &Path, project_name: &[u8]) -> i32 {
    if project_name.is_empty() {
        return 1;
    }
    let workspace = dir.join(path::to_native(&with_suffix(project_name, b".xcworkspace")));
    let project = dir.join(path::to_native(&with_suffix(project_name, b".xcodeproj")));
    let target = if is_dir(host, &workspace) {
        workspace
    } else if is_dir(host, &project) {
        project
    } else {
        return 1;
    };
    launch(host, dir, "open", vec![path::from_native(&target)])
}

fn open_visual_studio(host: &dyn Host, dir: &Path, project_name: &[u8]) -> i32 {
    if project_name.is_empty() {
        return 1;
    }
    let slnx = dir.join(path::to_native(&with_suffix(project_name, b".slnx")));
    let sln = dir.join(path::to_native(&with_suffix(project_name, b".sln")));
    let target = if is_file(host, &slnx) {
        slnx
    } else if is_file(host, &sln) {
        sln
    } else {
        return 1;
    };
    launch(
        host,
        dir,
        "cmd",
        vec![
            BString::from("/C"),
            BString::from("start"),
            BString::default(),
            path::from_native(&target),
        ],
    )
}

fn launch(host: &dyn Host, cwd: &Path, program: &str, args: Vec<BString>) -> i32 {
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: BString::from(program),
            args,
        }],
        cwd: cwd.to_path_buf(),
        env: host.env_all(),
        stdin: Input::Inherit,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: None,
        tee: true,
    };
    match host.run(&request, &mut |stream, bytes| {
        host.write_out(stream, bytes)
    }) {
        Ok(output) if output.results.first() == Some(&ProcessResult::Exited(0)) => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use rcmake_base::BString;
    use rcmake_host::{Host, PureHost, Stream};

    use crate::run;

    fn invoke(host: &Arc<PureHost>, args: &[&str]) -> i32 {
        let argv: Vec<_> = args.iter().copied().map(BString::from).collect();
        run(&argv, host.clone())
    }

    #[test]
    fn open_validates_arguments_and_cache_entries() {
        let host = Arc::new(PureHost::default());
        assert_eq!(invoke(&host, &["cmake", "--open"]), 1);
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            b"Usage: cmake --open <dir>\n"
        );

        let host = Arc::new(PureHost::default());
        assert_eq!(invoke(&host, &["rcmake", "--open"]), 1);
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            b"Usage: rcmake --open <dir>\n"
        );

        let host = Arc::new(PureHost::default());
        assert_eq!(invoke(&host, &["cmake", "--open", "build", "--extra"]), 1);
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            b"Unknown argument --extra\nUsage: cmake --open <dir>\n"
        );

        let host = Arc::new(PureHost::default());
        assert_eq!(invoke(&host, &["cmake", "--open", "missing"]), 1);
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            b"Error: /work/missing is not a directory\n"
        );

        let host = Arc::new(PureHost::default());
        host.create_dir_all(Path::new("/work/empty")).unwrap();
        assert_eq!(invoke(&host, &["cmake", "--open", "empty"]), 1);
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            b"Error: not a CMake build directory (missing CMakeCache.txt)\n"
        );

        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/no_gen/CMakeCache.txt"),
            b"CMAKE_PROJECT_NAME:STATIC=Demo\n",
        )
        .unwrap();
        assert_eq!(invoke(&host, &["cmake", "--open", "no_gen"]), 1);
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            b"Error: could not find CMAKE_GENERATOR in Cache\n"
        );

        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/bad_gen/CMakeCache.txt"),
            b"CMAKE_GENERATOR:INTERNAL=Bogus\nCMAKE_PROJECT_NAME:STATIC=Demo\n",
        )
        .unwrap();
        assert_eq!(invoke(&host, &["cmake", "--open", "bad_gen"]), 1);
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            b"Error: could not create CMAKE_GENERATOR \"Bogus\"\n"
        );

        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/no_proj/CMakeCache.txt"),
            b"CMAKE_GENERATOR:INTERNAL=Ninja\n",
        )
        .unwrap();
        assert_eq!(invoke(&host, &["cmake", "--open", "no_proj"]), 1);
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            b"Error: could not find CMAKE_PROJECT_NAME in Cache\n"
        );
    }

    #[test]
    fn open_rejects_non_ide_generators_with_persona_appropriate_output() {
        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/build/CMakeCache.txt"),
            b"CMAKE_GENERATOR:INTERNAL=Ninja\nCMAKE_PROJECT_NAME:STATIC=Demo\n",
        )
        .unwrap();
        assert_eq!(invoke(&host, &["cmake", "--open", "build"]), 1);
        assert!(host.stream_bytes(Stream::Stderr).is_empty());

        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/build/CMakeCache.txt"),
            b"CMAKE_GENERATOR:INTERNAL=Ninja\nCMAKE_PROJECT_NAME:STATIC=Demo\n",
        )
        .unwrap();
        assert_eq!(invoke(&host, &["rcmake", "--open", "build"]), 1);
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            b"Error: generator \"Ninja\" does not support --open\n"
        );
    }

    #[cfg(any(target_os = "macos", feature = "xcode-cross"))]
    #[test]
    fn open_launches_xcode_project_and_prefers_workspace() {
        use std::sync::Mutex;

        use rcmake_host::{ProcessResult, RunOutput};

        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/build/CMakeCache.txt"),
            b"CMAKE_GENERATOR:INTERNAL=Xcode\nCMAKE_PROJECT_NAME:STATIC=Demo\n",
        )
        .unwrap();
        host.create_dir_all(Path::new("/work/build/Demo.xcodeproj"))
            .unwrap();
        let seen = Arc::new(Mutex::new(Vec::<Vec<BString>>::new()));
        let seen_clone = Arc::clone(&seen);
        host.set_command_handler(move |_, request, _| {
            let mut cmd = vec![request.pipeline[0].program.clone()];
            cmd.extend(request.pipeline[0].args.iter().cloned());
            seen_clone.lock().unwrap().push(cmd);
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        })
        .unwrap();

        assert_eq!(invoke(&host, &["cmake", "--open", "build"]), 0);
        host.create_dir_all(Path::new("/work/build/Demo.xcworkspace"))
            .unwrap();
        assert_eq!(invoke(&host, &["cmake", "--open", "build"]), 0);

        let recorded = seen.lock().unwrap();
        assert_eq!(
            recorded.as_slice(),
            [
                vec![BString::from("open"), BString::from("/work/build/Demo.xcodeproj")],
                vec![BString::from("open"), BString::from("/work/build/Demo.xcworkspace")],
            ]
        );
    }

    #[cfg(any(windows, feature = "vs-cross"))]
    #[test]
    fn open_launches_visual_studio_solution_and_prefers_slnx() {
        use std::sync::Mutex;

        use rcmake_host::{ProcessResult, RunOutput};

        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/build/CMakeCache.txt"),
            b"CMAKE_GENERATOR:INTERNAL=Visual Studio 17 2022\nCMAKE_PROJECT_NAME:STATIC=Demo\n",
        )
        .unwrap();
        host.seed_file(Path::new("/work/build/Demo.sln"), b"")
            .unwrap();
        let seen = Arc::new(Mutex::new(Vec::<Vec<BString>>::new()));
        let seen_clone = Arc::clone(&seen);
        host.set_command_handler(move |_, request, _| {
            let mut cmd = vec![request.pipeline[0].program.clone()];
            cmd.extend(request.pipeline[0].args.iter().cloned());
            seen_clone.lock().unwrap().push(cmd);
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        })
        .unwrap();

        assert_eq!(invoke(&host, &["cmake", "--open", "build"]), 0);
        host.seed_file(Path::new("/work/build/Demo.slnx"), b"")
            .unwrap();
        assert_eq!(invoke(&host, &["cmake", "--open", "build"]), 0);

        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0][0], "cmd");
        assert!(recorded[0][4].to_string().ends_with("Demo.sln"));
        assert!(recorded[1][4].to_string().ends_with("Demo.slnx"));
    }
}
