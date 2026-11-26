use super::*;

fn stderr(host: &PureHost) -> String {
    String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned()
}

fn file_names(tree: &BTreeMap<String, Entry>) -> Vec<&str> {
    tree.keys().map(String::as_str).collect()
}

fn kind(host: &dyn Host, path: &Path) -> Option<FileKind> {
    host.metadata(path, Follow::No).ok().map(|meta| meta.kind)
}

#[test]
fn package_preset_config_file_expands_environment_macros() {
    let host = memory_host();
    write(
        host.as_ref(),
        "/work/CMakePresets.json",
        br#"{
        "version": 13,
        "configurePresets": [{"name": "cfg", "generator": "Ninja", "binaryDir": "out"}],
        "packagePresets": [{
            "name": "pkg", "configurePreset": "cfg", "generators": ["TGZ"],
            "environment": {"PACK_CONFIG": "custom.cmake"},
            "configFile": "${sourceDir}/out/$env{PACK_CONFIG}",
            "variables": {"CPACK_PACKAGE_VENDOR": "$env{PACK_CONFIG}"}
        }]
    }"#,
    );
    write(
        host.as_ref(),
        "/work/out/custom.cmake",
        br#"
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME config)
set(CPACK_PACKAGE_VENDOR from-config)
set(CPACK_INSTALLED_DIRECTORIES "/work/input;/")
set(CPACK_POST_BUILD_SCRIPTS "/work/check.cmake")
"#,
    );
    write(
        host.as_ref(),
        "/work/check.cmake",
        br#"
if(NOT CPACK_PACKAGE_VENDOR STREQUAL "custom.cmake")
  message(FATAL_ERROR "Unexpected vendor: ${CPACK_PACKAGE_VENDOR}")
endif()
"#,
    );
    write(host.as_ref(), "/work/input/content", b"data");
    assert_eq!(
        invoke(
            host.clone(),
            &["--preset", "pkg", "-DCPACK_PACKAGE_FILE_NAME=cli"]
        ),
        0,
        "{}",
        stderr(&host)
    );
    let tree = extract(
        host.as_ref(),
        Path::new("/work/out/cli.tar.gz"),
        Path::new("/work/unpacked"),
    );
    assert_eq!(file_names(&tree), ["cli/content"]);
    assert_eq!(
        kind(host.as_ref(), Path::new("/work/out/config.tar.gz")),
        None
    );
}

#[test]
fn empty_package_preset_strings_use_the_configuration() {
    let host = memory_host();
    write(
        host.as_ref(),
        "/work/CMakePresets.json",
        br#"{
        "version": 13,
        "configurePresets": [{"name": "cfg", "generator": "Ninja", "binaryDir": "out"}],
        "packagePresets": [{
            "name": "pkg", "configurePreset": "cfg", "generators": ["TGZ"],
            "configFile": "$env{RCMAKE_TEST_UNSET}",
            "packageName": "$env{RCMAKE_TEST_UNSET}",
            "vendorName": "$env{RCMAKE_TEST_UNSET}",
            "variables": {"CPACK_EMPTY_VALUE": "$env{RCMAKE_TEST_UNSET}"}
        }]
    }"#,
    );
    write(
        host.as_ref(),
        "/work/out/CPackConfig.cmake",
        br#"
set(CPACK_PACKAGE_NAME default-name)
set(CPACK_PACKAGE_VENDOR default-vendor)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME default-file)
set(CPACK_EMPTY_VALUE from-config)
set(CPACK_INSTALLED_DIRECTORIES "/work/input;/")
set(CPACK_POST_BUILD_SCRIPTS "/work/check.cmake")
"#,
    );
    write(
        host.as_ref(),
        "/work/check.cmake",
        br#"
if(NOT CPACK_PACKAGE_NAME STREQUAL "default-name"
    OR NOT CPACK_PACKAGE_VENDOR STREQUAL "default-vendor"
    OR NOT CPACK_EMPTY_VALUE STREQUAL "")
  message(FATAL_ERROR "Unexpected preset values")
endif()
"#,
    );
    write(host.as_ref(), "/work/input/content", b"data");
    assert_eq!(
        invoke(host.clone(), &["--preset", "pkg"]),
        0,
        "{}",
        stderr(&host)
    );
    assert_eq!(
        kind(host.as_ref(), Path::new("/work/out/default-file.tar.gz")),
        Some(FileKind::File)
    );
}

#[test]
fn installed_directory_pairs_use_packaging_prefix_without_destdir() {
    let host = memory_host();
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        br#"
set(CPACK_GENERATOR TGZ)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY OFF)
set(CPACK_INSTALLED_DIRECTORIES "/work/payload;/assets")
"#,
    );
    write(host.as_ref(), "/work/payload/marker", b"marker");
    for (index, (args, expected)) in [
        (
            &["-DCPACK_PACKAGING_INSTALL_PREFIX=/opt/demo"][..],
            "opt/demo/assets/marker",
        ),
        (
            &[
                "-DCPACK_PACKAGING_INSTALL_PREFIX=/opt/demo",
                "-DCPACK_SET_DESTDIR=ON",
            ][..],
            "assets/marker",
        ),
        (&[][..], "assets/marker"),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = format!("/work/out{index}");
        let mut argv = vec!["-B", dir.as_str()];
        argv.extend(args);
        assert_eq!(invoke(host.clone(), &argv), 0, "{}", stderr(&host));
        let tree = extract(
            host.as_ref(),
            &Path::new(&dir).join("demo.tar.gz"),
            &Path::new(&dir).join("unpacked"),
        );
        assert_eq!(file_names(&tree), [expected], "{args:?}");
    }

    for (index, args) in [
        &["-DCPACK_INSTALLED_DIRECTORIES=/work/payload;../escape"][..],
        &["-DCPACK_PACKAGING_INSTALL_PREFIX=/opt/../escape"][..],
    ]
    .into_iter()
    .enumerate()
    {
        let host = memory_host();
        write(host.as_ref(), "/work/payload/marker", b"marker");
        write(
            host.as_ref(),
            "/work/CPackConfig.cmake",
            br#"
set(CPACK_GENERATOR TGZ)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_INSTALLED_DIRECTORIES "/work/payload;/assets")
"#,
        );
        let dir = format!("/work/bad{index}");
        let mut argv = vec!["-B", dir.as_str()];
        argv.extend(args);
        assert_eq!(invoke(host.clone(), &argv), 1);
        assert!(
            stderr(&host).contains("Invalid CPack install path"),
            "{}",
            stderr(&host)
        );
    }

    let host = memory_host();
    write(host.as_ref(), "/work/payload/marker", b"marker");
    write(
        host.as_ref(),
        "/work/link.cmake",
        br#"
file(MAKE_DIRECTORY "${CMAKE_INSTALL_PREFIX}")
file(CREATE_LINK "/work/elsewhere" "${CMAKE_INSTALL_PREFIX}/assets" SYMBOLIC)
"#,
    );
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        br#"
set(CPACK_GENERATOR TGZ)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_PACKAGING_INSTALL_PREFIX "/opt/demo")
set(CPACK_INSTALL_SCRIPT "/work/link.cmake")
set(CPACK_INSTALLED_DIRECTORIES "/work/payload;/assets")
"#,
    );
    assert_eq!(invoke(host.clone(), &[]), 1);
    assert!(
        stderr(&host).contains("Expected a staging directory"),
        "{}",
        stderr(&host)
    );
    assert_eq!(
        kind(host.as_ref(), Path::new("/work/elsewhere/marker")),
        None
    );
}

const BUILD_TYPE_INSTALL: &[u8] = br#"
if(DEFINED CMAKE_INSTALL_CONFIG_NAME)
  set(config defined)
else()
  set(config undefined)
endif()
if(DEFINED BUILD_TYPE)
  set(build set)
else()
  set(build unset)
endif()
file(WRITE "${CMAKE_INSTALL_PREFIX}/marker-${BUILD_TYPE}.txt" "${config};${build}")
"#;

#[test]
fn cpack_project_install_sets_build_type_for_each_configuration() {
    let host = memory_host();
    write(
        host.as_ref(),
        "/work/build/cmake_install.cmake",
        BUILD_TYPE_INSTALL,
    );
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        br#"
set(CPACK_GENERATOR TAR)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY OFF)
set(CPACK_CMAKE_GENERATOR Ninja)
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;demo;ALL;/")
"#,
    );
    for (dir, args, expected) in [
        (
            "/work/multi",
            &["-C", "Debug;Release"][..],
            &[
                ("marker-Debug.txt", "undefined;set"),
                ("marker-Release.txt", "undefined;set"),
            ][..],
        ),
        (
            "/work/none",
            &[][..],
            &[("marker-.txt", "undefined;unset")][..],
        ),
        (
            "/work/custom",
            &[
                "-C",
                "Debug",
                "-DCPACK_CUSTOM_INSTALL_VARIABLES=BUILD_TYPE=Custom",
            ][..],
            &[("marker-Custom.txt", "undefined;set")][..],
        ),
    ] {
        let mut argv = vec!["-B", dir];
        argv.extend(args);
        assert_eq!(invoke(host.clone(), &argv), 0, "{}", stderr(&host));
        let unpacked = Path::new(dir).join("unpacked");
        let tree = extract(host.as_ref(), &Path::new(dir).join("demo.tar"), &unpacked);
        assert_eq!(
            file_names(&tree),
            expected.iter().map(|(name, _)| *name).collect::<Vec<_>>()
        );
        for (name, contents) in expected {
            assert_eq!(
                read(host.as_ref(), &unpacked.join(name)),
                contents.as_bytes()
            );
        }
    }

    write(
        host.as_ref(),
        "/work/build/cmake_install.cmake",
        br#"
if(NOT DEFINED CMAKE_INSTALL_CONFIG_NAME)
  if(BUILD_TYPE)
    string(REGEX REPLACE "^[^A-Za-z0-9_]+" "" CMAKE_INSTALL_CONFIG_NAME "${BUILD_TYPE}")
  else()
    set(CMAKE_INSTALL_CONFIG_NAME "Debug")
  endif()
endif()
if(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Rr][Ee][Ll][Ee][Aa][Ss][Ee])$")
  file(WRITE "${CMAKE_INSTALL_PREFIX}/release-only.txt" "${CMAKE_INSTALL_CONFIG_NAME}")
endif()
file(WRITE "${CMAKE_INSTALL_PREFIX}/${CMAKE_INSTALL_CONFIG_NAME}.txt" "${CMAKE_INSTALL_CONFIG_NAME}")
"#,
    );
    for (dir, args, expected) in [
        (
            "/work/generated",
            &["-C", "Release;Debug"][..],
            &["Debug.txt", "Release.txt", "release-only.txt"][..],
        ),
        ("/work/default", &[][..], &["Debug.txt"][..]),
    ] {
        let mut argv = vec!["-B", dir];
        argv.extend(args);
        assert_eq!(invoke(host.clone(), &argv), 0, "{}", stderr(&host));
        let tree = extract(
            host.as_ref(),
            &Path::new(dir).join("demo.tar"),
            &Path::new(dir).join("unpacked"),
        );
        assert_eq!(file_names(&tree), expected);
    }
}

const COMPONENT_CONFIG: &[u8] = br#"
set(CPACK_GENERATOR TGZ)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_CMAKE_GENERATOR Ninja)
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;demo;ALL;/")
set(CPACK_ARCHIVE_COMPONENT_INSTALL ON)
set(CPACK_COMPONENTS_ALL "runtime;docs")
set(CPACK_COMPONENTS_GROUPING IGNORE)
set(CPACK_PACKAGING_INSTALL_PREFIX "/usr")
set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY OFF)
set(CPACK_PRE_BUILD_SCRIPTS "/work/pre.cmake")
"#;

const COMPONENT_INSTALL: &[u8] = br#"
if(NOT DEFINED CMAKE_INSTALL_COMPONENT OR CMAKE_INSTALL_COMPONENT STREQUAL "runtime")
  file(WRITE "${CMAKE_INSTALL_PREFIX}/nested/before" "installed")
  file(MAKE_DIRECTORY "${CMAKE_INSTALL_PREFIX}/empty")
  file(CREATE_LINK "nested/before" "${CMAKE_INSTALL_PREFIX}/link" SYMBOLIC)
endif()
if(NOT DEFINED CMAKE_INSTALL_COMPONENT OR CMAKE_INSTALL_COMPONENT STREQUAL "docs")
  file(WRITE "${CMAKE_INSTALL_PREFIX}/doc.txt" "docs")
endif()
"#;

#[test]
fn component_archives_use_install_membership_and_post_hook_bytes() {
    let host = memory_host();
    write(host.as_ref(), "/work/CPackConfig.cmake", COMPONENT_CONFIG);
    write(
        host.as_ref(),
        "/work/build/cmake_install.cmake",
        COMPONENT_INSTALL,
    );
    write(
        host.as_ref(),
        "/work/pre.cmake",
        br#"
set(root "${CPACK_TEMPORARY_DIRECTORY}/runtime/usr")
file(WRITE "${root}/nested/before" "updated")
file(WRITE "${root}/nested/after" "hook")
file(WRITE "${CPACK_TEMPORARY_DIRECTORY}/runtime/marker" "hook")
file(WRITE "${CPACK_TEMPORARY_DIRECTORY}/docs/usr/extra.txt" "hook")
set(CPACK_COMPONENTS_ALL "")
"#,
    );
    assert_eq!(invoke(host.clone(), &[]), 0, "{}", stderr(&host));

    let unpacked = Path::new("/work/unpacked/runtime");
    let runtime = extract(
        host.as_ref(),
        Path::new("/work/demo-runtime.tar.gz"),
        unpacked,
    );
    assert_eq!(file_names(&runtime), ["usr/link", "usr/nested/before"]);
    assert_eq!(
        runtime["usr/link"].target.as_deref(),
        Some(Path::new("nested/before"))
    );
    assert_eq!(
        read(host.as_ref(), &unpacked.join("usr/nested/before")),
        b"updated"
    );
    assert_eq!(
        kind(host.as_ref(), &unpacked.join("usr/empty")),
        Some(FileKind::Dir)
    );
    assert_eq!(
        kind(host.as_ref(), &unpacked.join("usr/nested/after")),
        None
    );
    assert_eq!(kind(host.as_ref(), &unpacked.join("marker")), None);

    let docs = extract(
        host.as_ref(),
        Path::new("/work/demo-docs.tar.gz"),
        Path::new("/work/unpacked/docs"),
    );
    assert_eq!(file_names(&docs), ["usr/doc.txt"]);
    assert_eq!(kind(host.as_ref(), Path::new("/work/demo.tar.gz")), None);

    write(
        host.as_ref(),
        "/work/pre.cmake",
        br#"file(REMOVE "${CPACK_TEMPORARY_DIRECTORY}/runtime/usr/nested/before")"#,
    );
    assert_eq!(invoke(host.clone(), &["-B", "/work/removed"]), 1);
    assert!(
        stderr(&host).contains("CPack cannot read the installed component file"),
        "{}",
        stderr(&host)
    );
}

#[test]
fn monolithic_archives_include_pre_build_hook_output() {
    let host = memory_host();
    write(host.as_ref(), "/work/CPackConfig.cmake", COMPONENT_CONFIG);
    write(
        host.as_ref(),
        "/work/build/cmake_install.cmake",
        COMPONENT_INSTALL,
    );
    write(
        host.as_ref(),
        "/work/pre.cmake",
        br#"file(WRITE "${CPACK_TEMPORARY_DIRECTORY}/usr/added" "hook")"#,
    );
    assert_eq!(
        invoke(host.clone(), &["-DCPACK_ARCHIVE_COMPONENT_INSTALL=OFF"]),
        0,
        "{}",
        stderr(&host)
    );
    let tree = extract(
        host.as_ref(),
        Path::new("/work/demo.tar.gz"),
        Path::new("/work/unpacked"),
    );
    assert_eq!(
        file_names(&tree),
        ["usr/added", "usr/doc.txt", "usr/link", "usr/nested/before"]
    );
}

#[test]
fn component_groups_merge_only_recorded_members() {
    let host = memory_host();
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        br#"
set(CPACK_GENERATOR TGZ)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_CMAKE_GENERATOR Ninja)
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/one;One;ALL;/;/work/two;Two;ALL;/")
set(CPACK_ARCHIVE_COMPONENT_INSTALL ON)
set(CPACK_COMPONENTS_ALL "runtime;docs")
set(CPACK_COMPONENT_RUNTIME_GROUP Main)
set(CPACK_COMPONENT_DOCS_GROUP Main)
set(CPACK_COMPONENT_INCLUDE_TOPLEVEL_DIRECTORY ON)
set(CPACK_PACKAGING_INSTALL_PREFIX "/usr")
set(CPACK_PRE_BUILD_SCRIPTS "/work/pre.cmake")
"#,
    );
    // Both components install `shared.txt`. A group archive accepts identical copies.
    let install = |project: &str, shared: &str| {
        format!(
            r#"
if(CMAKE_INSTALL_COMPONENT STREQUAL "runtime")
  file(WRITE "${{CMAKE_INSTALL_PREFIX}}/shared.txt" "{shared}")
  file(WRITE "${{CMAKE_INSTALL_PREFIX}}/{project}.txt" "{project}")
else()
  file(WRITE "${{CMAKE_INSTALL_PREFIX}}/shared.txt" "same")
  file(WRITE "${{CMAKE_INSTALL_PREFIX}}/{project}-doc.txt" "{project}")
endif()
"#
        )
    };
    write(
        host.as_ref(),
        "/work/one/cmake_install.cmake",
        install("one", "same").as_bytes(),
    );
    write(
        host.as_ref(),
        "/work/two/cmake_install.cmake",
        install("two", "same").as_bytes(),
    );
    write(
        host.as_ref(),
        "/work/pre.cmake",
        br#"
file(WRITE "${CPACK_TEMPORARY_DIRECTORY}/runtime/demo/usr/hook-only" "one")
file(WRITE "${CPACK_TEMPORARY_DIRECTORY}/docs/demo/usr/hook-only" "two")
"#,
    );
    assert_eq!(invoke(host.clone(), &[]), 0, "{}", stderr(&host));
    let unpacked = Path::new("/work/unpacked");
    let tree = extract(host.as_ref(), Path::new("/work/demo-Main.tar.gz"), unpacked);
    assert_eq!(
        file_names(&tree),
        [
            "demo/usr/one-doc.txt",
            "demo/usr/one.txt",
            "demo/usr/shared.txt",
            "demo/usr/two-doc.txt",
            "demo/usr/two.txt",
        ]
    );
    assert_eq!(
        read(host.as_ref(), &unpacked.join("demo/usr/shared.txt")),
        b"same"
    );

    for project in ["one", "two"] {
        write(
            host.as_ref(),
            format!("/work/{project}/cmake_install.cmake"),
            install(project, "different").as_bytes(),
        );
    }
    assert_eq!(invoke(host.clone(), &["-B", "/work/conflict"]), 1);
    assert!(
        stderr(&host).contains("The data in files with the same filename is different"),
        "{}",
        stderr(&host)
    );
}

#[test]
fn component_archives_read_recorded_paths_from_the_component_directory() {
    let host = memory_host();
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        br#"
set(CPACK_GENERATOR TGZ)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_CMAKE_GENERATOR Ninja)
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/one;One;ALL;/;/work/two;Two;ALL;/sub")
set(CPACK_ARCHIVE_COMPONENT_INSTALL ON)
set(CPACK_COMPONENTS_ALL runtime)
set(CPACK_PACKAGING_INSTALL_PREFIX "/usr")
"#,
    );
    for project in ["one", "two"] {
        write(
            host.as_ref(),
            format!("/work/{project}/cmake_install.cmake"),
            format!(r#"file(WRITE "${{CMAKE_INSTALL_PREFIX}}/{project}.txt" "{project}")"#)
                .as_bytes(),
        );
    }
    // The second project installs below `sub/runtime`, but CPack reads `runtime`.
    assert_eq!(invoke(host.clone(), &[]), 1);
    assert!(
        stderr(&host).contains("CPack cannot read the installed component file"),
        "{}",
        stderr(&host)
    );

    let host = memory_host();
    write(host.as_ref(), "/work/build/file", b"data");
    write(
        host.as_ref(),
        "/work/build/cmake_install.cmake",
        br#"file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/nested" TYPE FILE FILES "/work/build/file")"#,
    );
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        br#"
set(CPACK_GENERATOR TGZ)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_CMAKE_GENERATOR Ninja)
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;demo;ALL;/")
set(CPACK_ARCHIVE_COMPONENT_INSTALL ON)
set(CPACK_COMPONENTS_ALL runtime)
set(CPACK_SET_DESTDIR ON)
set(CPACK_INSTALL_PREFIX "/opt")
set(CPACK_PACKAGING_INSTALL_PREFIX "/usr")
"#,
    );
    // With DESTDIR, the recorded paths contain the install prefix and CPack adds the
    // packaging prefix, so `usr/opt/nested` does not exist.
    assert_eq!(invoke(host.clone(), &["-B", "/work/destdir"]), 1);
    assert!(
        stderr(&host).contains("CPack cannot read the installed component file"),
        "{}",
        stderr(&host)
    );
    assert_eq!(
        invoke(
            host.clone(),
            &["-B", "/work/root", "-DCPACK_PACKAGING_INSTALL_PREFIX=/"]
        ),
        0,
        "{}",
        stderr(&host)
    );
    let tree = extract(
        host.as_ref(),
        Path::new("/work/root/demo-runtime.tar.gz"),
        Path::new("/work/root/unpacked"),
    );
    assert_eq!(file_names(&tree), ["opt/nested/file"]);
}

#[test]
fn install_script_status_messages_print_only_in_verbose_mode() {
    for (args, verbose) in [
        (&[][..], false),
        (&["--debug"][..], false),
        (&["-V"][..], true),
    ] {
        let host = memory_host();
        write(
            host.as_ref(),
            "/work/build/cmake_install.cmake",
            b"message(STATUS \"custom-status\")\nmessage(\"plain-notice\")\n",
        );
        write(
            host.as_ref(),
            "/work/CPackConfig.cmake",
            br#"
set(CPACK_GENERATOR TAR)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_CMAKE_GENERATOR Ninja)
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;demo;ALL;/")
"#,
        );
        assert_eq!(invoke(host.clone(), args), 0, "{}", stderr(&host));
        let stdout = String::from_utf8_lossy(&host.stream_bytes(Stream::Stdout)).into_owned();
        assert_eq!(
            stdout.contains("custom-status"),
            verbose,
            "{args:?}: {stdout}"
        );
        assert_eq!(
            stdout.contains("CPack Verbose: custom-status\n"),
            verbose,
            "{args:?}: {stdout}"
        );
        assert!(!stdout.contains("-- custom-status"), "{stdout}");
        assert!(stderr(&host).contains("plain-notice\n"));
    }
}

#[test]
fn cpack_applies_default_directory_modes_to_package_entries() {
    let host = memory_host();
    write(host.as_ref(), "/work/input.txt", b"data");
    write(
        host.as_ref(),
        "/work/build/cmake_install.cmake",
        br#"
if(NOT DEFINED CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS)
  set(CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS OWNER_READ OWNER_WRITE OWNER_EXECUTE)
endif()
file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/foo/bar" TYPE FILE FILES "/work/input.txt")
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
set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY OFF)
set(CPACK_PACKAGING_INSTALL_PREFIX /usr)
set(CPACK_CMAKE_GENERATOR Ninja)
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;demo;ALL;/")
"#,
    );
    for (output, override_mode, directory_mode) in [
        ("/work/implicit", None, 0o700),
        (
            "/work/empty-override",
            Some("-DCPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS="),
            0o700,
        ),
        (
            "/work/override",
            Some(
                "-DCPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS=OWNER_READ;OWNER_WRITE;OWNER_EXECUTE;GROUP_READ",
            ),
            0o740,
        ),
    ] {
        let mut args = vec!["-B", output];
        args.extend(override_mode);
        assert_eq!(invoke(host.clone(), &args), 0, "{}", stderr(&host));
        let output = Path::new(output);
        let destination = output.join("extract");
        extract(host.as_ref(), &output.join("demo.tar"), &destination);
        let modes = directory_modes(host.as_ref(), &destination);
        assert_eq!(modes[&PathBuf::from("usr/foo")], directory_mode);
        assert_eq!(modes[&PathBuf::from("usr/foo/bar")], directory_mode);
        if override_mode
            == Some(
                "-DCPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS=OWNER_READ;OWNER_WRITE;OWNER_EXECUTE;GROUP_READ",
            )
        {
            assert_eq!(modes[&PathBuf::from("usr")], directory_mode);
        }
    }
    assert_eq!(
        invoke(
            host.clone(),
            &["-DCPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS=INVALID"]
        ),
        1
    );
    assert!(stderr(&host).contains("CPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS value is invalid"));
    write(
        host.as_ref(),
        "/work/build/cmake_install.cmake",
        br#"
set(CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS INVALID)
file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/foo" TYPE FILE FILES "/work/input.txt")
"#,
    );
    assert_eq!(invoke(host.clone(), &[]), 1);
    assert!(stderr(&host).contains("CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS variable"));
}

#[test]
fn cpack_installed_directories_apply_default_directory_mode_to_parent_dirs() {
    let host = memory_host();
    write(host.as_ref(), "/work/payload/real.txt", b"data");
    host.symlink(Path::new("real.txt"), Path::new("/work/payload/link.txt"))
        .unwrap();
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        br#"
set(CPACK_GENERATOR TAR)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY OFF)
set(CPACK_PACKAGING_INSTALL_PREFIX /opt/demo)
set(CPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS OWNER_READ;OWNER_WRITE;OWNER_EXECUTE;GROUP_READ)
set(CPACK_INSTALLED_DIRECTORIES "/work/payload;/assets/links")
"#,
    );
    assert_eq!(invoke(host.clone(), &[]), 0, "{}", stderr(&host));
    let destination = Path::new("/work/extract");
    let tree = extract(host.as_ref(), Path::new("/work/demo.tar"), destination);
    assert_eq!(
        file_names(&tree),
        [
            "opt/demo/assets/links/link.txt",
            "opt/demo/assets/links/real.txt"
        ]
    );
    assert_eq!(
        tree["opt/demo/assets/links/link.txt"].target.as_deref(),
        Some(Path::new("real.txt"))
    );
    let modes = directory_modes(host.as_ref(), destination);
    for dir in [
        "opt",
        "opt/demo",
        "opt/demo/assets",
        "opt/demo/assets/links",
    ] {
        assert_eq!(modes[&PathBuf::from(dir)], 0o740, "{dir}");
    }
}

#[test]
fn deb_component_file_name_override_accepts_a_slash_in_the_component() {
    let host = memory_host();
    let modules = seed_package_module(
        &host,
        "CPackDeb.cmake",
        include_bytes!("../../../../modules/cmake-4.5/Modules/Internal/CPack/CPackDeb.cmake"),
    );
    write(host.as_ref(), "/work/input.txt", b"data");
    write(
        host.as_ref(),
        "/work/build/cmake_install.cmake",
        br#"file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/share" TYPE FILE FILES "/work/input.txt")"#,
    );
    write(
        host.as_ref(),
        "/work/CPackConfig.cmake",
        format!(
            r#"
set(CMAKE_ROOT "{modules}")
set(CPACK_GENERATOR DEB)
set(CPACK_PACKAGE_NAME demo)
set(CPACK_PACKAGE_VERSION 1)
set(CPACK_PACKAGE_FILE_NAME demo)
set(CPACK_PACKAGE_CONTACT Maintainer)
set(CPACK_PACKAGE_DESCRIPTION_SUMMARY Summary)
set(CPACK_CMAKE_GENERATOR Ninja)
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;demo;ALL;/")
set(CPACK_DEB_COMPONENT_INSTALL ON)
set(CPACK_COMPONENTS_ALL part/variant)
set(CPACK_DEBIAN_PACKAGE_ARCHITECTURE all)
set(CPACK_DEBIAN_PART/VARIANT_FILE_NAME adjusted)
"#
        )
        .as_bytes(),
    );
    assert_eq!(invoke(host.clone(), &[]), 0, "{}", stderr(&host));
    assert_eq!(
        list_command_archive(
            host.as_ref(),
            Path::new("/work/adjusted.deb"),
            Vec::new(),
            None
        )
        .expect("read Debian archive"),
        [
            BString::from("debian-binary"),
            "control.tar.gz".into(),
            "data.tar.gz".into()
        ]
    );
}
