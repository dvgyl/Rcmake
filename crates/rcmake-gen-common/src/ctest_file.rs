//! `CTestTestfile.cmake` scripts for `ctest`.

use std::collections::{BTreeSet, HashMap};

use rcmake_base::{BString, ByteSlice, list, path_lex::Lexical, truth};
use rcmake_diag::{
    DiagCategory, Diagnostic, Effect, Location, MessageKind, PolicyId, PolicyStatus, RenderOptions,
    render as render_diagnostic,
};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::{
    Host, Stream,
    path::{from_native, to_native},
};
use rcmake_model::{
    BT, DirId, DiscoverySpec, ProjectModel, Target, TargetKind, TestEntry, TestKind,
};
use rcmake_resolve::{GenerationResult, lower::compute_artifacts, usage::UsageResolver};

use crate::{
    GenError, configuration_pattern, genex, quote, script_argument as test_argument,
    write_if_changed,
};

mod test_command;

use test_command::{resolve_argument, resolve_discovery, resolve_test, resolve_test_program};

/// Render the test file for a directory. Commands and properties use the values
/// stored in the model.
pub fn render(model: &ProjectModel, dir: DirId) -> Result<BString, GenError> {
    render_file(model, dir, None, None)
}

/// Render executable paths and supported generator expressions in test commands
/// and properties for each build configuration.
pub fn render_resolved(
    model: &ProjectModel,
    dir: DirId,
    result: &GenerationResult,
) -> Result<BString, GenError> {
    render_file(model, dir, Some(result), None)
}

/// Render tests and resolve relative file dependencies against the source tree.
pub fn render_resolved_with_host(
    model: &ProjectModel,
    dir: DirId,
    result: &GenerationResult,
    host: &dyn Host,
) -> Result<BString, GenError> {
    if dir == DirId::ROOT {
        warn_fixture_defaults(model, host);
    }
    render_file(model, dir, Some(result), Some(host))
}

fn render_file(
    model: &ProjectModel,
    dir: DirId,
    result: Option<&GenerationResult>,
    host: Option<&dyn Host>,
) -> Result<BString, GenError> {
    let targets: HashMap<_, _> = result
        .into_iter()
        .flat_map(|result| &result.targets)
        .map(|target| ((target.config.0.as_slice(), target.id), target))
        .collect();
    let multi_config = model.generator.is_some_and(|kind| kind.multi_config());
    let dir_id = dir;
    let dir = model.dir(dir_id);
    let mut out = BString::from("# CMake generated Testfile for \n");
    comment(&mut out, b"# Source directory: ", &dir.source_dir);
    comment(&mut out, b"# Build directory: ", &dir.binary_dir);
    out.extend_from_slice(
        b"# \n# This file includes the relevant testing commands required for \n\
          # testing this directory and lists subdirectories to be tested as well.\n",
    );
    let mut usage = UsageResolver::new(model);
    let configs = result.map_or(&[][..], |result| result.configs.as_slice());
    let multi_config = multi_config || configs.len() > 1;
    render_includes(&mut out, model, dir_id, &mut usage, configs, multi_config)?;
    for (_, test) in model.dir_tests(dir_id) {
        let policy = PolicyId::new(110);
        let quoting = dir
            .final_policies
            .map_or(PolicyStatus::New, |policies| policies.get(policy));
        let name = |out: &mut BString| {
            if quoting == PolicyStatus::New {
                test_argument(out, &test.name);
            } else {
                out.extend_from_slice(&test.name);
            }
        };
        if quoting == PolicyStatus::Warn
            && let Some(host) = host
            && test
                .name
                .iter()
                .any(|byte| b"$[] #;\t\n\"\\".contains(byte))
        {
            warn_policy(
                model,
                host,
                policy,
                Location::FileLine {
                    file: Lexical::new(&dir.source_dir)
                        .append(b"CMakeLists.txt")
                        .as_slice()
                        .into(),
                    line: 0,
                },
                "",
                &format!(
                    "The following name given to add_test() is invalid if CMP0110 is not set or set to OLD:\n  `{}´\n",
                    test.name
                ),
            );
        }
        let Some(result) = result.filter(|r| !test.old_style && !r.configs.is_empty()) else {
            render_guarded_test(&mut out, test, name);
            continue;
        };
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
            let context = EvalContext {
                config: config.clone(),
                ..EvalContext::default()
            };
            let mut evaluate = |value: &[u8]| genex(&mut usage, test.dir, &context, value, None);
            let mut command_usage = UsageResolver::new(model);
            let mut resolved = resolve_test(
                test,
                |value| {
                    if test.is_discovery() {
                        genex(&mut command_usage, test.dir, &context, value, None)
                    } else {
                        resolve_argument(value, &config.0, &lookup).map_err(GenError::Invalid)
                    }
                },
                &mut evaluate,
            )?;
            resolve_test_program(model, &mut resolved, lookup, &mut evaluate, host)?;
            if let TestKind::Discover(spec) = &mut resolved.kind {
                resolve_discovery(spec, &mut evaluate)?;
            }
            let mut dependencies = BTreeSet::new();
            let program = test
                .command
                .first()
                .map(|value| resolve_argument(value, &config.0, &lookup))
                .transpose()
                .map_err(GenError::Invalid)?;
            if let Some((target, Some(file))) = program.as_ref().and_then(|name| lookup(name))
                && target.kind == TargetKind::Executable
                && !target.is_imported()
            {
                dependencies.insert(file);
            }
            for argument in &test.command {
                let mut remaining = argument.as_slice();
                while let Some((_, suffix)) = remaining.split_once_str(b"$<TARGET_FILE:")
                    && let Some((name, tail)) = suffix.split_once_str(b">")
                {
                    if let Some((target, Some(file))) = lookup(name)
                        && !target.is_imported()
                        && resolved
                            .command
                            .iter()
                            .any(|argument| argument.contains_str(&file))
                    {
                        dependencies.insert(file);
                    }
                    remaining = tail;
                }
            }
            for entry in &test.build_depends {
                for value in list::split(&evaluate(&entry.value)?, list::Empty::Drop) {
                    if let Some((target, file)) = lookup(&value) {
                        if !target.is_imported() {
                            dependencies.extend(file);
                            if target.kind == TargetKind::ObjectLibrary
                                && let Some(id) = model.find_target(test.dir, &value)
                                && let Some(entry) = targets.get(&(config.0.as_slice(), id))
                            {
                                let root = Lexical::new(&model.dir(DirId::ROOT).binary_dir);
                                dependencies.extend(
                                    entry
                                        .compile_units
                                        .iter()
                                        .map(|unit| root.append(&unit.object)),
                                );
                            }
                        }
                    } else {
                        let source = Lexical::new(&dir.source_dir).append(&value);
                        let file = if host.is_some_and(|host| {
                            host.metadata(&to_native(&source), rcmake_host::Follow::Yes)
                                .is_ok()
                        }) {
                            source
                        } else {
                            Lexical::new(&dir.binary_dir).append(&value)
                        };
                        dependencies.insert(Lexical::new(&file).normalize());
                    }
                }
            }
            resolved.build_depends = dependencies
                .into_iter()
                .map(|value| BT::new(value, test.backtrace.clone()))
                .collect();
            if !resolved.is_discovery()
                && resolved.props.get(b"_CMAKE_TEST_BUILD_DEPENDS").is_none()
            {
                resolved
                    .props
                    .set("_CMAKE_TEST_BUILD_DEPENDS".into(), BString::default());
            }
            if multi_config {
                out.extend_from_slice(if guarded { b"elseif" } else { b"if" });
                configuration_condition(&mut out, std::slice::from_ref(&config.0));
                guarded = true;
                render_test(&mut out, &resolved, b"  ", name);
            } else {
                render_guarded_test(&mut out, &resolved, name);
            }
        }
        if guarded {
            if test.configurations.is_empty() && !test.is_discovery() {
                out.extend_from_slice(b"else()\n  add_test(");
                name(&mut out);
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
        test_argument(
            &mut out,
            &rcmake_base::path_lex::cmake_style(&from_native(path)),
        );
        out.extend_from_slice(b")\n");
    }
    let labels = list::join(
        dir.props
            .get(b"LABELS")
            .into_iter()
            .chain(model.definition(dir_id, b"CMAKE_DIRECTORY_LABELS"))
            .filter(|labels| !labels.is_empty()),
    );
    if !labels.is_empty() {
        out.extend_from_slice(b"set_directory_properties(PROPERTIES LABELS ");
        test_argument(&mut out, &labels);
        out.extend_from_slice(b")\n");
    }
    Ok(out)
}

fn render_includes(
    out: &mut BString,
    model: &ProjectModel,
    dir: DirId,
    usage: &mut UsageResolver<'_>,
    configs: &[ConfigId],
    multi_config: bool,
) -> Result<(), GenError> {
    let props = &model.dir(dir).props;
    let includes = props.get(b"TEST_INCLUDE_FILES");
    for item in includes
        .into_iter()
        .flat_map(|value| list::split(value, list::Empty::Drop))
    {
        let render = |out: &mut BString, value: &[u8], indent: &[u8]| {
            for path in list::split(value, list::Empty::Drop) {
                out.extend_from_slice(indent);
                out.extend_from_slice(b"include(");
                test_argument(out, &path);
                out.extend_from_slice(b")\n");
            }
        };
        if rcmake_genex::find_genex(&item).is_none() {
            out.extend_from_slice(b"include(");
            test_argument(out, &item);
            out.extend_from_slice(b")\n");
            continue;
        }
        let fallback = [ConfigId(
            model
                .definition(dir, b"CMAKE_BUILD_TYPE")
                .cloned()
                .unwrap_or_default(),
        )];
        let configs = if configs.is_empty() {
            &fallback
        } else {
            configs
        };
        let values = configs
            .iter()
            .map(|config| {
                genex(
                    usage,
                    dir,
                    &EvalContext {
                        config: config.clone(),
                        ..EvalContext::default()
                    },
                    &item,
                    None,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !multi_config || values.iter().all(|value| Some(value) == values.first()) {
            if let Some(value) = values.first() {
                render(out, value, b"");
            }
            continue;
        }
        let mut guarded = false;
        for (config, value) in configs
            .iter()
            .zip(values)
            .filter(|(_, value)| !value.is_empty())
        {
            out.extend_from_slice(if guarded { b"elseif" } else { b"if" });
            configuration_condition(out, std::slice::from_ref(&config.0));
            render(out, &value, b"  ");
            guarded = true;
        }
        if guarded {
            out.extend_from_slice(b"endif()\n");
        }
    }
    Ok(())
}

fn fixture_default_applies(test: &TestEntry) -> bool {
    !test.is_discovery()
        && test.props.get(b"FIXTURE_REPEAT_MODE").is_none()
        && (test.props.get(b"FIXTURES_SETUP").is_some()
            || test.props.get(b"FIXTURES_CLEANUP").is_some())
}

fn warn_fixture_defaults(model: &ProjectModel, host: &dyn Host) {
    let policy = PolicyId::new(224);
    let tests: BTreeSet<_> = model
        .dirs()
        .filter(|(dir, _)| {
            model
                .definition(*dir, b"CMAKE_POLICY_WARNING_CMP0224")
                .is_some_and(|value| truth::is_on(value))
        })
        .flat_map(|(dir, _)| model.dir_tests(dir))
        .map(|(_, test)| test)
        .filter(|test| {
            fixture_default_applies(test) && test.policies.get(policy) == PolicyStatus::Warn
        })
        .map(|test| test.name.to_string())
        .collect();
    if !tests.is_empty() {
        let names = tests.into_iter().collect::<Vec<_>>().join("\n ");
        warn_policy(
            model,
            host,
            policy,
            Location::None,
            "",
            &format!(
                "For compatibility with older versions of CMake, the fixtures of the following tests will use EACH_TEST_SEPARATELY mode:\n {names}\n"
            ),
        );
    }
}

fn warn_policy(
    model: &ProjectModel,
    host: &dyn Host,
    id: PolicyId,
    location: Location,
    preface: &str,
    postface: &str,
) {
    if model
        .definition(DirId::ROOT, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
        .is_some_and(|value| truth::is_on(value))
    {
        return;
    }
    let summary = if id == PolicyId::new(224) {
        rcmake_diag::CMP0224_SUMMARY.into()
    } else {
        id.info().map_or("", |info| info.summary).replace("``", "")
    };
    let policy = format!(
        "Policy {id} is not set: {summary}  Run \"cmake --help-policy {id}\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning."
    );
    let parts = [preface, &policy, postface]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let diagnostic = Diagnostic {
        kind: MessageKind::AuthorWarning,
        effect: Effect::None,
        category: Some(DiagCategory::Policy),
        policy: Some(id),
        location,
        body: parts
            .join(if id == PolicyId::new(110) || id == PolicyId::new(224) {
                "\n\n"
            } else {
                "\n"
            })
            .into(),
    };
    host.write_out(
        Stream::Stderr,
        &render_diagnostic(
            &diagnostic,
            &RenderOptions {
                relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                ..Default::default()
            },
        ),
    );
}

/// Write the test file to the directory's existing binary directory.
pub fn write_ctest_file(model: &ProjectModel, dir: DirId, host: &dyn Host) -> Result<(), GenError> {
    let content = render_file(model, dir, None, Some(host))?;
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

fn render_guarded_test(out: &mut BString, test: &TestEntry, name: impl Fn(&mut BString)) {
    if test.configurations.is_empty() {
        render_test(out, test, b"", name);
    } else {
        out.extend_from_slice(b"if");
        configuration_condition(out, &test.configurations);
        render_test(out, test, b"  ", name);
        out.extend_from_slice(b"endif()\n");
    }
}

fn render_test(out: &mut BString, test: &TestEntry, indent: &[u8], name: impl Fn(&mut BString)) {
    if let TestKind::Discover(spec) = &test.kind {
        render_discovery(out, test, spec, indent);
        return;
    }
    out.extend_from_slice(indent);
    out.extend_from_slice(b"add_test(");
    name(out);
    for arg in &test.command {
        out.push(b' ');
        test_argument(out, arg);
    }
    out.extend_from_slice(b")\n");
    out.extend_from_slice(indent);
    out.extend_from_slice(b"set_tests_properties(");
    name(out);
    out.extend_from_slice(b" PROPERTIES ");

    let mut names = test.props.names();
    if test.working_directory.is_some() && test.props.get(b"WORKING_DIRECTORY").is_none() {
        names.push("WORKING_DIRECTORY".into());
    }
    if !test.build_depends.is_empty() && test.props.get(b"_CMAKE_TEST_BUILD_DEPENDS").is_none() {
        names.push("_CMAKE_TEST_BUILD_DEPENDS".into());
    }
    names.sort_unstable();
    let dependencies = list::join(test.build_depends.iter().map(|entry| &entry.value));
    for name in names {
        if name == b"_BACKTRACE_TRIPLES" {
            continue;
        }
        if name == b"_CMAKE_TEST_BUILD_DEPENDS" {
            let value = if dependencies.is_empty() {
                test.props
                    .get(&name)
                    .map_or(&[][..], |value| value.as_slice())
            } else {
                &dependencies
            };
            out.extend_from_slice(b" _CMAKE_TEST_BUILD_DEPENDS ");
            test_argument(out, value);
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

    if test.policies.get(PolicyId::new(224)) == PolicyStatus::New && fixture_default_applies(test) {
        out.extend_from_slice(b" _CMAKE_DEFAULT_FIXTURE_REPEAT_MODE AROUND_EACH_REPEAT");
    }
    out.extend_from_slice(b" _BACKTRACE_TRIPLES ");
    test_argument(out, &backtrace_triples(test));
    out.extend_from_slice(b")\n");
}

fn backtrace_triples(test: &TestEntry) -> BString {
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
    triples
}

fn render_discovery(out: &mut BString, test: &TestEntry, spec: &DiscoverySpec, indent: &[u8]) {
    out.extend_from_slice(indent);
    out.extend_from_slice(b"discover_tests(COMMAND");
    discovery_args(out, &test.command);
    discovery_line(out, indent, b"DISCOVERY_ARGS");
    discovery_args(out, &spec.discovery_args);
    discovery_line(out, indent, b"DISCOVERY_MATCH ");
    test_argument(out, &spec.discovery_match);
    if !spec.discovery_props.is_empty() {
        discovery_properties(out, indent, b"DISCOVERY_PROPERTIES", &spec.discovery_props);
    }
    discovery_line(out, indent, b"TEST_NAME ");
    test_argument(out, &spec.test_name);
    discovery_line(out, indent, b"TEST_ARGS");
    discovery_args(out, &spec.test_args);
    discovery_properties(out, indent, b"TEST_PROPERTIES", &spec.test_props);
    discovery_line(out, indent, b"  _BACKTRACE_TRIPLES ");
    test_argument(out, &backtrace_triples(test));
    if !test.build_depends.is_empty() {
        discovery_line(out, indent, b"BUILD_DEPENDS");
        discovery_args(out, test.build_depends.iter().map(|entry| &entry.value));
    }
    discovery_line(out, indent, b")\n");
}

fn discovery_line(out: &mut BString, indent: &[u8], text: &[u8]) {
    out.push(b'\n');
    out.extend_from_slice(indent);
    out.extend_from_slice(b"  ");
    out.extend_from_slice(text);
}

fn discovery_args<'a>(out: &mut BString, values: impl IntoIterator<Item = &'a BString>) {
    for value in values {
        out.push(b' ');
        test_argument(out, value);
    }
}

fn discovery_properties(
    out: &mut BString,
    indent: &[u8],
    label: &[u8],
    properties: &[(BString, BString)],
) {
    discovery_line(out, indent, label);
    for (key, value) in properties {
        discovery_line(out, indent, b"  ");
        test_argument(out, key);
        out.push(b' ');
        test_argument(out, value);
    }
}

#[cfg(test)]
mod tests;
