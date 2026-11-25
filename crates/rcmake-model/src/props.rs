//! Typed property scopes for `set_property()` / `get_property()` (ADR-007).

use rcmake_base::{BString, list, truth};
use rcmake_diag::{
    Backtrace,
    annotate::Confidence,
    suggest::{Namespace, best_match_in},
};

use crate::target::join_entries;
use crate::{
    CacheEntry, DirId, FileSetId, ImportedVisibility, LinkItem, ProjectModel, Target, TargetId,
    TargetOrigin, UsageField,
};

/// The object that owns a property.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PropScope {
    /// `GLOBAL`.
    Global,
    /// `DIRECTORY`.
    Directory(DirId),
    /// `TARGET`.
    Target(TargetId),
    /// `SOURCE`: properties of `path` as seen from directory `dir`.
    Source {
        /// Directory that owns the source file properties.
        dir: DirId,
        /// Absolute source path, CMake style.
        path: BString,
    },
    /// `TEST`: test `name` in directory `dir`.
    Test {
        /// Directory of the test.
        dir: DirId,
        /// Test name.
        name: BString,
    },
    /// `CACHE`: properties of a cache entry.
    Cache(BString),
    /// `FILE_SET`: properties of one file set of a target.
    FileSet(FileSetId),
    /// `INSTALL`: properties of one installed file.
    Install {
        /// Name of the installed file.
        file: BString,
    },
}

/// `set_property(APPEND)` or `set_property(APPEND_STRING)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendMode {
    /// `APPEND`: add a list item.
    List,
    /// `APPEND_STRING`: add raw text.
    String,
}

/// Call site of a property write. Usage requirements record the backtrace, and link
/// items record the directory for target name lookup (`CMP0079`).
#[derive(Clone, Copy, Debug)]
pub struct PropOrigin<'a> {
    /// Directory of the call.
    pub dir: DirId,
    /// Backtrace of the call.
    pub backtrace: &'a Backtrace,
}

impl ProjectModel {
    /// Find a close match among the known property names for a target.
    pub fn suggest_target_property(
        &self,
        id: TargetId,
        name: &[u8],
    ) -> Option<(BString, Confidence)> {
        if self.global.defined_property(b"TARGET", name).is_some() {
            return None;
        }
        let target = self.target(id)?;
        let names = target.props.names();
        let candidates = names
            .iter()
            .map(|value| value.as_slice())
            .chain(
                target
                    .usage
                    .fields
                    .keys()
                    .map(|field| field.name().as_bytes()),
            )
            .chain(
                self.global
                    .defined_props
                    .iter()
                    .filter(|property| property.scope == b"TARGET")
                    .map(|property| property.name.as_slice()),
            );
        best_match_in(Namespace::Name, name, candidates)
    }

    /// Value of property `name` on `scope`, without inheritance. `None` when not set.
    #[must_use]
    pub fn get_prop(&self, scope: &PropScope, name: &[u8]) -> Option<BString> {
        match scope {
            PropScope::Global => self.global.props.get(name).cloned(),
            PropScope::Directory(id) => self.directory_prop(*id, name),
            PropScope::Target(id) => self.target_prop(self.target(*id)?, name),
            PropScope::Source { dir, path } => {
                let directory = self.dir(*dir);
                if name == b"GENERATED" && directory.generated_sources.contains(path) {
                    return Some("1".into());
                }
                let value = directory.source_props.get(path)?.get(name)?;
                if name == b"GENERATED" {
                    Some(if truth::is_on(value) { "1" } else { "0" }.into())
                } else {
                    Some(value.clone())
                }
            }
            PropScope::Test { dir, name: test } => {
                let id = self.find_test(*dir, test)?;
                self.test(id)?.props.get(name).cloned()
            }
            PropScope::Cache(entry) => cache_prop(self.cache.get(entry)?, name),
            PropScope::FileSet(id) => self.file_set(*id)?.props.get(name).cloned(),
            PropScope::Install { file } => self.install_props.get(file)?.get(name).cloned(),
        }
    }

    /// Value of property `name` with `INHERITED` lookup: object, then its directory,
    /// then parent directories, then global. A set value (also an empty one) stops the
    /// walk. The caller checks that the property is defined `INHERITED`.
    #[must_use]
    pub fn get_prop_inherited(&self, scope: &PropScope, name: &[u8]) -> Option<BString> {
        let mut scope = scope.clone();
        loop {
            if let Some(value) = self.get_prop(&scope, name) {
                return Some(value);
            }
            scope = match scope {
                PropScope::Target(id) => PropScope::Directory(self.target(id)?.dir),
                PropScope::FileSet(id) => PropScope::Target(self.file_set(id)?.target),
                PropScope::Source { dir, .. } | PropScope::Test { dir, .. } => {
                    PropScope::Directory(dir)
                }
                PropScope::Directory(id) => {
                    return self
                        .ancestors(id)
                        .skip(1)
                        .find_map(|parent| self.directory_prop(parent, name))
                        .or_else(|| self.global.props.get(name).cloned());
                }
                PropScope::Global | PropScope::Cache(_) | PropScope::Install { .. } => return None,
            };
        }
    }

    fn directory_prop(&self, id: DirId, name: &[u8]) -> Option<BString> {
        let dir = self.dir(id);
        let names = |ids: &mut dyn Iterator<Item = TargetId>| {
            list::join(
                ids.filter_map(|t| self.target(t))
                    .map(|t| t.name.as_slice()),
            )
        };
        Some(match name {
            b"SOURCE_DIR" => dir.source_dir.clone(),
            b"BINARY_DIR" => dir.binary_dir.clone(),
            b"PARENT_DIRECTORY" => dir
                .parent
                .map(|p| self.dir(p).source_dir.clone())
                .unwrap_or_default(),
            b"SUBDIRECTORIES" => list::join(
                dir.children
                    .iter()
                    .map(|c| self.dir(*c).source_dir.as_slice()),
            ),
            b"BUILDSYSTEM_TARGETS" => names(
                &mut dir
                    .targets
                    .iter()
                    .copied()
                    .filter(|t| self.target(*t).is_some_and(|t| !t.is_imported())),
            ),
            b"IMPORTED_TARGETS" => names(
                &mut dir
                    .targets
                    .iter()
                    .copied()
                    .filter(|t| self.target(*t).is_some_and(Target::is_imported)),
            ),
            b"TESTS" => list::join(dir.tests.keys()),
            _ => {
                return match UsageField::from_name(name).filter(|f| f.on_directory()) {
                    Some(field) => dir.usage.joined(field),
                    None => dir.props.get(name).cloned(),
                };
            }
        })
    }

    fn target_prop(&self, target: &Target, name: &[u8]) -> Option<BString> {
        let flag = |on: bool| BString::from(if on { "TRUE" } else { "FALSE" });
        match name {
            b"NAME" => Some(target.name.clone()),
            b"TYPE" => Some(target.kind.as_str().into()),
            b"IMPORTED" => Some(flag(target.is_imported())),
            b"IMPORTED_GLOBAL" => Some(flag(matches!(
                target.origin,
                TargetOrigin::Imported(ImportedVisibility::Global)
            ))),
            b"SOURCE_DIR" => Some(self.dir(target.dir).source_dir.clone()),
            b"BINARY_DIR" => Some(self.dir(target.dir).binary_dir.clone()),
            b"SOURCES" => (!target.sources.is_empty())
                .then(|| join_entries(target.sources.iter().map(|s| s.value.as_slice()))),
            b"LINK_LIBRARIES" => link_value(target.usage.link_libraries.as_deref()),
            b"INTERFACE_LINK_LIBRARIES" => {
                link_value(target.usage.interface_link_libraries.as_deref())
            }
            _ => match UsageField::from_name(name) {
                Some(field) => target.usage.joined(field),
                None => target.props.get(name).cloned(),
            },
        }
    }
}

fn link_value(items: Option<&[LinkItem]>) -> Option<BString> {
    items.map(|items| {
        let mut values = Vec::new();
        for item in items {
            if item.opens_lookup_scope {
                values.push(BString::from(format!(
                    "::@(0x{:x})",
                    item.lookup_dir.index()
                )));
            }
            if !item.text.value.is_empty()
                || (!item.opens_lookup_scope && !item.closes_lookup_scope)
            {
                values.push(item.text.value.clone());
            }
            if item.closes_lookup_scope {
                values.push(BString::from("::@"));
            }
        }
        list::join(values)
    })
}

fn cache_prop(entry: &CacheEntry, name: &[u8]) -> Option<BString> {
    match name {
        b"VALUE" => Some(entry.value.clone()),
        b"TYPE" => Some(entry.ty.as_str().into()),
        b"HELPSTRING" => Some(entry.help.clone()),
        b"ADVANCED" => entry
            .props
            .get(name)
            .cloned()
            .or_else(|| entry.advanced.then(|| "1".into())),
        _ => entry.props.get(name).cloned(),
    }
}
