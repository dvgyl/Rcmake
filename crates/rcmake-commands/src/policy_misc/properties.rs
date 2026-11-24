use rcmake_base::{BString, list, truth};
use rcmake_diag::{Effect, FrameKind, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, RunMode, UserCommandKind};
use rcmake_expr::ExpandedArg;
use rcmake_host::path;
use rcmake_model::{
    AppendMode, CacheType, DefinedProperty, DirId, FileSetId, FileSetKind, GeneratorKind,
    ModelError, PropOrigin, PropScope, TargetBinding, TargetId, UsageField, UsageScope,
};

use crate::dirs::{find_directory, normalized_path, scoped_test_directory};
use crate::targets::{find_file_set, replace_file_set_paths, require_sycl, scope_name};

use super::{quoted, status, values};

const DIRECTORY_ERROR: &str = "DIRECTORY scope provided but requested directory was not found.  This could be because the directory argument was invalid or, it is valid but has not been processed yet.";

// These CMake 4.5.1 lists are available before the language bootstrap runs.
const KNOWN_FEATURES: [(&[u8], &str); 4] = [
    (
        b"CMAKE_C_KNOWN_FEATURES",
        "c_std_90;c_std_99;c_std_11;c_std_17;c_std_23;c_function_prototypes;c_restrict;\
         c_variadic_macros;c_static_assert",
    ),
    (
        b"CMAKE_CXX_KNOWN_FEATURES",
        "cxx_std_98;cxx_std_11;cxx_std_14;cxx_std_17;cxx_std_20;cxx_std_23;cxx_std_26;\
         cxx_template_template_parameters;cxx_alias_templates;cxx_alignas;cxx_alignof;\
         cxx_attributes;cxx_auto_type;cxx_constexpr;cxx_decltype;\
         cxx_decltype_incomplete_return_types;cxx_default_function_template_args;\
         cxx_defaulted_functions;cxx_defaulted_move_initializers;cxx_delegating_constructors;\
         cxx_deleted_functions;cxx_enum_forward_declarations;cxx_explicit_conversions;\
         cxx_extended_friend_declarations;cxx_extern_templates;cxx_final;\
         cxx_func_identifier;cxx_generalized_initializers;cxx_inheriting_constructors;\
         cxx_inline_namespaces;cxx_lambdas;cxx_local_type_template_args;\
         cxx_long_long_type;cxx_noexcept;cxx_nonstatic_member_init;cxx_nullptr;\
         cxx_override;cxx_range_for;cxx_raw_string_literals;\
         cxx_reference_qualified_functions;cxx_right_angle_brackets;\
         cxx_rvalue_references;cxx_sizeof_member;cxx_static_assert;cxx_strong_enums;\
         cxx_thread_local;cxx_trailing_return_types;cxx_unicode_literals;\
         cxx_uniform_initialization;cxx_unrestricted_unions;cxx_user_literals;\
         cxx_variadic_macros;cxx_variadic_templates;cxx_aggregate_default_initializers;\
         cxx_attribute_deprecated;cxx_binary_literals;cxx_contextual_conversions;\
         cxx_decltype_auto;cxx_digit_separators;cxx_generic_lambdas;\
         cxx_lambda_init_captures;cxx_relaxed_constexpr;cxx_return_type_deduction;\
         cxx_variable_templates",
    ),
    (
        b"CMAKE_CUDA_KNOWN_FEATURES",
        "cuda_std_03;cuda_std_11;cuda_std_14;cuda_std_17;cuda_std_20;cuda_std_23;cuda_std_26",
    ),
    (
        b"CMAKE_HIP_KNOWN_FEATURES",
        "hip_std_98;hip_std_11;hip_std_14;hip_std_17;hip_std_20;hip_std_23;hip_std_26",
    ),
];

fn known_features(name: &[u8]) -> Option<&'static str> {
    KNOWN_FEATURES
        .into_iter()
        .find_map(|(property, features)| (name == property).then_some(features))
}

pub(super) fn define(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [scope, b"PROPERTY", name, tail @ ..] = args.as_slice() else {
        return Err(status("called with incorrect number of arguments"));
    };
    if !matches!(
        *scope,
        b"GLOBAL"
            | b"DIRECTORY"
            | b"TARGET"
            | b"SOURCE"
            | b"TEST"
            | b"VARIABLE"
            | b"CACHED_VARIABLE"
    ) {
        return Err(status(quoted("given invalid scope ", scope, ".")));
    }
    let mut entry = DefinedProperty {
        scope: (*scope).into(),
        name: (*name).into(),
        inherited: false,
        brief_docs: BString::default(),
        full_docs: BString::default(),
        initialize_from_variable: None,
    };
    let mut docs = None;
    let mut tail = tail.iter().copied();
    while let Some(arg) = tail.next() {
        match arg {
            b"INHERITED" => {
                entry.inherited = true;
                docs = None;
            }
            b"BRIEF_DOCS" => docs = Some(&mut entry.brief_docs),
            b"FULL_DOCS" => docs = Some(&mut entry.full_docs),
            b"INITIALIZE_FROM_VARIABLE" => {
                let variable = tail.next().ok_or_else(|| {
                    status("missing variable name after INITIALIZE_FROM_VARIABLE")
                })?;
                validate_initializer(scope, name, variable)?;
                entry.initialize_from_variable = Some(variable.into());
                docs = None;
            }
            other => {
                let Some(target) = docs.as_deref_mut() else {
                    return Err(status(quoted("given invalid argument \"", other, "\".")));
                };
                target.extend_from_slice(other);
            }
        }
    }
    let definitions = &mut cx.global().defined_props;
    if !definitions
        .iter()
        .any(|existing| existing.scope == entry.scope && existing.name == entry.name)
    {
        definitions.push(entry);
    }
    Ok(Flow::Normal)
}

fn validate_initializer(scope: &[u8], property: &[u8], variable: &[u8]) -> Result<(), CmdError> {
    if scope != b"TARGET" {
        return Err(status(
            "Scope must be TARGET if INITIALIZE_FROM_VARIABLE is specified",
        ));
    }
    if variable.starts_with(b"CMAKE_") || variable.starts_with(b"_CMAKE_") {
        return Err(status(quoted(
            "variable name \"",
            variable,
            "\" is reserved",
        )));
    }
    if !variable.ends_with(property) {
        return Err(status(
            [
                b"Variable name \"".as_slice(),
                variable,
                b"\" does not end with property name \"",
                property,
                b"\"",
            ]
            .concat(),
        ));
    }
    if !property.contains(&b'_') {
        return Err(status(quoted(
            "Property name \"",
            property,
            "\" defined with INITIALIZE_FROM_VARIABLE does not contain underscore",
        )));
    }
    Ok(())
}

pub(super) fn get_directory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [out, args @ ..] = args.as_slice() else {
        return Err(status("called with incorrect number of arguments"));
    };
    let (directory, query) = match args {
        [b"DIRECTORY", dir, rest @ ..] => (Some(*dir), rest),
        _ => (None, args),
    };
    let dir = check_directory(cx, directory.as_slice())?;
    let result = match query {
        [b"DEFINITION", name] if dir == cx.current_dir_id() => cx.var(name),
        [b"DEFINITION", _] => None,
        [b""] => return Err(status("given empty string for the property name to get")),
        [name] if *name != b"DIRECTORY" => {
            let inherited = is_inherited(cx, b"DIRECTORY", name);
            directory_value(cx, dir, name, inherited)
        }
        _ => return Err(status("called with incorrect number of arguments")),
    };
    cx.set_var(out, result.unwrap_or_default());
    Ok(Flow::Normal)
}

pub(super) fn set_directory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [b"PROPERTIES", pairs @ ..] = args.as_slice() else {
        return Err(status("Wrong number of arguments"));
    };
    let dir = PropScope::Directory(cx.current_dir_id());
    for [name, value] in property_pairs(pairs)? {
        update_scoped(cx, &dir, name, Some((*value).into()), Update::Set)?;
    }
    Ok(Flow::Normal)
}

pub(super) fn set_sources(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let Some(split) = args.iter().position(|arg| *arg == b"PROPERTIES") else {
        return Err(status("called with incorrect number of arguments"));
    };
    let (sources, tail) = args.split_at(split);
    let pairs = property_pairs(tail.get(1..).unwrap_or_default())?;
    for scope in source_scopes(cx, sources, false)? {
        for [name, value] in pairs {
            update_scoped(cx, &scope, name, Some((*value).into()), Update::Set)?;
        }
    }
    Ok(Flow::Normal)
}

pub(super) fn get_source(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [out, source, options @ ..] = args.as_slice() else {
        return Err(status("called with incorrect number of arguments"));
    };
    let (dir, name) = match options {
        [option @ (b"DIRECTORY" | b"TARGET_DIRECTORY"), value, name] => {
            (source_directory(cx, option, value)?, *name)
        }
        [name] => (cx.current_dir_id(), *name),
        _ => return Err(status("called with incorrect number of arguments")),
    };
    let scope = source_scope(cx, dir, source);
    let inherited = is_inherited(cx, b"SOURCE", name);
    let value = source_value(cx, &scope, name, inherited).unwrap_or_else(|| "NOTFOUND".into());
    cx.set_var(out, value);
    Ok(Flow::Normal)
}

fn property_pairs<'a, 'b>(args: &'a [&'b [u8]]) -> Result<&'a [[&'b [u8]; 2]], CmdError> {
    let (pairs, extra) = args.as_chunks::<2>();
    if !extra.is_empty() {
        return Err(status("called with incorrect number of arguments"));
    }
    Ok(pairs)
}

fn is_inherited(cx: &mut Cx<'_>, scope: &[u8], name: &[u8]) -> bool {
    cx.global()
        .defined_props
        .iter()
        .any(|entry| entry.scope == scope && entry.name == name && entry.inherited)
}

#[derive(Clone, Copy)]
enum Update {
    Set,
    Append,
    AppendString,
}

impl Update {
    fn value(self, old: &[u8], new: &[u8]) -> BString {
        match self {
            Self::Set => new.into(),
            Self::Append if !old.is_empty() && !new.is_empty() => list::join([old, new]),
            _ => [old, new].concat().into(),
        }
    }
}

pub(super) fn set(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let Some((scope, rest)) = args.split_first() else {
        return Err(status("called with incorrect number of arguments"));
    };
    let mut words = rest.splitn(2, |arg| *arg == b"PROPERTY");
    let mut names = words.next().unwrap_or_default().to_vec();
    let Some([name, tail @ ..]) = words.next() else {
        return Err(status("not given a PROPERTY <name> argument."));
    };
    let mut update = Update::Set;
    names.retain(|arg| match *arg {
        b"APPEND" => {
            update = Update::Append;
            false
        }
        b"APPEND_STRING" => {
            update = Update::AppendString;
            false
        }
        _ => true,
    });
    let value = (!tail.is_empty()).then(|| list::join(tail));
    match *scope {
        b"GLOBAL" if names.is_empty() => {
            if !matches!(
                *name,
                b"IN_TRY_COMPILE" | b"ENABLED_LANGUAGES" | b"GENERATOR_IS_MULTI_CONFIG"
            ) && known_features(name).is_none()
            {
                update_scoped(cx, &PropScope::Global, name, value, update)?;
            }
        }
        b"GLOBAL" => return Err(status("given names for GLOBAL scope.")),
        b"DIRECTORY" => {
            let dir = PropScope::Directory(check_directory(cx, &names)?);
            update_scoped(cx, &dir, name, value, update)?;
        }
        b"SOURCE" => {
            for scope in source_scopes(cx, &names, false)? {
                update_scoped(cx, &scope, name, value.clone(), update)?;
            }
        }
        b"FILE_SET" => {
            let (sets, target) = file_set_selection(&names)?;
            let target_id = file_set_target(cx, target)?;
            for set in sets {
                let id = file_set_id(cx, target_id, target, set)?;
                set_file_set(cx, id, name, value.clone(), update)?;
            }
        }
        b"TARGET" => {
            let targets = names
                .iter()
                .map(|target| writable_target(cx, target))
                .collect::<Result<Vec<_>, _>>()?;
            for target in targets {
                if *name == b"IMPORTED_GLOBAL" {
                    set_imported_global(cx, target, value.as_ref(), update)?;
                } else {
                    let scope = PropScope::Target(target);
                    update_scoped(cx, &scope, name, value.clone(), update)?;
                }
            }
        }
        b"TEST" => {
            let (tests, dir) = test_names(cx, &names)?;
            let missing = tests
                .iter()
                .filter(|test| cx.project().find_test(dir, test).is_none())
                .flat_map(|test| [b"\n  ".as_slice(), *test])
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                return Err(status(
                    [
                        b"given TEST names that do not exist:\n".as_slice(),
                        &missing,
                    ]
                    .concat(),
                ));
            }
            for test in tests {
                let scope = PropScope::Test {
                    dir,
                    name: (*test).into(),
                };
                update_scoped(cx, &scope, name, value.clone(), update)?;
            }
        }
        b"INSTALL" => {
            for file in names {
                let key = install_prop_name(file, name);
                update_scoped(cx, &PropScope::Global, &key, value.clone(), update)?;
            }
        }
        b"CACHE" => {
            for entry_name in names {
                set_cache(cx, entry_name, name, value.clone(), update)?;
            }
        }
        _ => return Err(status(quoted("given invalid scope ", scope, "."))),
    }
    Ok(Flow::Normal)
}

fn set_imported_global(
    cx: &mut Cx<'_>,
    id: TargetId,
    value: Option<&BString>,
    update: Update,
) -> Result<(), CmdError> {
    let dir = cx.current_dir_id();
    let project = cx.project();
    let target = project
        .target(id)
        .ok_or_else(|| status(ModelError::UnknownObject.to_string()))?;
    let name = target.name.clone();
    let invalid_directory =
        target.dir != dir && target.is_imported() && matches!(update, Update::Set);
    let error = if !target.is_imported() {
        Some(format!(
            "IMPORTED_GLOBAL property can't be set on non-imported targets(\"{name}\")"
        ))
    } else if !matches!(update, Update::Set) {
        Some(format!(
            "IMPORTED_GLOBAL property can't be appended, only set on imported targets (\"{name}\")"
        ))
    } else if !value.is_some_and(|value| rcmake_base::truth::is_on(value)) {
        Some(format!(
            "IMPORTED_GLOBAL property can't be set to FALSE on targets (\"{name}\")"
        ))
    } else {
        project
            .set_imported_global(id)
            .map_err(|err| status(err.to_string()))?;
        None
    };
    if let Some(error) = error {
        cx.issue(MessageKind::Error, Effect::SetError, None, error.into());
    }
    if invalid_directory {
        cx.issue(MessageKind::Error, Effect::SetError, None, format!(
            "Attempt to promote imported target \"{name}\" to global scope (by setting IMPORTED_GLOBAL) which is not built in this directory."
        ).into());
    }
    Ok(())
}

/// Apply `update` to a model-owned property scope.
fn update_scoped(
    cx: &mut Cx<'_>,
    scope: &PropScope,
    name: &[u8],
    value: Option<BString>,
    update: Update,
) -> Result<(), CmdError> {
    if matches!(scope, PropScope::Directory(_))
        && matches!(
            name,
            b"SUBDIRECTORIES"
                | b"BUILDSYSTEM_TARGETS"
                | b"IMPORTED_TARGETS"
                | b"PARENT_DIRECTORY"
                | b"SOURCE_DIR"
                | b"BINARY_DIR"
                | b"LISTFILE_STACK"
                | b"TESTS"
        )
    {
        return Ok(());
    }
    let value = if matches!(scope, PropScope::Target(_))
        && name == b"SOURCES"
        && !matches!(update, Update::AppendString)
    {
        value.map(|value| list::join(list::split(&value, list::Empty::Drop)))
    } else {
        value
    };
    let backtrace = cx.backtrace();
    let origin = PropOrigin {
        dir: cx.current_dir_id(),
        backtrace: &backtrace,
    };
    let project = cx.project();
    let result = match (update, value) {
        (Update::Set, value) => project.set_prop(scope, name, value, origin),
        (Update::Append, Some(value)) => {
            project.append_prop(scope, name, &value, AppendMode::List, origin)
        }
        (Update::AppendString, Some(value)) => {
            project.append_prop(scope, name, &value, AppendMode::String, origin)
        }
        (_, None) => Ok(()),
    };
    match result {
        Ok(()) => {
            if let (PropScope::Test { dir, name: test }, b"WORKING_DIRECTORY") = (scope, name) {
                let value = project.get_prop(scope, name);
                if let Some(id) = project.find_test(*dir, test)
                    && let Some(entry) = project.test_mut(id)
                {
                    entry.working_directory = value;
                }
            }
            Ok(())
        }
        Err(ModelError::ReadOnly) => {
            cx.issue(
                MessageKind::Error,
                Effect::Fatal,
                None,
                quoted("", name, " property is read-only\n"),
            );
            Err(CmdError::Reported)
        }
        Err(other) => Err(status(other.to_string())),
    }
}

fn source_scope(cx: &mut Cx<'_>, dir: DirId, source: &[u8]) -> PropScope {
    let resolved = normalized_path(&cx.resolve_path(source));
    let mut key = path::from_native(&resolved);
    let binary = normalized_path(&cx.current_binary_dir().join(path::to_native(source)));
    let binary = path::from_native(&binary);
    let known = &mut cx.project().dir_mut(dir).source_props;
    if !known.contains_key(&key) {
        if known.contains_key(&binary) {
            key = binary;
        } else if resolved.extension().is_none()
            && let Some(existing) = known.keys().find(|candidate| {
                [key.as_slice(), binary.as_slice()].iter().any(|prefix| {
                    candidate
                        .strip_prefix(*prefix)
                        .is_some_and(|suffix| suffix.starts_with(b".") && !suffix.contains(&b'/'))
                })
            })
        {
            key = existing.clone();
        }
    }
    known.entry(key.clone()).or_default();
    PropScope::Source { dir, path: key }
}

fn install_prop_name(file: &[u8], name: &[u8]) -> BString {
    // CMake indexes install properties globally by the exact file name.
    // The frozen model has no install property map.
    [
        b"\0INSTALL".as_slice(),
        &file.len().to_be_bytes(),
        file,
        name,
    ]
    .concat()
    .into()
}

fn set_cache(
    cx: &mut Cx<'_>,
    entry: &[u8],
    name: &[u8],
    value: Option<BString>,
    update: Update,
) -> Result<(), CmdError> {
    let ty = cx.cache().get(entry).map(|entry| entry.ty).ok_or_else(|| {
        status(quoted(
            "could not find CACHE variable ",
            entry,
            ".  Perhaps it has not yet been created.",
        ))
    })?;
    match name {
        b"VALUE" | b"HELPSTRING" | b"STRINGS" => {}
        b"TYPE" => {
            let new = value.as_ref().map_or(b"".as_slice(), AsRef::as_ref);
            let new = update.value(ty.as_str().as_bytes(), new);
            if !CacheType::parse(&new).is_some_and(|ty| ty.as_str().as_bytes() == new.as_slice()) {
                return Err(status(quoted(
                    "given invalid CACHE entry TYPE \"",
                    &new,
                    "\"",
                )));
            }
        }
        b"ADVANCED" => {
            if let Some(new) = &value
                && !truth::is_on(new)
                && !truth::is_off(new)
            {
                return Err(status(quoted(
                    "given non-boolean value \"",
                    new,
                    "\" for CACHE property \"ADVANCED\".",
                )));
            }
        }
        _ => {
            return Err(status(quoted(
                "given invalid CACHE property ",
                name,
                ".  Settable CACHE properties are: ADVANCED, HELPSTRING, STRINGS, TYPE, and VALUE.",
            )));
        }
    }
    update_scoped(cx, &PropScope::Cache(entry.into()), name, value, update)
}

fn file_set_selection<'a, 'b>(
    names: &'a [&'b [u8]],
) -> Result<(&'a [&'b [u8]], &'b [u8]), CmdError> {
    let Some(index) = names.iter().position(|word| *word == b"TARGET") else {
        return Err(status("required TARGET option is missing"));
    };
    let (sets, tail) = names.split_at(index);
    match tail {
        [_, target] => Ok((sets, target)),
        [_] => Err(status(
            "called with incorrect number of arguments no value provided to the TARGET option",
        )),
        [_, _, extra, ..] => Err(status(quoted("given invalid argument \"", extra, "\"."))),
        [] => Err(status("required TARGET option is missing")),
    }
}

fn file_set_target(cx: &mut Cx<'_>, target: &[u8]) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    cx.project()
        .find_target(dir, target)
        .ok_or_else(|| status(quoted("given non-existent TARGET ", target, "")))
}

fn file_set_id(
    cx: &mut Cx<'_>,
    target_id: TargetId,
    target: &[u8],
    set: &[u8],
) -> Result<FileSetId, CmdError> {
    find_file_set(cx.project(), target_id, set).ok_or_else(|| {
        status(format!(
            "could not find FILE_SET {} for TARGET {}.  Perhaps it has not yet been created.",
            BString::from(set),
            BString::from(target)
        ))
    })
}

fn set_file_set(
    cx: &mut Cx<'_>,
    id: FileSetId,
    name: &[u8],
    value: Option<BString>,
    update: Update,
) -> Result<(), CmdError> {
    let model = cx.project();
    let set = model
        .file_set(id)
        .ok_or_else(|| status("file set does not exist"))?;
    match name {
        b"TYPE" | b"SCOPE" => {
            let target = model
                .target(set.target)
                .ok_or_else(|| status("target does not exist"))?;
            let body = format!(
                "{} property is read-only for the file set \"{} of the target \"{}\"\n",
                BString::from(name),
                set.name,
                target.name
            );
            cx.issue(MessageKind::Error, Effect::SetError, None, body.into());
            Ok(())
        }
        b"BASE_DIRS" | b"SOURCES" | b"INTERFACE_SOURCES" => {
            if (name == b"SOURCES" && set.scope == UsageScope::Interface)
                || (name == b"INTERFACE_SOURCES" && set.scope == UsageScope::Private)
                || (value.is_none() && !matches!(update, Update::Set))
            {
                return Ok(());
            }
            replace_file_set_paths(
                cx,
                id,
                name == b"BASE_DIRS",
                value,
                !matches!(update, Update::Set),
            )
        }
        b"LANGUAGE" => {
            if !matches!(update, Update::Set) {
                return Err(crate::command_error(
                    cx,
                    "LANGUAGE file set property does not support appending values.",
                ));
            }
            if value
                .as_ref()
                .is_some_and(|value| value.windows(2).any(|pair| pair == b"$<"))
            {
                return Err(crate::command_error(
                    cx,
                    "LANGUAGE file set property does not support generator expressions.",
                ));
            }
            match set.kind {
                FileSetKind::Headers => {
                    require_sycl(cx, "LANGUAGE property on a HEADERS file set")?
                }
                FileSetKind::CxxModules => {
                    return Err(crate::command_error(
                        cx,
                        "LANGUAGE property may only be set on a SOURCES, HEADERS, or custom rule file set.",
                    ));
                }
                FileSetKind::Sources => {}
            }
            update_scoped(cx, &PropScope::FileSet(id), name, value, update)
        }
        _ => update_scoped(cx, &PropScope::FileSet(id), name, value, update),
    }
}

fn file_set_value(cx: &mut Cx<'_>, id: FileSetId, name: &[u8], inherited: bool) -> Option<BString> {
    let set = cx.project().file_set(id)?;
    let entries = match name {
        b"TYPE" => return Some(set.kind.as_str().into()),
        b"SCOPE" => return Some(scope_name(set.scope).into()),
        b"BASE_DIRS" => &set.base_dirs,
        b"SOURCES" if set.scope != UsageScope::Interface => &set.files,
        b"INTERFACE_SOURCES" if set.scope != UsageScope::Private => &set.files,
        b"SOURCES" | b"INTERFACE_SOURCES" => return None,
        _ => return model_value(cx, &PropScope::FileSet(id), name, inherited),
    };
    (!entries.is_empty()).then(|| list::join(entries.iter().map(|entry| &entry.value)))
}

#[derive(Clone, Copy)]
enum Query {
    Value,
    Set,
    Defined,
    Brief,
    Full,
}

pub(super) fn get(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [out, scope, rest @ ..] = args.as_slice() else {
        return Err(status("called with incorrect number of arguments"));
    };
    if !matches!(
        *scope,
        b"GLOBAL"
            | b"DIRECTORY"
            | b"RULE"
            | b"TARGET"
            | b"FILE_SET"
            | b"SOURCE"
            | b"TEST"
            | b"VARIABLE"
            | b"CACHE"
            | b"INSTALL"
    ) {
        return Err(status(quoted(
            "given invalid scope ",
            scope,
            ".  Valid scopes are GLOBAL, DIRECTORY, RULE, TARGET, FILE_SET, SOURCE, TEST, VARIABLE, CACHE, INSTALL.",
        )));
    }
    let mut words = rest.splitn(2, |arg| *arg == b"PROPERTY");
    let names = words.next().unwrap_or_default();
    let Some([name, tail @ ..]) = words.next() else {
        return Err(status("not given a PROPERTY <name> argument."));
    };
    let mut query = Query::Value;
    for option in tail {
        query = match *option {
            b"SET" => Query::Set,
            b"DEFINED" => Query::Defined,
            b"BRIEF_DOCS" => Query::Brief,
            b"FULL_DOCS" => Query::Full,
            other => return Err(status(quoted("given invalid argument \"", other, "\"."))),
        };
    }
    let file_set = if *scope == b"FILE_SET" {
        let (names, target) = file_set_selection(names)?;
        let set = single_name(scope, names)?;
        if set.is_empty() {
            return Err(status("not given name for FILE_SET scope."));
        }
        Some((set, target))
    } else {
        None
    };
    let mut object = None;
    match (*scope, names) {
        (b"GLOBAL" | b"VARIABLE", [])
        | (b"CACHE" | b"INSTALL" | b"TARGET", [_])
        | (b"FILE_SET", _) => {}
        (b"DIRECTORY", _) => {
            object = Some(PropScope::Directory(check_directory(cx, names)?));
        }
        (b"SOURCE", _) => object = source_scopes(cx, names, true)?.pop(),
        (b"TEST", _) => {
            let (tests, dir) = test_names(cx, names)?;
            let test = single_name(scope, tests)?;
            object = Some(PropScope::Test {
                dir,
                name: test.into(),
            });
        }
        (b"GLOBAL" | b"VARIABLE", _) => {
            return Err(status(quoted("given name for ", scope, " scope.")));
        }
        (b"TARGET" | b"CACHE" | b"INSTALL", _) => {
            single_name(scope, names)?;
        }
        _ => return Err(status(quoted("given invalid scope ", scope, "."))),
    }
    let definition_scope: &[u8] = if *scope == b"CACHE" {
        b"CACHED_VARIABLE"
    } else {
        scope
    };
    let definition = cx
        .global()
        .defined_props
        .iter()
        .find(|entry| entry.scope == definition_scope && entry.name == *name);
    let result = match query {
        Query::Defined => Some(boolean(definition.is_some())),
        Query::Brief | Query::Full => {
            let doc = definition.map(|entry| match query {
                Query::Brief => &entry.brief_docs,
                _ => &entry.full_docs,
            });
            Some(
                doc.filter(|doc| !doc.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "NOTFOUND".into()),
            )
        }
        Query::Value | Query::Set => {
            let inherited = definition.is_some_and(|entry| entry.inherited);
            let value = match object {
                Some(PropScope::Directory(dir)) => directory_value(cx, dir, name, inherited),
                Some(scope @ PropScope::Source { .. }) => source_value(cx, &scope, name, inherited),
                Some(scope @ PropScope::Test { .. }) => {
                    if let PropScope::Test { dir, name: test } = &scope
                        && cx.project().find_test(*dir, test).is_none()
                    {
                        return Err(status(quoted(
                            "given TEST name that does not exist: ",
                            test,
                            "",
                        )));
                    }
                    model_value(cx, &scope, name, inherited)
                }
                _ => match (*scope, names) {
                    (b"GLOBAL", _) => global_value(cx, name),
                    (b"TARGET", [target]) => {
                        let scope = PropScope::Target(find_target(cx, target)?);
                        alias_value(cx, target, name)
                            .or_else(|| model_value(cx, &scope, name, inherited))
                    }
                    (b"FILE_SET", _) => {
                        let (set, target) =
                            file_set.ok_or_else(|| status("not given name for FILE_SET scope."))?;
                        let target_id = file_set_target(cx, target)?;
                        let id = file_set_id(cx, target_id, target, set)?;
                        file_set_value(cx, id, name, inherited)
                    }
                    (b"CACHE", [entry]) => cx
                        .project()
                        .get_prop(&PropScope::Cache((*entry).into()), name),
                    (b"INSTALL", [file]) => cx
                        .project()
                        .get_prop(&PropScope::Global, &install_prop_name(file, name)),
                    (b"VARIABLE", _) => cx.var(name),
                    _ => None,
                },
            };
            if matches!(query, Query::Set) {
                Some(boolean(value.is_some()))
            } else {
                value
            }
        }
    };
    if let Some(value) = result {
        cx.set_var(out, value);
    } else {
        cx.unset_var(out);
    }
    Ok(Flow::Normal)
}

fn single_name<'a>(scope: &[u8], names: &[&'a [u8]]) -> Result<&'a [u8], CmdError> {
    match names {
        [name] => Ok(name),
        [] => Err(status(quoted("not given name for ", scope, " scope."))),
        [_, extra, ..] => Err(status(quoted("given invalid argument \"", extra, "\"."))),
    }
}

fn test_names<'a, 'b>(
    cx: &mut Cx<'_>,
    names: &'a [&'b [u8]],
) -> Result<(&'a [&'b [u8]], DirId), CmdError> {
    match names {
        [tests @ .., b"DIRECTORY", directory] => Ok((tests, scoped_test_directory(cx, directory)?)),
        [.., b"DIRECTORY"] => Err(status(
            "called with incorrect number of arguments no value provided to the DIRECTORY option",
        )),
        _ => Ok((names, cx.current_dir_id())),
    }
}

fn source_scopes(
    cx: &mut Cx<'_>,
    mut names: &[&[u8]],
    query: bool,
) -> Result<Vec<PropScope>, CmdError> {
    let is_option = |word: &[u8]| matches!(word, b"DIRECTORY" | b"TARGET_DIRECTORY");
    let section_end = |words: &[&[u8]]| {
        words
            .iter()
            .position(|word| is_option(word))
            .unwrap_or(words.len())
    };
    let (sources, rest) = names.split_at(section_end(names));
    names = rest;
    if query {
        single_name(b"SOURCE", sources)?;
    }
    let mut dirs = Vec::new();
    while let Some((option, rest)) = names.split_first() {
        let (selected, tail) = rest.split_at(section_end(rest));
        if selected.is_empty() {
            return Err(status(quoted(
                "called with incorrect number of arguments no value provided to the ",
                option,
                " option",
            )));
        }
        if query {
            single_name(option, selected)?;
        }
        for name in selected {
            let dir = source_directory(cx, option, name)?;
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        names = tail;
    }
    if dirs.is_empty() {
        dirs.push(cx.current_dir_id());
    }
    let mut scopes = Vec::new();
    for dir in dirs {
        scopes.extend(sources.iter().map(|source| source_scope(cx, dir, source)));
    }
    Ok(scopes)
}

fn source_directory(cx: &mut Cx<'_>, option: &[u8], value: &[u8]) -> Result<DirId, CmdError> {
    if option == b"DIRECTORY" {
        if cx.mode() != RunMode::Script
            && let Some(dir) = find_directory(cx, value)
        {
            return Ok(dir);
        }
        return Err(status(quoted("given non-existent DIRECTORY ", value, "")));
    }
    let target = find_target(cx, value)
        .ok()
        .and_then(|id| cx.project().target(id));
    target.map(|target| target.dir).ok_or_else(|| {
        status(quoted(
            "given non-existent target for TARGET_DIRECTORY ",
            value,
            "",
        ))
    })
}

fn source_value(
    cx: &mut Cx<'_>,
    scope: &PropScope,
    name: &[u8],
    inherited: bool,
) -> Option<BString> {
    let PropScope::Source { path: source, .. } = scope else {
        return None;
    };
    model_value(cx, scope, name, inherited).or_else(|| match name {
        b"GENERATED" => Some("0".into()),
        b"LOCATION" => Some(source.clone()),
        _ => None,
    })
}

fn find_target(cx: &mut Cx<'_>, target: &[u8]) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    cx.project().find_target(dir, target).ok_or_else(|| {
        status(quoted(
            "could not find TARGET ",
            target,
            ".  Perhaps it has not yet been created.",
        ))
    })
}

fn writable_target(cx: &mut Cx<'_>, name: &[u8]) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    if matches!(
        cx.project().lookup_target(dir, name),
        Some(TargetBinding::Alias(_))
    ) {
        return Err(status("can not be used on an ALIAS target."));
    }
    find_target(cx, name)
}

fn alias_value(cx: &mut Cx<'_>, target: &[u8], name: &[u8]) -> Option<BString> {
    let dir = cx.current_dir_id();
    let model = cx.project();
    let TargetBinding::Alias(id) = model.lookup_target(dir, target)? else {
        return None;
    };
    let target = model.target(id)?;
    match name {
        b"ALIASED_TARGET" => Some(target.name.clone()),
        b"ALIAS_GLOBAL" => Some(if target.is_global() { "TRUE" } else { "FALSE" }.into()),
        _ => None,
    }
}

fn check_directory(cx: &mut Cx<'_>, names: &[&[u8]]) -> Result<DirId, CmdError> {
    match names {
        [] => Ok(cx.current_dir_id()),
        [name] => find_directory(cx, name).ok_or_else(|| status(DIRECTORY_ERROR)),
        _ => Err(status("given more than one name for DIRECTORY scope.")),
    }
}

fn model_value(
    cx: &mut Cx<'_>,
    scope: &PropScope,
    name: &[u8],
    inherited: bool,
) -> Option<BString> {
    if inherited {
        cx.project().get_prop_inherited(scope, name)
    } else {
        cx.project().get_prop(scope, name)
    }
}

fn directory_value(cx: &mut Cx<'_>, dir: DirId, name: &[u8], inherited: bool) -> Option<BString> {
    match name {
        b"VARIABLES" if dir == cx.current_dir_id() => Some(list::join(cx.variable_names())),
        b"CACHE_VARIABLES" => Some(cache_names(cx)),
        b"RULES" => Some(list::join(
            cx.project()
                .dir(dir)
                .props
                .names()
                .iter()
                .filter_map(|key| key.strip_prefix(b"_RCMAKE_CUSTOM_RULE_")),
        )),
        b"MACROS" => Some(
            cx.project()
                .dir(dir)
                .props
                .get(name)
                .cloned()
                .unwrap_or_default(),
        ),
        b"LISTFILE_STACK" => Some(listfile_stack(cx, dir)),
        _ => model_value(cx, &PropScope::Directory(dir), name, inherited).or_else(|| {
            UsageField::from_name(name)
                .filter(|field| field.on_directory())
                .map(|_| BString::default())
        }),
    }
}

fn listfile_stack(cx: &mut Cx<'_>, dir: DirId) -> BString {
    let entry = cx.project().dir(dir).listfiles.first().cloned();
    let backtrace = cx.backtrace();
    let mut frames: Vec<_> = backtrace.frames().collect();
    frames.reverse();
    let mut files = Vec::new();
    let mut function = false;
    for frame in frames {
        if frame.kind == FrameKind::ModuleInclude {
            if entry
                .as_ref()
                .is_some_and(|entry| entry.as_slice() == &*frame.file)
            {
                files.clear();
            }
            files.push(&*frame.file);
        } else if function {
            files.push(&*frame.file);
        }
        function = frame
            .name
            .as_deref()
            .is_some_and(|name| cx.user_command_kind(name) == Some(UserCommandKind::Function));
    }
    if dir != cx.current_dir_id() {
        return entry.unwrap_or_default();
    }
    list::join(files)
}

fn global_value(cx: &mut Cx<'_>, name: &[u8]) -> Option<BString> {
    match name {
        b"CMAKE_ROLE" => Some(
            match cx.mode() {
                RunMode::Script => "SCRIPT",
                RunMode::Configure => "PROJECT",
            }
            .into(),
        ),
        b"GENERATOR_IS_MULTI_CONFIG" => Some(boolean(generator_is_multi_config(cx))),
        b"IN_TRY_COMPILE" => Some(boolean(
            cx.global()
                .props
                .get(name)
                .is_some_and(|value| truth::is_on(value)),
        )),
        b"ENABLED_LANGUAGES" => Some(cx.global().props.get(name).cloned().unwrap_or_default()),
        _ => known_features(name)
            .map(BString::from)
            .or_else(|| cx.global().props.get(name).cloned()),
    }
}

fn generator_is_multi_config(cx: &mut Cx<'_>) -> bool {
    if cx.mode() == RunMode::Script {
        return false;
    }
    if let Some(generator) = cx.project().generator {
        return generator.multi_config();
    }
    // Preload scripts and generators without a model kind use the cache entry.
    cx.cache().get(b"CMAKE_GENERATOR").is_some_and(|entry| {
        entry.ty == CacheType::Internal
            && (GeneratorKind::from_name(&entry.value).is_some_and(GeneratorKind::multi_config)
                || entry.value == b"Xcode"
                || entry.value.starts_with(b"Visual Studio "))
    })
}

fn cache_names(cx: &mut Cx<'_>) -> BString {
    let mut names: Vec<_> = cx.cache().iter().map(|(name, _)| name).collect();
    names.sort();
    list::join(names)
}

fn boolean(value: bool) -> BString {
    if value { "1" } else { "0" }.into()
}

pub(super) fn get_cmake(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [out, name, ..] = args else {
        return Err(status("called with incorrect number of arguments"));
    };
    let dir = cx.current_dir_id();
    let value = match name.value.as_slice() {
        b"COMMANDS" => Some(list::join(cx.registered_command_names())),
        b"VARIABLES" | b"MACROS" | b"CACHE_VARIABLES" => {
            directory_value(cx, dir, &name.value, false)
        }
        b"COMPONENTS" => Some(list::join(cx.project().install_components())),
        _ => global_value(cx, &name.value),
    };
    cx.set_var(&out.value, value.unwrap_or_else(|| "NOTFOUND".into()));
    Ok(Flow::Normal)
}

pub(super) fn print_value(
    cx: &mut Cx<'_>,
    scope: &[u8],
    object: &[u8],
    name: &[u8],
) -> Result<Option<Option<BString>>, CmdError> {
    let dir = cx.current_dir_id();
    let inherited = is_inherited(cx, scope, name);
    let value = match scope {
        b"TARGET" => {
            let id = find_target(cx, object)?;
            if let Some(config) = location_config(name) {
                let model = cx.project();
                if !model
                    .target(id)
                    .is_some_and(rcmake_model::Target::is_imported)
                {
                    cx.issue(
                        rcmake_diag::MessageKind::Warning,
                        rcmake_diag::Effect::None,
                        None,
                        format!(
                            "The {} property may not be read from non-imported target \"{}\"; skipping.",
                            BString::from(name),
                            BString::from(object)
                        ).into(),
                    );
                    return Ok(None);
                }
                let owner = PropScope::Target(id);
                let configuration = config
                    .filter(|name| !name.is_empty())
                    .map(|name| name.to_ascii_uppercase())
                    .or_else(|| {
                        let value = cx.var_or_empty(b"CMAKE_BUILD_TYPE");
                        (!value.is_empty()).then(|| value.to_ascii_uppercase())
                    });
                let model = cx.project();
                configuration
                    .and_then(|config| {
                        model.get_prop(&owner, &[b"IMPORTED_LOCATION_", config.as_slice()].concat())
                    })
                    .or_else(|| model.get_prop(&owner, b"IMPORTED_LOCATION"))
                    .or_else(|| {
                        let configurations = model.get_prop(&owner, b"IMPORTED_CONFIGURATIONS")?;
                        list::split(&configurations, list::Empty::Drop)
                            .into_iter()
                            .find_map(|config| {
                                model.get_prop(
                                    &owner,
                                    &[
                                        b"IMPORTED_LOCATION_".as_slice(),
                                        &config.to_ascii_uppercase(),
                                    ]
                                    .concat(),
                                )
                            })
                    })
            } else {
                alias_value(cx, object, name)
                    .or_else(|| model_value(cx, &PropScope::Target(id), name, inherited))
            }
        }
        b"DIRECTORY" => {
            let id = check_directory(cx, &[object])?;
            directory_value(cx, id, name, inherited)
        }
        b"SOURCE" => {
            let owner = source_scope(cx, dir, object);
            source_value(cx, &owner, name, inherited).or_else(|| {
                (name == b"LANGUAGE").then(|| {
                    let extension = object.rsplit(|&b| b == b'.').next().unwrap_or_default();
                    let languages = global_value(cx, b"ENABLED_LANGUAGES").unwrap_or_default();
                    for language in list::split(&languages, list::Empty::Drop) {
                        let key =
                            [b"CMAKE_", language.as_slice(), b"_SOURCE_FILE_EXTENSIONS"].concat();
                        if list::split(&cx.var_or_empty(&key), list::Empty::Drop)
                            .iter()
                            .any(|item| item == extension)
                        {
                            return language;
                        }
                    }
                    BString::default()
                })
            })
        }
        b"TEST" => {
            if cx.project().find_test(dir, object).is_none() {
                return Err(status(quoted(
                    "given TEST name that does not exist: ",
                    object,
                    "",
                )));
            }
            model_value(
                cx,
                &PropScope::Test {
                    dir,
                    name: object.into(),
                },
                name,
                inherited,
            )
        }
        b"CACHE" => cx
            .project()
            .get_prop(&PropScope::Cache(object.into()), name),
        _ => None,
    };
    Ok(Some(value))
}

// The suffix form excludes the IMPORTED_LOCATION property.
fn location_config(name: &[u8]) -> Option<Option<&[u8]>> {
    if name == b"LOCATION" {
        Some(None)
    } else if let Some(config) = name.strip_prefix(b"LOCATION_") {
        Some(Some(config))
    } else if let Some(config) = name
        .strip_suffix(b"_LOCATION")
        .filter(|&p| p != b"IMPORTED")
    {
        Some(Some(config))
    } else {
        None
    }
}
