//! `CTestTestfile.cmake` scripts for `ctest`.

use std::collections::HashMap;

use rcmake_base::{BString, ByteSlice, list, path_lex::Lexical};
use rcmake_host::{
    Host,
    path::{from_native, to_native},
};
use rcmake_model::{DirId, ProjectModel, TargetKind, TestEntry};
use rcmake_resolve::{GenerationResult, ResolvedTarget};

use crate::{GenError, configuration_pattern, quote, write_if_changed};

/// Render the test file for a directory. Commands and properties use the values
/// stored in the model.
pub fn render(model: &ProjectModel, dir: DirId) -> Result<BString, GenError> {
    render_file(model, dir, None)
}

/// Render the test file with executable paths and supported command generator
/// expressions resolved for each build configuration.
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
            render_guarded_test(&mut out, test, &test.command);
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
                targets.get(&(config.0.as_slice(), id)).copied()
            };
            let command = resolve_command(test, &config.0, lookup, &dir.binary_dir)?;
            if multi_config {
                out.extend_from_slice(if guarded { b"elseif" } else { b"if" });
                configuration_condition(&mut out, std::slice::from_ref(&config.0));
                guarded = true;
                render_test(&mut out, test, &command, b"  ");
            } else {
                render_guarded_test(&mut out, test, &command);
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

fn resolve_command<'a>(
    test: &TestEntry,
    config: &[u8],
    lookup: impl Fn(&[u8]) -> Option<&'a ResolvedTarget>,
    binary_dir: &[u8],
) -> Result<Vec<BString>, GenError> {
    let mut command = Vec::new();
    for arg in &test.command {
        let value = resolve_argument(arg, config, &lookup).map_err(|message| GenError::Write {
            path: from_native(&to_native(binary_dir).join("CTestTestfile.cmake")),
            message,
        })?;
        if test.command_expand_lists {
            command.extend(list::split(&value, list::Empty::Drop));
        } else {
            command.push(value);
        }
    }
    if let Some(program) = command.first_mut()
        && let Some(target) = lookup(program)
        && target.metadata.kind == TargetKind::Executable
        && let Some(file) = &target.artifacts.file
    {
        *program = file.clone();
    }
    Ok(command)
}

fn resolve_argument<'a>(
    arg: &[u8],
    config: &[u8],
    lookup: &impl Fn(&[u8]) -> Option<&'a ResolvedTarget>,
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
            let file = lookup(name)
                .and_then(|target| target.artifacts.file.as_ref())
                .ok_or_else(|| {
                    BString::from(
                        [
                            b"No target file for ".as_slice(),
                            name,
                            b" in test command.",
                        ]
                        .concat(),
                    )
                })?;
            let path = Lexical::new(file);
            out.extend_from_slice(match op {
                b"TARGET_FILE_NAME" => path.filename(),
                b"TARGET_FILE_DIR" => path.parent(),
                _ => file.as_slice(),
            });
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

fn render_guarded_test(out: &mut BString, test: &TestEntry, command: &[BString]) {
    if test.configurations.is_empty() {
        render_test(out, test, command, b"");
    } else {
        out.extend_from_slice(b"if");
        configuration_condition(out, &test.configurations);
        render_test(out, test, command, b"  ");
        out.extend_from_slice(b"endif()\n");
    }
}

fn render_test(out: &mut BString, test: &TestEntry, command: &[BString], indent: &[u8]) {
    out.extend_from_slice(indent);
    out.extend_from_slice(b"add_test(");
    test_argument(out, &test.name);
    for arg in command {
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
    use rcmake_model::{Target, TargetOrigin};
    use rcmake_resolve::{ArtifactSet, TargetMetadata};

    use super::*;

    #[test]
    fn resolves_executable_and_command_expressions_by_configuration() {
        let mut model = ProjectModel::default();
        let dir = DirId::ROOT;
        model.set_root_dirs("/src".into(), "/build".into());
        let id = model
            .add_target(Target::new(
                "tool".into(),
                TargetKind::Executable,
                TargetOrigin::Normal,
                dir,
                Default::default(),
                Default::default(),
            ))
            .unwrap();
        for (name, old_style, program) in [
            ("direct", false, "tool"),
            ("wrapper", false, "cmake"),
            ("legacy", true, "tool"),
        ] {
            model
                .add_test(TestEntry {
                    name: name.into(),
                    dir,
                    backtrace: Default::default(),
                    command: [
                        program,
                        "-DCOMMAND=$<TARGET_FILE:tool>",
                        "$<TARGET_FILE_NAME:tool>",
                        "$<TARGET_FILE_DIR:tool>",
                        "$<CONFIG>",
                    ]
                    .map(BString::from)
                    .into(),
                    configurations: Vec::new(),
                    working_directory: None,
                    command_expand_lists: false,
                    old_style,
                    props: Default::default(),
                })
                .unwrap();
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
}
