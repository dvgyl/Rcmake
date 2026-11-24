use rcmake_model::{GeneratorKind, ImportedVisibility, TargetOrigin};
use rcmake_resolve::{ArtifactSet, ResolvedTarget, TargetMetadata};

use super::*;

#[test]
fn resolves_executable_and_command_expressions_by_configuration() {
    let mut model = ProjectModel::default();
    let dir = DirId::ROOT;
    model.set_root_dirs("/src".into(), "/build".into());
    let id = add_executable(&mut model, "tool", TargetOrigin::Normal);
    for (name, old_style, program) in [
        ("direct", false, "tool"),
        ("wrapper", false, "cmake"),
        ("legacy", true, "tool"),
    ] {
        let mut test = test_entry(
            name,
            [
                program,
                "-DCOMMAND=$<TARGET_FILE:tool>",
                "$<TARGET_FILE_NAME:tool>",
                "$<TARGET_FILE_DIR:tool>",
                "$<CONFIG>",
            ],
        );
        test.old_style = old_style;
        model.add_test(test).unwrap();
    }
    let mut result = GenerationResult::default();
    result.configs.resize_with(2, Default::default);
    for (id_config, config) in result.configs.iter_mut().zip(["Debug", "Release"]) {
        id_config.0 = config.into();
        result.targets.push(ResolvedTarget {
            id,
            config: id_config.clone(),
            metadata: TargetMetadata {
                name: "tool".into(),
                kind: TargetKind::Executable,
                source_dir: "/src".into(),
                binary_dir: "/build".into(),
                backtrace: Default::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: ArtifactSet {
                file: Some(format!("/build/{config}/tool").into()),
                ..Default::default()
            },
            compile_groups: Vec::new(),
            compile_units: Vec::new(),
            link: None,
            dependencies: Vec::new(),
            cxx_modules: None,
            ide: None,
        });
    }
    let script = render_resolved(&model, dir, &result).unwrap().to_string();
    for config in ["Debug", "Release"] {
        assert!(script.contains(&format!(
            r#"add_test("direct" "/build/{config}/tool" "-DCOMMAND=/build/{config}/tool" "tool" "/build/{config}" "{config}")"#
        )));
        assert!(script.contains(&format!(
            r#"add_test("wrapper" "cmake" "-DCOMMAND=/build/{config}/tool""#
        )));
    }
    assert!(script.contains(r#"_CMAKE_TEST_BUILD_DEPENDS "/build/Debug/tool""#));
    assert!(script.contains(r#"_CMAKE_TEST_BUILD_DEPENDS "/build/Release/tool""#));
    assert!(script.contains(r#"add_test("legacy" "tool" [[-DCOMMAND=$<TARGET_FILE:tool>]]"#));
}

#[test]
fn resolves_imported_executables_and_aliases_in_commands_and_properties() {
    let mut model = ProjectModel::default();
    let dir = DirId::ROOT;
    model.set_root_dirs("/src".into(), "/build".into());
    let python = add_executable(
        &mut model,
        "Python3::Interpreter",
        TargetOrigin::Imported(ImportedVisibility::Local),
    );
    model.target_mut(python).unwrap().props.set(
        "IMPORTED_LOCATION".into(),
        "/opt/python/bin/python3.13".into(),
    );
    model.add_alias(dir, "py_alias".into(), python).unwrap();
    add_executable(&mut model, "app", TargetOrigin::Normal);
    model
        .add_test(test_entry(
            "py",
            ["Python3::Interpreter", "script.py", "$<TARGET_FILE:app>"],
        ))
        .unwrap();
    let mut alias = test_entry(
        "alias",
        [
            "py_alias",
            "PY=$<TARGET_FILE:Python3::Interpreter>",
            "$<TARGET_FILE_NAME:py_alias>",
            "$<TARGET_FILE_BASE_NAME:py_alias>",
        ],
    );
    alias.working_directory = Some("$<TARGET_FILE_DIR:py_alias>".into());
    alias.props.set(
        "ENVIRONMENT".into(),
        "PY=$<TARGET_FILE:Python3::Interpreter>;ALIAS=$<TARGET_FILE:py_alias>".into(),
    );
    alias.props.set(
        "ENVIRONMENT_MODIFICATION".into(),
        "PATH=path_list_prepend:$<TARGET_FILE_DIR:Python3::Interpreter>".into(),
    );
    model.add_test(alias).unwrap();
    let result = GenerationResult {
        configs: vec![Default::default()],
        ..Default::default()
    };

    let script = render_resolved(&model, dir, &result).unwrap().to_string();
    assert!(
        script.contains(r#"add_test("py" "/opt/python/bin/python3.13" "script.py" "/build/app")"#)
    );
    assert!(script.contains(
        r#"add_test("alias" "/opt/python/bin/python3.13" "PY=/opt/python/bin/python3.13" "python3.13" "Python3::Interpreter")"#
    ));
    assert!(script.contains(
        r#"set_tests_properties("alias" PROPERTIES  "ENVIRONMENT" "PY=/opt/python/bin/python3.13;ALIAS=/opt/python/bin/python3.13" "ENVIRONMENT_MODIFICATION" "PATH=path_list_prepend:/opt/python/bin" "WORKING_DIRECTORY" "/opt/python/bin""#
    ));
}

#[test]
fn resolves_imported_locations_and_properties_per_configuration() {
    let mut model = ProjectModel::default();
    let dir = DirId::ROOT;
    model.set_root_dirs("/src".into(), "/build".into());
    model.generator = Some(GeneratorKind::NinjaMultiConfig);
    let python = add_executable(
        &mut model,
        "Python3::Interpreter",
        TargetOrigin::Imported(ImportedVisibility::Local),
    );
    let props = &mut model.target_mut(python).unwrap().props;
    for (name, value) in [
        ("IMPORTED_CONFIGURATIONS", "Release;Debug"),
        ("IMPORTED_LOCATION_DEBUG", "/opt/debug/python"),
        ("IMPORTED_LOCATION_RELEASE", "/opt/release/python"),
        ("MAP_IMPORTED_CONFIG_RELWITHDEBINFO", "Debug"),
    ] {
        props.set(name.into(), value.into());
    }
    let mut test = test_entry("configured", ["Python3::Interpreter"]);
    test.props.set(
        "WORKING_DIRECTORY".into(),
        "$<TARGET_FILE_DIR:Python3::Interpreter>".into(),
    );
    model.add_test(test).unwrap();
    let mut result = GenerationResult::default();
    let cases = [
        ("Debug", "debug"),
        ("Release", "release"),
        ("RelWithDebInfo", "debug"),
        ("MinSizeRel", "release"),
    ];
    result.configs.resize_with(cases.len(), Default::default);
    for (config, (name, _)) in result.configs.iter_mut().zip(cases) {
        config.0 = name.into();
    }

    let script = render_resolved(&model, dir, &result).unwrap().to_string();
    for (name, directory) in cases {
        let mut condition = BString::default();
        configuration_condition(&mut condition, &[name.into()]);
        assert!(script.contains(&format!(
            "{condition}  \
             add_test(\"configured\" \"/opt/{directory}/python\")\n  \
             set_tests_properties(\"configured\" PROPERTIES  \"WORKING_DIRECTORY\" \"/opt/{directory}\""
        )), "{script}");
    }
    assert!(script.contains("else()\n  add_test(\"configured\" NOT_AVAILABLE)\nendif()\n"));
}

fn add_executable(
    model: &mut ProjectModel,
    name: &str,
    origin: TargetOrigin,
) -> rcmake_model::TargetId {
    model
        .add_target(Target::new(
            name.into(),
            TargetKind::Executable,
            origin,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        ))
        .unwrap()
}

fn test_entry<const N: usize>(name: &str, command: [&str; N]) -> TestEntry {
    TestEntry {
        name: name.into(),
        dir: DirId::ROOT,
        backtrace: Default::default(),
        command: command.map(BString::from).into(),
        configurations: Vec::new(),
        working_directory: None,
        command_expand_lists: false,
        old_style: false,
        policies: Default::default(),
        props: Default::default(),
        build_depends: Vec::new(),
        kind: rcmake_model::TestKind::Single,
    }
}
