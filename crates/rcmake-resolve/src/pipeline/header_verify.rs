use std::collections::BTreeSet;
use std::path::Path;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_diag::{
    DiagCategory, Diagnostic, Effect, MessageKind, PolicyId, PolicyStatus, RenderOptions, render,
};
use rcmake_genex::ConfigId;
use rcmake_host::{Host, ReadRange, Stream, WriteMode, path};
use rcmake_model::{
    BT, DirId, FileSetKind, GeneratorKind, LinkItem, ProjectModel, Target, TargetId, TargetKind,
    TargetOrigin, UsageField, UsageScope,
};

use super::{ConfigTargets, enabled_languages};
use crate::{
    ResolveError, ResolvedTarget,
    cxx_modules::generation_location,
    file_sets::set_files,
    lower::{definition, source_language},
    sources::{SourcePaths, TargetInputs, properties},
    usage::UsageResolver,
};

const LANGUAGES: &[(&[u8], &[u8])] = &[
    (b"OBJCXX", b".mm"),
    (b"SYCL", b".sycl"),
    (b"CXX", b".cxx"),
    (b"OBJC", b".m"),
    (b"C", b".c"),
];

#[derive(Clone, Copy)]
enum Side {
    Private,
    Interface,
}

impl Side {
    fn suffix(self) -> &'static str {
        match self {
            Self::Private => "_verify_private_header_sets",
            Self::Interface => "_verify_interface_header_sets",
        }
    }

    fn settings(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Private => (
                "VERIFY_PRIVATE_HEADER_SETS",
                "PRIVATE_HEADER_SETS_TO_VERIFY",
                "PRIVATE",
            ),
            Self::Interface => (
                "VERIFY_INTERFACE_HEADER_SETS",
                "INTERFACE_HEADER_SETS_TO_VERIFY",
                "INTERFACE",
            ),
        }
    }

    fn accepts(self, scope: UsageScope) -> bool {
        match self {
            Self::Private => scope != UsageScope::Interface,
            Self::Interface => scope != UsageScope::Private,
        }
    }

    fn enabled(self, target: &Target) -> bool {
        use TargetKind::*;
        matches!(
            target.kind,
            Executable
                | StaticLibrary
                | SharedLibrary
                | UnknownLibrary
                | ObjectLibrary
                | InterfaceLibrary
        ) || (target.kind == ModuleLibrary && matches!(self, Self::Private))
    }
}

struct Stub {
    header: BString,
    language: BString,
    inputs: IndexMap<ConfigId, BString>,
}

pub(super) struct Verification {
    pub(super) model: ProjectModel,
    targets: IndexMap<TargetId, Request>,
}

struct Request {
    original: TargetId,
    headers: IndexMap<ConfigId, IndexSet<BString>>,
}

pub(super) fn prepare(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    configs: &[ConfigId],
    configured: &[ConfigTargets],
) -> Result<Option<Verification>, ResolveError> {
    let enabled = enabled_languages(model);
    let mut usage = UsageResolver::with_source_paths(model, paths);
    let mut prepared = None;
    let mut aggregates = Vec::new();
    for side in [Side::Private, Side::Interface] {
        let mut names = Vec::new();
        for (id, target) in model.targets() {
            if target.is_imported()
                || !side.enabled(target)
                || !is_on(target, side.settings().0.as_bytes())
            {
                continue;
            }
            let Some(stubs) = plan(model, paths, &mut usage, target, side, configs, || {
                default_language(id, configured, &enabled)
            })?
            else {
                continue;
            };
            let updated = prepared.get_or_insert_with(|| Verification {
                model: model.clone(),
                targets: IndexMap::new(),
            });
            let name = BString::from(format!("{}{}", target.name, side.suffix()));
            let mut request = Request {
                original: id,
                headers: IndexMap::new(),
            };
            for stub in stubs.values() {
                for (config, file) in &stub.inputs {
                    request
                        .headers
                        .entry(config.clone())
                        .or_default()
                        .insert(file.clone());
                }
            }
            let verify = verification_target(
                &mut updated.model,
                paths.host,
                target,
                side,
                name.clone(),
                stubs,
                configs,
            )?;
            let has_stubs = verify.kind == TargetKind::ObjectLibrary;
            let id = add_target(&mut updated.model, verify)?;
            if has_stubs {
                updated.targets.insert(id, request);
            }
            names.push(name);
        }
        if let Some(updated) = &mut prepared
            && !names.is_empty()
        {
            let name = BString::from(format!("all{}", side.suffix()));
            add_aggregate(&mut updated.model, name.clone(), names)?;
            aggregates.push(name);
        }
    }
    if let Some(updated) = &mut prepared {
        add_aggregate(
            &mut updated.model,
            "all_verify_header_sets".into(),
            aggregates,
        )?;
    }
    Ok(prepared)
}

impl Verification {
    pub(super) fn add_header_inputs(
        &self,
        config: &ConfigId,
        resolved: &mut IndexMap<TargetId, ResolvedTarget>,
        inputs: &mut IndexMap<TargetId, TargetInputs>,
    ) {
        let ninja = matches!(
            self.model.generator,
            Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
        );
        for (&id, request) in &self.targets {
            let Some(target) = resolved.get(&id) else {
                continue;
            };
            let mut sources = request.headers.get(config).cloned().unwrap_or_default();
            let mut dependencies = IndexSet::new();
            if ninja {
                let mut pending = target.dependencies.clone();
                pending.push(request.original);
                let mut visited = IndexSet::from([id]);
                while let Some(dep) = pending.pop() {
                    if !visited.insert(dep) {
                        continue;
                    }
                    let Some(facts) = resolved.get(&dep) else {
                        continue;
                    };
                    if matches!(
                        facts.metadata.kind,
                        TargetKind::Utility | TargetKind::GlobalTarget
                    ) {
                        dependencies.insert(dep);
                    } else {
                        if let Some(input) = inputs.get(&dep) {
                            sources.extend(input.sources.iter().cloned());
                        }
                        pending.extend(&facts.dependencies);
                    }
                }
                if let Some(target) = resolved.get_mut(&id) {
                    target.dependencies = dependencies.into_iter().collect();
                }
            }
            if let Some(target_inputs) = inputs.get_mut(&id) {
                target_inputs.sources.extend(sources);
            }
        }
    }
}

fn plan(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    usage: &mut UsageResolver<'_>,
    target: &Target,
    side: Side,
    configs: &[ConfigId],
    default_language: impl FnOnce() -> Option<&'static [u8]>,
) -> Result<Option<IndexMap<BString, Stub>>, ResolveError> {
    let (_, selection, scope) = side.settings();
    let select = target
        .props
        .get(selection.as_bytes())
        .filter(|value| !value.is_empty());
    let mut missing: BTreeSet<_> = select
        .into_iter()
        .flat_map(|value| list::split(value, list::Empty::Drop))
        .collect();
    let mut sets = Vec::new();
    for set in target.file_sets.iter().filter_map(|id| model.file_set(*id)) {
        if set.kind == FileSetKind::Headers
            && side.accepts(set.scope)
            && (select.is_none() || missing.remove(&set.name))
            && !set
                .props
                .get(b"SKIP_LINTING")
                .is_some_and(|value| truth::is_on(value))
        {
            sets.push(set);
        }
    }
    let policy = target.policies.policies.get(PolicyId::new(209));
    if matches!(side, Side::Interface)
        && target.kind == TargetKind::Executable
        && !is_on(target, b"ENABLE_EXPORTS")
        && policy != PolicyStatus::New
    {
        if policy == PolicyStatus::Warn && !sets.is_empty() {
            policy_warning(model, paths.host, target);
        }
        return Ok(None);
    }
    if !missing.is_empty() {
        let names = missing
            .iter()
            .map(|value| value.as_slice())
            .collect::<Vec<_>>()
            .join(b"\n  ".as_slice());
        let body = BString::from(format!(
            "Property {selection} of target \"{}\" contained the following header sets that are nonexistent or not {scope}:\n\n  {}",
            target.name,
            BString::from(names)
        ));
        paths.emit(
            model,
            MessageKind::Error,
            generation_location(model, target),
            body.clone(),
        );
        return Err(ResolveError::Reported(body));
    }
    let mut stubs = IndexMap::<BString, Stub>::new();
    let fallback = if sets.is_empty() {
        None
    } else {
        default_language()
    };
    let dir = model.dir(target.dir);
    for config in configs {
        for set in &sets {
            for (file, header) in set_files(model, usage, target, set, config)?
                .into_values()
                .flatten()
            {
                let props = properties(dir, &file);
                if props
                    .get(b"SKIP_LINTING")
                    .is_some_and(|value| truth::is_on(value))
                {
                    continue;
                }
                let explicit = props
                    .get(b"LANGUAGE")
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .or_else(|| source_language(model, target, &file));
                let language = explicit.as_ref().map(|value| value.as_slice()).or(fallback);
                let Some(&(language, ext)) =
                    LANGUAGES.iter().find(|(lang, _)| Some(*lang) == language)
                else {
                    continue;
                };
                let relative = [header.as_slice(), ext].concat().into();
                stubs
                    .entry(relative)
                    .or_insert_with(|| Stub {
                        header,
                        language: language.into(),
                        inputs: IndexMap::new(),
                    })
                    .inputs
                    .insert(config.clone(), file);
            }
        }
    }
    Ok(Some(stubs))
}

fn default_language(
    id: TargetId,
    configured: &[ConfigTargets],
    enabled: &[BString],
) -> Option<&'static [u8]> {
    let supported = |lang: &[u8]| LANGUAGES.iter().any(|(candidate, _)| *candidate == lang);
    let mut languages: IndexSet<_> = configured
        .iter()
        .filter_map(|config| config.targets.get(&id))
        .flat_map(|facts| {
            facts
                .compile_groups
                .iter()
                .map(|group| group.language.as_slice())
        })
        .filter(|lang| supported(lang))
        .collect();
    if languages.is_empty() {
        languages.extend(enabled.iter().map(|value| value.as_slice()));
    }
    if languages.contains(b"OBJC".as_slice())
        && (languages.contains(b"CXX".as_slice()) || languages.contains(b"SYCL".as_slice()))
    {
        return Some(b"OBJCXX");
    }
    LANGUAGES
        .iter()
        .find_map(|&(lang, _)| languages.contains(lang).then_some(lang))
}

fn verification_target(
    model: &mut ProjectModel,
    host: &dyn Host,
    original: &Target,
    side: Side,
    name: BString,
    stubs: IndexMap<BString, Stub>,
    configs: &[ConfigId],
) -> Result<Target, ResolveError> {
    let kind = if stubs.is_empty() {
        TargetKind::Utility
    } else {
        TargetKind::ObjectLibrary
    };
    let mut target = Target::new(
        name,
        kind,
        TargetOrigin::Normal,
        original.dir,
        original.backtrace.clone(),
        original.policies.policies,
    );
    target.props.set("EXCLUDE_FROM_ALL".into(), "TRUE".into());
    if stubs.is_empty() {
        return Ok(target);
    }
    target
        .policies
        .policies
        .set(PolicyId::new(119), PolicyStatus::New)
        .map_err(|error| ResolveError::Message(error.to_string().into()))?;
    let original_name = BT::new(original.name.clone(), original.backtrace.clone());
    match side {
        Side::Interface
            if original.kind != TargetKind::Executable || is_on(original, b"ENABLE_EXPORTS") =>
        {
            target.usage.link_libraries = Some(vec![LinkItem {
                text: original_name,
                lookup_dir: original.dir,
            }]);
        }
        Side::Interface => {}
        Side::Private => {
            for field in [
                UsageField::CompileDefinitions,
                UsageField::CompileOptions,
                UsageField::CompileFeatures,
                UsageField::IncludeDirectories,
            ] {
                if let Some(entries) = original.usage.fields.get(&field) {
                    target.usage.fields.insert(field, entries.clone());
                }
            }
            target
                .usage
                .link_libraries
                .clone_from(&original.usage.link_libraries);
            if !matches!(
                model.generator,
                Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
            ) {
                target.utility_deps.push(original_name);
            }
            for key in [
                b"COMPILE_FLAGS".as_slice(),
                b"DEFINE_SYMBOL",
                b"C_STANDARD",
                b"C_STANDARD_REQUIRED",
                b"C_EXTENSIONS",
                b"CXX_STANDARD",
                b"CXX_STANDARD_REQUIRED",
                b"CXX_EXTENSIONS",
                b"OBJC_STANDARD",
                b"OBJC_STANDARD_REQUIRED",
                b"OBJC_EXTENSIONS",
                b"OBJCXX_STANDARD",
                b"OBJCXX_STANDARD_REQUIRED",
                b"OBJCXX_EXTENSIONS",
                b"SYCL_CXX_STANDARD",
                b"SYCL_CXX_STANDARD_REQUIRED",
                b"SYCL_EXTENSIONS",
            ] {
                copy_property(&mut target, original, key);
            }
        }
    }
    copy_property(&mut target, original, b"SYCL_EXTENSION_MODE");
    for (key, value) in [
        ("AUTOMOC", "OFF"),
        ("AUTORCC", "OFF"),
        ("AUTOUIC", "OFF"),
        ("DISABLE_PRECOMPILE_HEADERS", "ON"),
        ("UNITY_BUILD", "OFF"),
        ("CXX_SCAN_FOR_MODULES", "OFF"),
    ] {
        target.props.set(key.into(), value.into());
    }
    let dir = model.dir_mut(original.dir);
    let base = Lexical::new(&dir.binary_dir).append(&target.name);
    for (relative, stub) in stubs {
        let filename = Lexical::new(&base).append(&relative);
        let body = [
            b"/* NOLINTNEXTLINE(misc-header-include-cycle,misc-include-cleaner) */\n#include <"
                .as_slice(),
            &stub.header,
            b"> /* IWYU pragma: associated */\n",
        ]
        .concat();
        write_stub(host, &path::to_native(&filename), &body)?;
        let props = dir.source_props.entry(filename.clone()).or_default();
        props.set("LANGUAGE".into(), stub.language);
        let entry = if stub.inputs.len() == configs.len() {
            filename
        } else {
            let values: Vec<_> = stub
                .inputs
                .keys()
                .map(|config| config.0.as_slice())
                .collect();
            [
                b"$<$<CONFIG:".as_slice(),
                &values.join(&b','),
                b">:",
                &filename,
                b">",
            ]
            .concat()
            .into()
        };
        target
            .sources
            .push(BT::new(entry, original.backtrace.clone()));
    }
    Ok(target)
}

fn copy_property(dst: &mut Target, src: &Target, key: &[u8]) {
    if let Some(value) = src.props.get(key) {
        dst.props.set(key.into(), value.clone());
    }
}

fn write_stub(host: &dyn Host, path: &Path, body: &[u8]) -> Result<(), ResolveError> {
    if host
        .read(
            path,
            ReadRange::All {
                limit: body.len() as u64,
            },
        )
        .is_ok_and(|old| old == body)
    {
        return Ok(());
    }
    let write = || {
        if let Some(parent) = path.parent() {
            host.create_dir_all(parent)?;
        }
        host.write(path, body, WriteMode::Truncate)
    };
    write().map_err(|error| {
        ResolveError::Message(format!("Cannot write {}: {error}", path.display()).into())
    })
}

fn add_aggregate(
    model: &mut ProjectModel,
    name: BString,
    deps: Vec<BString>,
) -> Result<(), ResolveError> {
    let mut target = Target::new(
        name,
        TargetKind::Utility,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    target.props.set("EXCLUDE_FROM_ALL".into(), "TRUE".into());
    target.utility_deps = deps
        .into_iter()
        .map(|dep| BT::new(dep, Default::default()))
        .collect();
    add_target(model, target).map(|_| ())
}

fn add_target(model: &mut ProjectModel, target: Target) -> Result<TargetId, ResolveError> {
    let name = target.name.clone();
    model.add_target(target).map_err(|error| {
        ResolveError::Message(
            format!("Cannot add header verification target \"{name}\": {error}").into(),
        )
    })
}

fn is_on(target: &Target, key: &[u8]) -> bool {
    target
        .props
        .get(key)
        .is_some_and(|value| truth::is_on(value))
}

fn policy_warning(model: &ProjectModel, host: &dyn Host, target: &Target) {
    if definition(model, target.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
        .is_some_and(|value| truth::is_on(value))
    {
        return;
    }
    let body = format!(
        "Policy CMP0209 is not set: Verify interface header sets checks executables without exports.  Run \"cmake --help-policy CMP0209\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\n\nExecutable target \"{}\" has interface header file sets, but it does not enable exports.  Those headers would be verified under CMP0209 NEW behavior.",
        target.name
    );
    let diag = Diagnostic {
        kind: MessageKind::AuthorWarning,
        effect: Effect::None,
        category: Some(DiagCategory::Policy),
        policy: Some(PolicyId::new(209)),
        location: generation_location(model, target),
        body: body.into(),
    };
    let options = RenderOptions {
        relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
        ..RenderOptions::default()
    };
    let mut warning = render(&diag, &options);
    let footer = b"This warning is for project developers.";
    if let Some(start) = warning
        .windows(footer.len())
        .position(|part| part == footer)
    {
        warning.insert(start, b'\n');
    }
    host.write_out(Stream::Stderr, &warning);
}
