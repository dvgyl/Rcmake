use std::collections::BTreeMap;

use indexmap::IndexMap;
use rcmake_base::BString;
use rcmake_diag::{Backtrace, FrameKind};
use rcmake_model::{BT, Target, UsageField};
use rcmake_resolve::{ChainKind, CompileGroup, CompileUnit, LinkFragmentRole, ResolvedTarget};

use crate::{CompilePosition, ConfigureResult, ExplainCompileGroup, ExplainEntry, ExplainInput};

pub(crate) fn set_spelling(entry: &mut ExplainEntry) {
    let prefix: &[u8] = match entry.kind {
        ChainKind::Usage(
            UsageField::CompileDefinitions | UsageField::InterfaceCompileDefinitions,
        ) => b"-D",
        ChainKind::Usage(UsageField::InterfaceSystemIncludeDirectories) => b"-isystem ",
        ChainKind::Usage(
            UsageField::IncludeDirectories | UsageField::InterfaceIncludeDirectories,
        ) => {
            if entry.system_include == Some(true) {
                b"-isystem "
            } else {
                b"-I"
            }
        }
        _ => return,
    };
    entry.spelling = Some([prefix, entry.value.as_slice()].concat().into());
}

#[derive(PartialEq, Eq, Hash)]
struct GroupKey<'a> {
    kind: ChainKind,
    value: &'a BString,
    system_include: Option<bool>,
    origin: Vec<FrameKey<'a>>,
}

type FrameKey<'a> = (&'a [u8], u32, Option<&'a [u8]>, u8);

fn frame_key(origin: &Backtrace) -> Vec<FrameKey<'_>> {
    origin
        .frames()
        .map(|frame| {
            let kind = match frame.kind {
                FrameKind::Command => 0,
                FrameKind::Deferred => 1,
                FrameKind::ModuleInclude => 2,
            };
            (frame.file.as_ref(), frame.line, frame.name.as_deref(), kind)
        })
        .collect()
}

pub(crate) fn build_entries(
    result: &ConfigureResult,
    target: &Target,
    resolved: &ResolvedTarget,
) -> (Vec<ExplainCompileGroup>, Vec<ExplainEntry>) {
    let (groups, mut entries) = compile_entries(
        result,
        target,
        &resolved.config.0,
        &resolved.compile_groups,
        &resolved.compile_units,
    );
    for fragment in resolved.link.iter().flat_map(|link| &link.fragments) {
        let kind = match fragment.role {
            LinkFragmentRole::Flags => ChainKind::Usage(UsageField::LinkOptions),
            LinkFragmentRole::Libraries => ChainKind::LinkItem,
            LinkFragmentRole::LibraryPath | LinkFragmentRole::FrameworkPath => {
                ChainKind::Usage(UsageField::LinkDirectories)
            }
        };
        let mut entry = ExplainEntry::new(kind, &fragment.text);
        entry.link_role = Some(fragment.role);
        entry.build_entry = true;
        entries.push(entry);
    }
    (groups, entries)
}

fn compile_entries(
    result: &ConfigureResult,
    target: &Target,
    config: &[u8],
    compile_groups: &[CompileGroup],
    compile_units: &[CompileUnit],
) -> (Vec<ExplainCompileGroup>, Vec<ExplainEntry>) {
    let mut entries = Vec::<ExplainEntry>::new();
    let mut groups = Vec::new();
    let mut compile = IndexMap::<GroupKey<'_>, Vec<usize>>::new();
    let mut flag_inputs = IndexMap::<BString, usize>::new();
    for (group, settings) in compile_groups.iter().enumerate() {
        groups.push(ExplainCompileGroup {
            language: settings.language.clone(),
            sources: settings
                .units
                .iter()
                .filter_map(|&index| {
                    compile_units
                        .get(index)
                        .map(|unit| unit.source.value.clone())
                })
                .collect(),
        });
        let mut ordinal = 0;
        let base = [b"CMAKE_", settings.language.as_slice(), b"_FLAGS"].concat();
        let configured = [base.as_slice(), b"_", &config.to_ascii_uppercase()].concat();
        for name in [base, configured] {
            let Some(value) = result
                .model
                .definition(target.dir, &name)
                .filter(|v| !v.is_empty())
            else {
                continue;
            };
            let index = *flag_inputs
                .entry(name.as_slice().into())
                .or_insert_with(|| {
                    let mut entry = ExplainEntry::new(
                        ChainKind::Usage(UsageField::CompileOptions),
                        &BT::new(value.clone(), Backtrace::default()),
                    );
                    entry.input = Some(ExplainInput {
                        location: result.binding_location(target.dir, &name),
                        name: name.into(),
                    });
                    entry.build_entry = true;
                    entry.chain = Some(Vec::new());
                    entries.push(entry);
                    entries.len() - 1
                });
            if let Some(entry) = entries.get_mut(index) {
                entry
                    .compile_positions
                    .push(CompilePosition { group, ordinal });
            }
            ordinal += 1;
        }
        let flags = settings
            .flags
            .iter()
            .filter(|flag| !flag.value.is_empty())
            .map(|flag| (UsageField::CompileOptions, flag, None));
        let definitions = settings
            .defines
            .iter()
            .map(|value| (UsageField::CompileDefinitions, value, None));
        let includes = settings.includes.iter().map(|include| {
            (
                UsageField::IncludeDirectories,
                &include.path,
                Some(include.system),
            )
        });
        let mut counts = BTreeMap::<usize, usize>::new();
        for (ordinal, (field, value, system_include)) in
            (ordinal..).zip(flags.chain(definitions).chain(includes))
        {
            let kind = ChainKind::Usage(field);
            let key = GroupKey {
                kind,
                value: &value.value,
                system_include,
                origin: frame_key(&value.backtrace),
            };
            let (key_index, seen) = match compile.entry(key) {
                indexmap::map::Entry::Occupied(entry) => (entry.index(), entry.into_mut()),
                indexmap::map::Entry::Vacant(entry) => (entry.index(), entry.insert(Vec::new())),
            };
            let occurrence = counts.entry(key_index).or_default();
            let position = CompilePosition { group, ordinal };
            if let Some(entry) = seen
                .get(*occurrence)
                .and_then(|&index| entries.get_mut(index))
            {
                entry.compile_positions.push(position);
            } else {
                let mut entry = ExplainEntry::new(kind, value);
                entry.build_entry = true;
                entry.system_include = system_include;
                entry.compile_positions.push(position);
                set_spelling(&mut entry);
                seen.push(entries.len());
                entries.push(entry);
            }
            *occurrence += 1;
        }
    }
    (groups, entries)
}

// Lowering can split values and insert flags. Only attach a recorded chain
// when the value and its full origin agree. Consume duplicates in order.
pub(crate) fn join_entries(
    mut build: Vec<ExplainEntry>,
    recorded: Vec<ExplainEntry>,
) -> Vec<ExplainEntry> {
    let mut used = vec![false; recorded.len()];
    {
        let mut keys = IndexMap::<_, Vec<_>>::new();
        for (index, entry) in recorded.iter().enumerate() {
            keys.entry((entry.kind, entry.value.as_slice(), frame_key(&entry.origin)))
                .or_default()
                .push(index);
        }
        let mut next = BTreeMap::<usize, usize>::new();
        for entry in build.iter_mut().filter(|entry| entry.input.is_none()) {
            let key = (entry.kind, entry.value.as_slice(), frame_key(&entry.origin));
            let Some((key_index, _, matches)) = keys.get_full(&key) else {
                continue;
            };
            let cursor = next.entry(key_index).or_default();
            let Some((&index, record)) = matches
                .get(*cursor)
                .and_then(|index| recorded.get(*index).map(|record| (index, record)))
            else {
                continue;
            };
            if entry.compile_positions.len() > 1
                && matches
                    .iter()
                    .filter_map(|&index| recorded.get(index))
                    .any(|other| !same_provenance(record, other))
            {
                continue;
            }
            if let Some(used) = used.get_mut(index) {
                *used = true;
            }
            entry.chain.clone_from(&record.chain);
            entry.link_targets.clone_from(&record.link_targets);
            *cursor += 1;
        }
    }
    build.extend(
        recorded
            .into_iter()
            .zip(used)
            .filter_map(|(entry, used)| (!used).then_some(entry)),
    );
    build
}

fn same_provenance(first: &ExplainEntry, second: &ExplainEntry) -> bool {
    if first.link_targets != second.link_targets {
        return false;
    }
    match (&first.chain, &second.chain) {
        (None, None) => true,
        (Some(first), Some(second)) => {
            first.len() == second.len()
                && first.iter().zip(second).all(|(a, b)| {
                    a.id == b.id && a.via == b.via && a.origin.frames().eq(b.origin.frames())
                })
        }
        _ => false,
    }
}

pub(crate) fn link_role(role: LinkFragmentRole) -> &'static str {
    match role {
        LinkFragmentRole::Flags => "flags",
        LinkFragmentRole::Libraries => "libraries",
        LinkFragmentRole::LibraryPath => "libraryPath",
        LinkFragmentRole::FrameworkPath => "frameworkPath",
    }
}

#[cfg(test)]
mod tests;
