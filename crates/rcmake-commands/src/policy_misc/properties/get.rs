use rcmake_base::{BString, list, truth};
use rcmake_diag::{Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Follow, path};
use rcmake_model::{DirId, PropScope, TargetKind};
use rcmake_resolve::usage::UsageResolver;

use super::computed::{boolean, directory_value, global_value, source_value};
use super::file_set::{file_set_id, file_set_selection, file_set_target, file_set_value};
use super::scope::{
    RESOLVED_SOURCE_LOCATION, SOURCE_EXTENSIONS, SourceSelection, alias_value, check_directory,
    find_target, select_source, single_name, source_directory, source_paths, source_scopes,
    test_names,
};
use super::{is_inherited, model_value, quoted, values};

fn target_location(
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
    let (dir, name, scoped) = match options {
        [option @ (b"DIRECTORY" | b"TARGET_DIRECTORY"), value, name] => {
            (source_directory(cx, option, value)?, *name, true)
        }
        [name] => (cx.current_dir_id(), *name, false),
        _ => {
            return Err(CmdError::status(
                "called with incorrect number of arguments",
            ));
        }
    };
    let inherited = is_inherited(cx, b"SOURCE", name);
    let value = query_source(
        cx,
        SourceSelection {
            dir,
            source,
            scoped,
        },
        name,
        inherited,
        false,
    )
    .unwrap_or_else(|| "NOTFOUND".into());
    cx.set_var(out, value);
    Ok(Flow::Normal)
}

fn query_source(
    cx: &mut Cx<'_>,
    selection: SourceSelection<'_>,
    name: &[u8],
    inherited: bool,
    create: bool,
) -> Option<BString> {
    let SourceSelection {
        dir,
        source,
        scoped,
    } = selection;
    if name == b"GENERATED"
        && let Some(value) = super::generated::global_value(cx, dir, source)
    {
        return Some(value);
    }
    let scope = select_source(cx, dir, source, scoped, create || name == b"LOCATION")?;
    if name == b"LOCATION"
        && let PropScope::Source { path: key, .. } = &scope
    {
        if let Some(path) = cx.project().get_prop(&scope, RESOLVED_SOURCE_LOCATION) {
            return Some(path);
        }
        let path = source_location(cx, dir, source, key, scoped)?;
        cx.project()
            .dir_mut(dir)
            .source_props
            .entry(key.clone())
            .or_default()
            .set(RESOLVED_SOURCE_LOCATION.into(), path.clone());
        return Some(path);
    }
    if name == b"GENERATED"
        && super::generated::is_strict(cx, dir)
        && let PropScope::Source { path, .. } = &scope
    {
        return Some(boolean(
            cx.project().dir(dir).generated_sources.contains(path),
        ));
    }
    source_value(cx, &scope, name, inherited)
}

fn source_location(
    cx: &mut Cx<'_>,
    dir: DirId,
    source: &[u8],
    known: &BString,
    scoped: bool,
) -> Option<BString> {
    let [source_path, binary_path] = source_paths(cx, source);
    let paths = std::iter::once(&source_path).chain((!scoped).then_some(&binary_path));
    let is_global = super::generated::is_strict(cx, dir);
    let is_generated = |cx: &mut Cx<'_>, path: &BString| {
        let model = cx.project();
        model.dir(dir).generated_sources.contains(path)
            || is_global && model.is_globally_generated(path)
    };
    let source_props = cx.project().dir(dir).source_props.get(known);
    if source_props
        .and_then(|props| props.get(b"GENERATED"))
        .is_some_and(|value| truth::is_on(value))
    {
        return Some(if scoped { source_path } else { binary_path });
    }
    let policy = PolicyId::new(115);
    let status = super::generated::policy(cx, dir, policy);
    let accept = |cx: &mut Cx<'_>, path: &BString| {
        is_generated(cx, path)
            || cx
                .host()
                .metadata(&path::to_native(path), Follow::Yes)
                .is_ok()
    };
    for path in paths {
        if accept(cx, path) {
            return Some(path.clone());
        }
        if status != PolicyStatus::New {
            for extension in SOURCE_EXTENSIONS.split_ascii_whitespace() {
                let candidate: BString = [path.as_slice(), b".", extension.as_bytes()]
                    .concat()
                    .into();
                if accept(cx, &candidate) {
                    if status == PolicyStatus::Warn {
                        let detail = [b"File:\n\n  ", candidate.as_slice()].concat();
                        cx.issue_policy_warning(policy, b"", &detail);
                    }
                    return Some(candidate);
                }
            }
        }
    }
    let mut body: BString = [b"Cannot find source file:\n\n  ", source].concat().into();
    if status != PolicyStatus::New {
        body.extend_from_slice(b"\n\nTried extensions");
        for extension in SOURCE_EXTENSIONS.split_ascii_whitespace() {
            body.extend_from_slice(b" .");
            body.extend_from_slice(extension.as_bytes());
        }
    }
    cx.issue(MessageKind::Error, Effect::SetError, None, body);
    None
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
    Rule(&'a [u8]),
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
            b"RULE" => Self::Rule(single_name(scope, names)?),
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
            Self::Rule(rule) => {
                let dir = cx.current_dir_id();
                let properties = cx.project().custom_rule(dir, rule).ok_or_else(|| {
                    CmdError::status(format!(
                        "could not find RULE {}.  Perhaps it has not yet been created.",
                        BString::from(rule)
                    ))
                })?;
                if let Some(index) = super::rule::command_index(name) {
                    let index = std::str::from_utf8(index)
                        .ok()
                        .and_then(|index| index.parse::<usize>().ok());
                    Some(
                        index
                            .and_then(|index| properties.get(format!("COMMAND_{index}").as_bytes()))
                            .cloned()
                            .unwrap_or_else(|| "NOTFOUND".into()),
                    )
                } else {
                    properties.get(name).cloned()
                }
            }
            Self::Target(target) => {
                let id = find_target(cx, target)?;
                if let Some(value) = target_location(cx, id, name) {
                    return Ok(value);
                }
                if name == b"MANUALLY_ADDED_DEPENDENCIES" {
                    return Ok(cx.project().target(id).and_then(super::manual_dependencies));
                }
                let scope = PropScope::Target(id);
                alias_value(cx, target, name).or_else(|| model_value(cx, &scope, name, inherited))
            }
            Self::FileSet { set, target } => {
                let target_id = file_set_target(cx, target)?;
                let id = file_set_id(cx, target_id, target, set)?;
                file_set_value(cx, id, name, inherited)
            }
            Self::Source(scope) => {
                scope.and_then(|scope| query_source(cx, scope, name, inherited, true))
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
        b"COMPONENTS" => Some(list::join(crate::install::registered(cx.project()))),
        _ => global_value(cx, &name.value),
    };
    cx.set_var(&out.value, value.unwrap_or_else(|| "NOTFOUND".into()));
    Ok(Flow::Normal)
}
