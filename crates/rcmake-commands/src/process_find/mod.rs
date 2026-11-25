//! Process execution and filesystem search commands.

#![forbid(unsafe_code)]

use std::path::{Component, PathBuf};

use rcmake_base::BString;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdFlags, Cx, Registry};
use search_paths::{cmake_paths, resolved_lexical_path};

mod cps_import;
mod environment;
mod execute;
mod find;
mod find_package;
mod pkg_config;
mod registry_path;
mod search_paths;
#[cfg(test)]
mod tests;
mod user_paths;

pub(crate) fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: true };
    registry.add("execute_process", execute::execute_process, flags);
    registry.add("find_file", find::find_file, flags);
    registry.add("find_path", find::find_path, flags);
    registry.add("find_program", find::find_program, flags);
    registry.add("find_library", find::find_library, flags);
    registry.add("find_package", find_package::find_package, flags);
    registry.add("cmake_pkg_config", pkg_config::run, flags);
}

fn diagnostic(cx: &mut Cx<'_>, text: BString) -> CmdError {
    cx.issue(MessageKind::Error, Effect::Fatal, None, text);
    CmdError::Reported
}

fn text(parts: &[&[u8]]) -> BString {
    BString::from(parts.concat())
}

#[derive(Clone, Copy, Default)]
enum RootMode {
    #[default]
    Default,
    Both,
    Only,
    Never,
}

impl RootMode {
    fn option(arg: &[u8]) -> Option<Self> {
        match arg {
            b"CMAKE_FIND_ROOT_PATH_BOTH" => Some(Self::Both),
            b"ONLY_CMAKE_FIND_ROOT_PATH" => Some(Self::Only),
            b"NO_CMAKE_FIND_ROOT_PATH" => Some(Self::Never),
            _ => None,
        }
    }

    fn resolve(self, cx: &Cx<'_>, setting: &[u8]) -> Self {
        match self {
            Self::Default => match cx.var_or_empty(setting).as_slice() {
                b"ONLY" => Self::Only,
                b"NEVER" => Self::Never,
                _ => Self::Both,
            },
            mode => mode,
        }
    }

    fn reroot(self, cx: &Cx<'_>, paths: Vec<PathBuf>) -> Vec<PathBuf> {
        if matches!(self, Self::Never) {
            return paths;
        }
        let paths_from_var = |key| {
            cmake_paths(cx, key)
                .into_iter()
                .map(|value| resolved_lexical_path(cx, &value))
        };
        let roots: Vec<_> = [
            b"CMAKE_FIND_ROOT_PATH".as_slice(),
            b"CMAKE_SYSROOT_COMPILE",
            b"CMAKE_SYSROOT_LINK",
            b"CMAKE_SYSROOT",
        ]
        .into_iter()
        .flat_map(paths_from_var)
        .collect();
        if roots.is_empty() {
            return paths;
        }
        let staging = paths_from_var(b"CMAKE_STAGING_PREFIX").next();
        let mut dirs = Vec::new();
        for root in roots {
            for path in &paths {
                if path.starts_with(&root)
                    || staging.as_ref().is_some_and(|dir| path.starts_with(dir))
                {
                    dirs.push(path.clone());
                } else {
                    let relative: PathBuf = path
                        .components()
                        .filter(|part| matches!(part, Component::Normal(_)))
                        .collect();
                    dirs.push(root.join(relative));
                }
            }
        }
        if matches!(self, Self::Both) {
            dirs.extend(paths);
        }
        dirs
    }
}

fn is_elf_x32(cx: &Cx<'_>) -> bool {
    cx.var_or_empty(b"CMAKE_INTERNAL_PLATFORM_ABI") == b"ELF X32"
}
