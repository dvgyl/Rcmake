//! `CTestTestfile.cmake` scripts for `ctest`.

use std::collections::HashMap;

use rcmake_base::{BString, ByteSlice, list, path_lex::Lexical};
use rcmake_host::{
    Host,
    path::{from_native, to_native},
};
use rcmake_model::{DirId, ProjectModel, Target, TargetKind, TestEntry};
use rcmake_resolve::{GenerationResult, lower::compute_artifacts};

use crate::{GenError, configuration_pattern, quote, write_if_changed};

/// Render the test file for a directory. Commands and properties use the values
/// stored in the model.
pub fn render(model: &ProjectModel, dir: DirId) -> Result<BString, GenError> {
    render_file(model, dir, None)
}

/// Render executable paths and supported generator expressions in test commands
/// and properties for each build configuration.
pub fn render_resolved(
    model: &ProjectModel,
    dir: DirId,
    result: &GenerationResult,
) -> Result<BString, GenError> {
    render_file(model, dir, Some(result))
}

fn render_file(
    model: &ProjectModel,
    dir: DirId,
    result: Option<&GenerationResult>,
) -> Result<BString, GenError> {
    let targets: HashMap<_, _> = result
        .into_iter()
        .flat_map(|result| &result.targets)
        .map(|target| ((target.config.0.as_slice(), target.id), target))
        .collect();
    let multi_config = model.generator.is_some_and(|kind| kind.multi_config());
    let dir = model.dir(dir);
    let mut out = BString::from("# CMake generated Testfile for \n");
    comment(&mut out, b"# Source directory: ", &dir.source_dir);
    comment(&mut out, b"# Build directory: ", &dir.binary_dir);
    out.extend_from_slice(
        b"# \n# This file includes the relevant testing commands required for \n\
          # testing this directory and lists subdirectories to be tested as well.\n",
    );
    for test in dir.tests.values().filter_map(|&id| model.test(id)) {
        let Some(result) = result.filter(|r| !test.old_style && !r.configs.is_empty()) else {
            render_guarded_test(&mut out, test);
            continue;
        };
        let multi_config = multi_config || result.configs.len() > 1;
        let mut guarded = false;
        for config in &result.configs {
            if multi_config
                && !test.configurations.is_empty()
                && !test.configurations.iter().any(|name| config.matches(name))
            {
                continue;
            }
            let lookup = |name: &[u8]| {
                let id = model.find_target(test.dir, name)?;
                let target = model.target(id)?;
                let file = targets
                    .get(&(config.0.as_slice(), id))
                    .and_then(|resolved| resolved.artifacts.file.clone())
                    .or_else(|| compute_artifacts(model, target, config).file)
                    .or_else(|| {
                        if target.is_imported() && target.kind == TargetKind::Executable {
                            target.props.get(b"LOCATION").cloned()
                        } else {
                            None
                        }
                    });
                Some((target, file))
            };
            let resolved =
                resolve_test(test, &config.0, lookup).map_err(|message| GenError::Write {
                    path: from_native(&to_native(&dir.binary_dir).join("CTestTestfile.cmake")),
                    message,
                })?;
            if multi_config {
                out.extend_from_slice(if guarded { b"elseif" } else { b"if" });
                configuration_condition(&mut out, std::slice::from_ref(&config.0));
                guarded = true;
                render_test(&mut out, &resolved, b"  ");
            } else {
                render_guarded_test(&mut out, &resolved);
            }
        }
        if guarded {
            if test.configurations.is_empty() {
                out.extend_from_slice(b"else()\n  add_test(");
                test_argument(&mut out, &test.name);
                out.extend_from_slice(b" NOT_AVAILABLE)\n");
            }
            out.extend_from_slice(b"endif()\n");
        }
    }
    let binary_dir = to_native(&dir.binary_dir);
    for &child in &dir.children {
        let path = to_native(&model.dir(child).binary_dir);
        let path = path.strip_prefix(&binary_dir).unwrap_or(&path);
        out.extend_from_slice(b"subdirs(");
        test_argument(&mut out, &from_native(path));
        out.extend_from_slice(b")\n");
    }
    Ok(out)
}

fn resolve_test<'a>(
    test: &TestEntry,
    config: &[u8],
    lookup: impl Fn(&[u8]) -> Option<(&'a Target, Option<BString>)>,
) -> Result<TestEntry, BString> {
    let resolve = |value: &[u8]| resolve_argument(value, config, &lookup);
    let mut resolved = test.clone();
    resolved.command.clear();
    for arg in &test.command {
        let value = resolve(arg)?;
        if test.command_expand_lists {
            resolved
                .command
                .extend(list::split(&value, list::Empty::Drop));
        } else {
            resolved.command.push(value);
        }
    }
    if let Some(program) = resolved.command.first_mut()
        && let Some((target, Some(file))) = lookup(program)
        && target.kind == TargetKind::Executable
    {
        *program = file;
    }
    for name in test.props.names() {
        if name != b"_BACKTRACE_TRIPLES"
            && let Some(value) = test.props.get(&name)
        {
            resolved.props.set(name, resolve(value)?);
        }
    }
    if test.props.get(b"WORKING_DIRECTORY").is_none()
        && let Some(value) = &test.working_directory
    {
        resolved.working_directory = Some(resolve(value)?);
    }
    Ok(resolved)
}

fn resolve_argument<'a>(
    arg: &[u8],
    config: &[u8],
    lookup: &impl Fn(&[u8]) -> Option<(&'a Target, Option<BString>)>,
) -> Result<BString, BString> {
    let mut out = BString::default();
    let mut rest = arg;
    while let Some((before, after_open)) = rest.split_once_str(b"$<")
        && let Some((expr, tail)) = after_open.split_once_str(b">")
    {
        out.extend_from_slice(before);
        if expr == b"CONFIG" {
            out.extend_from_slice(config);
        } else if let Some((op, name)) = expr.split_once_str(b":")
            && matches!(
                op,
                b"TARGET_FILE" | b"TARGET_FILE_NAME" | b"TARGET_FILE_DIR"
            )
        {
            let file = lookup(name).and_then(|(_, file)| file).ok_or_else(|| {
                BString::from([b"No target file for ".as_slice(), name, b" in test."].concat())
            })?;
            let path = Lexical::new(&file);
            out.extend_from_slice(match op {
                b"TARGET_FILE_NAME" => path.filename(),
                b"TARGET_FILE_DIR" => path.parent(),
                _ => file.as_slice(),
            });
        } else if let Some(name) = expr.strip_prefix(b"TARGET_FILE_BASE_NAME:")
            && let Some((target, _)) = lookup(name)
            && target.is_imported()
        {
            out.extend_from_slice(&target.name);
        } else {
            out.extend_from_slice(b"$<");
            out.extend_from_slice(expr);
            out.push(b'>');
        }
        rest = tail;
    }
    out.extend_from_slice(rest);
    Ok(out)
}

/// Write the test file to the directory's existing binary directory.
pub fn write_ctest_file(model: &ProjectModel, dir: DirId, host: &dyn Host) -> Result<(), GenError> {
    let content = render(model, dir)?;
    let path = to_native(&model.dir(dir).binary_dir).join("CTestTestfile.cmake");
    write_if_changed(host, &path, &content)
}

fn comment(out: &mut BString, label: &[u8], value: &[u8]) {
    out.extend_from_slice(label);
    for &byte in value {
        out.push(if matches!(byte, b'\n' | b'\r') {
            b' '
        } else {
            byte
        });
    }
    out.push(b'\n');
}

fn configuration_condition(out: &mut BString, configurations: &[BString]) {
    out.extend_from_slice(b"(CTEST_CONFIGURATION_TYPE MATCHES ");
    quote(out, &configuration_pattern(configurations));
    out.extend_from_slice(b")\n");
}

fn render_guarded_test(out: &mut BString, test: &TestEntry) {
    if test.configurations.is_empty() {
        render_test(out, test, b"");
    } else {
        out.extend_from_slice(b"if");
        configuration_condition(out, &test.configurations);
        render_test(out, test, b"  ");
        out.extend_from_slice(b"endif()\n");
    }
}

fn render_test(out: &mut BString, test: &TestEntry, indent: &[u8]) {
    out.extend_from_slice(indent);
    out.extend_from_slice(b"add_test(");
    test_argument(out, &test.name);
    for arg in &test.command {
        out.push(b' ');
        test_argument(out, arg);
    }
    out.extend_from_slice(b")\n");
    out.extend_from_slice(indent);
    out.extend_from_slice(b"set_tests_properties(");
    test_argument(out, &test.name);
    out.extend_from_slice(b" PROPERTIES ");

    let mut names = test.props.names();
    if test.working_directory.is_some() && test.props.get(b"WORKING_DIRECTORY").is_none() {
        names.push("WORKING_DIRECTORY".into());
    }
    names.sort_unstable();
    for name in names {
        if name == b"_BACKTRACE_TRIPLES" {
            continue;
        }
        let value = test.props.get(&name).or(test.working_directory.as_ref());
        if let Some(value) = value {
            out.push(b' ');
            test_argument(out, &name);
            out.push(b' ');
            test_argument(out, value);
        }
    }

    out.extend_from_slice(b" _BACKTRACE_TRIPLES ");
    let mut triples = BString::default();
    for (index, frame) in test.backtrace.frames().enumerate() {
        if index != 0 {
            triples.push(b';');
        }
        triples.extend_from_slice(&frame.file);
        triples.push(b';');
        triples.extend_from_slice(frame.line.to_string().as_bytes());
        triples.push(b';');
        if let Some(name) = &frame.name {
            triples.extend_from_slice(name);
        }
    }
    test_argument(out, &triples);
    out.extend_from_slice(b")\n");
}

fn test_argument(out: &mut BString, value: &[u8]) {
    if !value
        .iter()
        .any(|&byte| matches!(byte, b'"' | b'\\' | b'$'))
    {
        quote(out, value);
        return;
    }
    // Leave room for any closing delimiter inside the argument or at its end.
    let width = value
        .split(|&byte| byte == b']')
        .skip(1)
        .map(|part| part.iter().take_while(|&&byte| byte == b'=').count() + 1)
        .max()
        .unwrap_or_default();
    let delimiter = std::iter::repeat_n(b'=', width);
    out.push(b'[');
    out.extend(delimiter.clone());
    out.push(b'[');
    if value.starts_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(value);
    out.push(b']');
    out.extend(delimiter);
    out.push(b']');
}

#[cfg(test)]
mod tests {
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
            script.contains(
                r#"add_test("py" "/opt/python/bin/python3.13" "script.py" "/build/app")"#
            )
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
            props: Default::default(),
            build_depends: Vec::new(),
            kind: rcmake_model::TestKind::Single,
        }
    }
}
