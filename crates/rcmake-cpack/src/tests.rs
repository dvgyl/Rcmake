use std::sync::Arc;

use rcmake_base::{BString, ByteSlice};
use rcmake_host::{Host, PureHost, Stream};

use crate::archive_gen::ArchiveKind;
use crate::generators;

#[test]
fn registry_knows_milestone_three_generators() {
    for kind in ArchiveKind::ALL {
        let generator = generators::create(kind.name().as_bytes());
        assert_eq!(generator.map(|g| g.extension()), Some(kind.extension()));
    }
    assert_eq!(
        generators::create(b"DragNDrop").map(|g| g.name()),
        Some("DragNDrop")
    );
    assert_eq!(
        generators::create(b"Bundle").map(|g| g.name()),
        Some("Bundle")
    );
    assert_eq!(
        generators::create(b"External").map(|g| g.extension()),
        Some(".json")
    );
    assert!(generators::create(b"DEB").is_none());
}

#[test]
fn main_requires_a_generator_or_configuration() {
    let host = Arc::new(PureHost::new("/work".into(), "/bin/cpack".into()));
    let shared: Arc<dyn Host> = host.clone();
    let code = crate::main(&[BString::from("cpack")], shared);
    assert_eq!(code, 1);
    let stderr = host.stream_bytes(Stream::Stderr);
    assert!(stderr.starts_with(b"CPack Error: "));
}

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_archive::{ExtractOptions, extract_archive, list_command_archive};
use rcmake_base::hash::HashAlgo;
use rcmake_host::path::from_native;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Input, NativeHost, Output, ProcessResult, ReadRange, RunOutput,
    RunRequest, WriteMode,
};

fn memory_host() -> Arc<PureHost> {
    Arc::new(PureHost::new("/work".into(), "/bin/cpack".into()))
}

fn invoke(host: Arc<dyn Host>, args: &[&str]) -> i32 {
    let argv = std::iter::once("cpack")
        .chain(args.iter().copied())
        .map(BString::from)
        .collect::<Vec<_>>();
    crate::main(&argv, host)
}

fn write(host: &dyn Host, path: impl AsRef<Path>, bytes: &[u8]) {
    let path = path.as_ref();
    host.create_dir_all(path.parent().unwrap()).unwrap();
    host.write(path, bytes, WriteMode::Truncate).unwrap();
}

fn read(host: &dyn Host, path: &Path) -> Vec<u8> {
    host.read(
        path,
        ReadRange::All {
            limit: 20 * 1024 * 1024,
        },
    )
    .unwrap()
}

#[test]
fn cli_reports_oracle_diagnostics() {
    for (args, message) in [
        (
            &["--verbos"][..],
            "CPack Error: Unknown argument: --verbos. Did you mean: --verbose?\n",
        ),
        (&["-G"][..], "CMake Error: Invalid value used with -G\n"),
        (
            &["--preset"][..],
            "CMake Error: No preset specified for --preset\n",
        ),
        (
            &["-D", "name"][..],
            "CPack Error: Please specify CPack definitions as: KEY=VALUE\n",
        ),
        (
            &["--config", "missing.cmake"][..],
            "CPack Error: Cannot find CPack config file: \"/work/missing.cmake\"\n",
        ),
    ] {
        let host = memory_host();
        assert_eq!(invoke(host.clone(), args), 1);
        assert_eq!(host.stream_bytes(Stream::Stderr), message.as_bytes());
    }
}

#[test]
fn config_precedence_and_generator_hooks() {
    let host = memory_host();
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        br#"
if(NOT CPACK_BUILD_CONFIG STREQUAL "Debug")
  message(FATAL_ERROR "Build configuration was unavailable")
endif()
set(CPACK_GENERATOR "invalid")
set(CPACK_PACKAGE_NAME "config")
set(CPACK_PACKAGE_VERSION "1")
set(CPACK_PACKAGE_DIRECTORY "unused")
set(CPACK_PROJECT_CONFIG_FILE "per-generator.cmake")
set(CPACK_INSTALL_SCRIPTS "install.cmake")
set(CPACK_PRE_BUILD_SCRIPTS "pre.cmake")
set(CPACK_POST_BUILD_SCRIPTS "post.cmake")
set(CPACK_PACKAGE_CHECKSUM "MD5;SHA256")
"#,
    );
    write(host.as_ref(), "/work/per-generator.cmake", br#"
if(NOT CPACK_PACKAGE_NAME STREQUAL "final" OR NOT CPACK_PACKAGE_VERSION STREQUAL "2" OR NOT CPACK_PACKAGE_VENDOR STREQUAL "Vendor")
  message(FATAL_ERROR "CPack overrides were not applied")
endif()
if(DEFINED CPACK_PREVIOUS_GENERATOR)
  message(FATAL_ERROR "Generator variables leaked")
endif()
set(CPACK_PREVIOUS_GENERATOR "${CPACK_GENERATOR}")
set(CPACK_PACKAGE_FILE_NAME "${CPACK_GENERATOR}-${CPACK_PACKAGE_NAME}")
"#);
    write(
        host.as_ref(),
        "/work/install.cmake",
        br#"
file(WRITE "${CMAKE_INSTALL_PREFIX}/hook.txt" "installed")
"#,
    );
    write(
        host.as_ref(),
        "/work/pre.cmake",
        br#"
file(APPEND "${CPACK_TEMPORARY_INSTALL_DIRECTORY}/hook.txt" " and staged")
"#,
    );
    write(
        host.as_ref(),
        "/work/post.cmake",
        br#"
foreach(package IN LISTS CPACK_PACKAGE_FILES)
  if(NOT EXISTS "${package}")
    message(FATAL_ERROR "Package unavailable to post-build script")
  endif()
endforeach()
"#,
    );
    write(
        host.as_ref(),
        "/work/output/_CPack_Packages/TAR/TAR-final/stale",
        b"stale",
    );
    assert_eq!(
        invoke(
            host.clone(),
            &[
                "-G",
                "TAR;ZIP_STORE",
                "-C",
                "Debug",
                "-P",
                "cli",
                "-DCPACK_PACKAGE_NAME=final",
                "-R",
                "2",
                "--vendor",
                "Vendor",
                "-B",
                "output",
            ]
        ),
        0,
        "{}",
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
    );
    for (file, root) in [
        ("TAR-final.tar", "TAR-final"),
        ("ZIP_STORE-final.zip", "ZIP_STORE-final"),
    ] {
        let package = Path::new("/work/output").join(file);
        let extracted = Path::new("/work/extracted").join(root);
        let entries = list_command_archive(host.as_ref(), &package, Vec::new(), None).unwrap();
        assert_eq!(entries, [BString::from(format!("{root}/hook.txt"))]);
        extract(host.as_ref(), &package, &extracted);
        assert_eq!(
            read(host.as_ref(), &extracted.join(root).join("hook.txt")),
            b"installed and staged"
        );
        check_checksums(host.as_ref(), &package);
    }
}

#[test]
fn archive_applies_only_valid_source_date_epochs() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_host::FileTime;

    let original_mtime = FileTime {
        secs: 1000,
        nanos: 0,
    };
    for (value, expected) in [
        ("", original_mtime.secs),
        ("invalid", original_mtime.secs),
        ("123 ", original_mtime.secs),
        ("9223372036854775808", original_mtime.secs),
        (" \t+123", 123),
        ("-0", 0),
    ] {
        let host = memory_host();
        let input = Path::new("/work/source/file");
        write(host.as_ref(), input, b"contents");
        host.set_times(input, None, Some(original_mtime))?;
        host.env_set(b"SOURCE_DATE_EPOCH", Some(value.as_bytes()));
        write(
            host.as_ref(),
            "/work/CPackConfig.cmake",
            br#"
set(CPACK_GENERATOR TGZ)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_INSTALLED_DIRECTORIES "/work/source;/")
"#,
        );
        assert_eq!(invoke(host.clone(), &[]), 0, "SOURCE_DATE_EPOCH={value:?}");
        let package = Path::new("/work/demo.tar.gz");
        let archive = read(host.as_ref(), package);
        assert_eq!(archive.get(4..8), Some([0; 4].as_slice()));
        let destination = Path::new("/work/extracted");
        extract(host.as_ref(), package, destination);
        let metadata = host.metadata(&destination.join("demo/file"), Follow::No)?;
        assert_eq!(metadata.mtime.secs, expected, "SOURCE_DATE_EPOCH={value:?}");
    }
    Ok(())
}

#[test]
fn component_merge_compares_bytes_beyond_the_first_chunk() -> Result<(), crate::CpackError> {
    let host = memory_host();
    let source = Path::new("/work/component");
    let destination = Path::new("/work/merged");
    let mut contents = vec![b'a'; 1024 * 1024 + 1];
    write(host.as_ref(), source.join("file"), &contents);
    write(host.as_ref(), destination.join("file"), &contents);
    crate::source::merge_component(host.as_ref(), source, destination)?;

    contents.pop();
    contents.push(b'b');
    write(host.as_ref(), source.join("file"), &contents);
    assert!(crate::source::merge_component(host.as_ref(), source, destination).is_err());
    Ok(())
}

#[test]
fn source_package_filters_paths_and_preserves_links() {
    let host = memory_host();
    write(host.as_ref(), "/work/source/keep", b"data");
    write(
        host.as_ref(),
        "/work/source/ignored/file",
        b"skip directory",
    );
    write(host.as_ref(), "/work/source/temp.bak", b"skip file");
    host.set_permissions(Path::new("/work/source/keep"), 0o751)
        .unwrap();
    host.symlink(Path::new("keep"), Path::new("/work/source/link"))
        .unwrap();
    write(
        host.as_ref(),
        "/work/CPackSourceConfig.cmake",
        br#"
set(CPACK_GENERATOR TAR)
set(CPACK_PACKAGE_NAME source)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME source-1)
set(CPACK_INSTALLED_DIRECTORIES "/work/source;/")
set(CPACK_SOURCE_IGNORE_FILES [[/ignored/$;[.]bak$]])
set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY OFF)
set(CPACK_PACKAGE_DIRECTORY "/work/source/out")
"#,
    );
    assert_eq!(
        invoke(host.clone(), &["--config", "CPackSourceConfig.cmake"]),
        0
    );
    let tree = extract(
        host.as_ref(),
        Path::new("/work/source/out/source-1.tar"),
        Path::new("/work/unpacked"),
    );
    assert_eq!(
        tree.keys().map(String::as_str).collect::<Vec<_>>(),
        ["keep", "link"]
    );
    assert_eq!(tree["keep"].mode, 0o751);
    assert_eq!(tree["link"].target.as_deref(), Some(Path::new("keep")));
}

#[test]
fn component_groups_select_install_components() {
    let host = memory_host();
    write(
        host.as_ref(),
        "/work/build/cmake_install.cmake",
        br#"
file(WRITE "${CMAKE_INSTALL_PREFIX}/${CMAKE_INSTALL_COMPONENT}.txt" "${CMAKE_INSTALL_CONFIG_NAME}")
"#,
    );
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        br#"
set(CPACK_GENERATOR TAR)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;demo;ALL;/")
set(CPACK_ARCHIVE_COMPONENT_INSTALL ON)
set(CPACK_COMPONENTS_ALL "api;bin;docs")
set(CPACK_COMPONENT_API_GROUP Tools)
set(CPACK_COMPONENT_BIN_GROUP Tools)
set(CPACK_COMPONENT_INCLUDE_TOPLEVEL_DIRECTORY ON)
"#,
    );
    for (mode, expected) in [
        (
            "ONE_PER_GROUP",
            vec![
                ("demo-Tools.tar", vec!["api", "bin"]),
                ("demo-docs.tar", vec!["docs"]),
            ],
        ),
        (
            "IGNORE",
            vec![
                ("demo-api.tar", vec!["api"]),
                ("demo-bin.tar", vec!["bin"]),
                ("demo-docs.tar", vec!["docs"]),
            ],
        ),
        (
            "ALL_COMPONENTS_IN_ONE",
            vec![("demo.tar", vec!["api", "bin", "docs"])],
        ),
    ] {
        let dir = format!("/work/{mode}");
        let setting = format!("CPACK_COMPONENTS_GROUPING={mode}");
        assert_eq!(
            invoke(host.clone(), &["-B", &dir, "-C", "Release", "-D", &setting]),
            0
        );
        for (archive, components) in expected {
            let extracted = Path::new(&dir).join("unpacked").join(archive);
            let tree = extract(host.as_ref(), &Path::new(&dir).join(archive), &extracted);
            let names = components
                .iter()
                .map(|name| format!("demo/{name}.txt"))
                .collect::<Vec<_>>();
            assert_eq!(tree.keys().cloned().collect::<Vec<_>>(), names);
            for name in names {
                assert_eq!(read(host.as_ref(), &extracted.join(name)), b"Release");
            }
        }
    }
}

#[test]
fn package_presets_use_binary_directory_and_cli_overrides() {
    let host = memory_host();
    write(host.as_ref(), "/work/CMakePresets.json", br#"{
        "version": 13,
        "configurePresets": [{"name": "build", "generator": "Ninja", "binaryDir": "${sourceDir}/out"}],
        "packagePresets": [
            {"name": "base", "hidden": true, "generators": ["TAR"]},
            {"name": "release", "displayName": "Release package", "inherits": "base", "configurePreset": "build",
             "variables": {"CPACK_PACKAGE_FILE_NAME": "preset"}, "environment": {"PACKAGE_ENV": "from-preset"}}
        ]
    }"#);
    write(
        host.as_ref(),
        "/work/out/CPackConfig.cmake",
        br#"
if(NOT "$ENV{PACKAGE_ENV}" STREQUAL "from-preset")
  message(FATAL_ERROR "Preset environment was unavailable")
endif()
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_INSTALLED_DIRECTORIES "/work/input;/")
"#,
    );
    write(host.as_ref(), "/work/input/content", b"data");
    assert_eq!(invoke(host.clone(), &["--list-presets"]), 0);
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        b"Available package presets:\n\n  \"release\" - Release package\n"
    );
    assert_eq!(
        invoke(
            host.clone(),
            &["--preset=release", "-DCPACK_PACKAGE_FILE_NAME=cli"]
        ),
        0
    );
    assert!(
        host.metadata(Path::new("/work/out/cli.tar"), Follow::Yes)
            .is_ok()
    );
    assert_eq!(host.env_get(b"PACKAGE_ENV"), None);
}

#[test]
fn makefile_preinstall_runs_for_each_selected_configuration() {
    let host = memory_host();
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        br#"
set(CPACK_GENERATOR TAR)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_CMAKE_GENERATOR "Unix Makefiles")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;demo;ALL;/")
set(CMAKE_COMMAND "/other/bin/cmake")
"#,
    );
    write(host.as_ref(), "/work/build/CMakeCache.txt", b"");
    write(
        host.as_ref(),
        "/work/build/cmake_install.cmake",
        br#"
file(WRITE "${CMAKE_INSTALL_PREFIX}/${CMAKE_INSTALL_CONFIG_NAME}" "staged")
"#,
    );
    host.set_command_handler(|host, request, _| {
        let command = request.pipeline.first().unwrap();
        assert_eq!(command.program, "/bin/cmake");
        assert_eq!(request.timeout, None);
        assert_eq!(
            &command.args[..5],
            [
                "--build",
                "/work/build",
                "--target",
                "preinstall",
                "--config"
            ]
        );
        let marker = Path::new("/work").join(command.args.last().unwrap().to_str().unwrap());
        host.write(&marker, b"preinstall", WriteMode::CreateNew)?;
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..Default::default()
        })
    })
    .unwrap();
    assert_eq!(invoke(host.clone(), &["-C", "Debug;Release;Debug"]), 0);
    assert_eq!(read(host.as_ref(), Path::new("/work/Debug")), b"preinstall");
    assert_eq!(
        read(host.as_ref(), Path::new("/work/Release")),
        b"preinstall"
    );
    assert_eq!(
        invoke(host.clone(), &["-C", "Debug", "-DCPACK_SKIP_PREINSTALL=ON"]),
        0
    );
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    mode: u32,
    digest: String,
    target: Option<PathBuf>,
}

fn extract(host: &dyn Host, package: &Path, destination: &Path) -> BTreeMap<String, Entry> {
    let names = extract_archive(
        host,
        package,
        &ExtractOptions {
            destination: destination.to_path_buf(),
            ..Default::default()
        },
    )
    .unwrap_or_else(|err| panic!("{}: {err}", package.display()));
    let mut entries = BTreeMap::new();
    for name in names {
        let name = name.to_str_lossy().into_owned();
        let path = destination.join(&name);
        if path
            .file_name()
            .is_some_and(|part| part.as_encoded_bytes().starts_with(b"._"))
        {
            continue;
        }
        let meta = host.metadata(&path, Follow::No).unwrap();
        let (mode, digest, target) = match meta.kind {
            FileKind::File => (
                meta.mode & 0o777,
                HashAlgo::Sha256.hex_digest(&read(host, &path)),
                None,
            ),
            FileKind::Symlink => (
                meta.mode & 0o777,
                String::new(),
                Some(host.read_link(&path).unwrap()),
            ),
            _ => continue,
        };
        entries.insert(
            name,
            Entry {
                mode,
                digest,
                target,
            },
        );
    }
    entries
}

fn directory_modes(host: &dyn Host, root: &Path) -> BTreeMap<PathBuf, u32> {
    let mut dirs = BTreeMap::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let current = root.join(&relative);
        for entry in host.read_dir(&current).unwrap() {
            if entry.kind == FileKind::Dir {
                let child = relative.join(rcmake_host::path::to_native(&entry.name));
                let mode = host.metadata(&root.join(&child), Follow::No).unwrap().mode & 0o777;
                dirs.insert(child.clone(), mode);
                pending.push(child);
            }
        }
    }
    dirs
}

fn check_checksums(host: &dyn Host, package: &Path) {
    let bytes = read(host, package);
    for (extension, algorithm) in [("md5", HashAlgo::Md5), ("sha256", HashAlgo::Sha256)] {
        let name = package.file_name().unwrap().to_str().unwrap();
        let checksum = package.with_file_name(format!("{name}.{extension}"));
        let expected = format!("{}  {name}\n", algorithm.hex_digest(&bytes));
        assert_eq!(read(host, &checksum), expected.as_bytes());
    }
}

fn run(host: &dyn Host, program: BString, cwd: &Path, args: Vec<BString>) {
    let output = host
        .run(
            &RunRequest {
                pipeline: vec![CommandSpec { program, args }],
                cwd: cwd.to_owned(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::SharedWithStdout,
                timeout: Some(Duration::from_secs(90)),
                tee: false,
            },
            &mut |_, _| {},
        )
        .unwrap();
    assert_eq!(
        output.results,
        [ProcessResult::Exited(0)],
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

struct TempDirectory<'a> {
    host: &'a dyn Host,
    path: PathBuf,
}

impl Drop for TempDirectory<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.path);
    }
}

#[test]
fn archives_match_cmake_4_5_1() {
    let host: Arc<dyn Host> = Arc::new(NativeHost::new().unwrap());
    let Some(cmake) = host.env_get(b"RCMAKE_TEST_REFERENCE_CMAKE") else {
        return;
    };
    let Some(cpack) = host.env_get(b"RCMAKE_TEST_REFERENCE_CPACK") else {
        return;
    };
    let temp = TempDirectory {
        host: host.as_ref(),
        path: std::env::temp_dir().join(format!("rcmake-cpack-{}", host.system().pid)),
    };
    let root = &temp.path;
    let project = root.join("project");
    let build = root.join("build");
    write(
        host.as_ref(),
        project.join("CMakeLists.txt"),
        br#"
cmake_minimum_required(VERSION 4.5)
project(CpackDifferential VERSION 1.2.3 LANGUAGES C)
add_executable(demo main.c)
install(TARGETS demo DESTINATION bin)
install(FILES README DESTINATION share/demo)
install(CODE [[file(CREATE_LINK "demo" "${CMAKE_INSTALL_PREFIX}/bin/demo-link" SYMBOLIC)]])
set(CPACK_PACKAGE_CHECKSUM "MD5;SHA256")
include(CPack)
"#,
    );
    write(
        host.as_ref(),
        project.join("main.c"),
        b"int main(void) { return 0; }\n",
    );
    write(
        host.as_ref(),
        project.join("README"),
        b"CPack differential test\n",
    );
    run(
        host.as_ref(),
        cmake.clone(),
        root,
        vec![
            "-S".into(),
            from_native(&project),
            "-B".into(),
            from_native(&build),
            "-G".into(),
            "Ninja".into(),
            "-DCMAKE_BUILD_TYPE=Release".into(),
        ],
    );
    run(
        host.as_ref(),
        cmake,
        root,
        vec!["--build".into(), from_native(&build)],
    );
    let kinds = "TGZ;TXZ;TZST;TBZ2;TZ;TAR;ZIP";
    let config = from_native(&build.join("CPackConfig.cmake"));
    let oracle = root.join("oracle");
    let native = root.join("native");
    run(
        host.as_ref(),
        cpack,
        &build,
        vec![
            "--config".into(),
            config.clone(),
            "-G".into(),
            kinds.into(),
            "-B".into(),
            from_native(&oracle),
        ],
    );
    let args = [
        "--config",
        config.to_str().unwrap(),
        "-G",
        kinds,
        "-B",
        native.to_str().unwrap(),
    ];
    assert_eq!(invoke(host.clone(), &args), 0);
    let file_names = |dir: &Path| {
        host.read_dir(dir)
            .unwrap()
            .into_iter()
            .filter(|entry| entry.kind == FileKind::File)
            .map(|entry| entry.name)
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(file_names(&native), file_names(&oracle));
    for kind in ArchiveKind::ALL.into_iter().take(7) {
        let name = file_names(&oracle)
            .into_iter()
            .find(|name| name.ends_with(kind.extension().as_bytes()))
            .unwrap();
        let name = name.to_str().unwrap();
        let left = oracle.join(name);
        let right = native.join(name);
        check_checksums(host.as_ref(), &left);
        check_checksums(host.as_ref(), &right);
        let oracle_dir = root.join("oracle-tree").join(kind.name());
        let native_dir = root.join("native-tree").join(kind.name());
        let oracle_tree = extract(host.as_ref(), &left, &oracle_dir);
        let native_tree = extract(host.as_ref(), &right, &native_dir);
        assert_eq!(
            directory_modes(host.as_ref(), &oracle_dir),
            directory_modes(host.as_ref(), &native_dir),
            "{}",
            kind.name()
        );
        assert_eq!(
            oracle_tree.keys().collect::<Vec<_>>(),
            native_tree.keys().collect::<Vec<_>>(),
            "{}",
            kind.name()
        );
        for (name, expected) in oracle_tree {
            assert_eq!(expected, native_tree[&name], "{} {name}", kind.name());
        }
    }
}
