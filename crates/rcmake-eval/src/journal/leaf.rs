//! Portable state for literal assignments and targets in one listfile.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::{BString, list};
use rcmake_diag::{PolicyId, PolicyMap, PolicyStatus};
use rcmake_model::{
    BT, DirId, LinkItem, PropertyMap, Target, TargetBinding, TargetKind, TargetOrigin, TestEntry,
    UsageField, UsageProps, UsageScope,
};
use rcmake_syntax::{ArgKind, Argument};
use serde::{Deserialize, Serialize};

use super::{JournalEffect, PropertyOrigin, VariableScope};
use crate::{Interp, Registry, code::Code};

mod install;
pub(crate) mod shape;

pub(super) use install::earlier_install_property;

pub(crate) const REPLAY_LINK_CHECK: &[u8] = b"\0rcmake-check-links";

const COMMANDS: &[&[u8]] = &[
    b"set",
    b"unset",
    b"add_library",
    b"add_executable",
    b"add_compile_definitions",
    b"target_compile_definitions",
    b"target_compile_options",
    b"target_sources",
    b"target_include_directories",
    b"target_compile_features",
    b"target_link_options",
    b"target_link_libraries",
    b"set_target_properties",
    b"set_property",
    b"option",
    b"add_test",
    b"install",
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
    pub(super) cache: BTreeSet<BString>,
    pub(super) targets: Vec<(BString, TargetKind)>,
    aliases: Vec<(BString, BString)>,
    tests: Vec<BString>,
    pub(super) installs: bool,
    pub(super) links: bool,
    pub(super) directory_usage: bool,
    pub(super) record_variable_reads: bool,
}

impl Writes {
    pub(super) fn bootstrap_variable_reads(code: &Code) -> bool {
        code.cmds
            .iter()
            .all(|command| match command.lower.as_slice() {
                b"set" => true,
                b"if" | b"elseif" => matches!(
                    command.args.as_slice(),
                    [name] if plain_name(&name.text)
                ),
                b"else" | b"endif" => command.args.is_empty(),
                _ => false,
            })
    }

    pub(super) fn classify(interp: &Interp, code: &Code) -> Option<Self> {
        let dir = interp.current_dir_id();
        if code.cmds.is_empty()
            || code.len() > 4096
            || !interp.hooks.watches.is_empty()
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
            cache: BTreeSet::new(),
            targets: Vec::new(),
            aliases: Vec::new(),
            tests: Vec::new(),
            installs: false,
            links: false,
            directory_usage: false,
            record_variable_reads: true,
        };
        let mut names = BTreeSet::new();
        for cmd in code.cmds.iter() {
            let name = cmd.lower.as_slice();
            writes.record_variable_reads &= matches!(
                name,
                b"add_library"
                    | b"add_executable"
                    | b"add_compile_definitions"
                    | b"target_compile_definitions"
                    | b"target_compile_options"
                    | b"target_sources"
                    | b"target_include_directories"
                    | b"target_compile_features"
                    | b"target_link_options"
                    | b"target_link_libraries"
                    | b"set_target_properties"
                    | b"set_property"
                    | b"install"
            ) && (name != b"target_link_libraries"
                || cmd.args.iter().all(|arg| !arg.text.contains(&b'$')));
            let flow = matches!(
                name,
                b"if" | b"elseif" | b"else" | b"endif" | b"foreach" | b"endforeach"
            );
            if interp
                .expanded_observer
                .as_ref()
                .is_some_and(|observer| observer.names.contains(&name))
                || interp.scripted.contains_key(name)
                || (!flow
                    && !interp
                        .registry
                        .get_lower(name)
                        .is_some_and(|entry| entry.replay))
                || cmd.args.iter().any(|arg| {
                    arg.text.len() > 8192
                        || (arg.kind != ArgKind::Bracket && arg.text.contains(&b'\\'))
                })
            {
                return None;
            }
            let args: Vec<_> = cmd.args.iter().map(|arg| arg.text.as_slice()).collect();
            match (name, args.as_slice()) {
                (b"if" | b"elseif", [_, ..]) | (b"else" | b"endif" | b"endforeach", []) => {}
                (b"foreach", [var, b"IN", b"LISTS" | b"ITEMS", rest @ ..])
                    if plain_name(var) && !rest.iter().any(|value| *value == b"ZIP_LISTS") =>
                {
                    writes.vars.entry((*var).into()).or_insert(None);
                }
                (b"option", [name, _, ..]) if plain_name(name) => {
                    writes.cache.insert((*name).into());
                    writes.vars.entry((*name).into()).or_insert(None);
                }
                (b"add_test", [b"NAME", test, _, ..] | [test, _, ..])
                    if plain_target(test)
                        && !writes.tests.iter().any(|name| name.as_slice() == *test)
                        && interp.project.find_test(dir, test).is_none() =>
                {
                    writes.tests.push((*test).into());
                }
                (b"install", args)
                    if shape::install_targets(args).is_some_and(|targets| {
                        targets.iter().all(|target| names.contains(target))
                    }) =>
                {
                    writes.installs = true;
                }
                (b"add_library", [alias, b"ALIAS", target])
                    if plain_alias(alias)
                        && names.contains(target)
                        && !names.contains(alias)
                        && !writes
                            .aliases
                            .iter()
                            .any(|(name, _)| name.as_slice() == *alias)
                        && interp.project.find_target(dir, alias).is_none() =>
                {
                    writes.aliases.push(((*alias).into(), (*target).into()));
                }
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
                    writes.vars.insert((*var).into(), value);
                }
                (
                    b"add_library",
                    [
                        target,
                        b"STATIC" | b"SHARED" | b"OBJECT" | b"INTERFACE",
                        sources @ ..,
                    ],
                )
                | (b"add_executable", [target, sources @ ..])
                    if plain_target(target) && shape::ordinary_sources(sources) =>
                {
                    if interp.project.find_target(dir, target).is_some()
                        || !names.insert(*target)
                        || interp
                            .project
                            .cache
                            .contains(&[*target, b"_LIB_DEPENDS"].concat())
                    {
                        return None;
                    }
                    let kind = match (name, args.get(1).copied()) {
                        (b"add_library", Some(b"STATIC")) => TargetKind::StaticLibrary,
                        (b"add_library", Some(b"SHARED")) => TargetKind::SharedLibrary,
                        (b"add_library", Some(b"OBJECT")) => TargetKind::ObjectLibrary,
                        (b"add_library", Some(b"INTERFACE")) => TargetKind::InterfaceLibrary,
                        _ => TargetKind::Executable,
                    };
                    writes.targets.push(((*target).into(), kind));
                }
                (
                    b"target_compile_definitions"
                    | b"target_compile_options"
                    | b"target_sources"
                    | b"target_include_directories"
                    | b"target_compile_features"
                    | b"target_link_options",
                    [target, b"PRIVATE" | b"PUBLIC" | b"INTERFACE", rest @ ..],
                ) if names.contains(target) && !rest.contains(&b"FILE_SET".as_slice()) => {}
                (b"target_link_libraries", [target, b"PRIVATE" | b"PUBLIC" | b"INTERFACE", ..])
                    if names.contains(target)
                        && interp.policy_status(PolicyId::new(73)) == PolicyStatus::New =>
                {
                    writes.links = true;
                }
                (b"set_target_properties", args) if ordinary_properties(args, &names) => {}
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
                (b"add_compile_definitions", _) => {
                    writes.directory_usage = true;
                }
                _ => return None,
            }
        }
        Some(writes)
    }

    pub(super) fn check_effects(&self, effects: &[JournalEffect], limit: usize) -> bool {
        let mut vars = BTreeSet::new();
        let mut cache = BTreeSet::new();
        effects.iter().all(|effect| match effect {
            JournalEffect::SetVar {
                scope: VariableScope::Current,
                name,
                value,
            } => {
                self.vars.contains_key(name.as_slice())
                    && vars.insert(name)
                    && value.as_ref().is_none_or(|v| v.len() <= limit)
            }
            JournalEffect::SetCache { name, entry } => {
                self.cache.contains(name.as_slice())
                    && cache.insert(name)
                    && entry.as_ref().is_none_or(|value| {
                        value
                            .to_entry()
                            .is_some_and(|entry| entry.value.len() <= limit)
                    })
            }
            _ => false,
        })
    }
}

fn ordinary_properties(args: &[&[u8]], targets: &BTreeSet<&[u8]>) -> bool {
    let Some(marker) = args.iter().position(|arg| *arg == b"PROPERTIES") else {
        return false;
    };
    let (names, tail) = args.split_at(marker);
    let Some((_, items)) = tail.split_first() else {
        return false;
    };
    let (pairs, trailing) = items.as_chunks::<2>();
    !names.is_empty()
        && names.iter().all(|name| targets.contains(name))
        && trailing.is_empty()
        && pairs.iter().all(|[property, _]| {
            matches!(
                *property,
                b"OUTPUT_NAME"
                    | b"PREFIX"
                    | b"SUFFIX"
                    | b"VERSION"
                    | b"SOVERSION"
                    | b"FOLDER"
                    | b"EXPORT_NAME"
                    | b"C_STANDARD"
                    | b"CXX_STANDARD"
                    | b"POSITION_INDEPENDENT_CODE"
                    | b"DEBUG_POSTFIX"
                    | b"PUBLIC_HEADER"
                    | b"PRIVATE_HEADER"
                    | b"RESOURCE"
            )
        })
}

fn plain_name(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn plain_alias(name: &[u8]) -> bool {
    name.split(|&byte| byte == b':')
        .any(|part| !part.is_empty())
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_+.-:".contains(byte))
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) variable_inputs: Option<(usize, super::JournalDigest)>,
    targets: Vec<TargetRecord>,
    aliases: Vec<(Vec<u8>, Vec<u8>)>,
    tests: Vec<TestRecord>,
    installs: Option<install::Installs>,
    link_check: Option<PropertyOrigin>,
    directory_usage: Option<Usage>,
    used_cli: Vec<Vec<u8>>,
}

impl LocalState {
    pub(super) fn capture(
        interp: &Interp,
        writes: &Writes,
        used_cli: BTreeSet<BString>,
        install_start: Option<(usize, usize)>,
        link_check: Option<PropertyOrigin>,
    ) -> Option<Self> {
        let dir = interp.current_dir_id();
        let path = interp.current_binary_dir();
        let mut targets = BTreeMap::new();
        for (name, kind) in &writes.targets {
            let id = interp.project.find_target(dir, name)?;
            let target = interp.project.target(id)?;
            if target.dir != dir || target.kind != *kind || &target.name != name {
                return None;
            }
            if targets
                .insert(id, TargetRecord::capture(target, &path)?)
                .is_some()
            {
                return None;
            }
        }
        let mut aliases = Vec::new();
        for (alias, target) in &writes.aliases {
            let id = interp.project.find_target(dir, target)?;
            if interp.project.lookup_target(dir, alias) != Some(TargetBinding::Alias(id)) {
                return None;
            }
            aliases.push((alias.to_vec(), target.to_vec()));
        }
        let mut tests = BTreeMap::new();
        for name in &writes.tests {
            let id = interp.project.find_test(dir, name)?;
            let test = interp.project.test(id)?;
            if test.dir != dir || &test.name != name {
                return None;
            }
            if tests
                .insert(id, TestRecord::capture(test, &path, interp.policies.top())?)
                .is_some()
            {
                return None;
            }
        }
        if install_start.is_some()
            && writes.record_variable_reads
            && !targets
                .values()
                .map(|target| target.name.as_slice())
                .eq(writes.targets.iter().map(|(name, _)| name.as_slice()))
        {
            return None;
        }
        let installs = match install_start {
            Some((start, target_start)) => Some(install::Installs::capture(
                &interp.project,
                &path,
                dir,
                start,
                target_start,
            )?),
            None => None,
        };
        let directory_usage = if writes.directory_usage {
            Some(Usage::capture(&interp.project.dir(dir).usage, &path, dir)?)
        } else {
            None
        };
        Some(Self {
            variable_inputs: None,
            targets: targets.into_values().collect(),
            aliases,
            tests: tests.into_values().collect(),
            installs,
            link_check,
            directory_usage,
            used_cli: used_cli.iter().map(|name| name.to_vec()).collect(),
        })
    }

    /// Prepare every fallible conversion before changing the live model.
    pub(super) fn restore(&self, interp: &mut Interp, writes: &Writes) -> bool {
        let dir = interp.current_dir_id();
        let bin = interp.current_binary_dir();
        if self.targets.len() != writes.targets.len()
            || self.installs.is_some() != writes.installs
            || !self
                .aliases
                .iter()
                .map(|(alias, target)| (alias.as_slice(), target.as_slice()))
                .eq(writes
                    .aliases
                    .iter()
                    .map(|(alias, target)| (alias.as_slice(), target.as_slice())))
            || self.tests.len() != writes.tests.len()
            || writes.directory_usage != self.directory_usage.is_some()
            || (!writes.record_variable_reads
                && self
                    .used_cli
                    .iter()
                    .any(|name| !interp.variable_access.contains_key(name.as_slice())))
        {
            return false;
        }
        let mut target_kinds: BTreeMap<_, _> = writes
            .targets
            .iter()
            .map(|(name, kind)| (name.as_slice(), (name, *kind)))
            .collect();
        let mut targets = Vec::with_capacity(self.targets.len());
        for record in &self.targets {
            let Some((name, kind)) = target_kinds.remove(record.name.as_slice()) else {
                return false;
            };
            let Some(target) = record.restore(name, kind, dir, &bin, *interp.policies.top()) else {
                return false;
            };
            targets.push(target);
        }
        let usage = match &self.directory_usage {
            Some(usage) => match usage.restore(&bin, dir) {
                Some(value) => Some(value),
                None => return false,
            },
            None => None,
        };
        let aliases = self
            .aliases
            .iter()
            .map(|(alias, target)| (alias.as_slice().into(), target.as_slice().into()))
            .collect();
        let mut test_names: BTreeSet<_> = writes.tests.iter().map(|name| name.as_slice()).collect();
        let Some(tests) = self
            .tests
            .iter()
            .map(|test| {
                if !test_names.remove(test.name.as_slice()) {
                    return None;
                }
                test.restore(dir, &bin, *interp.policies.top())
            })
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        let rules = if let Some(installs) = &self.installs {
            if writes.record_variable_reads
                && !targets
                    .iter()
                    .map(|target| target.name.as_slice())
                    .eq(writes.targets.iter().map(|(name, _)| name.as_slice()))
            {
                return false;
            }
            let Some(rules) = installs.resolve(&interp.project, &targets, &bin, dir) else {
                return false;
            };
            rules
        } else {
            Vec::new()
        };
        let link_check = if let Some(origin) = &self.link_check {
            let bt = origin.backtrace();
            let Some(frame) = bt.top() else { return false };
            if !valid_origin(origin, &bin)
                || interp.root_frame.defer.is_none()
                || interp.project.global.props.get(REPLAY_LINK_CHECK).is_some()
                || !interp
                    .registry
                    .get_lower(b"target_link_libraries")
                    .is_some_and(|entry| entry.replay)
                || interp
                    .scripted
                    .contains_key(b"target_link_libraries".as_slice())
            {
                return false;
            }
            Some((frame.line, bt))
        } else {
            None
        };
        if !interp
            .project
            .restore_directory_objects(dir, targets, aliases, tests)
        {
            return false;
        }
        for rule in rules {
            rule.insert(&mut interp.project);
        }
        if let Some((line, bt)) = link_check {
            let saved_line = std::mem::replace(&mut interp.cur_line, line);
            let saved_bt = std::mem::replace(&mut interp.bt, bt);
            let id = interp.new_defer_id();
            let _ = interp.queue_deferred(
                DirId::ROOT,
                id,
                b"target_link_libraries",
                &[Argument {
                    text: REPLAY_LINK_CHECK.into(),
                    line,
                    span: 0..0,
                    kind: ArgKind::Bracket,
                }],
            );
            interp.bt = saved_bt;
            interp.cur_line = saved_line;
            interp
                .project
                .global
                .props
                .set(REPLAY_LINK_CHECK.into(), "1".into());
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
struct TestRecord {
    name: Vec<u8>,
    origin: PropertyOrigin,
    command: Vec<Vec<u8>>,
    configurations: Vec<Vec<u8>>,
    working_directory: Option<Vec<u8>>,
    command_expand_lists: bool,
    old_style: bool,
    build_depends: Vec<Value>,
}

impl TestRecord {
    fn capture(test: &TestEntry, bin: &Path, policies: &PolicyMap) -> Option<Self> {
        if test.is_discovery() || !test.props.names().is_empty() || test.policies != *policies {
            return None;
        }
        Some(Self {
            name: test.name.to_vec(),
            origin: PropertyOrigin::new(bin.into(), &test.backtrace),
            command: test.command.iter().map(|value| value.to_vec()).collect(),
            configurations: test
                .configurations
                .iter()
                .map(|value| value.to_vec())
                .collect(),
            working_directory: test.working_directory.as_ref().map(|value| value.to_vec()),
            command_expand_lists: test.command_expand_lists,
            old_style: test.old_style,
            build_depends: capture_values(&test.build_depends, bin),
        })
    }

    fn restore(&self, dir: DirId, bin: &Path, policies: PolicyMap) -> Option<TestEntry> {
        if !valid_origin(&self.origin, bin) || self.command.is_empty() {
            return None;
        }
        Some(TestEntry {
            name: self.name.as_slice().into(),
            dir,
            backtrace: self.origin.backtrace(),
            command: self
                .command
                .iter()
                .map(|value| value.as_slice().into())
                .collect(),
            configurations: self
                .configurations
                .iter()
                .map(|value| value.as_slice().into())
                .collect(),
            working_directory: self.working_directory.as_deref().map(Into::into),
            command_expand_lists: self.command_expand_lists,
            old_style: self.old_style,
            policies,
            props: Default::default(),
            build_depends: restore_values(&self.build_depends, bin)?,
            kind: rcmake_model::TestKind::Single,
        })
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
            || target
                .direct_link_entries(b"INTERFACE_LINK_LIBRARIES_DIRECT")
                .is_some()
            || target
                .direct_link_entries(b"INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE")
                .is_some()
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
            usage: Usage::capture(&target.usage, path, target.dir)?,
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
        target.usage = self.usage.restore(bin, dir)?;
        target.sources = restore_values(&self.sources, bin)?;
        Some(target)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Usage {
    fields: Vec<(String, Vec<Value>)>,
    links: Option<Vec<Link>>,
    interface_links: Option<Vec<Link>>,
}

impl Usage {
    pub(super) fn capture(usage: &UsageProps, path: &Path, dir: DirId) -> Option<Self> {
        Some(Self {
            fields: usage
                .fields
                .iter()
                .map(|(field, values)| (field.name().into(), capture_values(values, path)))
                .collect(),
            links: capture_links(usage.link_libraries.as_deref(), path, dir)?,
            interface_links: capture_links(usage.interface_link_libraries.as_deref(), path, dir)?,
        })
    }

    fn restore(&self, path: &Path, dir: DirId) -> Option<UsageProps> {
        let mut usage = UsageProps {
            link_libraries: restore_links(self.links.as_deref(), path, dir)?,
            interface_link_libraries: restore_links(self.interface_links.as_deref(), path, dir)?,
            ..UsageProps::default()
        };
        for (name, entries) in &self.fields {
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
struct Link(Value, Option<u8>);

fn capture_links(items: Option<&[LinkItem]>, bin: &Path, dir: DirId) -> Option<Option<Vec<Link>>> {
    let Some(items) = items else {
        return Some(None);
    };
    items
        .iter()
        .map(|item| {
            if item.lookup_dir != dir || item.opens_lookup_scope || item.closes_lookup_scope {
                return None;
            }
            let scope = item.scope.map(|scope| match scope {
                UsageScope::Private => 0,
                UsageScope::Public => 1,
                UsageScope::Interface => 2,
            });
            let text = &item.text;
            Some(Link(
                Value(
                    text.value.to_vec(),
                    PropertyOrigin::new(bin.into(), &text.backtrace),
                ),
                scope,
            ))
        })
        .collect::<Option<Vec<_>>>()
        .map(Some)
}

fn restore_links(items: Option<&[Link]>, bin: &Path, dir: DirId) -> Option<Option<Vec<LinkItem>>> {
    let Some(items) = items else {
        return Some(None);
    };
    items
        .iter()
        .map(|Link(Value(text, origin), scope)| {
            if !valid_origin(origin, bin) {
                return None;
            }
            let scope = match scope {
                Some(0) => Some(UsageScope::Private),
                Some(1) => Some(UsageScope::Public),
                Some(2) => Some(UsageScope::Interface),
                None => None,
                Some(_) => return None,
            };
            Some(LinkItem {
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                text: BT::new(text.as_slice().into(), origin.backtrace()),
                lookup_dir: dir,
                scope,
            })
        })
        .collect::<Option<Vec<_>>>()
        .map(Some)
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
