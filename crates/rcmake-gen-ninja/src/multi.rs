use std::path::Path;

use rcmake_base::{BString, list};
use rcmake_gen_common::GenError;
use rcmake_genex::ConfigId;

use crate::generation_error;
use crate::graph::qualified;
use crate::syntax::{Edge, Ninja, shell_word};

pub(crate) struct Selection {
    pub(crate) default: usize,
    pub(crate) cross: Vec<usize>,
    pub(crate) defaults: Vec<usize>,
}

impl Selection {
    pub(crate) fn new(
        configs: &[ConfigId],
        build_dir: &Path,
        setting: impl Fn(&[u8]) -> Option<BString>,
    ) -> Result<Self, GenError> {
        if configs.is_empty() {
            return Err(generation_error(
                build_dir,
                "Ninja Multi-Config requires at least one configuration.",
            ));
        }
        let index = |name: &[u8]| configs.iter().position(|config| config.0 == name);
        let requested = setting(b"CMAKE_DEFAULT_BUILD_TYPE").filter(|value| !value.is_empty());
        let default = match requested.as_ref() {
            Some(name) => index(name).ok_or_else(|| generation_error(build_dir, format!(
                "The configuration specified by CMAKE_DEFAULT_BUILD_TYPE ({name}) is not present in CMAKE_CONFIGURATION_TYPES"
            )))?,
            None => 0,
        };
        let cross_setting = setting(b"CMAKE_CROSS_CONFIGS");
        let cross = match cross_setting.as_ref().map(|value| value.as_slice()) {
            Some(b"all") => (0..configs.len()).collect(),
            Some(value) => parse(value, index).ok_or_else(|| {
                generation_error(
                    build_dir,
                    "CMAKE_CROSS_CONFIGS is not a subset of CMAKE_CONFIGURATION_TYPES",
                )
            })?,
            None => Vec::new(),
        };
        let defaults = match setting(b"CMAKE_DEFAULT_CONFIGS").filter(|value| !value.is_empty()) {
            Some(value) if cross.is_empty() && (requested.is_none() || value == "all") => {
                return Err(generation_error(
                    build_dir,
                    "CMAKE_DEFAULT_CONFIGS cannot be used without CMAKE_DEFAULT_BUILD_TYPE or CMAKE_CROSS_CONFIGS",
                ));
            }
            Some(value) if value == "all" && !cross.is_empty() => cross.clone(),
            Some(value) => parse(&value, |name| {
                index(name).filter(|config| *config == default || cross.contains(config))
            })
            .ok_or_else(|| {
                generation_error(
                    build_dir,
                    "CMAKE_DEFAULT_CONFIGS is not a subset of CMAKE_CROSS_CONFIGS",
                )
            })?,
            None => vec![default],
        };
        Ok(Self {
            default,
            cross,
            defaults,
        })
    }
}

fn parse(value: &[u8], index: impl Fn(&[u8]) -> Option<usize>) -> Option<Vec<usize>> {
    let mut selected = Vec::new();
    for name in list::split(value, list::Empty::Drop) {
        let index = index(&name)?;
        if !selected.contains(&index) {
            selected.push(index);
        }
    }
    Some(selected)
}

pub(crate) fn emit_clean<'a>(
    ninja: &mut Ninja,
    implementation: &[u8],
    included: impl Iterator<Item = &'a [u8]>,
    cross: impl Iterator<Item = &'a [u8]>,
) {
    let mut base = b"ninja -f ".to_vec();
    shell_word(implementation, &mut base);
    base.extend_from_slice(b" -t clean");
    let mut emit = |name, configs: &mut dyn Iterator<Item = &'a [u8]>| {
        let mut command = base.clone();
        for config in configs {
            command.push(b' ');
            shell_word(
                &qualified(&"CMakeFiles/rcmake-clean".into(), Some(config)),
                &mut command,
            );
        }
        ninja.edge(
            b"CLEAN",
            Edge {
                outputs: &[qualified(&"clean".into(), Some(name))],
                ..Edge::default()
            },
        );
        ninja.binding(b"COMMAND", &command);
    };
    for config in included {
        emit(config, &mut std::iter::once(config));
    }
    let mut cross = cross.peekable();
    if cross.peek().is_some() {
        emit(b"all", &mut cross);
    }
}
