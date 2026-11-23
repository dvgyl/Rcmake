use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{PureHost, Stream};
use rcmake_model::{DirId, UsageField};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn subdirectory_commands_share_the_model_with_property_commands() -> TestResult {
    let source = PathBuf::from("/work/src");
    let binary = PathBuf::from("/work/build");
    let command = PathBuf::from("/bin/cmake");
    let host = Arc::new(PureHost::new(source.clone(), command.clone()));
    host.seed_file(
        &source.join("CMakeLists.txt"),
        br#"
        cmake_minimum_required(VERSION 4.5)
        project(check NONE)
        define_property(SOURCE PROPERTY INHERIT INHERITED)
        set_property(DIRECTORY PROPERTY INHERIT parent)
        set_property(SOURCE shared.c PROPERTY LABEL root)
        include_directories(one)
        include_directories(BEFORE SYSTEM two three)
        add_definitions(-DKEEP -DDROP)
        remove_definitions(-DDROP)
        add_compile_definitions(EXTRA=1)
        enable_testing()
        add_test(duplicate echo old)
        add_test(duplicate echo new)
        add_subdirectory(child other EXCLUDE_FROM_ALL SYSTEM)
        set_property(DIRECTORY "${CMAKE_BINARY_DIR}/other" PROPERTY LOCAL child)
        set_property(SOURCE shared.c DIRECTORY "${CMAKE_BINARY_DIR}/other" PROPERTY LABEL child)
        get_source_file_property(label shared.c DIRECTORY child LABEL)
        get_property(parent SOURCE shared.c DIRECTORY child PROPERTY INHERIT)
        get_property(child_dir DIRECTORY child PROPERTY BINARY_DIR)
        get_property(tests DIRECTORY "${CMAKE_BINARY_DIR}/other" PROPERTY TESTS)
        set_tests_properties(example DIRECTORY child PROPERTIES LABELS initial)
        set_property(TEST example DIRECTORY "${CMAKE_BINARY_DIR}/other" APPEND PROPERTY LABELS appended)
        get_test_property(example LABELS DIRECTORY "${CMAKE_BINARY_DIR}/other" labels)
        get_test_property(missing LABELS missing)
        set_property(INSTALL artifact PROPERTY LABEL value)
        get_property(install_label INSTALL artifact PROPERTY LABEL)
        if(NOT label STREQUAL "child" OR NOT parent STREQUAL "parent"
           OR NOT child_dir STREQUAL "${CMAKE_BINARY_DIR}/other"
           OR NOT tests STREQUAL "example" OR NOT labels STREQUAL "initial;appended"
           OR NOT missing STREQUAL "NOTFOUND" OR NOT install_label STREQUAL "value")
          message(FATAL_ERROR "Scoped property query failed")
        endif()
        "#,
    )?;
    host.seed_file(
        Path::new("/work/src/child/CMakeLists.txt"),
        br#"
        add_test(NAME example COMMAND echo "a;b"
                 CONFIGURATIONS Release WORKING_DIRECTORY here COMMAND_EXPAND_LISTS)
        if(NOT TEST example OR TEST duplicate OR NOT CMAKE_TESTING_ENABLED)
          message(FATAL_ERROR "Unexpected test visibility")
        endif()
        "#,
    )?;
    let mut interp = Interp::new(
        InterpConfig {
            host: host.clone(),
            mode: RunMode::Configure,
            cmake_root: "/share/cmake-4.5".into(),
            cmake_command: command,
            cwd: source.clone(),
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    let outcome = interp.run_configure(&source, &binary, &[]);
    assert_eq!(
        outcome.exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );

    let mut cx = interp.cx();
    let model = cx.project();
    let root = model.dir(DirId::ROOT);
    let [child_id] = root.children.as_slice() else {
        return Err("Expected one child directory".into());
    };
    let child = model.dir(*child_id);
    assert!(child.exclude_from_all && child.system);
    assert_eq!(
        child.usage.joined(UsageField::IncludeDirectories),
        Some("/work/src/three;/work/src/two;/work/src/one".into())
    );
    assert_eq!(
        child.usage.joined(UsageField::CompileDefinitions),
        Some("KEEP;EXTRA=1".into())
    );
    let legacy = model
        .find_test(DirId::ROOT, b"duplicate")
        .and_then(|id| model.test(id));
    assert_eq!(
        legacy
            .and_then(|test| test.command.last())
            .map(|arg| arg.as_slice()),
        Some(b"new".as_slice())
    );
    let Some(test) = model
        .find_test(*child_id, b"example")
        .and_then(|id| model.test(id))
    else {
        return Err("Expected a test entry in the child directory".into());
    };
    assert_eq!(
        test.working_directory.as_ref().map(|dir| dir.as_slice()),
        Some(b"here".as_slice())
    );
    assert_eq!(test.configurations, ["Release"]);
    assert!(test.command_expand_lists);
    Ok(())
}
