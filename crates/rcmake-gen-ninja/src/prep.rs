//! Phony targets that build the inputs of tests and install rules.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, hash::HashAlgo, list, path_lex::Lexical, truth};
use rcmake_gen_common::GenError;
use rcmake_genex::{EvalContext, evaluate, parse};
use rcmake_host::{Follow, Host, path::to_native};
use rcmake_model::{DirId, InstallKind, InstallRule, ProjectModel, TargetKind};
use rcmake_resolve::{
    GenerationResult, InstallAction, InstallActionKind, InstallEntry, InstallManifest,
    lower::lower_install_manifest, usage::UsageResolver,
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

fn action_directories(manifest: &InstallManifest) -> Vec<Option<DirId>> {
    let mut dirs = vec![None; manifest.actions.len()];
    for (&dir, entries) in &manifest.directories {
        for entry in entries {
            if let InstallEntry::Actions(range) = entry {
                for slot in dirs.get_mut(range.clone()).into_iter().flatten() {
                    *slot = Some(dir);
                }
            }
        }
    }
    dirs
}

fn is_empty_install_action(action: &InstallAction) -> bool {
    matches!(&action.kind, InstallActionKind::Files { files, .. } if files.is_empty())
        || matches!(&action.kind, InstallActionKind::Directory { dirs, .. } if dirs.is_empty())
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
    let dirs = action_directories(&result.install);
    for (index, action) in result.install.actions.iter().enumerate() {
        if is_empty_install_action(action) {
            continue;
        }
        let excluded_dir = model.is_some_and(|model| {
            dirs.get(index)
                .copied()
                .flatten()
                .is_some_and(|dir| model.dir_excluded_from_all(dir))
        });
        if let Some(deps) = add_component(graph, targets, &action.component)
            && !excluded_dir
        {
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
    let manifest_dirs = if filtered.is_some() {
        action_directories(manifest)
    } else {
        dirs
    };
    // An artifact group or directory can be excluded from the default installation.
    let inputs = manifest
        .actions
        .iter()
        .enumerate()
        .filter(|(index, action)| {
            !action.exclude_from_all
                && !model.is_some_and(|model| {
                    manifest_dirs
                        .get(*index)
                        .copied()
                        .flatten()
                        .is_some_and(|dir| model.dir_excluded_from_all(dir))
                })
        })
        .flat_map(|(_, action)| install_dependencies(graph, action))
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
            for (target_name, inputs) in targets.iter_mut() {
                if target_name.starts_with(b"install_prep/") && target_name != &all {
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

/// Collect build-system dependencies for `id`, walking `utility_deps` through
/// imported or non-buildsystem targets (`cmGlobalGenerator::GetInstallPrepTargets`).
fn target_build_deps(
    graph: &Graph<'_>,
    model: &ProjectModel,
    id: rcmake_model::TargetId,
    visited: &mut BTreeSet<rcmake_model::TargetId>,
    out: &mut BTreeSet<BString>,
) {
    if !visited.insert(id) {
        return;
    }
    let Some(target) = model.target(id) else {
        return;
    };
    if target.is_imported() || !model.is_in_build_system(target) {
        for dep in &target.utility_deps {
            if let Some(dep_id) = model.find_target(target.dir, &dep.value) {
                target_build_deps(graph, model, dep_id, visited, out);
            }
        }
        return;
    }
    for node in graph.target_dependencies(&target.name) {
        out.insert(node);
    }
}

fn imported_runtime_artifact_targets<'a>(args: &[&'a BString]) -> Vec<&'a [u8]> {
    const KEYWORDS: &[&[u8]] = &[
        b"RUNTIME_DEPENDENCY_SET",
        b"RUNTIME_DEPENDENCIES",
        b"RUNTIME",
        b"LIBRARY",
        b"FRAMEWORK",
        b"BUNDLE",
        b"DESTINATION",
        b"COMPONENT",
        b"CONFIGURATIONS",
        b"PERMISSIONS",
        b"OPTIONAL",
        b"EXCLUDE_FROM_ALL",
        b"NAMELINK_COMPONENT",
        b"NAMELINK_ONLY",
        b"NAMELINK_SKIP",
        b"PRE_INCLUDE_REGEXES",
        b"PRE_EXCLUDE_REGEXES",
        b"POST_INCLUDE_REGEXES",
        b"POST_EXCLUDE_REGEXES",
        b"DIRECTORIES",
    ];
    args.get(1..)
        .unwrap_or_default()
        .iter()
        .map(|arg| arg.as_slice())
        .take_while(|arg| !KEYWORDS.contains(arg))
        .collect()
}

/// Make the components that install runtime dependencies build the targets
/// that the install step scans.
fn collect_runtime_dependency_inputs(
    graph: &Graph<'_>,
    model: &ProjectModel,
    targets: &mut Targets,
) {
    let mut sets = BTreeMap::<BString, BTreeSet<BString>>::new();
    let all = BString::from("install_prep/all");
    let mut add_filtered_options =
        |dir: DirId, options: &rcmake_model::InstallOptions, deps: &BTreeSet<BString>| {
            if !options.configurations.is_empty()
                && !options
                    .configurations
                    .iter()
                    .any(|config| graph.input.config.matches(config))
            {
                return;
            }
            if !options.exclude_from_all && available(graph, &all) {
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
                        .definition(dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                        .filter(|value| !value.is_empty())
                })
                .map_or(b"Unspecified".as_slice(), |value| value.as_slice());
            if let Some(inputs) = add_component(graph, targets, component) {
                inputs.extend(deps.iter().cloned());
            }
        };
    for (index, rule) in model.installs.iter().enumerate() {
        let args = rule_args(model, index);
        let named_set = |keyword: &[u8]| {
            args.windows(2)
                .find(|pair| pair.first().is_some_and(|key| key.as_slice() == keyword))
                .and_then(|pair| pair.get(1).map(|value| (*value).clone()))
        };
        if let InstallKind::Targets {
            targets: installed, ..
        } = &rule.kind
        {
            let mut deps = BTreeSet::new();
            let mut visited = BTreeSet::new();
            for &id in installed {
                target_build_deps(graph, model, id, &mut visited, &mut deps);
            }
            if !model.dir_excluded_from_all(rule.dir) {
                for options in
                    rcmake_resolve::lower::unnamed_runtime_dependency_groups(model, index)
                {
                    add_filtered_options(rule.dir, &options, &deps);
                }
            }
            if let Some(set) = named_set(b"RUNTIME_DEPENDENCY_SET") {
                sets.entry(set).or_default().extend(deps);
            }
        } else if args
            .first()
            .is_some_and(|arg| arg.as_slice() == b"IMPORTED_RUNTIME_ARTIFACTS")
        {
            let mut deps = BTreeSet::new();
            let mut visited = BTreeSet::new();
            for name in imported_runtime_artifact_targets(&args) {
                if let Some(id) = model.find_target(rule.dir, name) {
                    target_build_deps(graph, model, id, &mut visited, &mut deps);
                }
            }
            if let Some(set) = named_set(b"RUNTIME_DEPENDENCY_SET") {
                sets.entry(set).or_default().extend(deps);
            }
        }
    }
    for install in &model.runtime_dependency_installs {
        if model.dir_excluded_from_all(install.dir) {
            continue;
        }
        let Some(deps) = sets.get(&install.name) else {
            continue;
        };
        for options in std::iter::once(&install.library).chain(install.framework.as_ref()) {
            add_filtered_options(install.dir, options, deps);
        }
    }
}

/// Emit author warnings for install rules with build dependencies whose
/// components cannot produce an `install_prep/` target (`cmGlobalGenerator::CheckInstallPrepTargets`).
pub(crate) fn warn_install_prep_components(
    graphs: &[&Graph<'_>],
    model: Option<&ProjectModel>,
    host: &dyn Host,
) {
    let Some(model) = model else { return };
    if skip(Some(model), b"CMAKE_SKIP_INSTALL_PREP_TARGETS") || graphs.is_empty() {
        return;
    }
    let root = model.dir(DirId::ROOT).source_dir.clone();
    let mut warned_components = BTreeSet::<BString>::new();
    let emit_warning = |dir: DirId, backtrace: &rcmake_diag::Backtrace, body: String| {
        if model
            .definition(dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
            .is_some_and(|value| truth::is_on(value))
        {
            return;
        }
        let diagnostic = rcmake_diag::Diagnostic {
            annotations: None,
            kind: rcmake_diag::MessageKind::AuthorWarning,
            effect: rcmake_diag::Effect::None,
            category: Some(rcmake_diag::DiagCategory::Author),
            policy: None,
            location: rcmake_diag::Location::Backtrace(backtrace.clone()),
            body: body.into(),
        };
        let text = rcmake_diag::render(
            &diagnostic,
            &rcmake_diag::RenderOptions {
                relative_base: Some(root.clone()),
                ..Default::default()
            },
        );
        host.write_out(rcmake_host::Stream::Stderr, &text);
    };
    let mut check_component =
        |dir: DirId, backtrace: &rcmake_diag::Backtrace, component: &[u8], exclude: bool| {
            if component == b"all" {
                if exclude {
                    emit_warning(
                        dir,
                        backtrace,
                        "An install rule in the component named \"all\" uses EXCLUDE_FROM_ALL and \
                         has build dependencies, but no install_prep/ target builds them: \
                         install_prep/all omits EXCLUDE_FROM_ALL rules, and the \"all\" component \
                         has no preparation target of its own.  Rename the component or remove \
                         EXCLUDE_FROM_ALL."
                            .to_owned(),
                    );
                }
            } else if !install_component(component) && warned_components.insert(component.into()) {
                emit_warning(
                    dir,
                    backtrace,
                    format!(
                        "The install component \"{}\" has build dependencies, but its name cannot \
                         be used in a build target name with this generator.  No \"install_prep/\" \
                         target will be generated for it.  Rename the component to build its \
                         dependencies with an \"install_prep/\" target.",
                        BString::from(component)
                    ),
                );
            }
        };

    for (index, rule) in model.installs.iter().enumerate() {
        if model.dir_excluded_from_all(rule.dir) || rule.options.all_components {
            continue;
        }
        let has_deps = match &rule.kind {
            InstallKind::Targets { targets, .. } => targets.iter().any(|&id| {
                let mut visited = BTreeSet::new();
                let mut deps = BTreeSet::new();
                for graph in graphs {
                    target_build_deps(graph, model, id, &mut visited, &mut deps);
                }
                !deps.is_empty()
            }),
            _ => !rule.options.build_depends.is_empty(),
        };
        if !has_deps {
            continue;
        }
        let mut components = rule_components(model, index);
        if components.is_empty() {
            components.push(
                rule.options
                    .component
                    .clone()
                    .or_else(|| {
                        model
                            .definition(rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                            .filter(|value| !value.is_empty())
                            .cloned()
                    })
                    .unwrap_or_else(|| "Unspecified".into()),
            );
        }
        let args = rule_args(model, index);
        let exclude = rule.options.exclude_from_all
            || args.iter().any(|arg| arg.as_slice() == b"EXCLUDE_FROM_ALL");
        for component in components {
            check_component(rule.dir, &rule.backtrace, &component, exclude);
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
        let InstallKind::Targets {
            targets: installed, ..
        } = &rule.kind
        else {
            continue;
        };
        let excluded_dir = model.dir_excluded_from_all(rule.dir);
        for target in installed.iter().filter_map(|id| model.target(*id)) {
            for set in target.file_sets.iter().filter_map(|id| model.file_set(*id)) {
                let Some(options) =
                    rcmake_resolve::file_sets::options_for_set(model, index, &set.name)
                else {
                    continue;
                };
                let component = options
                    .component
                    .as_ref()
                    .or_else(|| {
                        model
                            .definition(rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                            .filter(|value| !value.is_empty())
                    })
                    .map_or(b"Unspecified".as_slice(), |value| value.as_slice());
                let config_matches = options.configurations.is_empty()
                    || options
                        .configurations
                        .iter()
                        .any(|config| graph.input.config.matches(config));
                if !config_matches || excluded_dir {
                    add_component(graph, targets, component);
                    continue;
                }
                let Ok(files) = rcmake_resolve::file_sets::set_files(
                    model,
                    &mut usage,
                    target,
                    set,
                    &graph.input.config,
                ) else {
                    add_component(graph, targets, component);
                    continue;
                };
                let deps: BTreeSet<_> = files
                    .values()
                    .flatten()
                    .filter_map(|(file, _)| graph.file_dependency(file))
                    .collect();
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
        return Ok(graph.target_dependencies(&target.name).into_iter().next());
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
        .flat_map(|file| graph.install_file_dependencies(file))
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
        for (_, test) in model.dir_tests(dir) {
            let target = name(b"test_prep/", &test.name);
            if !test.is_discovery()
                && (test.name == "all"
                    || test.name.is_empty()
                    || !test
                        .name
                        .iter()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.:+-".contains(byte))
                    || !available(graph, &target))
            {
                continue;
            }
            let mut deps = BTreeSet::new();
            for (index, arg) in test.command.iter().enumerate() {
                let ast = parse(arg)
                    .map_err(|error| generation_error(graph.build_dir, error.to_string()))?;
                let (value, _) = evaluate(&ast, &context, &mut host)
                    .map_err(|error| generation_error(graph.build_dir, error.to_string()))?;
                if index == 0
                    && let Some(id) = model.find_target(dir, &value)
                    && let Some(executable) = model
                        .target(id)
                        .filter(|target| target.kind == TargetKind::Executable)
                    && !executable.is_imported()
                {
                    deps.extend(graph.target_dependencies(&executable.name));
                }
                if arg.windows(2).any(|pair| pair == b"$<") {
                    collect_ast_target_deps(
                        &ast.nodes, graph, model, dir, &context, &mut host, &mut deps,
                    )?;
                    if let Some(dep) = graph.file_dependency(&value) {
                        deps.insert(dep);
                    } else if let Some(pos) = value.iter().position(|&byte| byte == b'=')
                        && let Some(rhs) = value.get(pos + 1..)
                        && let Some(dep) = graph.file_dependency(rhs)
                    {
                        deps.insert(dep);
                    }
                }
            }
            for input in &test.build_depends {
                let expanded = parse(&input.value)
                    .and_then(|expression| evaluate(&expression, &context, &mut host))
                    .map(|(value, _)| value)
                    .map_err(|error| generation_error(graph.build_dir, error.to_string()))?;
                for value in list::split(&expanded, list::Empty::Drop) {
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
            let expanded = parse(inputs)
                .and_then(|expression| evaluate(&expression, &context, &mut host))
                .map(|(value, _)| value)
                .map_err(|error| generation_error(graph.build_dir, error.to_string()))?;
            for input in list::split(&expanded, list::Empty::Drop) {
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

/// Walk a parsed test argument generator-expression AST and collect target
/// artifact dependencies referenced inside active branches, matching CMake's
/// `cmTestGenerator.cxx` (including nested expressions like
/// `$<BOOL:$<TARGET_FILE:tool>>`).
fn collect_ast_target_deps(
    nodes: &[rcmake_genex::GenexNode],
    graph: &Graph<'_>,
    model: &ProjectModel,
    dir: DirId,
    context: &EvalContext,
    host: &mut dyn rcmake_genex::GenexHost,
    deps: &mut BTreeSet<BString>,
) -> Result<(), GenError> {
    collect_ast_target_deps_inner(nodes, graph, model, dir, context, host, deps, 0)
}

#[allow(clippy::too_many_arguments)]
fn collect_ast_target_deps_inner(
    nodes: &[rcmake_genex::GenexNode],
    graph: &Graph<'_>,
    model: &ProjectModel,
    dir: DirId,
    context: &EvalContext,
    host: &mut dyn rcmake_genex::GenexHost,
    deps: &mut BTreeSet<BString>,
    depth: usize,
) -> Result<(), GenError> {
    if depth > rcmake_genex::MAX_NESTING {
        return Ok(());
    }
    for node in nodes {
        let rcmake_genex::GenexNode::Expr(expr) = node else {
            continue;
        };
        match expr.identifier.as_slice() {
            [rcmake_genex::GenexNode::Text(ident)] => match ident.as_slice() {
                b"0" => continue,
                b"IF" => {
                    if let Some(params) = &expr.params
                        && let Some(cond_nodes) = params.first()
                    {
                        collect_ast_target_deps_inner(
                            cond_nodes,
                            graph,
                            model,
                            dir,
                            context,
                            host,
                            deps,
                            depth + 1,
                        )?;
                        let cond_ast = rcmake_genex::GenexAst {
                            nodes: cond_nodes.clone(),
                        };
                        let (cond_val, _) =
                            evaluate(&cond_ast, context, host).map_err(|error| {
                                generation_error(graph.build_dir, error.to_string())
                            })?;
                        let branch = if truth::is_on(&cond_val) {
                            params.get(1)
                        } else {
                            params.get(2)
                        };
                        if let Some(branch) = branch {
                            collect_ast_target_deps_inner(
                                branch,
                                graph,
                                model,
                                dir,
                                context,
                                host,
                                deps,
                                depth + 1,
                            )?;
                        }
                    }
                }
                b"TARGET_FILE"
                | b"TARGET_LINKER_FILE"
                | b"TARGET_SONAME_FILE"
                | b"TARGET_IMPORT_FILE"
                | b"TARGET_PDB_FILE"
                | b"TARGET_BUNDLE_FILE"
                | b"TARGET_BUNDLE_DIR"
                | b"TARGET_RUNTIME_DLLS"
                | b"TARGET_FILE_DIR"
                | b"TARGET_FILE_NAME"
                | b"TARGET_FILE_BASE_NAME"
                | b"TARGET_FILE_PREFIX"
                | b"TARGET_FILE_SUFFIX" => {
                    let sub_ast = rcmake_genex::GenexAst {
                        nodes: vec![rcmake_genex::GenexNode::Expr(expr.clone())],
                    };
                    if let Ok((sub_val, _)) = evaluate(&sub_ast, context, host) {
                        for item in list::split(&sub_val, list::Empty::Drop) {
                            if let Some(dep) = graph.file_dependency(&item) {
                                deps.insert(dep);
                            }
                        }
                    }
                    if let Some(params) = &expr.params
                        && let Some(target_nodes) = params.first()
                    {
                        let target_ast = rcmake_genex::GenexAst {
                            nodes: target_nodes.clone(),
                        };
                        if let Ok((target_name, _)) = evaluate(&target_ast, context, host)
                            && let Some(id) = model.find_target(dir, &target_name)
                            && let Some(target) = model.target(id)
                            && !target.is_imported()
                        {
                            deps.extend(graph.target_dependencies(&target.name));
                        }
                    }
                }
                _ => {
                    for param in expr.params.iter().flatten() {
                        collect_ast_target_deps_inner(
                            param,
                            graph,
                            model,
                            dir,
                            context,
                            host,
                            deps,
                            depth + 1,
                        )?;
                    }
                }
            },
            _ => {
                // Conditional `$<cond:body>` form: collect deps from the condition
                // itself and recurse into `params` only when `cond` is truthy.
                collect_ast_target_deps_inner(
                    &expr.identifier,
                    graph,
                    model,
                    dir,
                    context,
                    host,
                    deps,
                    depth + 1,
                )?;
                let cond_ast = rcmake_genex::GenexAst {
                    nodes: expr.identifier.clone(),
                };
                let (cond_val, _) = evaluate(&cond_ast, context, host)
                    .map_err(|error| generation_error(graph.build_dir, error.to_string()))?;
                if truth::is_on(&cond_val) {
                    for param in expr.params.iter().flatten() {
                        collect_ast_target_deps_inner(
                            param,
                            graph,
                            model,
                            dir,
                            context,
                            host,
                            deps,
                            depth + 1,
                        )?;
                    }
                }
            }
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
        return graph.target_dependencies(&target.name).into_iter().next();
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

    #[test]
    fn runtime_dependency_set_install_prep_filters_configurations_and_excluded_dirs() {
        use std::collections::BTreeMap;
        use std::path::Path;

        use rcmake_genex::ConfigId;
        use rcmake_model::{
            DirId, InstallKind, InstallOptions, InstallRule, RuntimeDependencyInstall, Target,
            TargetKind, TargetOrigin,
        };
        use rcmake_resolve::{BuildGraph, GraphTarget};

        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/build".into());
        let app = Target::new(
            "app".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        );
        let app_id = model.add_target(app).unwrap_or_else(|e| panic!("{e}"));
        // Install the runtime dependency set BEFORE the targets rule populates it.
        model.installs.push(InstallRule {
            dir: DirId::ROOT,
            kind: InstallKind::Code(Default::default()),
            options: InstallOptions {
                component: Some("Runtime".into()),
                configurations: vec!["Release".into()],
                ..InstallOptions::default()
            },
            backtrace: Default::default(),
        });
        for (idx, arg) in [
            "RUNTIME_DEPENDENCY_SET",
            "deps",
            "COMPONENT",
            "Runtime",
            "CONFIGURATIONS",
            "Release",
        ]
        .into_iter()
        .enumerate()
        {
            model
                .global
                .props
                .set(format!("_RCMAKE_INSTALL_0_ARG{idx}").into(), arg.into());
        }
        model
            .runtime_dependency_installs
            .push(RuntimeDependencyInstall {
                dir: DirId::ROOT,
                position: 0,
                name: "deps".into(),
                backtrace: Default::default(),
                library: InstallOptions {
                    component: Some("Runtime".into()),
                    configurations: vec!["Release".into()],
                    ..InstallOptions::default()
                },
                framework: None,
                filters: Default::default(),
            });
        model.installs.push(InstallRule {
            dir: DirId::ROOT,
            kind: InstallKind::Targets {
                targets: vec![app_id],
                export: None,
            },
            options: InstallOptions::default(),
            backtrace: Default::default(),
        });
        for (idx, arg) in ["TARGETS", "app", "RUNTIME_DEPENDENCY_SET", "deps"]
            .into_iter()
            .enumerate()
        {
            model
                .global
                .props
                .set(format!("_RCMAKE_INSTALL_1_ARG{idx}").into(), arg.into());
        }

        let make_targets = |config: &str, m: &ProjectModel| {
            let input = BuildGraph {
                config: ConfigId(config.into()),
                targets: vec![GraphTarget {
                    name: "app".into(),
                    in_all: true,
                    ..GraphTarget::default()
                }],
                ..BuildGraph::default()
            };
            let graph = super::Graph::new(&input, &BTreeMap::new(), Path::new("/build"), None)
                .unwrap_or_else(|e| panic!("{e}"));
            let mut targets = super::Targets::new();
            super::collect_runtime_dependency_inputs(&graph, m, &mut targets);
            targets
        };

        let debug_targets = make_targets("Debug", &model);
        assert!(
            !debug_targets
                .get(b"install_prep/Runtime".as_slice())
                .is_some_and(|deps| deps.contains(b"app".as_slice())),
            "{debug_targets:?}"
        );
        let release_targets = make_targets("Release", &model);
        assert!(
            release_targets
                .get(b"install_prep/Runtime".as_slice())
                .is_some_and(|deps| deps.contains(b"app".as_slice())),
            "{release_targets:?}"
        );

        let excluded_dir = model
            .add_directory(
                DirId::ROOT,
                "/src/ex".into(),
                "/build/ex".into(),
                Default::default(),
            )
            .unwrap_or_else(|e| panic!("{e}"));
        model.dir_mut(excluded_dir).exclude_from_all = true;
        if let Some(install) = model.runtime_dependency_installs.first_mut() {
            install.dir = excluded_dir;
        }
        let excluded_targets = make_targets("Release", &model);
        assert!(
            !excluded_targets
                .get(b"install_prep/Runtime".as_slice())
                .is_some_and(|deps| deps.contains(b"app".as_slice())),
            "{excluded_targets:?}"
        );
    }

    #[test]
    fn unnamed_runtime_dependencies_install_prep_respects_group_config_filters()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::collections::BTreeMap;
        use std::path::Path;

        use rcmake_genex::ConfigId;
        use rcmake_model::{
            DirId, InstallKind, InstallOptions, InstallRule, Target, TargetKind, TargetOrigin,
        };
        use rcmake_resolve::{BuildGraph, GraphTarget};

        let run_case = |import_suffix: Option<&str>,
                        config: &str|
         -> Result<super::Targets, Box<dyn std::error::Error>> {
            let mut model = ProjectModel::default();
            model.set_root_dirs("/src".into(), "/build".into());
            model
                .dir_mut(DirId::ROOT)
                .vars
                .insert("CMAKE_SYSTEM_NAME".into(), "Linux".into());
            model
                .dir_mut(DirId::ROOT)
                .vars
                .insert("CMAKE_HOST_SYSTEM_NAME".into(), "Linux".into());
            if let Some(suffix) = import_suffix {
                model
                    .dir_mut(DirId::ROOT)
                    .vars
                    .insert("CMAKE_IMPORT_LIBRARY_SUFFIX".into(), suffix.into());
            }
            let app = Target::new(
                "app".into(),
                TargetKind::Executable,
                TargetOrigin::Normal,
                DirId::ROOT,
                Default::default(),
                Default::default(),
            );
            let app_id = model.add_target(app)?;
            model.installs.push(InstallRule {
                dir: DirId::ROOT,
                kind: InstallKind::Targets {
                    targets: vec![app_id],
                    export: None,
                },
                options: InstallOptions::default(),
                backtrace: Default::default(),
            });
            for (idx, arg) in [
                "TARGETS",
                "app",
                "RUNTIME_DEPENDENCIES",
                "RUNTIME",
                "COMPONENT",
                "RtBin",
                "CONFIGURATIONS",
                "Debug",
                "LIBRARY",
                "COMPONENT",
                "RtLib",
                "CONFIGURATIONS",
                "Release",
            ]
            .into_iter()
            .enumerate()
            {
                model
                    .global
                    .props
                    .set(format!("_RCMAKE_INSTALL_0_ARG{idx}").into(), arg.into());
            }

            let input = BuildGraph {
                config: ConfigId(config.into()),
                targets: vec![GraphTarget {
                    name: "app".into(),
                    in_all: true,
                    ..GraphTarget::default()
                }],
                ..BuildGraph::default()
            };
            let graph = super::Graph::new(&input, &BTreeMap::new(), Path::new("/build"), None)?;
            let mut targets = super::Targets::new();
            super::collect_runtime_dependency_inputs(&graph, &model, &mut targets);
            Ok(targets)
        };

        // Without CMAKE_IMPORT_LIBRARY_SUFFIX, selects LIBRARY (Release only, component RtLib).
        let lib_debug = run_case(None, "Debug")?;
        assert!(
            !lib_debug
                .get(b"install_prep/RtLib".as_slice())
                .is_some_and(|deps| deps.contains(b"app".as_slice())),
            "{lib_debug:?}"
        );
        let lib_release = run_case(None, "Release")?;
        assert!(
            lib_release
                .get(b"install_prep/RtLib".as_slice())
                .is_some_and(|deps| deps.contains(b"app".as_slice())),
            "{lib_release:?}"
        );
        assert!(
            !lib_release
                .get(b"install_prep/RtBin".as_slice())
                .is_some_and(|deps| deps.contains(b"app".as_slice())),
            "{lib_release:?}"
        );

        // With non-empty CMAKE_IMPORT_LIBRARY_SUFFIX (even on non-Windows CMAKE_SYSTEM_NAME),
        // selects RUNTIME (Debug only, component RtBin).
        let dll_debug = run_case(Some(".dll.a"), "Debug")?;
        assert!(
            dll_debug
                .get(b"install_prep/RtBin".as_slice())
                .is_some_and(|deps| deps.contains(b"app".as_slice())),
            "{dll_debug:?}"
        );
        let dll_release = run_case(Some(".dll.a"), "Release")?;
        assert!(
            !dll_release
                .get(b"install_prep/RtBin".as_slice())
                .is_some_and(|deps| deps.contains(b"app".as_slice())),
            "{dll_release:?}"
        );
        assert!(
            !dll_release
                .get(b"install_prep/RtLib".as_slice())
                .is_some_and(|deps| deps.contains(b"app".as_slice())),
            "{dll_release:?}"
        );
        Ok(())
    }

    #[test]
    fn test_prep_accepts_colons_and_embedded_target_file_genex()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::collections::BTreeMap;
        use std::path::Path;

        use rcmake_base::BString;
        use rcmake_diag::{Backtrace, PolicyMap};
        use rcmake_model::{
            DirId, ProjectModel, PropertyMap, Target, TargetKind, TargetOrigin, TestEntry, TestKind,
        };
        use rcmake_resolve::{
            ArtifactSet, BuildGraph, BuildStep, GraphTarget, Invocation, LinkKind, LinkStep,
        };

        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/build".into());
        let mut tool = Target::new(
            "tool".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        );
        tool.props
            .set("RUNTIME_OUTPUT_DIRECTORY".into(), "/build/bin".into());
        let tool_id = model.add_target(tool)?;
        let make_test = |name: &str, command: Vec<BString>| TestEntry {
            name: name.into(),
            dir: DirId::ROOT,
            backtrace: Backtrace::default(),
            command,
            configurations: Vec::new(),
            working_directory: None,
            command_expand_lists: false,
            old_style: false,
            policies: PolicyMap::default(),
            props: PropertyMap::default(),
            build_depends: Vec::new(),
            kind: TestKind::Single,
        };
        model.add_test(make_test(
            "VTK::ChartsCore-HeaderTest",
            vec!["/usr/bin/python3".into(), "check.py".into()],
        ))?;
        model.add_test(make_test(
            "Suite::embedded",
            vec![
                "/usr/bin/cmake".into(),
                "-DTEST_EXE=$<TARGET_FILE:tool>".into(),
                "-DDISABLED=$<$<BOOL:0>:$<TARGET_FILE:tool>>".into(),
            ],
        ))?;
        model.add_test(make_test(
            "Suite::nested_bool",
            vec![
                "/usr/bin/cmake".into(),
                "-DHAS_TOOL=$<BOOL:$<TARGET_FILE:tool>>".into(),
            ],
        ))?;
        model.add_test(make_test(
            "Suite::disabled_only",
            vec![
                "/usr/bin/cmake".into(),
                "-DDISABLED=$<$<BOOL:0>:$<TARGET_FILE:tool>>".into(),
            ],
        ))?;

        let input = BuildGraph {
            steps: vec![BuildStep::Link {
                inputs: Vec::new(),
                implicit_inputs: Vec::new(),
                step: LinkStep {
                    kind: LinkKind::Executable,
                    language: "CXX".into(),
                    fragments: Vec::new(),
                    output: "bin/tool".into(),
                },
                invocations: vec![Invocation::default()],
            }],
            targets: vec![GraphTarget {
                name: "tool".into(),
                steps: vec![0],
                depends: Vec::new(),
                in_all: true,
            }],
            target_ids: vec![tool_id],
            ..BuildGraph::default()
        };
        let artifacts = ArtifactSet {
            file: Some("/build/bin/tool".into()),
            ..ArtifactSet::default()
        };
        let artifact_map = BTreeMap::from([(b"tool".as_slice(), &artifacts)]);
        let graph = super::Graph::new(&input, &artifact_map, Path::new("/build"), None)?;
        let mut targets = super::Targets::new();
        super::collect_tests(&graph, &model, &mut targets)?;
        assert!(
            targets.contains_key(b"test_prep/VTK::ChartsCore-HeaderTest".as_slice()),
            "colon-scoped test with empty deps must be emitted: {targets:?}"
        );
        assert_eq!(
            targets
                .get(b"test_prep/Suite::embedded".as_slice())
                .map(|deps| deps.iter().cloned().collect::<Vec<BString>>()),
            Some(vec![BString::from("bin/tool")]),
            "embedded $<TARGET_FILE:tool> must capture executable dependency"
        );
        assert_eq!(
            targets
                .get(b"test_prep/Suite::nested_bool".as_slice())
                .map(|deps| deps.iter().cloned().collect::<Vec<BString>>()),
            Some(vec![BString::from("bin/tool")]),
            "nested $<BOOL:$<TARGET_FILE:tool>> must capture executable dependency"
        );
        assert_eq!(
            targets
                .get(b"test_prep/Suite::disabled_only".as_slice())
                .map(|deps| deps.iter().cloned().collect::<Vec<BString>>()),
            Some(Vec::new()),
            "inactive $<$<BOOL:0>:$<TARGET_FILE:tool>> must not record dependency"
        );
        Ok(())
    }
}
