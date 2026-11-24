use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, shell::quote, truth};
use rcmake_diag::MessageKind;
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::{Host, ReadRange, WriteMode, path::to_native};
use rcmake_model::{DirId, ProjectModel, Target, TargetId, UsageField};

use super::{definition, expand_rule, join, object_dir, policy_flags::Flags};
use crate::{
    CompileUnit, ResolveError, ResolvedTarget, cxx_modules::generation_location, sources,
    usage::UsageResolver,
};

const REUSE: &[u8] = b"PRECOMPILE_HEADERS_REUSE_FROM";

pub(super) struct Plan {
    pub(super) header: BString,
    pub(super) source: BString,
    pub(super) artifact: BString,
    pub(super) project_headers: Vec<BString>,
}

#[derive(Clone, Copy)]
pub(super) enum PchFlags<'a> {
    Create(&'a Plan),
    Use(&'a Plan),
}

impl PchFlags<'_> {
    pub(super) fn options(
        self,
        model: &ProjectModel,
        target: &Target,
        language: &[u8],
    ) -> Vec<BString> {
        let policy = Flags::new(model, target, language);
        let (plan, option): (_, &[u8]) = match self {
            Self::Create(plan) => (plan, b"COMPILE_OPTIONS_CREATE_PCH"),
            Self::Use(plan) => (plan, b"COMPILE_OPTIONS_USE_PCH"),
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
pub(super) struct Plans {
    owners: IndexMap<TargetId, TargetId>,
    units: IndexMap<(TargetId, BString), Plan>,
}

impl Plans {
    pub(super) fn new(
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
        for (&id, resolved) in targets {
            let Some(target) = model.target(id) else {
                continue;
            };
            if disabled(target) {
                continue;
            }
            if target.props.get(REUSE).is_some_and(|name| !name.is_empty()) {
                let owner = reuse_owner(model, id, target).map_err(|body| {
                    paths.emit(
                        model,
                        MessageKind::Error,
                        generation_location(model, target),
                        body.clone(),
                    );
                    ResolveError::Reported(body)
                })?;
                if let Some(owner_target) = model.target(owner).filter(|owner| disabled(owner)) {
                    paths.emit(
                        model,
                        MessageKind::Warning,
                        generation_location(model, target),
                        format!(
                            "Target \"{}\" for the \"{}\" target's \"PRECOMPILE_HEADERS_REUSE_FROM\" property has set \"DISABLE_PRECOMPILE_HEADERS\"; ignoring.",
                            owner_target.name, target.name
                        ).into(),
                    );
                } else {
                    plans.owners.insert(id, owner);
                }
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
                if let Some(plan) = plan(model, host, usage, id, target, config, language)? {
                    plans.units.insert((id, language.clone()), plan);
                }
            }
        }
        Ok(plans)
    }

    pub(super) fn find(&self, target: TargetId, language: &BString) -> Option<(TargetId, &Plan)> {
        let owner = self.owners.get(&target).copied().unwrap_or(target);
        self.units
            .get(&(owner, language.clone()))
            .map(|plan| (owner, plan))
    }
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

fn reuse_owner(model: &ProjectModel, id: TargetId, origin: &Target) -> Result<TargetId, BString> {
    let mut visited = IndexSet::from([id]);
    let (mut current, mut target) = (id, origin);
    while let Some(name) = target.props.get(REUSE).filter(|name| !name.is_empty()) {
        let missing = || {
            format!(
                "Unable to resolve precompiled header reuse target \"{name}\" for target \"{}\".",
                origin.name
            )
            .into()
        };
        current = model.find_target(target.dir, name).ok_or_else(missing)?;
        target = model.target(current).ok_or_else(missing)?;
        if !visited.insert(current) {
            return Err(format!("Circular PCH reuse target involving '{}'", target.name).into());
        }
    }
    Ok(current)
}

const LANGUAGES: &[(&[u8], &[u8], &[u8])] = &[
    (b"C", b"cmake_pch.h", b".c"),
    (b"CXX", b"cmake_pch.hxx", b".cxx"),
    (b"OBJC", b"cmake_pch.objc.h", b".m"),
    (b"OBJCXX", b"cmake_pch.objcxx.hxx", b".mm"),
];

fn plan(
    model: &ProjectModel,
    host: &dyn Host,
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    target: &Target,
    config: &ConfigId,
    language: &BString,
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
        || property(b"CMAKE_LINK_PCH").is_some_and(|value| truth::is_on(value))
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
    let relative = join(&object_dir(model, target, config), name);
    let header = join(&model.dir(DirId::ROOT).binary_dir, &relative);
    let source = BString::from([header.as_slice(), suffix].concat());
    let artifact = BString::from([relative.as_slice(), extension.as_slice()].concat());
    let mut content = Vec::new();
    let mut project_headers = Vec::new();
    if let Some(prologue) = property(b"CMAKE_PCH_PROLOGUE") {
        content.extend_from_slice(prologue);
        content.push(b'\n');
    }
    let cxx = matches!(language.as_slice(), b"CXX" | b"OBJCXX");
    if cxx {
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
    if cxx {
        content.extend_from_slice(b"#endif\n");
    }
    if let Some(epilogue) = property(b"CMAKE_PCH_EPILOGUE") {
        content.extend_from_slice(epilogue);
        content.push(b'\n');
    }
    write_source(host, &header, &content)?;
    write_source(host, &source, b"")?;
    Ok(Some(Plan {
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
