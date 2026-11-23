use rcmake_base::{BString, list, truth};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
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
    for arg in tail {
        match *arg {
            b"INHERITED" => {
                entry.inherited = true;
                docs = None;
            }
            b"BRIEF_DOCS" => docs = Some(&mut entry.brief_docs),
            b"FULL_DOCS" => docs = Some(&mut entry.full_docs),
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
    match (*scope, names) {
        (b"GLOBAL" | b"VARIABLE", []) | (b"CACHE", [_]) => {}
        (b"DIRECTORY", _) => check_directory(cx, names)?,
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
