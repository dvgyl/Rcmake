use rcmake_base::{BString, list, truth};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, RunMode};
use rcmake_expr::ExpandedArg;
use rcmake_host::path;
use rcmake_model::{CacheEntry, CacheType, DefinedProperty, PropertyMap};

use super::{files::normalize, quoted, status, values};

const DIRECTORY_ERROR: &str = "DIRECTORY scope provided but requested directory was not found.  This could be because the directory argument was invalid or, it is valid but has not been processed yet.";

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
    check_directory(cx, &directory.into_iter().collect::<Vec<_>>())?;
    let result = match query {
        [b"DEFINITION", name] => cx.var(name),
        [name] => {
            let inherited = is_inherited(cx, b"DIRECTORY", name);
            directory_value(cx, name)
                .or_else(|| inherited.then(|| global_value(cx, name)).flatten())
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
    for [name, value] in property_pairs(pairs)? {
        cx.directory_props().set((*name).into(), (*value).into());
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
    for source in source_names(cx, sources, false)? {
        let source = path::from_native(&cx.resolve_path(source));
        for [name, value] in pairs {
            let key = object_key(b"SOURCE", &source, name);
            cx.directory_props().set(key, (*value).into());
        }
    }
    Ok(Flow::Normal)
}

pub(super) fn get_source(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [out, source, options @ .., name] = args.as_slice() else {
        return Err(status("called with incorrect number of arguments"));
    };
    let mut names = vec![*source];
    names.extend_from_slice(options);
    let selected = source_names(cx, &names, true)?;
    let source = selected.first().copied().unwrap_or_default();
    let inherited = is_inherited(cx, b"SOURCE", name);
    let value = source_value(cx, source, name, inherited).unwrap_or_else(|| "NOTFOUND".into());
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
    fn property(self, props: &mut PropertyMap, name: &[u8], value: Option<BString>) {
        match (self, value) {
            (Self::Set, Some(value)) => props.set(name.into(), value),
            (Self::Set, None) => props.unset(name),
            (Self::Append, Some(value)) if !value.is_empty() => props.append(name.into(), &value),
            (Self::AppendString, Some(value)) if !value.is_empty() => {
                props.append_string(name.into(), &value)
            }
            _ => {}
        }
    }

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
        b"GLOBAL" if names.is_empty() => update.property(&mut cx.global().props, name, value),
        b"GLOBAL" => return Err(status("given names for GLOBAL scope.")),
        b"DIRECTORY" => {
            check_directory(cx, &names)?;
            update.property(cx.directory_props(), name, value);
        }
        b"SOURCE" => {
            for source in source_names(cx, &names, false)? {
                let source = path::from_native(&cx.resolve_path(source));
                let key = object_key(b"SOURCE", &source, name);
                update.property(cx.directory_props(), &key, value.clone());
            }
        }
        b"TARGET" => {
            for target in names {
                let key = target_key(cx, target, name)?;
                update.property(&mut cx.global().props, &key, value.clone());
            }
        }
        b"CACHE" => {
            for entry_name in names {
                let entry = cx.cache().get_mut(entry_name).ok_or_else(|| {
                    status(quoted(
                        "could not find CACHE variable ",
                        entry_name,
                        ".  Perhaps it has not yet been created.",
                    ))
                })?;
                set_cache(
                    entry,
                    name,
                    value.as_ref().map(|value| value.as_slice()),
                    update,
                )?;
            }
        }
        _ => return Err(status(quoted("given invalid scope ", scope, "."))),
    }
    Ok(Flow::Normal)
}

fn set_cache(
    entry: &mut CacheEntry,
    name: &[u8],
    value: Option<&[u8]>,
    update: Update,
) -> Result<(), CmdError> {
    let new = value.unwrap_or_default();
    match name {
        b"VALUE" => entry.value = update.value(&entry.value, new),
        b"HELPSTRING" => entry.help = update.value(&entry.help, new),
        b"TYPE" => {
            let value = update.value(entry.ty.as_str().as_bytes(), new);
            entry.ty = CacheType::parse(&value)
                .filter(|ty| ty.as_str().as_bytes() == value.as_slice())
                .ok_or_else(|| status(quoted("given invalid CACHE entry TYPE \"", &value, "\"")))?;
        }
        b"ADVANCED" => {
            if let Some(new) = value
                && !truth::is_on(new)
                && !truth::is_off(new)
            {
                return Err(status(quoted(
                    "given non-boolean value \"",
                    new,
                    "\" for CACHE property \"ADVANCED\".",
                )));
            }
            update.property(&mut entry.props, name, value.map(BString::from));
            entry.advanced = entry
                .props
                .get(name)
                .is_some_and(|value| truth::is_on(value));
        }
        b"STRINGS" => update.property(&mut entry.props, name, value.map(BString::from)),
        _ => {
            return Err(status(quoted(
                "given invalid CACHE property ",
                name,
                ".  Settable CACHE properties are: ADVANCED, HELPSTRING, STRINGS, TYPE, and VALUE.",
            )));
        }
    }
    Ok(())
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
    let source = match (*scope, names) {
        (b"SOURCE", _) => source_names(cx, names, true)?.first().copied(),
        _ => None,
    };
    match (*scope, names) {
        (b"GLOBAL" | b"VARIABLE", []) | (b"CACHE", [_]) => {}
        (b"DIRECTORY", _) => check_directory(cx, names)?,
        (b"SOURCE", _) | (b"TARGET", [_]) => {}
        (b"TARGET", []) => return Err(status("not given name for TARGET scope.")),
        (b"TARGET", [_, extra, ..]) => {
            return Err(status(quoted("given invalid argument \"", extra, "\".")));
        }
        (b"GLOBAL" | b"VARIABLE", _) => {
            return Err(status(quoted("given name for ", scope, " scope.")));
        }
        (b"CACHE", _) => return Err(status("not given name for CACHE scope.")),
        _ => return Err(status(quoted("given invalid scope ", scope, "."))),
    };
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
            let value = match *scope {
                b"GLOBAL" => global_value(cx, name),
                b"DIRECTORY" => directory_value(cx, name)
                    .or_else(|| inherited.then(|| global_value(cx, name)).flatten()),
                b"SOURCE" => source.and_then(|source| source_value(cx, source, name, inherited)),
                b"TARGET" => {
                    let [target] = names else {
                        return Err(status("not given name for TARGET scope."));
                    };
                    let key = target_key(cx, target, name)?;
                    cx.global().props.get(&key).cloned().or_else(|| {
                        inherited
                            .then(|| directory_value(cx, name).or_else(|| global_value(cx, name)))
                            .flatten()
                    })
                }
                b"CACHE" => names
                    .first()
                    .and_then(|entry| cx.cache().get(entry))
                    .and_then(|entry| cache_value(entry, name)),
                b"VARIABLE" => cx.var(name),
                _ => None,
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

fn source_names<'a>(
    cx: &mut Cx<'_>,
    names: &[&'a [u8]],
    query: bool,
) -> Result<Vec<&'a [u8]>, CmdError> {
    let mut sources = Vec::new();
    let mut words = names.iter().copied();
    while let Some(word) = words.next() {
        match word {
            b"DIRECTORY" | b"TARGET_DIRECTORY" => {
                let value = words.next().ok_or_else(|| {
                    status(quoted(
                        "called with incorrect number of arguments no value provided to the ",
                        word,
                        " option",
                    ))
                })?;
                if word == b"DIRECTORY" {
                    if cx.mode() == RunMode::Script || check_directory(cx, &[value]).is_err() {
                        return Err(status(quoted("given non-existent DIRECTORY ", value, "")));
                    }
                } else if !cx.project().target_names.contains_key(value) {
                    return Err(status(quoted(
                        "given non-existent target for TARGET_DIRECTORY ",
                        value,
                        "",
                    )));
                }
            }
            _ if query && !sources.is_empty() => {
                return Err(status(quoted("given invalid argument \"", word, "\".")));
            }
            _ => sources.push(word),
        }
    }
    if query && sources.is_empty() {
        return Err(status("not given name for SOURCE scope."));
    }
    Ok(sources)
}

fn source_value(cx: &mut Cx<'_>, source: &[u8], name: &[u8], inherited: bool) -> Option<BString> {
    let source = path::from_native(&cx.resolve_path(source));
    let key = object_key(b"SOURCE", &source, name);
    cx.directory_props()
        .get(&key)
        .cloned()
        .or_else(|| match name {
            b"GENERATED" => Some("0".into()),
            b"LOCATION" => Some(path::from_native(&normalize(&path::to_native(&source)))),
            _ if inherited => directory_value(cx, name).or_else(|| global_value(cx, name)),
            _ => None,
        })
}

fn target_key(cx: &mut Cx<'_>, target: &[u8], name: &[u8]) -> Result<BString, CmdError> {
    if cx.project().target_names.contains_key(target) {
        Ok(object_key(b"TARGET", target, name))
    } else {
        Err(status(quoted(
            "could not find TARGET ",
            target,
            ".  Perhaps it has not yet been created.",
        )))
    }
}

fn object_key(scope: &[u8], object: &[u8], name: &[u8]) -> BString {
    // Store object properties in the current model until it has source and target property maps.
    let length = object.len().to_string();
    [
        b"\0".as_slice(),
        scope,
        length.as_bytes(),
        b":",
        object,
        name,
    ]
    .concat()
    .into()
}

fn check_directory(cx: &Cx<'_>, names: &[&[u8]]) -> Result<(), CmdError> {
    match names {
        [] | [b""] => Ok(()),
        [name] => {
            let requested = normalize(&cx.resolve_path(name));
            let source = normalize(&cx.current_source_dir());
            let binary = normalize(&cx.current_binary_dir());
            if requested == source || requested == binary {
                Ok(())
            } else {
                Err(status(DIRECTORY_ERROR))
            }
        }
        _ => Err(status("given more than one name for DIRECTORY scope.")),
    }
}

fn directory_value(cx: &mut Cx<'_>, name: &[u8]) -> Option<BString> {
    match name {
        b"VARIABLES" => Some(list::join(cx.variable_names())),
        b"CACHE_VARIABLES" => Some(cache_names(cx)),
        b"MACROS" => Some(cx.directory_props().get(name).cloned().unwrap_or_default()),
        b"SOURCE_DIR" => Some(path::from_native(&cx.current_source_dir())),
        b"BINARY_DIR" => Some(path::from_native(&cx.current_binary_dir())),
        b"PARENT_DIRECTORY"
        | b"SUBDIRECTORIES"
        | b"BUILDSYSTEM_TARGETS"
        | b"IMPORTED_TARGETS"
        | b"TESTS" => Some(BString::default()),
        _ => cx.directory_props().get(name).cloned(),
    }
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
        b"GENERATOR_IS_MULTI_CONFIG" => {
            let generator = cx.var_or_empty(b"CMAKE_GENERATOR");
            Some(boolean(
                generator.starts_with(b"Visual Studio")
                    || matches!(generator.as_slice(), b"Xcode" | b"Ninja Multi-Config"),
            ))
        }
        _ => cx.global().props.get(name).cloned(),
    }
}

fn cache_value(entry: &CacheEntry, name: &[u8]) -> Option<BString> {
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
    let value = match name.value.as_slice() {
        b"COMMANDS" => Some(list::join(cx.registered_command_names())),
        b"VARIABLES" | b"MACROS" | b"CACHE_VARIABLES" => directory_value(cx, &name.value),
        b"COMPONENTS" => Some(
            cx.global()
                .props
                .get(&name.value)
                .cloned()
                .unwrap_or_default(),
        ),
        _ => global_value(cx, &name.value),
    };
    cx.set_var(&out.value, value.unwrap_or_else(|| "NOTFOUND".into()));
    Ok(Flow::Normal)
}
