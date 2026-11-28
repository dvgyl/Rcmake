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
    /// File-set sources ordered by file-set name (ascending), matching CMake's
    /// `std::map<std::string, cmFileSet>` iteration order in `cmGeneratorTarget`.
    pub(super) named_order: Vec<BString>,
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
        let mut per_set_files: Vec<(BString, usize, Vec<BString>)> = Vec::new();
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
            let mut set_paths = Vec::with_capacity(files.len());
            for file in files {
                let path = Lexical::new(&Lexical::new(dir).append(&file.value)).normalize();
                set_paths.push(path.clone());
                let member = result.sources.entry(path).or_insert_with(|| Membership {
                    indexes: Vec::new(),
                    traces: Vec::new(),
                    header: false,
                    interface: false,
                    in_build: false,
                    source_group: None,
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
            per_set_files.push((set.name.clone(), index, set_paths));
        }
        per_set_files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut named_seen = BTreeSet::new();
        for (_, _, paths) in per_set_files {
            for path in paths {
                if named_seen.insert(path.clone()) {
                    result.named_order.push(path);
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

#[cfg(test)]
mod tests {
    use rcmake_diag::{Backtrace, PolicyMap};
    use rcmake_model::{
        BT, DirId, FileSet, FileSetKind, ProjectModel, Target, TargetKind, TargetOrigin, UsageScope,
    };
    use rcmake_resolve::{ArtifactSet, ResolvedTarget, TargetMetadata, usage::UsageResolver};

    use super::*;

    #[test]
    fn named_order_sorts_file_sets_by_name_while_preserving_declaration_entries()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/build".into());
        let target_id = model.add_target(Target::new(
            "lib".into(),
            TargetKind::StaticLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        ))?;
        for (name, file) in [("z_last", "z.h"), ("a_first", "a.h")] {
            model.add_file_set(FileSet {
                target: target_id,
                name: name.into(),
                kind: FileSetKind::Headers,
                scope: UsageScope::Public,
                base_dirs: Vec::new(),
                files: vec![BT::new(file.into(), Backtrace::default())],
                props: Default::default(),
            })?;
        }
        let resolved = ResolvedTarget {
            id: target_id,
            config: rcmake_genex::ConfigId::default(),
            metadata: TargetMetadata {
                name: "lib".into(),
                kind: TargetKind::StaticLibrary,
                source_dir: "/src".into(),
                binary_dir: "/build".into(),
                backtrace: Backtrace::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: ArtifactSet::default(),
            compile_groups: Vec::new(),
            compile_units: Vec::new(),
            link: None,
            dependencies: Vec::new(),
            cxx_modules: None,
            fortran: None,
            ide: None,
        };
        let mut usage = UsageResolver::new(&model);
        let sets = FileSets::collect(
            &model,
            &resolved,
            model.target(target_id),
            b"/src",
            &mut usage,
        )?;
        let entry_names: Vec<BString> = sets
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Value::Object(map) => match map.get(b"name".as_slice()) {
                    Some(Value::String(name)) => Some(name.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            entry_names,
            vec![BString::from("z_last"), BString::from("a_first")]
        );
        assert_eq!(
            sets.named_order,
            vec![BString::from("/src/a.h"), BString::from("/src/z.h")]
        );
        Ok(())
    }
}
