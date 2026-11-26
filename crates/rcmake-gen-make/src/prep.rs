//! Rules that build the inputs of install rules and tests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::{BString, hash::HashAlgo, list, truth};
use rcmake_gen_common::GenError;
use rcmake_genex::{EvalContext, evaluate, parse};
use rcmake_host::path;
use rcmake_model::{DirId, InstallKind, ProjectModel, TargetKind};
use rcmake_resolve::{
    BuildGraph, GenerationResult, InstallAction, InstallActionKind, lower::lower_install_manifest,
    usage::UsageResolver,
};

use crate::{Target, build_path_with_prefix, invalid_graph};

type Dependencies = BTreeSet<BString>;
pub(crate) type Targets = BTreeMap<BString, Dependencies>;

struct Owners<'a> {
    names: BTreeMap<&'a [u8], &'a BString>,
    files: BTreeMap<&'a [u8], &'a BString>,
    build_dir: &'a Path,
    build_prefix: BString,
}

impl<'a> Owners<'a> {
    fn new(targets: &'a [Target<'_>], build_dir: &'a Path) -> Self {
        Self {
            names: targets
                .iter()
                .map(|target| (target.name, &target.goal))
                .collect(),
            files: targets
                .iter()
                .flat_map(|target| {
                    target.rules.iter().flat_map(move |rule| {
                        rule.outputs
                            .iter()
                            .map(move |file| (file.as_slice(), &target.goal))
                    })
                })
                .collect(),
            build_dir,
            build_prefix: path::from_native(build_dir),
        }
    }

    fn file(&self, file: &[u8]) -> Option<BString> {
        self.files
            .get(build_path_with_prefix(file, self.build_dir, &self.build_prefix).as_slice())
            .map(|goal| (*goal).clone())
    }

    fn target(&self, name: &[u8]) -> Option<BString> {
        self.names.get(name).map(|goal| (*goal).clone())
    }

    fn available(&self, name: &[u8]) -> bool {
        !self.names.contains_key(name) && !self.files.contains_key(name)
    }
}

pub(crate) fn targets(
    result: &GenerationResult,
    graph: &BuildGraph,
    targets: &[Target<'_>],
    build_dir: &Path,
    model: Option<&ProjectModel>,
) -> Result<Targets, GenError> {
    let mut prep = Targets::new();
    let Some(model) = model else {
        return Ok(prep);
    };
    let owners = Owners::new(targets, build_dir);
    if !skip(model, b"CMAKE_SKIP_INSTALL_PREP_TARGETS") {
        collect_install(result, graph, model, &owners, &mut prep);
    }
    if !skip(model, b"CMAKE_SKIP_TEST_PREP_TARGETS") {
        collect_tests(graph, model, &owners, &mut prep)?;
    }
    for (alias, goal) in [
        ("install_prep", "install_prep/all"),
        ("test_prep", "test_prep/all"),
    ] {
        if owners.available(alias.as_bytes()) && prep.contains_key(goal.as_bytes()) {
            prep.insert(alias.into(), BTreeSet::from([goal.into()]));
        }
    }
    Ok(prep)
}

fn skip(model: &ProjectModel, key: &[u8]) -> bool {
    model
        .definition(DirId::ROOT, key)
        .is_some_and(|value| truth::is_on(value))
}

fn name(prefix: &[u8], suffix: &[u8]) -> BString {
    [prefix, suffix].concat().into()
}

fn valid_segment(value: &[u8]) -> bool {
    !matches!(value, b"" | b"." | b"..")
        && value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(byte))
}

fn collect_install(
    result: &GenerationResult,
    graph: &BuildGraph,
    model: &ProjectModel,
    owners: &Owners<'_>,
    prep: &mut Targets,
) {
    if result.install.actions.is_empty() && model.installs.is_empty() {
        return;
    }
    for component in model.install_components() {
        if component.split(|&byte| byte == b'/').all(valid_segment) {
            let goal = name(b"install_prep/", &component);
            if owners.available(&goal) {
                prep.entry(goal).or_default();
            }
        }
    }
    for action in &result.install.actions {
        if action
            .component
            .split(|&byte| byte == b'/')
            .all(valid_segment)
        {
            let goal = name(b"install_prep/", &action.component);
            if owners.available(&goal) {
                prep.entry(goal)
                    .or_default()
                    .extend(install_dependencies(graph, owners, action));
            }
        }
    }
    if !owners.available(b"install_prep/all") {
        return;
    }
    let needs_refilter = !model.runtime_dependency_installs.is_empty()
        && model
            .installs
            .iter()
            .any(|rule| rule.options.exclude_from_all);
    let filtered = needs_refilter.then(|| {
        let mut model = model.clone();
        for rule in &mut model.installs {
            if rule.options.exclude_from_all {
                // Keep the rule indices that export sets reference.
                rule.kind = InstallKind::Code(BString::default());
            }
        }
        lower_install_manifest(&model, &result.targets)
    });
    let manifest = filtered.as_ref().unwrap_or(&result.install);
    prep.entry("install_prep/all".into()).or_default().extend(
        manifest
            .actions
            .iter()
            .filter(|action| !action.exclude_from_all)
            .flat_map(|action| install_dependencies(graph, owners, action)),
    );
}

fn install_dependencies<'a>(
    graph: &'a BuildGraph,
    owners: &'a Owners<'_>,
    action: &'a InstallAction,
) -> impl Iterator<Item = BString> + 'a {
    let files = match &action.kind {
        InstallActionKind::Files { files, .. } => files.as_slice(),
        InstallActionKind::Script(script) => std::slice::from_ref(script),
        _ => &[],
    };
    files.iter().filter_map(|file| {
        (action.configurations.is_empty()
            || action
                .configurations
                .iter()
                .any(|config| graph.config.matches(config)))
        .then(|| owners.file(file))
        .flatten()
    })
}

fn collect_tests(
    graph: &BuildGraph,
    model: &ProjectModel,
    owners: &Owners<'_>,
    prep: &mut Targets,
) -> Result<(), GenError> {
    let ctx = EvalContext {
        config: graph.config.clone(),
        ..EvalContext::default()
    };
    let mut usage: Option<UsageResolver<'_>> = None;
    for (dir, directory) in model.dirs() {
        let dir_build_depends = directory.props.get(b"CMAKE_TEST_BUILD_DEPENDS");
        let mut tests = model.dir_tests(dir).peekable();
        if tests.peek().is_none() && dir_build_depends.is_none() {
            continue;
        }
        let binary = path::to_native(&directory.binary_dir);
        let source = path::to_native(&directory.source_dir);
        let mut expand = |value: &[u8]| -> Result<BString, GenError> {
            if value.len() <= rcmake_base::Limits::default().max_value_bytes
                && rcmake_genex::find_genex(value).is_none()
            {
                return Ok(BString::from(value));
            }
            let resolver = usage.get_or_insert_with(|| UsageResolver::new(model));
            let mut host = resolver.host(dir);
            parse(value)
                .and_then(|expression| evaluate(&expression, &ctx, &mut host))
                .map(|(value, _)| value)
                .map_err(|error| invalid_graph(error.to_string()))
        };
        let dependency = |value: &[u8]| {
            if let Some(target) = model
                .find_target(dir, value)
                .and_then(|id| model.target(id))
            {
                return owners.target(&target.name);
            }
            let file = path::to_native(value);
            owners
                .file(&path::from_native(&binary.join(&file)))
                .or_else(|| owners.file(&path::from_native(&source.join(&file))))
        };
        let mut directory_deps = Dependencies::new();
        let mut has_directory_inputs = false;
        for (_, test) in tests {
            let goal = name(b"test_prep/", &test.name);
            if !test.is_discovery() && (!valid_segment(&test.name) || !owners.available(&goal)) {
                continue;
            }
            let mut deps = Dependencies::new();
            for (index, arg) in test.command.iter().enumerate() {
                let has_genex = !test.old_style && rcmake_genex::find_genex(arg).is_some();
                let value = if has_genex { expand(arg)? } else { arg.clone() };
                if index == 0
                    && let Some(target) = model
                        .find_target(dir, &value)
                        .and_then(|id| model.target(id))
                    && target.kind == TargetKind::Executable
                    && let Some(goal) = owners.target(&target.name)
                {
                    deps.insert(goal);
                }
                if has_genex && let Some(goal) = dependency(&value) {
                    deps.insert(goal);
                }
            }
            for input in &test.build_depends {
                let inputs = list::split(&expand(&input.value)?, list::Empty::Drop);
                has_directory_inputs |= test.is_discovery() && !inputs.is_empty();
                deps.extend(inputs.iter().filter_map(|input| dependency(input)));
            }
            if test.is_discovery() {
                directory_deps.extend(deps);
            } else {
                prep.entry(goal).or_default().extend(deps);
            }
        }
        if let Some(inputs) = dir_build_depends {
            let inputs = list::split(&expand(inputs)?, list::Empty::Drop);
            has_directory_inputs |= !inputs.is_empty();
            directory_deps.extend(inputs.iter().filter_map(|input| dependency(input)));
        }
        if has_directory_inputs || !directory_deps.is_empty() {
            let hash = HashAlgo::Md5.hex_digest(&directory.binary_dir);
            let goal = name(
                b"test_prep/directory_",
                hash.as_bytes().get(..8).unwrap_or_default(),
            );
            if owners.available(&goal) {
                prep.entry(goal).or_default().extend(directory_deps);
            }
        }
    }
    let all = BString::from("test_prep/all");
    if owners.available(&all) {
        let tests: Vec<_> = prep
            .keys()
            .filter(|goal| **goal != all && goal.starts_with(b"test_prep/"))
            .cloned()
            .collect();
        prep.entry(all).or_default().extend(tests);
    }
    Ok(())
}
