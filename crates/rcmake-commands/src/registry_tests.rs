use std::path::Path;
use std::sync::Arc;

use rcmake_base::{BString, list};
use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{Host, HostResult, PureHost, ReadRange, Stream};

const PROJECT: &str = "
    add_compile_definitions add_compile_options add_custom_command add_custom_rule
    add_custom_target add_definitions add_dependencies add_executable add_library
    add_link_options add_subdirectory add_test aux_source_directory block break
    build_command build_name cmake_diagnostic cmake_file_api cmake_host_system_information
    cmake_instrumentation cmake_language cmake_minimum_required cmake_parse_arguments
    cmake_path cmake_pkg_config cmake_policy configure_file continue create_test_sourcelist
    define_property discover_tests else elseif enable_language enable_testing
    endblock endforeach endfunction endif endmacro endwhile exec_program execute_process
    export export_library_dependencies file find_file find_library find_package find_path
    find_program fltk_wrap_ui foreach function get_cmake_property get_directory_property
    get_filename_component get_property get_source_file_property get_target_property
    get_test_property if include include_directories include_external_msproject
    include_guard include_regular_expression install install_files install_programs
    install_targets link_directories link_libraries list load_cache load_command macro
    make_directory mark_as_advanced math message option output_required_files project
    qt_wrap_cpp qt_wrap_ui remove remove_definitions return separate_arguments set
    set_directory_properties set_property set_source_files_properties set_target_properties
    set_tests_properties site_name source_group string subdir_depends subdirs
    target_compile_definitions target_compile_features target_compile_options
    target_include_directories target_link_directories target_link_libraries
    target_link_options target_precompile_headers target_sources try_compile try_run
    unset use_mangled_mesa utility_source variable_requires variable_watch while write_file
";

const PROJECT_ONLY: &[&str] = &[
    "add_compile_definitions",
    "add_custom_rule",
    "add_link_options",
    "cmake_instrumentation",
    "discover_tests",
    "install_files",
    "install_programs",
    "install_targets",
    "subdirs",
    "target_link_directories",
    "target_link_options",
    "target_precompile_headers",
];

fn session(mode: RunMode, source: &str) -> HostResult<(Arc<PureHost>, Interp, i32)> {
    let host = Arc::new(PureHost::default());
    let mut interp = Interp::new(
        InterpConfig {
            services: Default::default(),
            host: host.clone(),
            mode,
            cmake_root: "/share/cmake".into(),
            cmake_command: "/bin/cmake".into(),
            cwd: "/work".into(),
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    let status = match mode {
        RunMode::Script => {
            let path = Path::new("/work/test.cmake");
            host.seed_file(path, source.as_bytes())?;
            interp.run_script(path).exit_code
        }
        RunMode::Configure => {
            let root = Path::new("/work/src");
            host.seed_file(&root.join("CMakeLists.txt"), source.as_bytes())?;
            interp
                .run_configure(root, Path::new("/work/build"), &[])
                .exit_code
        }
    };
    Ok((host, interp, status))
}

#[test]
fn command_lists_match_cmake_4_5_1_in_both_modes() -> HostResult<()> {
    let commands =
        "get_cmake_property(names COMMANDS)\nget_property(global GLOBAL PROPERTY COMMANDS)";
    let project = format!("cmake_minimum_required(VERSION 4.5)\n{commands}\nproject(test NONE)");
    for (mode, count, source) in [
        (RunMode::Configure, 120, project.as_str()),
        (RunMode::Script, 108, commands),
    ] {
        let (host, mut interp, status) = session(mode, source)?;
        assert_eq!(status, 0, "{}", host.stream_bytes(Stream::Stderr));
        assert_eq!(
            interp.cx().var_or_empty(b"names"),
            interp.cx().var_or_empty(b"global")
        );
        let actual = list::split(&interp.cx().var_or_empty(b"names"), list::Empty::Drop);
        let expected: Vec<_> = PROJECT
            .split_whitespace()
            .filter(|name| mode == RunMode::Configure || !PROJECT_ONLY.contains(name))
            .map(BString::from)
            .collect();
        assert_eq!(actual.len(), count);
        assert_eq!(actual, expected);
    }
    Ok(())
}

#[test]
fn commands_absent_in_script_mode_are_reported_as_unknown() -> HostResult<()> {
    for name in PROJECT_ONLY {
        let name = name.to_ascii_uppercase();
        let (host, _, status) = session(RunMode::Script, &format!("{name}()\n"))?;
        assert_eq!(status, 1);
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(
            stderr.contains(&format!("Unknown CMake command \"{name}\".")),
            "{stderr}"
        );
    }
    Ok(())
}

#[test]
fn removed_commands_report_their_policy_or_script_restriction() -> HostResult<()> {
    for (name, policy, script_visible) in [
        ("subdir_depends", "CMP0029", false),
        ("use_mangled_mesa", "CMP0030", true),
        ("load_command", "CMP0031", false),
        ("output_required_files", "CMP0032", false),
        ("export_library_dependencies", "CMP0033", false),
        ("utility_source", "CMP0034", false),
        ("variable_requires", "CMP0035", false),
        ("build_name", "CMP0036", true),
    ] {
        let upper = name.to_ascii_uppercase();
        for mode in [RunMode::Configure, RunMode::Script] {
            let source = if mode == RunMode::Configure {
                format!("cmake_minimum_required(VERSION 4.5)\nproject(test NONE)\n{upper}()")
            } else {
                format!("{upper}()")
            };
            let (host, _, status) = session(mode, &source)?;
            assert_eq!(status, 1);
            let expected = if script_visible || mode == RunMode::Configure {
                format!("The {name} command has been removed; see {policy}.")
            } else {
                format!("{upper} command is not scriptable")
            };
            let stderr = host.stream_bytes(Stream::Stderr).to_string();
            assert!(stderr.contains(&expected), "{stderr}");
        }
    }
    Ok(())
}

#[test]
fn legacy_script_commands_create_files_and_remove_list_elements() -> HostResult<()> {
    let (host, mut interp, status) = session(
        RunMode::Script,
        r#"
        make_directory(dir)
        write_file(dir/file "first" "line")
        write_file(dir/file APPEND "second line")
        set(options "a;b;c;d;;b")
        remove(options "b;d")
        "#,
    )?;
    assert_eq!(status, 0, "{}", host.stream_bytes(Stream::Stderr));
    assert_eq!(
        host.read(Path::new("/work/dir/file"), ReadRange::All { limit: 100 })?,
        b"firstline\nsecond line\n",
    );
    assert_eq!(interp.cx().var_or_empty(b"options"), "a;c");
    Ok(())
}

#[test]
fn fltk_reports_missing_targets_after_configuration() -> HostResult<()> {
    let (host, mut interp, status) = session(
        RunMode::Configure,
        r#"
        cmake_minimum_required(VERSION 4.5)
        project(test NONE)
        set(FLTK_FLUID_EXECUTABLE /tools/fluid)
        fltk_wrap_ui(later present.fl)
        fltk_wrap_ui(missing absent.fl)
        add_custom_target(later)
        "#,
    )?;
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert_eq!(status, 0, "{stderr}");
    assert!(
        stderr.contains("target that was never created: missing."),
        "{stderr}"
    );
    assert!(
        !stderr.contains("target that was never created: later."),
        "{stderr}"
    );
    assert_eq!(
        interp.cx().var_or_empty(b"later_FLTK_UI_SRCS"),
        "/work/build/present.cxx"
    );
    Ok(())
}
