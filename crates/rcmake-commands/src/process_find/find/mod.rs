use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Follow, path};
use rcmake_model::{CacheEntry, CacheType};

use super::search_paths::setting_enabled;
use super::{diagnostic, text};
use directories::{directories, normalize};
use options::Options;
use search::Search;

mod directories;
mod options;
mod search;

#[derive(Clone, Copy)]
enum Kind {
    File,
    Path,
    Program,
    Library,
}

impl Kind {
    fn command(self) -> &'static str {
        match self {
            Self::File => "find_file",
            Self::Path => "find_path",
            Self::Program => "find_program",
            Self::Library => "find_library",
        }
    }

    fn cache_type(self) -> CacheType {
        match self {
            Self::Path => CacheType::Path,
            _ => CacheType::Filepath,
        }
    }

    fn help(self) -> BString {
        match self {
            Self::File | Self::Path => "Path to a file.",
            Self::Program => "Path to a program.",
            Self::Library => "Path to a library.",
        }
        .into()
    }

    fn root_mode_setting(self) -> &'static [u8] {
        match self {
            Self::File | Self::Path => b"CMAKE_FIND_ROOT_PATH_MODE_INCLUDE",
            Self::Program => b"CMAKE_FIND_ROOT_PATH_MODE_PROGRAM",
            Self::Library => b"CMAKE_FIND_ROOT_PATH_MODE_LIBRARY",
        }
    }
}

pub(super) fn find_file(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    find(cx, args, Kind::File)
}

pub(super) fn find_path(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    find(cx, args, Kind::Path)
}

pub(super) fn find_program(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    find(cx, args, Kind::Program)
}

pub(super) fn find_library(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    find(cx, args, Kind::Library)
}

fn find(cx: &mut Cx<'_>, args: &[ExpandedArg], kind: Kind) -> CmdResult {
    let Some((var, rest)) = args.split_first().filter(|(_, rest)| !rest.is_empty()) else {
        return Err(CmdError::Status(
            "called with incorrect number of arguments".into(),
        ));
    };
    let var = &var.value;
    let options = Options::parse(cx, rest)?;
    let had_normal = cx.var_normal_only(var).is_some();
    if cx
        .var(var)
        .is_some_and(|v| v.as_slice() != b"NOTFOUND" && !v.ends_with(b"-NOTFOUND"))
    {
        use_existing(cx, kind, var, &options);
        return Ok(Flow::Normal);
    }

    if options.names_per_dir && matches!(kind, Kind::File | Kind::Path) {
        return Err(CmdError::Status("does not support NAMES_PER_DIR".into()));
    }
    if options.optional && options.required {
        return Err(CmdError::Status(
            "cannot be both REQUIRED and OPTIONAL".into(),
        ));
    }

    let directories = directories(cx, kind, &options);
    let mut search = Search::new(cx, kind, &options, var);
    let found = search.run(cx, directories)?;
    search.log(cx, var);
    let mut value = found
        .as_deref()
        .map_or_else(|| text(&[var, b"-NOTFOUND"]), path::from_native);
    if found.is_some() && options.no_cache && matches!(kind, Kind::Path) && !value.ends_with(b"/") {
        value.push(b'/');
    }
    if options.no_cache || had_normal {
        cx.set_var(var, value.clone());
    }
    if !options.no_cache {
        let entry = cx.cache().get(var).cloned();
        cx.cache().insert(
            var.clone(),
            CacheEntry {
                value,
                ty: kind.cache_type(),
                help: options.doc.unwrap_or_else(|| kind.help()),
                advanced: entry.as_ref().is_some_and(|entry| entry.advanced),
                props: entry.map_or_else(Default::default, |entry| entry.props),
            },
        );
    }
    if found.is_none()
        && (options.required || !options.optional && cx.var_is_on(b"CMAKE_FIND_REQUIRED"))
    {
        let what: &[u8] = match kind {
            Kind::File | Kind::Path => b"files",
            _ => b"names",
        };
        let mut names = Vec::new();
        for (index, name) in options.names.iter().enumerate() {
            names.extend_from_slice(if index == 0 { b" " } else { b", " });
            names.extend_from_slice(name);
        }
        return Err(diagnostic(
            cx,
            text(&[
                b"Could not find ",
                var,
                b" using the following ",
                what,
                b":",
                &names,
            ]),
        ));
    }
    Ok(Flow::Normal)
}

fn use_existing(cx: &mut Cx<'_>, kind: Kind, var: &[u8], options: &Options) {
    let new = cx.policy(PolicyId::new(125)) == PolicyStatus::New;
    let normal = cx.var_normal_only(var);
    let cached = cx.cache().get(var).cloned();
    let untyped = cached
        .as_ref()
        .is_some_and(|entry| entry.ty == CacheType::Uninitialized);
    let value = if new {
        normal.or_else(|| cached.as_ref().map(|entry| entry.value.clone()))
    } else {
        cached
            .as_ref()
            .filter(|_| untyped || options.no_cache)
            .map(|entry| entry.value.clone())
            .or(normal)
    };
    let Some(mut value) = value else { return };
    if new && !value.is_empty() && !path::to_native(&value).is_absolute() {
        let relative = cx.current_binary_dir().join(path::to_native(&value));
        if cx.host().metadata(&relative, Follow::Yes).is_ok() {
            value = path::from_native(&normalize(&relative));
        }
    }
    if options.no_cache {
        cx.set_var(var, value);
    } else if let Some(entry) = cx.cache().get_mut(var)
        && untyped
    {
        entry.ty = kind.cache_type();
        entry.help = options.doc.clone().unwrap_or_else(|| kind.help());
        entry.value = value;
    }
}

fn use_default(cx: &Cx<'_>, disabled: bool, setting: &[u8]) -> bool {
    !disabled && setting_enabled(cx, setting)
}
