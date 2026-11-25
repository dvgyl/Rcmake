//! Portable state for literal assignments and targets in one listfile.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::{BString, list};
use rcmake_diag::{DiagAction, DiagCategory, PolicyMap};
use rcmake_model::{
    BT, DirId, PropertyMap, Target, TargetKind, TargetOrigin, UsageField, UsageProps,
};
use rcmake_syntax::ArgKind;
use serde::{Deserialize, Serialize};

use super::{JournalEffect, PropertyOrigin, VariableScope};
use crate::{Interp, Registry, code::Code};

const COMMANDS: &[&[u8]] = &[
    b"set",
    b"unset",
    b"add_library",
    b"add_executable",
    b"add_compile_definitions",
    b"target_compile_definitions",
    b"target_compile_options",
    b"target_sources",
    b"set_property",
];

impl Registry {
    /// Mark the standard implementations that have a configure replay contract.
    /// Call this after installing the standard command families. Registering a
    /// replacement clears the contract for that command.
    pub fn mark_replay_commands(&mut self) {
        for &name in COMMANDS {
            if let Some(builtin) = self.builtins.get_mut(name) {
                builtin.replay = true;
            }
        }
    }
}

/// The names that the listfile can write. Target commands can modify only
/// targets that this listfile creates.
pub(super) struct Writes {
    pub(super) vars: BTreeMap<BString, Option<BString>>,
    pub(super) targets: Vec<(BString, TargetKind)>,
    pub(super) directory_usage: bool,
}

impl Writes {
    pub(super) fn classify(interp: &Interp, code: &Code) -> Option<Self> {
        let dir = interp.current_dir_id();
        if code.cmds.is_empty()
            || code.len() > 4096
            || !interp.watches.is_empty()
            || interp
                .project
                .dir(dir)
                .props
                .get(b"\0source_group_has_regex")
                .is_some()
        {
            return None;
        }
        let mut writes = Self {
            vars: BTreeMap::new(),
            targets: Vec::new(),
            directory_usage: false,
        };
        let mut names = BTreeSet::new();
        for cmd in code.cmds.iter() {
            let name = cmd.lower.as_slice();
            if interp
                .expanded_observer
                .as_ref()
                .is_some_and(|observer| observer.names.contains(&name))
                || interp.scripted.contains_key(name)
                || !interp
                    .registry
                    .get_lower(name)
                    .is_some_and(|entry| entry.replay && !entry.raw)
                || !cmd.args.iter().all(|arg| {
                    !arg.text.is_empty()
                        && arg.text.len() <= 8192
                        && (arg.kind == ArgKind::Bracket
                            || !arg.text.iter().any(|b| {
                                matches!(b, b'$' | b'\\')
                                    || (*b == b';' && arg.kind == ArgKind::Unquoted)
                            }))
                })
            {
                return None;
            }
            let args: Vec<_> = cmd.args.iter().map(|arg| arg.text.as_slice()).collect();
            let first = *args.first()?;
            match (name, args.as_slice()) {
                (b"set", [var, rest @ ..]) | (b"unset", [var, rest @ ..])
                    if plain_name(var)
                        && !matches!(
                            *var,
                            b"CMAKE_WARN_DEPRECATED"
                                | b"CMAKE_ERROR_DEPRECATED"
                                | b"CMAKE_MAXIMUM_RECURSION_DEPTH"
                        )
                        && (name == b"set" || rest.is_empty())
                        && !rest
                            .iter()
                            .any(|arg| matches!(*arg, b"CACHE" | b"FORCE" | b"PARENT_SCOPE")) =>
                {
                    let value = (!rest.is_empty()).then(|| list::join(rest.iter().copied()));
                    writes.vars.insert(first.into(), value);
                }
                (b"add_library", [target, b"STATIC", sources @ ..])
                | (b"add_executable", [target, sources @ ..])
                    if plain_target(target)
                        && sources.iter().all(|item| {
                            !matches!(
                                *item,
                                b"ALIAS"
                                    | b"IMPORTED"
                                    | b"EXCLUDE_FROM_ALL"
                                    | b"GLOBAL"
                                    | b"WIN32"
                                    | b"MACOSX_BUNDLE"
                                    | b"STATIC"
                                    | b"SHARED"
                                    | b"MODULE"
                                    | b"INTERFACE"
                                    | b"OBJECT"
                                    | b"UNKNOWN"
                                    | b"SYMBOLIC"
                            )
                        }) =>
                {
                    if interp.project.find_target(dir, first).is_some()
                        || !names.insert(first)
                        || interp
                            .project
                            .cache
                            .contains(&[first, b"_LIB_DEPENDS"].concat())
                    {
                        return None;
                    }
                    let kind = if name == b"add_library" {
                        TargetKind::StaticLibrary
                    } else {
                        TargetKind::Executable
                    };
                    writes.targets.push((first.into(), kind));
                }
                (
                    b"target_compile_definitions" | b"target_compile_options" | b"target_sources",
                    [target, b"PRIVATE", rest @ ..],
                ) if names.contains(target)
                    && rest.iter().all(|item| {
                        !matches!(*item, b"FILE_SET" | b"PUBLIC" | b"INTERFACE" | b"PRIVATE")
                    }) => {}
                (
                    b"set_property",
                    [
                        b"TARGET",
                        target,
                        b"APPEND",
                        b"PROPERTY",
                        b"COMPILE_OPTIONS",
                        ..,
                    ],
                ) if names.contains(target) => {}
                (b"add_compile_definitions", _)
                    if interp.diags.top().get(DiagCategory::NonTargetDirective)
                        == DiagAction::Ignore =>
                {
                    writes.directory_usage = true;
                }
                _ => return None,
            }
        }
        Some(writes)
    }

    pub(super) fn check_effects(&self, effects: &[JournalEffect], limit: usize) -> bool {
        effects.len() == self.vars.len() && effects.iter().zip(&self.vars).all(|(effect, (expected, expected_value))| {
            matches!(effect, JournalEffect::SetVar {scope: VariableScope::Current, name, value}
                if name.as_slice() == expected.as_slice()
                            && value.as_deref() == expected_value.as_ref().map(|v| v.as_slice())
                            && value.as_ref().is_none_or(|v| v.len() <= limit))
        })
    }
}

fn plain_name(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn plain_target(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"_+.-".contains(b))
        && !matches!(
            name,
            b"package"
                | b"PACKAGE"
                | b"package_source"
                | b"PACKAGE_SOURCE"
                | b"test"
                | b"RUN_TESTS"
        )
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct LocalState {
    targets: Vec<TargetRecord>,
    directory_usage: Option<Usage>,
    used_cli: Vec<Vec<u8>>,
}

impl LocalState {
    pub(super) fn capture(interp: &Interp, writes: &Writes, before: &[BString]) -> Option<Self> {
        let dir = interp.current_dir_id();
        let path = interp.current_binary_dir();
        let mut targets = Vec::new();
        for (name, kind) in &writes.targets {
            let target = interp
                .project
                .target(interp.project.find_target(dir, name)?)?;
            if target.dir != dir || target.kind != *kind {
                return None;
            }
            targets.push(TargetRecord::capture(target, &path)?);
        }
        let directory_usage = if writes.directory_usage {
            Some(Usage::capture(&interp.project.dir(dir).usage, &path)?)
        } else {
            None
        };
        let unread: BTreeSet<_> = interp.unread_variables().collect();
        let used_cli = before
            .iter()
            .filter(|name| !unread.contains(name.as_slice()))
            .map(|v| v.to_vec())
            .collect();
        Some(Self {
            targets,
            directory_usage,
            used_cli,
        })
    }

    /// Prepare every fallible conversion before changing the live model.
    pub(super) fn restore(&self, interp: &mut Interp, writes: &Writes) -> bool {
        let dir = interp.current_dir_id();
        let bin = interp.current_binary_dir();
        if self.targets.len() != writes.targets.len()
            || writes.directory_usage != self.directory_usage.is_some()
            || self
                .used_cli
                .iter()
                .any(|name| !interp.variable_access.contains_key(name.as_slice()))
        {
            return false;
        }
        let mut targets = Vec::with_capacity(self.targets.len());
        for (record, (name, kind)) in self.targets.iter().zip(&writes.targets) {
            let Some(target) = record.restore(name, *kind, dir, &bin, *interp.policies.top())
            else {
                return false;
            };
            targets.push(target);
        }
        let usage = match &self.directory_usage {
            Some(usage) => match usage.restore(&bin) {
                Some(value) => Some(value),
                None => return false,
            },
            None => None,
        };
        if !interp.project.restore_directory_targets(dir, targets) {
            return false;
        }
        if let Some(usage) = usage {
            interp.project.dir_mut(dir).usage = usage;
        }
        for name in &self.used_cli {
            interp.record_variable_access(name);
        }
        true
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct TargetRecord {
    name: Vec<u8>,
    origin: PropertyOrigin,
    props: Vec<(Vec<u8>, Vec<u8>)>,
    usage: Usage,
    sources: Vec<Value>,
}

impl TargetRecord {
    fn capture(target: &Target, path: &Path) -> Option<Self> {
        if target.is_imported()
            || !target.file_sets.is_empty()
            || !target.utility_deps.is_empty()
            || !target.event_commands.is_empty()
        {
            return None;
        }
        let props = target
            .props
            .names()
            .into_iter()
            .filter_map(|name| Some((name.to_vec(), target.props.get(&name)?.to_vec())))
            .collect();
        Some(Self {
            name: target.name.to_vec(),
            origin: PropertyOrigin::new(path.into(), &target.backtrace),
            props,
            usage: Usage::capture(&target.usage, path)?,
            sources: capture_values(&target.sources, path),
        })
    }

    fn restore(
        &self,
        name: &BString,
        kind: TargetKind,
        dir: DirId,
        bin: &Path,
        policies: PolicyMap,
    ) -> Option<Target> {
        if name.as_slice() != self.name || !valid_origin(&self.origin, bin) {
            return None;
        }
        let mut target = Target::new(
            name.clone(),
            kind,
            TargetOrigin::Normal,
            dir,
            self.origin.backtrace(),
            policies,
        );
        let mut props = PropertyMap::default();
        for (key, value) in &self.props {
            if props.get(key).is_some() {
                return None;
            }
            props.set(key.as_slice().into(), value.as_slice().into());
        }
        target.props = props;
        target.usage = self.usage.restore(bin)?;
        target.sources = restore_values(&self.sources, bin)?;
        Some(target)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Usage(Vec<(String, Vec<Value>)>);

impl Usage {
    pub(super) fn capture(usage: &UsageProps, path: &Path) -> Option<Self> {
        if usage.link_libraries.is_some() || usage.interface_link_libraries.is_some() {
            return None;
        }
        Some(Self(
            usage
                .fields
                .iter()
                .map(|(field, values)| (field.name().into(), capture_values(values, path)))
                .collect(),
        ))
    }

    fn restore(&self, path: &Path) -> Option<UsageProps> {
        let mut usage = UsageProps::default();
        for (name, entries) in &self.0 {
            let field = UsageField::from_name(name.as_bytes())?;
            if usage
                .fields
                .insert(field, restore_values(entries, path)?)
                .is_some()
            {
                return None;
            }
        }
        Some(usage)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Value(Vec<u8>, PropertyOrigin);

fn capture_values(values: &[BT<BString>], path: &Path) -> Vec<Value> {
    values
        .iter()
        .map(|value| {
            Value(
                value.value.to_vec(),
                PropertyOrigin::new(path.into(), &value.backtrace),
            )
        })
        .collect()
}

fn valid_origin(origin: &PropertyOrigin, bin: &Path) -> bool {
    origin.binary_dir == bin && origin.frame_count() <= 256
}

fn restore_values(values: &[Value], bin: &Path) -> Option<Vec<BT<BString>>> {
    values
        .iter()
        .map(|value| {
            valid_origin(&value.1, bin)
                .then(|| BT::new(value.0.as_slice().into(), value.1.backtrace()))
        })
        .collect()
}
