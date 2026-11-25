use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, shell::quote, truth};
use rcmake_diag::MessageKind;
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::{Host, ReadRange, WriteMode, path::to_native};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetId, TargetKind, UsageField};

use super::{definition, expand_rule, join, object_dir, policy_flags::Flags};
use crate::{
    CompileUnit, ResolveError, ResolvedTarget,
    cxx_modules::generation_location,
    ide::{IdeCustomStep, IdePch},
    sources,
    usage::UsageResolver,
};

/// IDE facts that lowering computes for each target.
#[derive(Default)]
pub(crate) struct IdeLowering {
    /// Custom commands and build events.
    pub(crate) custom: IndexMap<TargetId, Vec<IdeCustomStep>>,
    /// Precompiled headers.
    pub(crate) pch: IndexMap<TargetId, Vec<IdePch>>,
}

pub(super) mod pdb;

const REUSE: &[u8] = b"PRECOMPILE_HEADERS_REUSE_FROM";

pub(super) struct Plan {
    pub(super) arch: Option<BString>,
    pub(super) companion_object: Option<BString>,
    pub(super) header: BString,
    pub(super) source: BString,
    pub(super) artifact: BString,
    pub(super) project_headers: Vec<BString>,
}

#[derive(Clone, Copy)]
pub(super) enum PchFlags<'a> {
    Create(&'a Plan),
    Use(&'a [&'a Plan]),
}

impl<'a> PchFlags<'a> {
    pub(super) fn architecture(self) -> Option<&'a [u8]> {
        match self {
            Self::Create(plan) => plan.arch.as_ref().map(|value| value.as_slice()),
            Self::Use(_) => None,
        }
    }

    pub(super) fn options(
        self,
        model: &ProjectModel,
        target: &Target,
        language: &[u8],
    ) -> Vec<BString> {
        let policy = Flags::new(model, target, language);
        let (plan, option): (_, &[u8]) = match self {
            Self::Create(plan) => (plan, b"COMPILE_OPTIONS_CREATE_PCH"),
            Self::Use([plan]) => (*plan, b"COMPILE_OPTIONS_USE_PCH"),
            Self::Use(plans) => {
                return plans
                    .iter()
                    .filter_map(|plan| {
                        let arch = plan.arch.as_ref()?;
                        Some([
                            quote(&[b"-Xarch_", arch.as_slice()].concat()),
                            quote(&[b"-include", plan.header.as_slice()].concat()),
                        ])
                    })
                    .flatten()
                    .collect();
            }
        };
        let mut flags = Vec::new();
        if target
            .props
            .get(b"PCH_WARN_INVALID")
            .is_none_or(|value| truth::is_on(value))
        {
            flags.extend(
                policy
                    .option(b"COMPILE_OPTIONS_INVALID_PCH")
                    .into_iter()
                    .flat_map(|value| list::split(value, list::Empty::Drop))
                    .map(|value| quote(&value)),
            );
        }
        flags.extend(
            policy
                .option(option)
                .into_iter()
                .flat_map(|value| list::split(value, list::Empty::Drop))
                .map(|option| {
                    expand_rule(&option, |key| match key {
                        b"PCH_HEADER" => Some(quote(&plan.header)),
                        b"PCH_FILE" => Some(quote(&plan.artifact)),
                        _ => None,
                    })
                }),
        );
        flags
    }
}

#[derive(Default)]
pub(crate) struct Plans {
    owners: IndexMap<TargetId, TargetId>,
    units: IndexMap<(TargetId, BString), Vec<Plan>>,
}

impl Plans {
    pub(crate) fn new(
        model: &ProjectModel,
        targets: &IndexMap<TargetId, &ResolvedTarget>,
        inputs: &IndexMap<TargetId, sources::TargetInputs>,
        config: &ConfigId,
        usage: &mut UsageResolver<'_>,
    ) -> Result<Self, ResolveError> {
        let Some(paths) = usage.source_paths() else {
            return Ok(Self::default());
        };
        let host = paths.host;
        if !model.targets().any(|(_, target)| {
            target.props.get(REUSE).is_some()
                || [
                    UsageField::PrecompileHeaders,
                    UsageField::InterfacePrecompileHeaders,
                ]
                .into_iter()
                .any(|field| !target.usage.entries(field).is_empty())
        }) {
            return Ok(Self::default());
        }
        let mut plans = Self::default();
        let mut warned = IndexSet::new();
        for (&id, resolved) in targets {
            let Some(target) = model.target(id) else {
                continue;
            };
            if disabled(target) {
                continue;
            }
            let owner = reuse_owner(model, id, target, |id, target, name| {
                if warned.insert(id) {
                    paths.emit(
                        model,
                        MessageKind::Warning,
                        generation_location(model, target),
                        format!(
                            "Target \"{name}\" for the \"{}\" target's \"PRECOMPILE_HEADERS_REUSE_FROM\" property has set \"DISABLE_PRECOMPILE_HEADERS\"; ignoring.",
                            target.name
                        ).into(),
                    );
                }
            }).map_err(|body| {
                paths.emit(
                    model,
                    MessageKind::Error,
                    generation_location(model, target),
                    body.clone(),
                );
                ResolveError::Reported(body)
            })?;
            if let Some(owner) = owner {
                plans.owners.insert(id, owner);
                continue;
            }
            let languages = resolved
                .compile_units
                .iter()
                .filter(|unit| !skip_unit(model, target, unit, inputs.get(&id)))
                .filter_map(|unit| resolved.compile_groups.get(unit.group))
                .map(|group| &group.language)
                .collect::<IndexSet<_>>();
            for language in languages {
                let architectures = selected_architectures(model, target, config, language);
                let architectures = if architectures.len() > 1 {
                    architectures.into_iter().map(Some).collect()
                } else {
                    vec![None]
                };
                let mut units = Vec::new();
                for arch in architectures {
                    if let Some(plan) =
                        plan(model, host, usage, id, target, config, language, arch)?
                    {
                        units.push(plan);
                    }
                }
                if !units.is_empty() {
                    plans.units.insert((id, language.clone()), units);
                }
            }
        }
        Ok(plans)
    }

    /// Return the precompiled headers of `target` for an IDE generator.
    pub(super) fn ide(&self, model: &ProjectModel, target: &ResolvedTarget) -> Vec<IdePch> {
        let languages: IndexSet<_> = target
            .compile_groups
            .iter()
            .map(|group| &group.language)
            .collect();
        languages
            .into_iter()
            .filter_map(|language| {
                let owner = self.owner(target.id);
                let plan = self.units.get(&(owner, language.clone()))?.first()?;
                Some(IdePch {
                    language: language.clone(),
                    header: plan.header.clone(),
                    source: plan.source.clone(),
                    artifact: join(&model.dir(DirId::ROOT).binary_dir, &plan.artifact),
                    compile_pdb: None,
                    companion: plan
                        .companion_object
                        .as_ref()
                        .map(|file| join(&model.dir(DirId::ROOT).binary_dir, file)),
                    reuse_from: (owner != target.id).then_some(owner),
                })
            })
            .collect()
    }

    fn owner(&self, target: TargetId) -> TargetId {
        self.owners.get(&target).copied().unwrap_or(target)
    }

    pub(super) fn find(
        &self,
        model: &ProjectModel,
        target: &Target,
        resolved: &ResolvedTarget,
        language: &BString,
    ) -> Result<Option<(TargetId, Vec<&Plan>)>, ResolveError> {
        let owner = self.owner(resolved.id);
        let Some(plans) = self.units.get(&(owner, language.clone())) else {
            return Ok(None);
        };
        if owner == resolved.id {
            return Ok(Some((owner, plans.iter().collect())));
        }
        let archs = selected_architectures(model, target, &resolved.config, language);
        let missing = || {
            ResolveError::Message(
            format!("Target \"{}\" cannot reuse a precompiled header for its selected architectures.", target.name).into()
        )
        };
        let selected = if let [plan] = plans.as_slice()
            && plan.arch.is_none()
        {
            let provider_archs = model
                .target(owner)
                .map(|provider| selected_architectures(model, provider, &resolved.config, language))
                .unwrap_or_default();
            if provider_archs != archs {
                return Err(missing());
            }
            vec![plan]
        } else {
            if archs.is_empty() {
                return Err(missing());
            }
            archs
                .iter()
                .map(|arch| {
                    plans
                        .iter()
                        .find(|plan| plan.arch.as_ref() == Some(arch))
                        .ok_or_else(missing)
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(Some((owner, selected)))
    }

    pub(crate) fn register_objects(
        &self,
        model: &ProjectModel,
        config: &ConfigId,
        usage: &mut UsageResolver<'_>,
    ) {
        usage.pch_objects.extend(
            model
                .targets()
                .filter(|(_, target)| target.kind == TargetKind::ObjectLibrary)
                .filter_map(|(id, _)| {
                    let objects: Vec<_> =
                        self.object_library_companions(model, id).cloned().collect();
                    (!objects.is_empty()).then(|| ((id, config.clone()), objects))
                }),
        );
    }

    pub(crate) fn object_library_companions(
        &self,
        model: &ProjectModel,
        id: TargetId,
    ) -> impl Iterator<Item = &BString> {
        let owner = self.owner(id);
        let include = owner == id
            || model
                .target(owner)
                .is_some_and(|target| target.kind == TargetKind::ObjectLibrary);
        self.units
            .iter()
            .filter(move |((id, _), _)| include && *id == owner)
            .flat_map(|(_, plans)| {
                plans
                    .iter()
                    .filter_map(|plan| plan.companion_object.as_ref())
            })
    }
}

fn selected_architectures(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
) -> Vec<BString> {
    if model.generator == Some(GeneratorKind::Xcode) {
        return Vec::new();
    }
    super::apple::arch::architectures(model, target, config, language)
}

fn disabled(target: &Target) -> bool {
    target
        .props
        .get(b"DISABLE_PRECOMPILE_HEADERS")
        .is_some_and(|value| truth::is_on(value))
}

pub(super) fn skip_unit(
    model: &ProjectModel,
    target: &Target,
    unit: &CompileUnit,
    inputs: Option<&sources::TargetInputs>,
) -> bool {
    sources::properties(model.dir(target.dir), &unit.source.value)
        .get(b"SKIP_PRECOMPILE_HEADERS")
        .is_some_and(|value| truth::is_on(value))
        || inputs.is_some_and(|inputs| inputs.sources_without_pch.contains(&unit.source.value))
}

fn reuse_owner(
    model: &ProjectModel,
    id: TargetId,
    origin: &Target,
    mut warn: impl FnMut(TargetId, &Target, &BString),
) -> Result<Option<TargetId>, BString> {
    let mut visited = IndexSet::from([id]);
    let (mut current, mut target) = (id, origin);
    while let Some(name) = target.props.get(REUSE).filter(|name| !name.is_empty()) {
        let missing = || -> BString {
            format!(
                "Unable to resolve precompiled header reuse target \"{name}\" for target \"{}\".",
                target.name
            )
            .into()
        };
        let next = model.find_target(target.dir, name).ok_or_else(missing)?;
        let provider = model.target(next).ok_or_else(missing)?;
        if disabled(provider) {
            warn(current, target, name);
            break;
        }
        if !visited.insert(next) {
            return Err(format!("Circular PCH reuse target involving '{}'", provider.name).into());
        }
        (current, target) = (next, provider);
    }
    Ok((current != id).then_some(current))
}

const LANGUAGES: &[(&[u8], &[u8], &[u8])] = &[
    (b"C", b"cmake_pch.h", b".c"),
    (b"CXX", b"cmake_pch.hxx", b".cxx"),
    (b"OBJC", b"cmake_pch.objc.h", b".m"),
    (b"OBJCXX", b"cmake_pch.objcxx.hxx", b".mm"),
];

#[expect(
    clippy::too_many_arguments,
    reason = "Each precompiled header is configured per language and architecture."
)]
fn plan(
    model: &ProjectModel,
    host: &dyn Host,
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    target: &Target,
    config: &ConfigId,
    language: &BString,
    arch: Option<BString>,
) -> Result<Option<Plan>, ResolveError> {
    let Some(&(_, name, suffix)) = LANGUAGES
        .iter()
        .find(|(key, ..)| *key == language.as_slice())
    else {
        return Ok(None);
    };
    let property = |key| definition(model, target.dir, key);
    let Some(extension) =
        property(b"CMAKE_PCH_EXTENSION").filter(|extension| !extension.is_empty())
    else {
        return Ok(None);
    };
    if Flags::new(model, target, language)
        .option(b"COMPILE_OPTIONS_CREATE_PCH")
        .is_none()
    {
        return Ok(None);
    }
    let ctx = EvalContext {
        config: config.clone(),
        language: Some(language.clone()),
        ..EvalContext::default()
    };
    let entries = usage
        .collect(id, UsageField::PrecompileHeaders, &ctx)?
        .entries;
    if entries.is_empty() {
        return Ok(None);
    }
    let linked = property(b"CMAKE_LINK_PCH").is_some_and(|value| truth::is_on(value));
    let stem = arch.as_ref().map_or_else(
        || BString::from("cmake_pch"),
        |arch| [b"cmake_pch_", arch.as_slice()].concat().into(),
    );
    let name = [&stem, name.strip_prefix(b"cmake_pch").unwrap_or(name)].concat();
    let dir = object_dir(model, target, config);
    let relative = join(&dir, &name);
    let root = &model.dir(DirId::ROOT).binary_dir;
    let header = join(root, &relative);
    let source = if linked {
        join(root, &join(&dir, &[stem.as_slice(), suffix].concat()))
    } else {
        [header.as_slice(), suffix].concat().into()
    };
    let companion_object = linked.then(|| super::object_path(model, target, config, &source));
    let artifact = if let Some(object) = &companion_object {
        let stem = object
            .iter()
            .rposition(|&byte| byte == b'.')
            .and_then(|index| object.get(..index))
            .unwrap_or(object.as_slice());
        [stem, extension.as_slice()].concat().into()
    } else {
        [relative.as_slice(), extension.as_slice()].concat().into()
    };
    let mut content = Vec::new();
    let mut project_headers = Vec::new();
    if let Some(prologue) = property(b"CMAKE_PCH_PROLOGUE") {
        content.extend_from_slice(prologue);
        content.push(b'\n');
    }
    let xcode = model.generator == Some(rcmake_model::GeneratorKind::Xcode);
    if xcode {
        content.extend_from_slice(b"#ifndef CMAKE_SKIP_PRECOMPILE_HEADERS\n");
    } else if language == b"CXX" {
        content.extend_from_slice(b"#ifdef __cplusplus\n");
    }
    for entry in entries {
        content.extend_from_slice(b"#include ");
        if entry.value.starts_with(b"<") || entry.value.starts_with(b"\"") {
            content.extend_from_slice(&entry.value);
        } else {
            if to_native(&entry.value).is_absolute() {
                project_headers.push(entry.value.clone());
            }
            content.push(b'"');
            content.extend_from_slice(&entry.value);
            content.push(b'"');
        }
        content.push(b'\n');
    }
    if xcode || language == b"CXX" {
        content.extend_from_slice(b"#endif\n");
    }
    if let Some(epilogue) = property(b"CMAKE_PCH_EPILOGUE") {
        content.extend_from_slice(epilogue);
        content.push(b'\n');
    }
    write_source(host, &header, &content)?;
    write_source(host, &source, b"")?;
    Ok(Some(Plan {
        arch,
        companion_object,
        header,
        source,
        artifact,
        project_headers,
    }))
}

fn write_source(host: &dyn Host, path: &BString, contents: &[u8]) -> Result<(), ResolveError> {
    let file = to_native(path);
    let limit = (contents.len() as u64).saturating_add(1);
    if host
        .read(&file, ReadRange::All { limit })
        .is_ok_and(|old| old == contents)
    {
        return Ok(());
    }
    let write = || {
        if let Some(parent) = file.parent() {
            host.create_dir_all(parent)?;
        }
        host.write(&file, contents, WriteMode::Truncate)
    };
    write().map_err(|error| {
        ResolveError::Message(
            format!("Cannot write precompiled header source {path}. {error}").into(),
        )
    })
}
