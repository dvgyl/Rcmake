use rcmake_base::BString;
use rcmake_eval::{CmdError, Cx, UserCommandKind};
use rcmake_expr::ExpandedArg;

use super::super::search_paths::{BUILD_ENV_SEPARATOR, split_env};
use super::super::{RootMode, text};

#[derive(Default)]
pub(super) struct Options {
    pub(super) names: Vec<BString>,
    pub(super) hints: Vec<BString>,
    pub(super) paths: Vec<BString>,
    pub(super) suffixes: Vec<BString>,
    pub(super) doc: Option<BString>,
    pub(super) no_cache: bool,
    pub(super) required: bool,
    pub(super) optional: bool,
    pub(super) names_per_dir: bool,
    pub(super) no_defaults: bool,
    pub(super) no_package_root: bool,
    pub(super) no_cmake: bool,
    pub(super) no_cmake_env: bool,
    pub(super) no_system_env: bool,
    pub(super) no_system: bool,
    pub(super) no_install: bool,
    pub(super) root_mode: RootMode,
    pub(super) registry_view: Option<BString>,
    pub(super) validator: Option<BString>,
}

#[derive(Clone, Copy)]
enum Section {
    Names,
    Hints,
    Paths,
    Suffixes,
}

impl Options {
    pub(super) fn parse(cx: &Cx<'_>, args: &[ExpandedArg]) -> Result<Self, CmdError> {
        let mut options = Self::default();
        let mut section = Section::Names;
        let mut long_form = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.value.as_slice() {
                b"NAMES" => {
                    section = Section::Names;
                    long_form = true;
                }
                b"HINTS" => {
                    section = Section::Hints;
                    long_form = true;
                }
                b"PATHS" => {
                    section = Section::Paths;
                    long_form = true;
                }
                b"PATH_SUFFIXES" => section = Section::Suffixes,
                b"DOC" => options.doc = args.next().map(|arg| arg.value.clone()),
                b"REGISTRY_VIEW" => {
                    let view = option_value(&mut args, "REGISTRY_VIEW")?;
                    if !super::super::registry_path::valid_view(view) {
                        return Err(CmdError::status(text(&[
                            b"given invalid value for REGISTRY_VIEW: ",
                            view,
                        ])));
                    }
                    options.registry_view = Some(view.into());
                }
                b"VALIDATOR" => {
                    let name = option_value(&mut args, "VALIDATOR")?;
                    if !cx.command_exists(name) {
                        return Err(CmdError::status(text(&[
                            b"command specified for VALIDATOR is undefined: ",
                            name,
                            b".",
                        ])));
                    }
                    if cx.user_command_kind(name) != Some(UserCommandKind::Function) {
                        return Err(CmdError::status(text(&[
                            b"command specified for VALIDATOR is not a function: ",
                            name,
                            b".",
                        ])));
                    }
                    options.validator = Some(name.into());
                }
                b"NO_CMAKE_INSTALL_PREFIX" => options.no_install = true,
                b"NO_CACHE" => options.no_cache = true,
                b"REQUIRED" => options.required = true,
                b"OPTIONAL" => options.optional = true,
                b"NAMES_PER_DIR" => options.names_per_dir = true,
                b"NO_DEFAULT_PATH" => options.no_defaults = true,
                b"NO_PACKAGE_ROOT_PATH" => options.no_package_root = true,
                b"NO_CMAKE_PATH" => options.no_cmake = true,
                b"NO_CMAKE_ENVIRONMENT_PATH" => options.no_cmake_env = true,
                b"NO_SYSTEM_ENVIRONMENT_PATH" => options.no_system_env = true,
                b"NO_CMAKE_SYSTEM_PATH" => options.no_system = true,
                key if let Some(mode) = RootMode::option(key) => options.root_mode = mode,
                b"ENV" if matches!(section, Section::Hints | Section::Paths) => {
                    if let Some(value) = args.next().and_then(|key| cx.host().env_get(&key.value)) {
                        options
                            .section(section)
                            .extend(split_env(&value, BUILD_ENV_SEPARATOR).map(BString::from));
                    }
                }
                _ => options.section(section).push(arg.value.clone()),
            }
        }
        if !long_form && options.names.len() > 1 {
            options.paths.extend(options.names.drain(1..));
        }
        Ok(options)
    }

    fn section(&mut self, section: Section) -> &mut Vec<BString> {
        match section {
            Section::Names => &mut self.names,
            Section::Hints => &mut self.hints,
            Section::Paths => &mut self.paths,
            Section::Suffixes => &mut self.suffixes,
        }
    }
}

fn option_value<'a>(
    args: &mut impl Iterator<Item = &'a ExpandedArg>,
    keyword: &str,
) -> Result<&'a [u8], CmdError> {
    args.next().map(|arg| arg.value.as_slice()).ok_or_else(|| {
        CmdError::status(text(&[
            b"missing required argument for ",
            keyword.as_bytes(),
        ]))
    })
}
