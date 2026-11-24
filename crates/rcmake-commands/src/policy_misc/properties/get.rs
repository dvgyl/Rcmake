use rcmake_base::{BString, list};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::PropScope;

use super::computed::{boolean, directory_value, global_value, source_value};
use super::file_set::{file_set_id, file_set_selection, file_set_target, file_set_value};
use super::scope::{
    SourceSelection, alias_value, check_directory, find_target, single_name, source_directory,
    source_scope, source_scopes, test_names,
};
use super::{is_inherited, model_value, quoted, values};

pub(in crate::policy_misc) fn get_directory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [out, args @ ..] = args.as_slice() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let (directory, query) = match args {
        [b"DIRECTORY", dir, rest @ ..] => (Some(*dir), rest),
        _ => (None, args),
    };
    let dir = check_directory(cx, directory.as_slice())?;
    let result = match query {
        [b"DEFINITION", name] if dir == cx.current_dir_id() => cx.var(name),
        [b"DEFINITION", name] => cx
            .project()
            .dir(dir)
            .vars
            .get(*name)
            .cloned()
            .or_else(|| cx.cache().get(name).map(|entry| entry.value.clone())),
        [b""] => {
            return Err(CmdError::status(
                "given empty string for the property name to get",
            ));
        }
        [name] if *name != b"DIRECTORY" => {
            let inherited = is_inherited(cx, b"DIRECTORY", name);
            directory_value(cx, dir, name, inherited)
        }
        _ => {
            return Err(CmdError::status(
                "called with incorrect number of arguments",
            ));
        }
    };
    cx.set_var(out, result.unwrap_or_default());
    Ok(Flow::Normal)
}

pub(in crate::policy_misc) fn get_source(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [out, source, options @ ..] = args.as_slice() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let (dir, name) = match options {
        [option @ (b"DIRECTORY" | b"TARGET_DIRECTORY"), value, name] => {
            (source_directory(cx, option, value)?, *name)
        }
        [name] => (cx.current_dir_id(), *name),
        _ => {
            return Err(CmdError::status(
                "called with incorrect number of arguments",
            ));
        }
    };
    let inherited = is_inherited(cx, b"SOURCE", name);
    let value = query_source(cx, dir, source, name, inherited).unwrap_or_else(|| "NOTFOUND".into());
    cx.set_var(out, value);
    Ok(Flow::Normal)
}

fn query_source(
    cx: &mut Cx<'_>,
    dir: rcmake_model::DirId,
    source: &[u8],
    name: &[u8],
    inherited: bool,
) -> Option<BString> {
    if name == b"GENERATED" {
        if let Some(value) = super::generated::global_value(cx, dir, source) {
            return Some(value);
        }
        if super::generated::is_strict(cx, dir) {
            let PropScope::Source { path, .. } = source_scope(cx, dir, source) else {
                return None;
            };
            return Some(boolean(
                cx.project().dir(dir).generated_sources.contains(&path),
            ));
        }
    }
    let scope = source_scope(cx, dir, source);
    source_value(cx, &scope, name, inherited)
}

#[derive(Clone, Copy)]
enum QueryMode {
    Value,
    Set,
    Defined,
    Brief,
    Full,
}

struct PropertyQuery<'a> {
    definition_scope: &'a [u8],
    name: &'a [u8],
    mode: QueryMode,
    owner: PropertyOwner<'a>,
}

impl<'a> PropertyQuery<'a> {
    fn parse(cx: &mut Cx<'_>, scope: &'a [u8], args: &[&'a [u8]]) -> Result<Self, CmdError> {
        if !matches!(
            scope,
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
            return Err(CmdError::status(quoted(
                "given invalid scope ",
                scope,
                ".  Valid scopes are GLOBAL, DIRECTORY, RULE, TARGET, FILE_SET, SOURCE, TEST, VARIABLE, CACHE, INSTALL.",
            )));
        }
        let mut words = args.splitn(2, |arg| *arg == b"PROPERTY");
        let names = words.next().unwrap_or_default();
        let Some([name, tail @ ..]) = words.next() else {
            return Err(CmdError::status("not given a PROPERTY <name> argument."));
        };
        let mut mode = QueryMode::Value;
        for option in tail {
            mode = match *option {
                b"SET" => QueryMode::Set,
                b"DEFINED" => QueryMode::Defined,
                b"BRIEF_DOCS" => QueryMode::Brief,
                b"FULL_DOCS" => QueryMode::Full,
                other => {
                    return Err(CmdError::status(quoted(
                        "given invalid argument \"",
                        other,
                        "\".",
                    )));
                }
            };
        }
        let owner = PropertyOwner::parse(cx, scope, names)?;
        let definition_scope = if scope == b"CACHE" {
            b"CACHED_VARIABLE"
        } else {
            scope
        };
        Ok(Self {
            definition_scope,
            name,
            mode,
            owner,
        })
    }

    fn value(self, cx: &mut Cx<'_>) -> Result<Option<BString>, CmdError> {
        let definition = cx
            .global()
            .defined_props
            .iter()
            .find(|entry| entry.scope == self.definition_scope && entry.name == self.name);
        let value = match self.mode {
            QueryMode::Defined => Some(boolean(definition.is_some())),
            QueryMode::Brief | QueryMode::Full => {
                let docs = definition.map(|entry| match self.mode {
                    QueryMode::Brief => &entry.brief_docs,
                    _ => &entry.full_docs,
                });
                Some(
                    docs.filter(|doc| !doc.is_empty())
                        .cloned()
                        .unwrap_or_else(|| "NOTFOUND".into()),
                )
            }
            QueryMode::Value | QueryMode::Set => {
                let inherited = definition.is_some_and(|entry| entry.inherited);
                let value = self.owner.value(cx, self.name, inherited)?;
                if matches!(self.mode, QueryMode::Set) {
                    Some(boolean(value.is_some()))
                } else {
                    value
                }
            }
        };
        Ok(value)
    }
}

enum PropertyOwner<'a> {
    Global,
    Variable,
    Target(&'a [u8]),
    FileSet { set: &'a [u8], target: &'a [u8] },
    Source(Option<SourceSelection<'a>>),
    Model(PropScope),
}

impl<'a> PropertyOwner<'a> {
    fn parse(cx: &mut Cx<'_>, scope: &[u8], names: &[&'a [u8]]) -> Result<Self, CmdError> {
        Ok(match scope {
            b"GLOBAL" if names.is_empty() => Self::Global,
            b"VARIABLE" if names.is_empty() => Self::Variable,
            b"GLOBAL" | b"VARIABLE" => {
                return Err(CmdError::status(quoted(
                    "given name for ",
                    scope,
                    " scope.",
                )));
            }
            b"TARGET" => Self::Target(single_name(scope, names)?),
            b"CACHE" => Self::Model(PropScope::Cache(single_name(scope, names)?.into())),
            b"INSTALL" => Self::Model(PropScope::Install {
                file: single_name(scope, names)?.into(),
            }),
            b"DIRECTORY" => Self::Model(PropScope::Directory(check_directory(cx, names)?)),
            b"SOURCE" => Self::Source(source_scopes(cx, names, true)?.pop()),
            b"TEST" => {
                let (tests, dir) = test_names(cx, names)?;
                let test = single_name(scope, tests)?;
                Self::Model(PropScope::Test {
                    dir,
                    name: test.into(),
                })
            }
            b"FILE_SET" => {
                let (names, target) = file_set_selection(names)?;
                let set = single_name(scope, names)?;
                if set.is_empty() {
                    return Err(CmdError::status("not given name for FILE_SET scope."));
                }
                Self::FileSet { set, target }
            }
            _ => return Err(CmdError::status(quoted("given invalid scope ", scope, "."))),
        })
    }

    fn value(
        self,
        cx: &mut Cx<'_>,
        name: &[u8],
        inherited: bool,
    ) -> Result<Option<BString>, CmdError> {
        Ok(match self {
            Self::Global => global_value(cx, name),
            Self::Variable => cx.var(name),
            Self::Target(target) => {
                let scope = PropScope::Target(find_target(cx, target)?);
                alias_value(cx, target, name).or_else(|| model_value(cx, &scope, name, inherited))
            }
            Self::FileSet { set, target } => {
                let target_id = file_set_target(cx, target)?;
                let id = file_set_id(cx, target_id, target, set)?;
                file_set_value(cx, id, name, inherited)
            }
            Self::Source(scope) => {
                scope.and_then(|scope| query_source(cx, scope.dir, scope.source, name, inherited))
            }
            Self::Model(PropScope::Directory(dir)) => directory_value(cx, dir, name, inherited),
            Self::Model(scope @ PropScope::Test { .. }) => {
                if let PropScope::Test { dir, name: test } = &scope
                    && cx.project().find_test(*dir, test).is_none()
                {
                    return Err(CmdError::status(quoted(
                        "given TEST name that does not exist: ",
                        test,
                        "",
                    )));
                }
                model_value(cx, &scope, name, inherited)
            }
            Self::Model(scope) => cx.project().get_prop(&scope, name),
        })
    }
}

pub(in crate::policy_misc) fn get(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [out, scope, rest @ ..] = args.as_slice() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let query = PropertyQuery::parse(cx, scope, rest)?;
    if let Some(value) = query.value(cx)? {
        cx.set_var(out, value);
    } else {
        cx.unset_var(out);
    }
    Ok(Flow::Normal)
}

pub(in crate::policy_misc) fn get_cmake(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [out, name, ..] = args else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let dir = cx.current_dir_id();
    let value = match name.value.as_slice() {
        b"VARIABLES" | b"MACROS" | b"CACHE_VARIABLES" => {
            directory_value(cx, dir, &name.value, false)
        }
        b"COMPONENTS" => Some(list::join(cx.project().install_components())),
        _ => global_value(cx, &name.value),
    };
    cx.set_var(&out.value, value.unwrap_or_else(|| "NOTFOUND".into()));
    Ok(Flow::Normal)
}
