use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Follow, path};
use rcmake_model::CacheType;

use super::cat;
use super::search_paths::setting_enabled;
use directories::{directories, join_normalized, normalize_owned};
use options::Options;
use search::Search;

mod architecture;
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
    let previous = cx.cache().get(var).cloned();
    let new = cx.policy(PolicyId::new(125)) == PolicyStatus::New;
    let existing = cx.var(var);
    let untyped = previous
        .as_ref()
        .is_some_and(|entry| entry.ty == CacheType::Uninitialized);
    let (ty, help) = previous.as_ref().filter(|_| !untyped).map_or_else(
        || {
            (
                kind.cache_type(),
                options.doc.clone().unwrap_or_else(|| kind.help()),
            )
        },
        |entry| (entry.ty, entry.help.clone()),
    );
    if let Some(existing) = existing
        .as_ref()
        .filter(|v| v.as_slice() != b"NOTFOUND" && !v.ends_with(b"-NOTFOUND"))
    {
        if new {
            let normalized = normalize_existing(cx, existing);
            if options.no_cache {
                cx.set_var(var, normalized);
            } else if normalized != *existing || untyped {
                cx.write_cache(var, normalized.clone(), ty, &help);
                update_normal(cx, var, normalized, false);
            }
        } else if options.no_cache {
            cx.set_var(var, existing.clone());
        } else if let Some(entry) = previous.filter(|_| untyped) {
            let value = cx.cache_paths(&entry.value);
            cx.write_cache(var, value.clone(), ty, &help);
            update_normal(cx, var, value, true);
        }
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

    let directories = directories(cx, kind, &options)?;
    let mut search = Search::new(cx, kind, &options, var);
    let found = search.run(cx, directories)?;
    search.log(cx, var);
    search.log_event(cx, var, found.as_deref(), existing.is_some());
    let mut value = found
        .as_deref()
        .map_or_else(|| cat(&[var, b"-NOTFOUND"]), path::from_native_cmake);
    if found.is_some() && options.no_cache && matches!(kind, Kind::Path) && !value.ends_with(b"/") {
        value.push(b'/');
    }
    if options.no_cache {
        cx.set_var(var, value);
    } else {
        let mut cached = value.clone();
        if let Some(entry) = previous.filter(|_| untyped) {
            if !new {
                cached = entry.value;
            }
            if matches!(ty, CacheType::Path | CacheType::Filepath) {
                cached = cx.cache_paths(&cached);
            }
        }
        cx.write_cache(var, cached, ty, &help);
        update_normal(cx, var, value, true);
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
        return Err(crate::command_error(
            cx,
            cat(&[
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

fn normalize_existing(cx: &Cx<'_>, value: &BString) -> BString {
    if value.is_empty() {
        return value.clone();
    }
    let absolute = normalize_owned(cx.host().initial_cwd().join(path::to_native(value)));
    if cx.host().metadata(&absolute, Follow::Yes).is_ok() {
        path::from_native_cmake(&absolute)
    } else {
        value.clone()
    }
}

fn update_normal(cx: &mut Cx<'_>, name: &[u8], value: BString, warn: bool) {
    if cx.policy(PolicyId::new(126)) == PolicyStatus::New {
        if cx.var_normal_only(name).is_some() {
            cx.set_var(name, value);
        }
    } else if warn {
        cx.clear_cache_shadow(name);
    } else {
        cx.unset_var(name);
    }
}

fn use_default(cx: &Cx<'_>, disabled: bool, setting: &[u8]) -> bool {
    !disabled && setting_enabled(cx, setting)
}
