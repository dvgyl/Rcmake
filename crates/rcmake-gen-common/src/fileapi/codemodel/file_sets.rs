//! File set membership and provenance in codemodel sources.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::Backtrace;
use rcmake_genex::{EvalContext, TargetHandle};
use rcmake_model::{
    Directory, FileSetKind, ProjectModel, Target, TargetId, UsageField, UsageScope,
};
use rcmake_resolve::{ResolvedTarget, usage::UsageResolver};

use crate::GenError;

use super::super::{Members, Value, object, relative, strings};
use super::{indices, number};

#[derive(Default)]
pub(super) struct FileSets {
    pub(super) entries: Vec<Value>,
    pub(super) sources: BTreeMap<BString, Membership>,
    pub(super) order: Vec<BString>,
}

pub(super) struct Membership {
    indexes: Vec<usize>,
    pub(super) traces: Vec<Backtrace>,
    pub(super) header: bool,
    pub(super) interface: bool,
    pub(super) in_build: bool,
    pub(super) source_group: Option<BString>,
}

pub(super) fn normalize_source_group_name(dir: &Directory, raw: &[u8]) -> BString {
    let delimiters = dir
        .vars
        .get(b"SOURCE_GROUP_DELIMITER")
        .map_or(b"\\/".as_slice(), |value| value.as_slice());
    raw.split(|byte| delimiters.contains(byte))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(&b'\\')
        .into()
}

impl Membership {
    pub(super) fn tag(&self, entry: &mut Members) {
        indices(entry, "fileSetIndexes", self.indexes.iter().copied());
        if let Some(&index) = self.indexes.last() {
            entry.insert("fileSetIndex".into(), number(index));
        }
    }
}

impl FileSets {
    pub(super) fn collect(
        model: &ProjectModel,
        target: &ResolvedTarget,
        raw: Option<&Target>,
        source: &[u8],
        usage: &mut UsageResolver<'_>,
    ) -> Result<Self, GenError> {
        let Some(raw) = raw else {
            return Ok(Self::default());
        };
        let mut result = Self::default();
        let dir_model = model.dir(raw.dir);
        let dir = &dir_model.source_dir;
        let context = EvalContext {
            config: target.config.clone(),
            head: u32::try_from(target.id.index()).ok().map(TargetHandle),
            ..Default::default()
        };
        for set in raw.file_sets.iter().filter_map(|id| model.file_set(*id)) {
            let set_source_group = set
                .props
                .get(b"SOURCE_GROUP")
                .filter(|value| !value.is_empty())
                .map(|value| normalize_source_group_name(dir_model, value))
                .or_else(|| set.props.get(b"\0source_group_file_set").cloned());
            let error =
                |error: rcmake_resolve::ResolveError| GenError::Invalid(error.to_string().into());
            let bases = usage
                .file_set_paths(set.target, b"BASE_DIRS", &set.base_dirs, &context)
                .map_err(error)?;
            let bases: Vec<_> = if bases.is_empty() {
                vec![relative(dir, source)]
            } else {
                let mut seen = BTreeSet::new();
                bases
                    .into_iter()
                    .map(|path| relative(&Lexical::new(dir).append(&path.value), source))
                    .filter(|path| seen.insert(path.clone()))
                    .collect()
            };
            let visibility = match set.scope {
                UsageScope::Private => "PRIVATE",
                UsageScope::Public => "PUBLIC",
                UsageScope::Interface => "INTERFACE",
            };
            let index = result.entries.len();
            result.entries.push(object([
                ("name", Value::string(set.name.clone())),
                ("type", Value::string(set.kind.as_str())),
                ("visibility", Value::string(visibility)),
                ("baseDirectories", strings(&bases)),
            ]));
            let files = usage
                .file_set_paths(set.target, b"SOURCES", &set.files, &context)
                .map_err(error)?;
            for file in files {
                let path = Lexical::new(&Lexical::new(dir).append(&file.value)).normalize();
                let member = result.sources.entry(path.clone()).or_insert_with(|| {
                    result.order.push(path);
                    Membership {
                        indexes: Vec::new(),
                        traces: Vec::new(),
                        header: false,
                        interface: false,
                        in_build: false,
                        source_group: None,
                    }
                });
                if member.indexes.last() != Some(&index) {
                    member.indexes.push(index);
                }
                member.traces.push(file.backtrace);
                member.header |= set.kind == FileSetKind::Headers;
                member.interface = set.scope != UsageScope::Private;
                member.in_build |= set.scope != UsageScope::Interface && !raw.is_imported();
                if member.source_group.is_none()
                    && let Some(group) = &set_source_group
                {
                    member.source_group = Some(group.clone());
                }
            }
        }
        Ok(result)
    }
}

pub(super) fn interface_sources(
    model: &ProjectModel,
    target: TargetId,
    raw: &Target,
    config: &rcmake_genex::ConfigId,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<BString>, GenError> {
    let context = EvalContext {
        config: config.clone(),
        head: u32::try_from(target.index()).ok().map(TargetHandle),
        ..Default::default()
    };
    let entries = raw.usage.entries(UsageField::InterfaceSources);
    let files = usage
        .file_set_paths(target, b"INTERFACE_SOURCES", entries, &context)
        .map_err(|error| GenError::Invalid(error.to_string().into()))?;
    let dir = &model.dir(raw.dir).source_dir;
    Ok(files
        .into_iter()
        .map(|file| Lexical::new(&Lexical::new(dir).append(&file.value)).normalize())
        .collect())
}
