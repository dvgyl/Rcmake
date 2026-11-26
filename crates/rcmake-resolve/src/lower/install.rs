//! Install actions and configure metadata.

use indexmap::{IndexMap, IndexSet};
pub(super) use rcmake_base::shell::{cmake_quote, cmake_quoted_content};
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::{
    Backtrace, DiagAction, DiagCategory, Diagnostic, Effect, Location, MessageKind, PolicyId,
    RenderOptions, render,
};
use rcmake_genex::{ConfigId, EvalContext, EvalSite, evaluate, parse};
use rcmake_host::{Host, Stream};
use rcmake_model::{
    DirId, GeneratorKind, InstallKind, InstallOptions, InstallRule, ProjectModel, Target, TargetId,
    TargetKind,
};

use super::{definition, join};
use crate::{
    InstallAction, InstallActionKind, InstallManifest, ResolveError, ResolvedTarget,
    usage::UsageResolver,
};

mod entries;
mod imported_runtime;
mod legacy;
mod objects;
mod projection;
mod runtime_set;
mod script_policy;
mod strip;
mod targets;
pub(super) mod transitive;

use entries::{InstallEntry, ordered_entries, ordered_install_rules};
use transitive::RuntimeTargets;

pub(crate) use objects::exported_path as exported_object_path;

pub use super::artifacts::collect_metadata;

/// Lower install rules in model order.
///
/// Export action indices refer to the plans for the install export rules in that
/// same order.
#[must_use]
pub fn lower_install_manifest(model: &ProjectModel, targets: &[ResolvedTarget]) -> InstallManifest {
    let runtime =
        transitive::collect(model, &mut UsageResolver::new(model), targets).unwrap_or_default();
    let edits = runtime_edits(
        model,
        targets,
        &IndexMap::new(),
        &runtime,
        None,
        |_, _, _| {},
    )
    .unwrap_or_default();
    lower_install_rules(
        model,
        targets,
        (&[], &edits, &runtime, None),
        &mut |_, _, _, value| Ok(value.into()),
        &mut |dir, config, source| {
            Ok(crate::sources::install_object_name(model, dir, source, config).unwrap_or_default())
        },
    )
    .unwrap_or_default()
}

pub(crate) fn lower_install_manifest_with_configs(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    configs: &[ConfigId],
    ide: Option<crate::ide::IdeFlavor>,
    host: &dyn Host,
    languages: &IndexMap<(TargetId, ConfigId), IndexSet<BString>>,
) -> Result<InstallManifest, ResolveError> {
    for (index, rule, _) in ordered_install_rules(model) {
        targets::warn_framework_change(model, index, rule, host)?;
        script_policy::warn(model, rule, host);
    }
    super::warn_install_name_policy(model, host);
    let mut usage = UsageResolver::new(model).with_ide(ide);
    let runtime = transitive::collect(model, &mut usage, targets)?;
    let edits = runtime_edits(
        model,
        targets,
        languages,
        &runtime,
        Some(host),
        |rule, raw, edit| {
            if let super::install_rpath::RuntimeEdit::Elf(elf) = edit
                && raw.policies.policies.get(rcmake_diag::PolicyId::new(95))
                    == rcmake_diag::PolicyStatus::Warn
                && elf.new.windows(2).any(|pair| pair == b"${")
            {
                warn_rpath_escaping(model, host, rule, raw);
            }
        },
    )?;
    lower_install_rules(
        model,
        targets,
        (configs, &edits, &runtime, Some(host)),
        &mut |dir, config, backtrace, value| {
            if !has_genex(value) {
                return Ok(value.into());
            }
            let context = EvalContext {
                config: config.clone(),
                site: EvalSite {
                    backtrace: backtrace.clone(),
                    property: None,
                },
                ..EvalContext::default()
            };
            let (value, _) = evaluate(&parse(value)?, &context, &mut usage.host(dir))?;
            Ok(value)
        },
        &mut |dir, config, source| {
            crate::sources::install_object_name(model, dir, source, config).map_err(|error| {
                let ResolveError::Message(body) = error else {
                    return error;
                };
                let paths = crate::sources::SourcePaths::new(host);
                let file = join(&model.dir(dir).source_dir, b"CMakeLists.txt");
                paths.emit(
                    model,
                    MessageKind::Error,
                    Location::FileLine {
                        file: file.as_slice().into(),
                        line: 0,
                    },
                    body.clone(),
                );
                ResolveError::Reported(body)
            })
        },
    )
}

fn warn_rpath_escaping(model: &ProjectModel, host: &dyn Host, rule: &InstallRule, target: &Target) {
    if definition(model, rule.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
        .is_some_and(|value| rcmake_base::truth::is_on(value))
    {
        return;
    }
    let policy = PolicyId::new(95);
    let warning = Diagnostic {
        annotations: None,
        kind: MessageKind::AuthorWarning,
        effect: Effect::None,
        category: Some(DiagCategory::Policy),
        policy: Some(policy),
        location: crate::sources::generation_source_location(Location::Backtrace(rule.backtrace.clone())),
        body: format!(
            concat!(
                "Policy CMP0095 is not set: RPATH entries are properly escaped in the intermediary CMake install script.  ",
                "Run \"cmake --help-policy CMP0095\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\n",
                "RPATH entries for target '{}' will not be escaped in the intermediary cmake_install.cmake script."
            ),
            target.name,
        ).into(),
    };
    host.write_out(
        Stream::Stderr,
        &render(
            &warning,
            &RenderOptions {
                relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                ..Default::default()
            },
        ),
    );
}

impl InstallManifest {
    pub(crate) fn validate_bmi_destinations(
        model: &ProjectModel,
        configs: &[ConfigId],
        host: &dyn Host,
    ) -> Result<(), ResolveError> {
        if !model.generator.is_some_and(GeneratorKind::is_ninja) {
            return Ok(());
        }
        let category = DiagCategory::InstallAbsoluteDestination;
        let mut usage = UsageResolver::new(model);
        let mut failure = None;
        for (index, rule, _) in ordered_install_rules(model) {
            let InstallKind::Targets { targets, .. } = &rule.kind else {
                continue;
            };
            let action = model
                .global
                .props
                .get(format!("_RCMAKE_INSTALL_{index}_ABSOLUTE_ACTION").as_bytes())
                .and_then(|value| DiagAction::parse(value))
                .unwrap_or_else(|| category.default_action());
            if action == DiagAction::Ignore {
                continue;
            }
            let recorded = recorded_args(model, index);
            let groups = target_options(&recorded);
            let Some(group) = groups.get(b"CXX_MODULES_BMI".as_slice()) else {
                continue;
            };
            let options = group.merge(&rule.options);
            let Some(value) = &options.destination else {
                continue;
            };
            let mut reported = IndexSet::new();
            for target in targets.iter().filter_map(|id| model.target(*id)) {
                if !target
                    .file_sets
                    .iter()
                    .filter_map(|id| model.file_set(*id))
                    .any(|set| set.kind == rcmake_model::FileSetKind::CxxModules)
                {
                    continue;
                }
                for config in configs
                    .iter()
                    .filter(|config| matches_config(&options, config))
                {
                    let context = EvalContext {
                        config: config.clone(),
                        site: EvalSite {
                            backtrace: rule.backtrace.clone(),
                            property: None,
                        },
                        ..EvalContext::default()
                    };
                    let path = evaluate(&parse(value)?, &context, &mut usage.host(target.dir))?.0;
                    if !Lexical::new(&path).absolute() || !reported.insert(path.clone()) {
                        continue;
                    }
                    let body: BString =
                        format!("INSTALL command given absolute DESTINATION path:\n  {path}")
                            .into();
                    let diagnostic = Diagnostic {
                        annotations: None,
                        kind: if action == DiagAction::Warn {
                            MessageKind::Warning
                        } else {
                            MessageKind::Error
                        },
                        effect: Effect::None,
                        category: Some(category),
                        policy: None,
                        location: crate::sources::generation_source_location(Location::Backtrace(
                            rule.backtrace.clone(),
                        )),
                        body: body.clone(),
                    };
                    host.write_out(
                        Stream::Stderr,
                        &render(
                            &diagnostic,
                            &RenderOptions {
                                relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                                ..RenderOptions::default()
                            },
                        ),
                    );
                    if action == DiagAction::FatalError {
                        return Err(ResolveError::Reported(body));
                    }
                    if action == DiagAction::SendError {
                        failure = Some(ResolveError::Reported(body));
                    }
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

fn has_genex(value: &[u8]) -> bool {
    value.windows(2).any(|pair| pair == b"$<")
}

fn matches_config(options: &InstallOptions, config: &ConfigId) -> bool {
    options.configurations.is_empty()
        || options
            .configurations
            .iter()
            .any(|name| config.matches(name))
}

type RuntimeEdits = IndexMap<(TargetId, ConfigId), super::install_rpath::RuntimeEdit>;

fn runtime_edits(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    languages: &IndexMap<(TargetId, ConfigId), IndexSet<BString>>,
    runtime: &RuntimeTargets,
    host: Option<&dyn Host>,
    mut warn: impl FnMut(&InstallRule, &Target, &super::install_rpath::RuntimeEdit),
) -> Result<RuntimeEdits, ResolveError> {
    let facts: IndexMap<_, _> = targets
        .iter()
        .map(|target| ((target.id, &target.config), target))
        .collect();
    let paths = host.map(crate::sources::SourcePaths::new);
    let mut resolvers = IndexMap::new();
    let mut edits = IndexMap::new();
    for target in targets {
        let Some(raw) = model.target(target.id) else {
            continue;
        };
        let mut rules = runtime_install_rules(model, raw, &target.config, runtime).peekable();
        if rules.peek().is_none() {
            continue;
        };
        let key = (target.id, target.config.clone());
        let fallback;
        let languages = if let Some(languages) = languages.get(&key) {
            languages
        } else {
            fallback = target
                .compile_groups
                .iter()
                .map(|group| group.language.clone())
                .collect();
            &fallback
        };
        let usage = resolvers.entry(&target.config).or_insert_with(|| {
            paths.as_ref().map_or_else(
                || UsageResolver::new(model),
                |paths| UsageResolver::with_source_paths(model, paths),
            )
        });
        if let Some(edit) =
            super::link::rpath::install_rpath(model, raw, target, languages, &facts, usage)?
        {
            for rule in rules {
                warn(rule, raw, &edit);
            }
            edits.insert(key, edit);
        }
    }
    Ok(edits)
}

pub(super) fn has_target_install(model: &ProjectModel, target: &Target) -> bool {
    model
        .find_target(target.dir, &target.name)
        .is_some_and(|id| {
            model.installs.iter().any(|rule| {
            matches!(&rule.kind, InstallKind::Targets { targets, .. } if targets.contains(&id))
        })
        })
}

fn runtime_install_rules<'a>(
    model: &'a ProjectModel,
    target: &Target,
    config: &'a ConfigId,
    runtime: &'a RuntimeTargets,
) -> impl Iterator<Item = &'a InstallRule> {
    let id = model.find_target(target.dir, &target.name);
    let group: Option<&[u8]> = match (target.kind, crate::ide::bundle_kind(target)) {
        (TargetKind::SharedLibrary, crate::ide::BundleKind::Framework { .. }) => {
            Some(b"FRAMEWORK".as_slice())
        }
        (TargetKind::Executable, crate::ide::BundleKind::App { .. })
        | (TargetKind::ModuleLibrary, crate::ide::BundleKind::Bundle { .. }) => Some(b"BUNDLE"),
        (TargetKind::Executable, _) => Some(b"RUNTIME"),
        (TargetKind::SharedLibrary | TargetKind::ModuleLibrary, _) => Some(b"LIBRARY"),
        _ => None,
    };
    model
        .installs
        .iter()
        .enumerate()
        .filter_map(move |(index, rule)| {
            let group = group?;
            let InstallKind::Targets { targets, .. } = &rule.kind else {
                return None;
            };
            let id = id?;
            let runtime_only = runtime
                .get(&index)
                .and_then(|deps| deps.get(&id))
                .is_some_and(|configs| configs.contains(config));
            if !runtime_only && !targets.contains(&id) {
                return None;
            }
            let group = if runtime_only && super::artifacts::is_dll_platform(model, target.dir) {
                b"RUNTIME"
            } else {
                group
            };
            let recorded = recorded_args(model, index);
            target_options(&recorded)
                .get(group)
                .map_or_else(
                    || matches_config(&rule.options, config),
                    |group| {
                        (runtime_only || !group.namelink_only)
                            && matches_config(&group.merge(&rule.options), config)
                    },
                )
                .then_some(rule)
        })
}

type EvaluateInstall<'a> =
    dyn FnMut(DirId, &ConfigId, &Backtrace, &[u8]) -> Result<BString, ResolveError> + 'a;
type InstallObjectName<'a> =
    dyn FnMut(DirId, &ConfigId, &[u8]) -> Result<BString, ResolveError> + 'a;

fn lower_install_rules(
    model: &ProjectModel,
    targets: &[ResolvedTarget],
    (configs, edits, runtime, host): (
        &[ConfigId],
        &RuntimeEdits,
        &RuntimeTargets,
        Option<&dyn Host>,
    ),
    eval: &mut EvaluateInstall<'_>,
    object_name: &mut InstallObjectName<'_>,
) -> Result<InstallManifest, ResolveError> {
    let mut manifest = InstallManifest::default();
    let mut strip = strip::Tools::new(host);
    let multi = configs.len() > 1 || model.generator.is_some_and(GeneratorKind::multi_config);
    for entry in ordered_entries(model) {
        let (index, rule, export_plan) = match entry {
            InstallEntry::Rule(index, rule, export) => (index, rule, export),
            InstallEntry::RuntimeSet(set) => {
                let first = manifest.actions.len();
                runtime_set::lower(model, targets, configs, set, eval, &mut manifest.actions)?;
                if first < manifest.actions.len() {
                    manifest
                        .directories
                        .entry(set.dir)
                        .or_default()
                        .push(crate::InstallEntry::Actions(first..manifest.actions.len()));
                }
                continue;
            }
            InstallEntry::Directory(dir) => {
                manifest.directories.entry(dir).or_default();
                if let Some(parent) = model.dir(dir).parent {
                    manifest
                        .directories
                        .entry(parent)
                        .or_default()
                        .push(crate::InstallEntry::Child(dir));
                }
                continue;
            }
        };
        let first = manifest.actions.len();
        (|| -> Result<(), ResolveError> {
            let options = &rule.options;
            let recorded = recorded_args(model, index);
            let source = &model.dir(rule.dir).source_dir;
            let message = model
                .global
                .props
                .get(format!("_RCMAKE_INSTALL_{index}_MESSAGE").as_bytes());
            let make_action = |options: &InstallOptions, destination: &[u8], kind| {
                let mut action = action(model, rule.dir, options, destination, kind);
                if let Some(message) = message {
                    action.message.clone_from(message);
                }
                action
            };
            let kind = match &rule.kind {
                InstallKind::Targets { targets: ids, .. } => {
                    targets::lower(
                        model,
                        (index, rule),
                        (
                            targets,
                            configs,
                            (ids, runtime.get(&index)),
                            edits,
                            &mut strip,
                        ),
                        &recorded,
                        &mut manifest,
                        &make_action,
                        (eval, object_name),
                    )?;
                    return Ok(());
                }
                InstallKind::Files(paths)
                | InstallKind::Programs(paths)
                | InstallKind::Directory(paths) => {
                    if let [b"IMPORTED_RUNTIME_ARTIFACTS", name, ..] = recorded.as_slice() {
                        imported_runtime::lower(
                            model,
                            rule,
                            name,
                            configs,
                            eval,
                            |options, kind| {
                                manifest.actions.push(make_action(options, b"", kind));
                            },
                        )?;
                        return Ok(());
                    }
                    let expand_paths = paths.iter().any(|path| has_genex(&path.value));
                    let expand_rename = options
                        .rename
                        .as_ref()
                        .is_some_and(|rename| has_genex(rename));
                    for config in
                        action_configs(configs, options, expand_paths || expand_rename, multi)
                    {
                        let mut options = options.clone();
                        let mut files = Vec::new();
                        if let Some(config) = config {
                            if let Some(destination) = &mut options.destination {
                                *destination =
                                    eval(rule.dir, config, &rule.backtrace, destination)?;
                            }
                            if let Some(rename) = options.rename.as_mut().filter(|_| expand_rename)
                            {
                                *rename = eval(rule.dir, config, &Backtrace::default(), rename)?;
                            }
                            if multi {
                                options.configurations = vec![config.0.clone()];
                            }
                        }
                        for path in paths {
                            if let Some(config) = config.filter(|_| expand_paths) {
                                let value = eval(rule.dir, config, &path.backtrace, &path.value)?;
                                files.extend(
                                    list::split(&value, list::Empty::Drop)
                                        .into_iter()
                                        .map(|path| join(source, &path)),
                                );
                            } else {
                                files.push(join(source, &path.value));
                            }
                        }
                        let kind = match &rule.kind {
                            _ if files.is_empty() && expand_paths => continue,
                            InstallKind::Directory(_) => InstallActionKind::Directory {
                                dirs: files,
                                options: directory_options(&recorded, &options.permissions),
                            },
                            InstallKind::Programs(_) => file_action(b"PROGRAM", files, &options),
                            _ => file_action(b"FILE", files, &options),
                        };
                        manifest.actions.push(make_action(&options, b"", kind));
                    }
                    return Ok(());
                }
                InstallKind::Script(value) | InstallKind::Code(value) => {
                    let script = matches!(rule.kind, InstallKind::Script(_));
                    let allow_genex =
                        script_policy::status(model, rule.dir) == rcmake_diag::PolicyStatus::New;
                    let expand = allow_genex && has_genex(value);
                    let value = if expand {
                        rcmake_genex::rewrite_install_prefix(value, b"${CMAKE_INSTALL_PREFIX}")
                    } else {
                        value.clone()
                    };
                    for config in action_configs(configs, options, expand, multi) {
                        let value = if let Some(config) = config {
                            eval(rule.dir, config, &rule.backtrace, &value)?
                        } else {
                            value.clone()
                        };
                        let kind = if script {
                            InstallActionKind::Script(
                                if value.starts_with(b"${CMAKE_INSTALL_PREFIX}") {
                                    value
                                } else {
                                    join(source, &value)
                                },
                            )
                        } else {
                            InstallActionKind::Code(value)
                        };
                        let mut action = make_action(options, b"", kind);
                        if let Some(config) = config.filter(|_| multi) {
                            action.configurations = vec![config.0.clone()];
                        }
                        manifest.actions.push(action);
                    }
                    return Ok(());
                }
                InstallKind::Export { .. } => {
                    let Some(plan) = export_plan else {
                        return Ok(());
                    };
                    InstallActionKind::Export { plan }
                }
                // CPS and SBOM files do not have install actions yet.
                InstallKind::PackageInfo(_) | InstallKind::Sbom(_) => return Ok(()),
            };
            manifest.actions.push(make_action(options, b"", kind));
            Ok(())
        })()?;
        if first < manifest.actions.len() {
            manifest
                .directories
                .entry(rule.dir)
                .or_default()
                .push(crate::InstallEntry::Actions(first..manifest.actions.len()));
        }
    }
    legacy::append_scripts(model, &mut manifest);
    Ok(manifest)
}

fn action_configs<'a>(
    configs: &'a [ConfigId],
    options: &InstallOptions,
    expand_paths: bool,
    multi: bool,
) -> Vec<Option<&'a ConfigId>> {
    if configs.is_empty()
        || (!expand_paths
            && !options
                .destination
                .as_ref()
                .is_some_and(|path| has_genex(path)))
    {
        vec![None]
    } else {
        configs
            .iter()
            .filter(|config| !multi || matches_config(options, config))
            .map(Some)
            .collect()
    }
}

fn action(
    model: &ProjectModel,
    dir: DirId,
    options: &InstallOptions,
    destination: &[u8],
    kind: InstallActionKind,
) -> InstallAction {
    InstallAction {
        all_components: options.all_components,
        exclude_from_all: options.exclude_from_all,
        component: options
            .component
            .clone()
            .or_else(|| definition(model, dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME").cloned())
            .unwrap_or_else(|| "Unspecified".into()),
        destination: options
            .destination
            .clone()
            .unwrap_or_else(|| destination.into()),
        configurations: options.configurations.clone(),
        kind,
        optional: options.optional,
        message: BString::default(),
    }
}

fn eval_destination(
    options: &mut InstallOptions,
    dir: DirId,
    config: &ConfigId,
    rule: &InstallRule,
    eval: &mut EvaluateInstall<'_>,
) -> Result<(), ResolveError> {
    if let Some(destination) = &mut options.destination {
        *destination = eval(dir, config, &rule.backtrace, destination)?;
    }
    Ok(())
}

fn add_bmi_include(
    manifest: &mut InstallManifest,
    model: &ProjectModel,
    rule: &InstallRule,
    options: &InstallOptions,
    target: &ResolvedTarget,
    multiple: bool,
) {
    let Some(directory) = &target.artifacts.object_dir else {
        return;
    };
    let multi = model.generator == Some(GeneratorKind::NinjaMultiConfig);
    if multiple
        && !options.configurations.is_empty()
        && !options
            .configurations
            .iter()
            .any(|name| target.config.matches(name))
    {
        return;
    }
    let tdi = join(directory, b"CXXDependInfo.json");
    let path = super::bmi_script_path(
        &model.dir(DirId::ROOT).binary_dir,
        &tdi,
        &target.config.0,
        multi,
    );
    let mut action = action(
        model,
        rule.dir,
        options,
        b"",
        InstallActionKind::Script(path.clone()),
    );
    action.optional = true;
    if multiple {
        action.configurations = vec![target.config.0.clone()];
    }
    if options.exclude_from_all {
        let mut body = BString::from("if(CMAKE_INSTALL_COMPONENT STREQUAL ");
        cmake_quote(&mut body, &action.component);
        body.extend_from_slice(b")\n    include(");
        cmake_quote(&mut body, &path);
        body.extend_from_slice(b" OPTIONAL)\n  endif()");
        action.kind = InstallActionKind::Code(body);
    }
    manifest.actions.push(action);
}

fn file_action(
    file_type: &[u8],
    files: Vec<BString>,
    options: &InstallOptions,
) -> InstallActionKind {
    InstallActionKind::Files {
        file_type: file_type.into(),
        files,
        rename: options.rename.clone(),
        permissions: options.permissions.clone(),
    }
}

fn recorded_args(model: &ProjectModel, rule: usize) -> Vec<&[u8]> {
    let section = format!("_RCMAKE_INSTALL_{rule}_ARG");
    (0..)
        .map_while(|index| {
            model
                .global
                .props
                .get(format!("{section}{index}").as_bytes())
                .map(|v| v.as_slice())
        })
        .collect()
}

#[derive(Default)]
struct Group<'a> {
    options: InstallOptions,
    namelink_only: bool,
    namelink_skip: bool,
    namelink_component: Option<&'a [u8]>,
}

impl Group<'_> {
    fn merge(&self, base: &InstallOptions) -> InstallOptions {
        base.with_group(&self.options)
    }
}

fn group_key(key: &[u8]) -> bool {
    matches!(
        key,
        b"ARCHIVE"
            | b"LIBRARY"
            | b"RUNTIME"
            | b"OBJECTS"
            | b"FRAMEWORK"
            | b"BUNDLE"
            | b"PRIVATE_HEADER"
            | b"PUBLIC_HEADER"
            | b"RESOURCE"
            | b"FILE_SET"
            | b"CXX_MODULES_BMI"
    )
}

fn target_option(key: &[u8]) -> bool {
    group_key(key)
        || matches!(
            key,
            b"EXPORT"
                | b"INCLUDES"
                | b"RUNTIME_DEPENDENCY_TARGETS"
                | b"DESTINATION"
                | b"COMPONENT"
                | b"PERMISSIONS"
                | b"CONFIGURATIONS"
                | b"OPTIONAL"
                | b"EXCLUDE_FROM_ALL"
                | b"NAMELINK_ONLY"
                | b"NAMELINK_SKIP"
                | b"NAMELINK_COMPONENT"
        )
}

fn target_options<'a>(recorded: &[&'a [u8]]) -> IndexMap<&'a [u8], Group<'a>> {
    let mut groups = IndexMap::new();
    let mut group = None;
    let mut args = recorded.iter().copied().skip(1).peekable();
    while let Some(key) = args.next() {
        if group_key(key) {
            group = Some(key);
            groups.entry(key).or_default();
            if key == b"FILE_SET" {
                args.next();
            }
            continue;
        }
        if key == b"INCLUDES" {
            args.next();
            while args.next_if(|arg| !target_option(arg)).is_some() {}
            continue;
        }
        let mut read = || {
            args.next_if(|value| !target_option(value))
                .map(BString::from)
        };
        let entry: &mut Group<'_> = match group {
            Some(group) => groups.entry(group).or_default(),
            None => {
                if matches!(key, b"EXPORT" | b"DESTINATION" | b"COMPONENT") {
                    read();
                }
                continue;
            }
        };
        match key {
            b"DESTINATION" => entry.options.destination = read().filter(|path| !path.is_empty()),
            b"COMPONENT" => entry.options.component = read(),
            b"OPTIONAL" => entry.options.optional = true,
            b"EXCLUDE_FROM_ALL" => entry.options.exclude_from_all = true,
            b"NAMELINK_ONLY" => entry.namelink_only = true,
            b"NAMELINK_SKIP" => entry.namelink_skip = true,
            b"NAMELINK_COMPONENT" => entry.namelink_component = args.next(),
            b"PERMISSIONS" | b"CONFIGURATIONS" => {
                let values = std::iter::from_fn(|| {
                    args.next_if(|arg| !target_option(arg)).map(BString::from)
                })
                .collect();
                if key == b"PERMISSIONS" {
                    entry.options.permissions = values;
                } else {
                    entry.options.configurations = values;
                }
            }
            _ => {}
        }
    }
    groups
}

fn directory_options(recorded: &[&[u8]], permissions: &[BString]) -> Vec<BString> {
    let mut options = Vec::new();
    if !permissions.is_empty() {
        options.push("FILE_PERMISSIONS".into());
        options.extend_from_slice(permissions);
    }
    let mut args = recorded.iter().copied().skip(1).peekable();
    while let Some(key) = args.next() {
        match key {
            b"DESTINATION" | b"TYPE" | b"COMPONENT" | b"RENAME" => {
                args.next();
            }
            b"PATTERN" | b"REGEX" => {
                options.push(key.into());
                options.extend(args.map(BString::from));
                break;
            }
            b"FILES_MATCHING" | b"USE_SOURCE_PERMISSIONS" | b"MESSAGE_NEVER" => {
                options.push(key.into())
            }
            b"DIRECTORY_PERMISSIONS" => {
                options.push(key.into());
                while let Some(permission) = args.next_if(|arg| permission_keyword(arg)) {
                    options.push(permission.into());
                }
            }
            _ => {}
        }
    }
    options
}

fn permission_keyword(value: &[u8]) -> bool {
    value.starts_with(b"OWNER_")
        || value.starts_with(b"GROUP_")
        || value.starts_with(b"WORLD_")
        || matches!(value, b"SETUID" | b"SETGID")
}

/// Return the effective options for an artifact group in a target install rule.
#[must_use]
pub fn install_target_group_options(
    model: &ProjectModel,
    index: usize,
    group: &[u8],
) -> Option<InstallOptions> {
    let base = &model.installs.get(index)?.options;
    let recorded = recorded_args(model, index);
    let groups = target_options(&recorded);
    Some(
        groups
            .get(group)
            .map_or_else(|| base.clone(), |group| group.merge(base)),
    )
}

/// Check whether a target install rule requests an unnamed dependency set.
#[must_use]
pub fn installs_unnamed_runtime_dependencies(model: &ProjectModel, rule: usize) -> bool {
    recorded_args(model, rule).contains(&b"RUNTIME_DEPENDENCIES".as_slice())
}
