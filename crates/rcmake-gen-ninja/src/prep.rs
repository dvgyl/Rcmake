//! Phony targets that build the inputs of tests and install rules.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, hash::HashAlgo, list, path_lex::Lexical, truth};
use rcmake_gen_common::GenError;
use rcmake_genex::{EvalContext, evaluate, parse};
use rcmake_host::{Follow, Host, path::to_native};
use rcmake_model::{DirId, InstallKind, InstallRule, ProjectModel, TargetKind};
use rcmake_resolve::{
    GenerationResult, InstallAction, InstallActionKind, lower::lower_install_manifest,
    usage::UsageResolver,
};

use crate::generation_error;
use crate::graph::{Graph, qualified};
use crate::syntax::Ninja;

type Targets = BTreeMap<BString, BTreeSet<BString>>;

pub(crate) struct PrepTargets(Targets);

impl PrepTargets {
    pub(crate) fn new(
        graph: &Graph<'_>,
        result: &GenerationResult,
        model: Option<&ProjectModel>,
        host: &dyn Host,
    ) -> Result<Self, GenError> {
        let mut targets = Targets::new();
        if !skip(model, b"CMAKE_SKIP_INSTALL_PREP_TARGETS") {
            collect_install(graph, result, model, &mut targets);
            if let Some(model) = model {
                collect_install_inputs(graph, model, host, &mut targets)?;
                collect_file_set_installs(graph, model, &mut targets);
            }
        }
        if !skip(model, b"CMAKE_SKIP_TEST_PREP_TARGETS") {
            if let Some(model) = model {
                collect_tests(graph, model, &mut targets)?;
            }
            let all = graph.node(b"test_prep/all");
            if available(graph, &all) {
                let tests = targets
                    .keys()
                    .filter(|name| name.starts_with(&graph.node(b"test_prep/")))
                    .map(|name| qualified(name, graph.config))
                    .collect();
                targets.insert(all, tests);
            }
        }
        Ok(Self(targets))
    }

    pub(crate) fn emit(&self, ninja: &mut Ninja, graph: &Graph<'_>) {
        for (name, inputs) in &self.0 {
            let inputs: Vec<_> = inputs.iter().cloned().collect();
            ninja.phony(&qualified(name, graph.config), &inputs);
        }
    }

    /// Write each plain name as an alias of its names in `configs`.
    pub(crate) fn emit_aliases(
        &self,
        ninja: &mut Ninja,
        graph: &Graph<'_>,
        configs: &[&Graph<'_>],
    ) {
        let outputs: BTreeSet<_> = self
            .0
            .keys()
            .map(|name| qualified(name, graph.config))
            .collect();
        for name in self.0.keys() {
            if !outputs.contains(name) {
                let inputs: Vec<_> = configs
                    .iter()
                    .map(|config| qualified(name, config.config))
                    .collect();
                ninja.phony(name, &inputs);
            }
        }
    }

    /// Write the `<name>:all` aliases of the cross configurations.
    pub(crate) fn emit_all_aliases(&self, ninja: &mut Ninja, configs: &[&Graph<'_>]) {
        for name in self.0.keys() {
            let inputs: Vec<_> = configs
                .iter()
                .map(|config| qualified(name, config.config))
                .collect();
            ninja.phony(&qualified(name, Some(b"all")), &inputs);
        }
    }
}

fn skip(model: Option<&ProjectModel>, key: &[u8]) -> bool {
    model
        .and_then(|model| model.definition(DirId::ROOT, key))
        .is_some_and(|value| truth::is_on(value))
}

fn name(prefix: &[u8], suffix: &[u8]) -> BString {
    [prefix, suffix].concat().into()
}

fn available(graph: &Graph<'_>, name: &BString) -> bool {
    !graph.prep_output_is_reserved(name)
        && !graph.prep_output_is_reserved(&qualified(name, graph.config))
}

fn install_component(component: &[u8]) -> bool {
    component != b"all"
        && component.split(|&byte| byte == b'/').all(|segment| {
            !matches!(segment, b"" | b"." | b"..")
                && segment
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.:+-".contains(byte))
        })
}

fn collect_install(
    graph: &Graph<'_>,
    result: &GenerationResult,
    model: Option<&ProjectModel>,
    targets: &mut Targets,
) {
    if result.install.actions.is_empty() && model.is_none_or(|model| model.installs.is_empty()) {
        return;
    }
    if let Some(model) = model {
        for component in model.install_components() {
            add_component(graph, targets, &component);
        }
    }
    for action in &result.install.actions {
        if let Some(deps) = add_component(graph, targets, &action.component) {
            deps.extend(install_dependencies(graph, action));
        }
    }
    let all = BString::from("install_prep/all");
    if !available(graph, &all) {
        return;
    }
    let filtered = model
        .filter(|model| {
            model
                .installs
                .iter()
                .any(|rule| rule.options.exclude_from_all)
        })
        .map(|model| {
            let mut model = model.clone();
            for rule in &mut model.installs {
                if rule.options.exclude_from_all {
                    rule.kind = InstallKind::Code(BString::default());
                }
            }
            lower_install_manifest(&model, &result.targets)
        });
    let manifest = filtered.as_ref().unwrap_or(&result.install);
    // An artifact group can be excluded from the default installation.
    let inputs = manifest
        .actions
        .iter()
        .filter(|action| !action.exclude_from_all)
        .flat_map(|action| install_dependencies(graph, action))
        .collect();
    targets.insert(all, inputs);
}

fn collect_install_inputs(
    graph: &Graph<'_>,
    model: &ProjectModel,
    host: &dyn Host,
    targets: &mut Targets,
) -> Result<(), GenError> {
    let context = EvalContext {
        config: graph.input.config.clone(),
        ..EvalContext::default()
    };
    let mut usage = UsageResolver::new(model);
    let components = model.install_components();
    let all = BString::from("install_prep/all");
    for rule in &model.installs {
        let options = &rule.options;
        if options.build_depends.is_empty()
            || model.dir_excluded_from_all(rule.dir)
            || !options.configurations.is_empty()
                && !options
                    .configurations
                    .iter()
                    .any(|config| context.config.matches(config))
        {
            continue;
        }
        let mut deps = BTreeSet::new();
        let mut genex_host = usage.host(rule.dir);
        for input in &options.build_depends {
            let (value, _) = parse(input)
                .and_then(|expression| evaluate(&expression, &context, &mut genex_host))
                .map_err(|error| generation_error(graph.build_dir, error.to_string()))?;
            for entry in list::split(&value, list::Empty::Drop) {
                if let Some(dependency) = install_dependency(graph, model, host, rule, &entry)? {
                    deps.insert(dependency);
                }
            }
        }
        if available(graph, &all) && !options.exclude_from_all {
            targets
                .entry(all.clone())
                .or_default()
                .extend(deps.iter().cloned());
        }
        let component = options
            .component
            .as_ref()
            .or_else(|| {
                model
                    .definition(rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                    .filter(|value| !value.is_empty())
            })
            .map_or(b"Unspecified".as_slice(), |value| value.as_slice());
        if options.all_components {
            for component in &components {
                if let Some(inputs) = add_component(graph, targets, component) {
                    inputs.extend(deps.iter().cloned());
                }
            }
        } else if let Some(inputs) = add_component(graph, targets, component) {
            inputs.extend(deps);
        }
    }
    Ok(())
}

/// Return each component that an install rule names after `COMPONENT` or
/// `NAMELINK_COMPONENT`, including the components of artifact groups.
fn rule_components(model: &ProjectModel, index: usize) -> Vec<BString> {
    rule_args(model, index)
        .windows(2)
        .filter(|pair| {
            matches!(
                pair.first().map(|key| key.as_slice()),
                Some(b"COMPONENT" | b"NAMELINK_COMPONENT")
            )
        })
        .filter_map(|pair| pair.get(1).map(|value| (*value).clone()))
        .collect()
}

/// Return the recorded arguments of an install rule.
fn rule_args(model: &ProjectModel, index: usize) -> Vec<&BString> {
    let prefix = format!("_RCMAKE_INSTALL_{index}_ARG");
    (0..)
        .map_while(|i| model.global.props.get(format!("{prefix}{i}").as_bytes()))
        .collect()
}

/// The targets that an install step scans for runtime dependencies.
enum Scanned {
    /// The build dependencies of the targets of the rule.
    Targets(BTreeSet<BString>),
    /// The targets of a named runtime dependency set.
    Set(BString),
}

/// Make the components that install runtime dependencies build the targets
/// that the install step scans.
fn collect_runtime_dependency_inputs(
    graph: &Graph<'_>,
    model: &ProjectModel,
    targets: &mut Targets,
) {
    let mut sets = BTreeMap::<BString, BTreeSet<BString>>::new();
    let mut requests = Vec::new();
    for (index, rule) in model.installs.iter().enumerate() {
        let args = rule_args(model, index);
        let named_set = |keyword: &[u8]| {
            args.windows(2)
                .find(|pair| pair.first().is_some_and(|key| key.as_slice() == keyword))
                .and_then(|pair| pair.get(1).map(|value| (*value).clone()))
        };
        let components = || {
            let mut components = rule_components(model, index);
            if components.is_empty() {
                components.push(
                    model
                        .definition(rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                        .filter(|value| !value.is_empty())
                        .cloned()
                        .unwrap_or_else(|| "Unspecified".into()),
                );
            }
            components
        };
        if let InstallKind::Targets {
            targets: installed, ..
        } = &rule.kind
        {
            let deps: BTreeSet<_> = installed
                .iter()
                .filter_map(|id| model.target(*id))
                .filter_map(|target| graph.target_dependency(&target.name))
                .collect();
            if args
                .iter()
                .any(|arg| arg.as_slice() == b"RUNTIME_DEPENDENCIES")
            {
                requests.push((components(), Scanned::Targets(deps.clone())));
            }
            if let Some(set) = named_set(b"RUNTIME_DEPENDENCY_SET") {
                sets.entry(set).or_default().extend(deps);
            }
        } else if args
            .first()
            .is_some_and(|arg| arg.as_slice() == b"RUNTIME_DEPENDENCY_SET")
            && let Some(set) = args.get(1)
        {
            requests.push((components(), Scanned::Set((*set).clone())));
        }
    }
    // A set can receive targets from rules after the rule that installs it.
    for (components, request) in requests {
        let deps = match request {
            Scanned::Set(set) => sets.get(&set).cloned().unwrap_or_default(),
            Scanned::Targets(deps) => deps,
        };
        for component in components {
            if let Some(inputs) = add_component(graph, targets, &component) {
                inputs.extend(deps.iter().cloned());
            }
        }
    }
}

/// Add the components and generated inputs of installed file sets. The
/// install script installs file sets outside of the install manifest.
fn collect_file_set_installs(graph: &Graph<'_>, model: &ProjectModel, targets: &mut Targets) {
    collect_runtime_dependency_inputs(graph, model, targets);
    let mut usage = UsageResolver::new(model);
    let all = BString::from("install_prep/all");
    for (index, rule) in model.installs.iter().enumerate() {
        for component in rule_components(model, index) {
            add_component(graph, targets, &component);
        }
        let InstallKind::Targets {
            targets: installed, ..
        } = &rule.kind
        else {
            continue;
        };
        for target in installed.iter().filter_map(|id| model.target(*id)) {
            for set in target.file_sets.iter().filter_map(|id| model.file_set(*id)) {
                let Some(options) =
                    rcmake_resolve::file_sets::options_for_set(model, index, &set.name)
                else {
                    continue;
                };
                if !options.configurations.is_empty()
                    && !options
                        .configurations
                        .iter()
                        .any(|config| graph.input.config.matches(config))
                {
                    continue;
                }
                let Ok(files) = rcmake_resolve::file_sets::set_files(
                    model,
                    &mut usage,
                    target,
                    set,
                    &graph.input.config,
                ) else {
                    continue;
                };
                let deps: BTreeSet<_> = files
                    .values()
                    .flatten()
                    .filter_map(|(file, _)| graph.file_dependency(file))
                    .collect();
                let component = options
                    .component
                    .as_ref()
                    .or_else(|| {
                        model
                            .definition(rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                            .filter(|value| !value.is_empty())
                    })
                    .map_or(b"Unspecified".as_slice(), |value| value.as_slice());
                if !options.exclude_from_all && available(graph, &all) {
                    targets
                        .entry(all.clone())
                        .or_default()
                        .extend(deps.iter().cloned());
                }
                if let Some(inputs) = add_component(graph, targets, component) {
                    inputs.extend(deps);
                }
            }
        }
    }
}

fn install_dependency(
    graph: &Graph<'_>,
    model: &ProjectModel,
    host: &dyn Host,
    rule: &InstallRule,
    entry: &[u8],
) -> Result<Option<BString>, GenError> {
    if let Some(target) = model
        .find_target(rule.dir, entry)
        .and_then(|id| model.target(id))
    {
        return Ok(graph.target_dependency(&target.name));
    }
    let directory = model.dir(rule.dir);
    let source = Lexical::new(&Lexical::new(&directory.source_dir).append(entry)).normalize();
    if host.metadata(&to_native(&source), Follow::Yes).is_ok() {
        return Ok(Some(graph.node(&source)));
    }
    let binary = Lexical::new(&Lexical::new(&directory.binary_dir).append(entry)).normalize();
    if let Some(file) = graph.file_dependency(&binary) {
        return Ok(Some(file));
    }
    if host.metadata(&to_native(&binary), Follow::Yes).is_ok() {
        return Ok(Some(graph.node(&binary)));
    }
    let signature = match rule.kind {
        InstallKind::Files(_) => "FILES",
        InstallKind::Programs(_) => "PROGRAMS",
        InstallKind::Directory(_) => "DIRECTORY",
        InstallKind::Script(_) => "SCRIPT",
        InstallKind::Code(_) => "CODE",
        _ => return Ok(None),
    };
    let diagnostic = rcmake_diag::Diagnostic { annotations: None,
        kind: rcmake_diag::MessageKind::Error,
        effect: rcmake_diag::Effect::SetError,
        category: None,
        policy: None,
        location: rcmake_diag::Location::Backtrace(rule.backtrace.clone()),
        body: format!(
            "install({signature}) BUILD_DEPENDS entry \"{}\" is neither a target nor a file that exists or is generated by the build.",
            String::from_utf8_lossy(entry)
        )
        .into(),
    };
    Err(GenError::Invalid(rcmake_diag::render(
        &diagnostic,
        &rcmake_diag::RenderOptions {
            relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
            ..Default::default()
        },
    )))
}

fn add_component<'a>(
    graph: &Graph<'_>,
    targets: &'a mut Targets,
    component: &[u8],
) -> Option<&'a mut BTreeSet<BString>> {
    if !install_component(component) {
        return None;
    }
    let name = name(b"install_prep/", component);
    available(graph, &name).then(|| targets.entry(name).or_default())
}

fn install_dependencies<'a>(
    graph: &'a Graph<'_>,
    action: &'a InstallAction,
) -> impl Iterator<Item = BString> + 'a {
    let files = match &action.kind {
        InstallActionKind::Files { files, .. } => files.as_slice(),
        InstallActionKind::Script(script) => std::slice::from_ref(script),
        _ => &[],
    };
    files
        .iter()
        .filter(|_| {
            action.configurations.is_empty()
                || action
                    .configurations
                    .iter()
                    .any(|config| graph.input.config.matches(config))
        })
        .filter_map(|file| graph.file_dependency(file))
}

fn collect_tests(
    graph: &Graph<'_>,
    model: &ProjectModel,
    targets: &mut Targets,
) -> Result<(), GenError> {
    let context = EvalContext {
        config: graph.input.config.clone(),
        ..EvalContext::default()
    };
    let mut usage = UsageResolver::new(model);
    for (dir, directory) in model.dirs() {
        let hash = HashAlgo::Md5.hex_digest(&directory.binary_dir);
        let directory_target = name(
            b"test_prep/directory_",
            hash.as_bytes().get(..8).unwrap_or_default(),
        );
        let mut directory_deps = BTreeSet::new();
        let mut host = usage.host(dir);
        let mut expand = |value: &[u8]| {
            parse(value)
                .and_then(|expression| evaluate(&expression, &context, &mut host))
                .map(|(value, _)| value)
                .map_err(|error| generation_error(graph.build_dir, error.to_string()))
        };
        for (_, test) in model.dir_tests(dir) {
            let target = name(b"test_prep/", &test.name);
            if !test.is_discovery()
                && (test.name == "all"
                    || test.name.is_empty()
                    || !test
                        .name
                        .iter()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(byte))
                    || !available(graph, &target))
            {
                continue;
            }
            let mut deps = BTreeSet::new();
            for (index, arg) in test.command.iter().enumerate() {
                let value = expand(arg)?;
                if index == 0
                    && let Some(id) = model.find_target(dir, &value)
                    && let Some(executable) = model
                        .target(id)
                        .filter(|target| target.kind == TargetKind::Executable)
                    && !executable.is_imported()
                    && let Some(dep) = graph.target_dependency(&executable.name)
                {
                    deps.insert(dep);
                }
                if arg.windows(2).any(|pair| pair == b"$<")
                    && let Some(dep) = graph.file_dependency(&value)
                {
                    deps.insert(dep);
                }
            }
            for input in &test.build_depends {
                for value in list::split(&expand(&input.value)?, list::Empty::Drop) {
                    deps.extend(test_dependency(graph, model, dir, &value));
                }
            }
            if test.is_discovery() {
                directory_deps.extend(deps);
            } else {
                targets.entry(target).or_default().extend(deps);
            }
        }
        if let Some(inputs) = directory.props.get(b"CMAKE_TEST_BUILD_DEPENDS") {
            for input in list::split(&expand(inputs)?, list::Empty::Drop) {
                directory_deps.extend(test_dependency(graph, model, dir, &input));
            }
        }
        if !directory_deps.is_empty() && available(graph, &directory_target) {
            targets
                .entry(directory_target)
                .or_default()
                .extend(directory_deps);
        }
    }
    Ok(())
}

fn test_dependency(
    graph: &Graph<'_>,
    model: &ProjectModel,
    dir: DirId,
    input: &[u8],
) -> Option<BString> {
    if let Some(target) = model
        .find_target(dir, input)
        .and_then(|id| model.target(id))
    {
        return graph.target_dependency(&target.name);
    }
    let directory = model.dir(dir);
    let binary = Lexical::new(&directory.binary_dir).append(input);
    graph.file_dependency(&binary).or_else(|| {
        let source = Lexical::new(&directory.source_dir).append(input);
        Some(graph.node(&source))
    })
}

#[cfg(test)]
mod tests {
    use rcmake_model::ProjectModel;

    use super::rule_components;

    #[test]
    fn rule_components_include_artifact_group_components() {
        let mut model = ProjectModel::default();
        for (index, value) in [
            "TARGETS",
            "lib",
            "ARCHIVE",
            "DESTINATION",
            "lib",
            "COMPONENT",
            "Archive",
            "FILE_SET",
            "HEADERS",
            "COMPONENT",
            "Headers",
            "LIBRARY",
            "NAMELINK_COMPONENT",
            "Development",
        ]
        .into_iter()
        .enumerate()
        {
            model
                .global
                .props
                .set(format!("_RCMAKE_INSTALL_0_ARG{index}").into(), value.into());
        }
        let components = rule_components(&model, 0);
        assert_eq!(components, ["Archive", "Headers", "Development"]);
        assert!(rule_components(&model, 1).is_empty());
    }
}
