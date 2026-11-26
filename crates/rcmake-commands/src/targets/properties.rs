use std::collections::{BTreeSet, HashMap, HashSet};

use rcmake_base::{BString, list, truth};
use rcmake_diag::{Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{
    BT, DirId, FileSet, FileSetId, FileSetKind, ModelError, ProjectModel, PropOrigin, PropScope,
    Target, TargetBinding, TargetId, TargetKind, UsageScope,
};

use rcmake_resolve::usage::UsageResolver;

use super::{bytes, model_error};

pub(super) fn set(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if args.len() < 2 {
        return Err(CmdError::status(
            "called with incorrect number of arguments.",
        ));
    }
    let Some(marker) = args.iter().position(|arg| bytes(arg) == b"PROPERTIES") else {
        return Err(CmdError::status(
            "called with illegal arguments, maybe missing a PROPERTIES specifier?",
        ));
    };
    let (names, tail) = args.split_at(marker);
    let [_, props @ ..] = tail else {
        return Err(CmdError::status(
            "called with incorrect number of arguments.",
        ));
    };
    let (props, remainder) = props.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(CmdError::status(
            "called with incorrect number of arguments.",
        ));
    }
    let dir = cx.current_dir_id();
    let bt = cx.backtrace();
    let origin = PropOrigin {
        dir,
        backtrace: &bt,
    };
    let model = cx.project();
    let targets = names
        .iter()
        .map(|name| {
            let binding = model.lookup_target(dir, bytes(name)).ok_or_else(|| {
                CmdError::status(format!(
                    "Can not find target to add properties to: {}",
                    name.value
                ))
            })?;
            let id = binding.id();
            if model.target(id).is_some_and(|target| {
                target
                    .props
                    .get(b"SYMBOLIC")
                    .is_some_and(|value| truth::is_on(value))
            }) {
                return Err(CmdError::status("can not be used on a SYMBOLIC target."));
            }
            Ok((name, binding))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for (name, binding) in targets {
        let id = binding.id();
        let is_alias = matches!(binding, TargetBinding::Alias(_));
        for [property, value] in props {
            let property = bytes(property);
            if is_alias && matches!(property, b"ALIASED_TARGET" | b"ALIAS_GLOBAL") {
                return Err(CmdError::status(format!(
                    "can not set property {} on an ALIAS target: {}",
                    BString::from(property),
                    name.value
                )));
            }
            if property == b"IMPORTED_GLOBAL" {
                promote(model, id, dir, bytes(value))?;
                continue;
            }
            model
                .set_prop(
                    &PropScope::Target(id),
                    property,
                    Some(value.value.clone()),
                    origin,
                )
                .map_err(|err| {
                    if err == ModelError::ReadOnly {
                        CmdError::status(format!(
                            "{} property is read-only",
                            BString::from(property)
                        ))
                    } else {
                        model_error(err)
                    }
                })?;
        }
    }
    Ok(Flow::Normal)
}

fn promote(
    model: &mut ProjectModel,
    id: TargetId,
    dir: DirId,
    value: &[u8],
) -> Result<(), CmdError> {
    let target = model
        .target(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    if !target.is_imported() || !truth::is_on(value) {
        return Err(CmdError::status(format!(
            "IMPORTED_GLOBAL property can't be set to FALSE on targets (\"{}\")",
            target.name
        )));
    }
    if dir != target.dir {
        return Err(CmdError::status(format!(
            "Attempt to promote imported target \"{}\" to global scope (by setting IMPORTED_GLOBAL) which is not built in this directory.",
            target.name
        )));
    }
    model.set_imported_global(id).map_err(model_error)
}

pub(super) fn get(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [var, name, property] = args else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let dir = cx.current_dir_id();
    let model = cx.project();
    let Some(binding) = model.lookup_target(dir, bytes(name)) else {
        return Err(super::reported(
            cx,
            format!(
                "get_target_property() called with non-existent target \"{}\".",
                name.value
            ),
        ));
    };
    let owner = PropScope::Target(binding.id());
    let property = bytes(property);
    let location = target_location(cx, binding.id(), property);
    let model = cx.project();
    let value = if let Some(value) = location {
        value
    } else {
        match (binding, property) {
            (TargetBinding::Alias(id), b"ALIASED_TARGET") => {
                model.target(id).map(|target| target.name.clone())
            }
            (TargetBinding::Alias(_), b"ALIAS_GLOBAL") => {
                let local = model.dir(dir).local_targets.contains_key(bytes(name));
                Some(if local { "FALSE" } else { "TRUE" }.into())
            }
            (_, b"MANUALLY_ADDED_DEPENDENCIES") => model
                .target(binding.id())
                .and_then(crate::policy_misc::properties::manual_dependencies),
            _ if model
                .global
                .defined_property(b"TARGET", property)
                .is_some_and(|prop| prop.inherited) =>
            {
                model.get_prop_inherited(&owner, property)
            }
            _ => file_set_property(model, binding.id(), property)
                .or_else(|| model.get_prop(&owner, property)),
        }
    };
    if value.is_none()
        && cx.collects_diagnostic_annotations()
        && !cx.is_bundled_module()
        && !UsageResolver::is_builtin_target_property(property)
        && let Some((candidate, _)) = cx.project().suggest_target_property(binding.id(), property)
    {
        cx.advise(
            format!(
                "Unknown target property \"{}\". Use \"{candidate}\".",
                BString::from(property)
            )
            .into(),
        );
    }
    let value = value.unwrap_or_else(|| [bytes(var), b"-NOTFOUND"].concat().into());
    cx.set_var(bytes(var), value);
    Ok(Flow::Normal)
}

pub(crate) fn target_location(
    cx: &mut Cx<'_>,
    id: rcmake_model::TargetId,
    property: &[u8],
) -> Option<Option<BString>> {
    let model = cx.project();
    let target = model.target(id)?;
    let config = UsageResolver::location_property_config(target.kind, property)?;
    let name = target.name.clone();
    if !target.is_imported() {
        cx.issue(
            MessageKind::Error,
            Effect::SetError,
            None,
            format!("The LOCATION property may not be read from target \"{name}\".  Use the target name directly with add_custom_command, or use the generator expression $<TARGET_FILE>, as appropriate.\n").into(),
        );
        return Some(None);
    }
    if let Some(location) = UsageResolver::new(model).imported_runtime_location(target, &config) {
        return Some(Some(location));
    }
    let policy = PolicyId::new(111);
    let status = target.policies.policies.get(policy);
    let property = if target.kind == TargetKind::SharedLibrary {
        "IMPORTED_LOCATION or IMPORTED_IMPLIB"
    } else {
        "IMPORTED_LOCATION"
    };
    let config = if config.0.is_empty() {
        String::new()
    } else {
        format!(" configuration \"{}\"", config.0)
    };
    let message = format!("{property} not set for imported target \"{name}\"{config}.");
    match status {
        PolicyStatus::New => cx.issue(MessageKind::Error, Effect::SetError, None, message.into()),
        PolicyStatus::Warn => cx.issue_policy_warning(policy, b"", message.as_bytes()),
        PolicyStatus::Old => {}
    }
    Some(Some(format!("{name}-NOTFOUND").into()))
}

const FILE_SET_KINDS: [(FileSetKind, &[u8]); 3] = [
    (FileSetKind::Headers, b"HEADER"),
    (FileSetKind::Sources, b"SOURCE"),
    (FileSetKind::CxxModules, b"CXX_MODULE"),
];

fn file_set_list_properties(kind: FileSetKind, prefix: &[u8]) -> [BString; 4] {
    let [own, interface] = type_list_properties(kind.as_str().as_bytes());
    [
        [prefix, b"_SETS"].concat().into(),
        [b"INTERFACE_", prefix, b"_SETS"].concat().into(),
        own,
        interface,
    ]
}

fn type_list_properties(name: &[u8]) -> [BString; 2] {
    [
        [b"FILE_SETS_", name].concat().into(),
        [b"INTERFACE_FILE_SETS_", name].concat().into(),
    ]
}

fn rule_name(set: &FileSet) -> Option<&[u8]> {
    set.props.get(b"_RCMAKE_RULE").map(|name| name.as_slice())
}

fn type_name(set: &FileSet) -> &[u8] {
    rule_name(set).unwrap_or_else(|| set.kind.as_str().as_bytes())
}

pub(super) fn initialize_file_set_properties(target: &mut Target) {
    target
        .props
        .set("FILE_SET_TYPES".into(), BString::default());
    for (kind, prefix) in FILE_SET_KINDS {
        for name in file_set_list_properties(kind, prefix) {
            target.props.set(name, BString::default());
        }
    }
}

/// Make the file set properties available through the model's target property lookup.
pub(super) fn store_file_set_properties(model: &mut ProjectModel, set: FileSetId) {
    let Some(set) = model.file_set(set) else {
        return;
    };
    let id = set.target;
    let mut names = vec!["FILE_SET_TYPES".into()];
    if let Some(rule) = rule_name(set) {
        names.extend(type_list_properties(rule));
    } else if let Some((kind, prefix)) = FILE_SET_KINDS
        .into_iter()
        .find(|(kind, _)| *kind == set.kind)
    {
        names.extend(file_set_list_properties(kind, prefix));
        for field in [b"_SET".as_slice(), b"_DIRS"] {
            names.push([prefix, field, b"_", set.name.as_slice()].concat().into());
            if set.name == kind.as_str() {
                names.push([prefix, field].concat().into());
            }
        }
    }
    let values: Vec<_> = names
        .into_iter()
        .filter_map(|name| {
            let value = file_set_property(model, id, &name)?;
            Some((name, value))
        })
        .collect();
    if let Some(target) = model.target_mut(id) {
        for (name, value) in values {
            target.props.set(name, value);
        }
    }
}

fn file_set_property(model: &ProjectModel, id: TargetId, property: &[u8]) -> Option<BString> {
    let target = model.target(id)?;
    let sets = || target.file_sets.iter().filter_map(|&id| model.file_set(id));
    if property == b"FILE_SET_TYPES" {
        let types: BTreeSet<_> = sets().map(type_name).collect();
        return Some(list::join(types));
    }
    for (prefix, interface) in [
        (b"FILE_SETS_".as_slice(), false),
        (b"INTERFACE_FILE_SETS_", true),
    ] {
        if let Some(kind) = property.strip_prefix(prefix) {
            return Some(list::join(
                sets()
                    .filter(|set| type_name(set) == kind)
                    .filter(|set| visible(set.scope, interface))
                    .map(|set| &set.name),
            ));
        }
    }
    for (kind, prefix) in FILE_SET_KINDS {
        let sets = || sets().filter(|set| set.kind == kind && rule_name(set).is_none());
        let own = [prefix, b"_SETS"].concat();
        let interface = [b"INTERFACE_".as_slice(), &own].concat();
        if property == own || property == interface {
            let interface = property == interface;
            return Some(list::join(
                sets()
                    .filter(|set| visible(set.scope, interface))
                    .map(|set| set.name.as_slice()),
            ));
        }
        for (suffix, dirs) in [(b"_SET".as_slice(), false), (b"_DIRS", true)] {
            let key = [prefix, suffix].concat();
            let name = if property == key {
                kind.as_str().as_bytes()
            } else if let Some(name) = property
                .strip_prefix(key.as_slice())
                .and_then(|suffix| suffix.strip_prefix(b"_"))
            {
                name
            } else {
                continue;
            };
            let set = sets().find(|set| set.name == name)?;
            let entries = if dirs { &set.base_dirs } else { &set.files };
            return Some(list::join(
                entries.iter().map(|entry| entry.value.as_slice()),
            ));
        }
    }
    None
}

fn visible(scope: UsageScope, interface: bool) -> bool {
    scope
        != if interface {
            UsageScope::Private
        } else {
            UsageScope::Interface
        }
}

#[derive(Default)]
struct UtilityDepIndex {
    by_target: HashMap<TargetId, (u64, usize, HashSet<BString>)>,
}

pub(super) fn add_dependencies(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, deps @ ..] = args else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let dir = cx.current_dir_id();
    let Some(id) = cx.project().find_target(dir, bytes(name)) else {
        cx.issue_and_continue(
            rcmake_diag::MessageKind::Error,
            rcmake_diag::Effect::SetError,
            None,
            format!(
                "Cannot add target-level dependencies to non-existent target \"{}\".\nThe add_dependencies works for top-level logical targets created by the add_executable, add_library, or add_custom_target commands.  If you want to add file-level dependencies see the DEPENDS option of the add_custom_target and add_custom_command commands.",
                name.value
            ).into(),
        );
        return Ok(Flow::Normal);
    };
    if deps.is_empty() {
        return Ok(Flow::Normal);
    }
    let bt = cx.backtrace();
    let (existing_len, epoch) = {
        let model = cx.project();
        let len = model
            .target(id)
            .ok_or_else(|| model_error(ModelError::UnknownObject))?
            .utility_deps
            .len();
        let epoch = model
            .target_mutation_epoch(id)
            .ok_or_else(|| model_error(ModelError::UnknownObject))?;
        (len, epoch)
    };
    if existing_len.saturating_add(deps.len()) < 32 {
        let target = cx
            .project()
            .target_mut(id)
            .ok_or_else(|| model_error(ModelError::UnknownObject))?;
        for dep in deps {
            if !target
                .utility_deps
                .iter()
                .any(|entry| entry.value == dep.value)
            {
                target
                    .utility_deps
                    .push(BT::new(dep.value.clone(), bt.clone()));
            }
        }
        return Ok(Flow::Normal);
    }

    let needs_seed = cx
        .session_data::<UtilityDepIndex>()
        .and_then(|index| index.by_target.get(&id))
        .is_none_or(|(cached_epoch, len, _)| *cached_epoch != epoch || *len != existing_len);
    let seed = if needs_seed {
        let existing = &cx
            .project()
            .target(id)
            .ok_or_else(|| model_error(ModelError::UnknownObject))?
            .utility_deps;
        let mut set = HashSet::with_capacity(existing.len().saturating_add(deps.len()));
        for entry in existing {
            set.insert(entry.value.clone());
        }
        Some(set)
    } else {
        None
    };

    let mut to_add = Vec::new();
    if let Some(index) = cx.session_data_mut_or_init::<UtilityDepIndex>() {
        if let Some(set) = seed {
            index.by_target.insert(id, (epoch, existing_len, set));
        }
        if let Some((_, cached_len, set)) = index.by_target.get_mut(&id) {
            for dep in deps {
                if !set.contains(&dep.value) {
                    set.insert(dep.value.clone());
                    to_add.push(dep.value.clone());
                }
            }
            *cached_len += to_add.len();
        }
    }
    if !to_add.is_empty() {
        cx.project()
            .push_utility_deps(id, to_add.into_iter().map(|dep| BT::new(dep, bt.clone())))
            .map_err(model_error)?;
    }
    Ok(Flow::Normal)
}
