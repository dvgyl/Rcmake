use std::path::Path;

use rcmake_base::{BString, list};
use rcmake_diag::{RenderOptions, format_paragraphs};
use rcmake_gen_common::GenError;
use rcmake_genex::ConfigId;

use crate::generation_error;
use crate::graph::{Graph, qualified};
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
        let Some(first) = configs.first() else {
            return Err(generation_error(
                build_dir,
                "Ninja Multi-Config requires at least one configuration.",
            ));
        };
        let index = |name: &[u8]| configs.iter().position(|config| config.0 == name);
        let requested = setting(b"CMAKE_DEFAULT_BUILD_TYPE").filter(|value| !value.is_empty());
        let default_name = requested.as_ref().unwrap_or(&first.0);
        let default = index(default_name).ok_or_else(|| config_error(format!(
            "The configuration specified by CMAKE_DEFAULT_BUILD_TYPE ({default_name}) is not present in CMAKE_CONFIGURATION_TYPES"
        )))?;
        let sorted = |mut selection: Vec<usize>| {
            selection.sort_unstable_by_key(|&index| configs.get(index).map(|config| &config.0));
            selection
        };
        let all = sorted((0..configs.len()).collect());
        let cross_setting = setting(b"CMAKE_CROSS_CONFIGS").unwrap_or_default();
        let cross = sorted(parse(&cross_setting, &all, index).ok_or_else(|| {
            config_error("CMAKE_CROSS_CONFIGS is not a subset of CMAKE_CONFIGURATION_TYPES")
        })?);
        let defaults_setting = setting(b"CMAKE_DEFAULT_CONFIGS").filter(|value| !value.is_empty());
        let defaults_name = defaults_setting.as_ref().unwrap_or(default_name);
        if defaults_name != default_name && (default_name.is_empty() || cross.is_empty()) {
            return Err(config_error(
                "CMAKE_DEFAULT_CONFIGS cannot be used without CMAKE_DEFAULT_BUILD_TYPE or CMAKE_CROSS_CONFIGS",
            ));
        }
        let defaults = sorted(
            parse(defaults_name, &cross, |name| {
                index(name).filter(|config| *config == default || cross.contains(config))
            })
            .ok_or_else(|| {
                config_error("CMAKE_DEFAULT_CONFIGS is not a subset of CMAKE_CROSS_CONFIGS")
            })?,
        );
        Ok(Self {
            default,
            cross,
            defaults,
        })
    }
}

fn config_error(message: impl AsRef<str>) -> GenError {
    let body = format_paragraphs(
        message.as_ref().as_bytes(),
        2,
        RenderOptions::default().wrap_width,
    );
    GenError::Invalid(
        [b"CMake Error:\n".as_slice(), &body, b"\n\n"]
            .concat()
            .into(),
    )
}

fn parse(
    value: &[u8],
    all: &[usize],
    index: impl Fn(&[u8]) -> Option<usize>,
) -> Option<Vec<usize>> {
    let names = list::split(value, list::Empty::Drop);
    if matches!(names.as_slice(), [name] if name == "all") {
        return Some(all.to_vec());
    }
    let mut selected = Vec::new();
    for name in names {
        if name == "all" {
            return None;
        }
        let index = index(&name)?;
        if !selected.contains(&index) {
            selected.push(index);
        }
    }
    Some(selected)
}

pub(crate) fn emit_clean<'a>(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    implementation: &[u8],
    included: impl Iterator<Item = &'a [u8]>,
    cross: impl Iterator<Item = &'a [u8]>,
) {
    let mut base = b"ninja -f ".to_vec();
    shell_word(&graph.node(implementation), &mut base);
    base.extend_from_slice(b" -t clean");
    let mut emit = |name, configs: &mut dyn Iterator<Item = &'a [u8]>| {
        let mut command = base.clone();
        for config in configs {
            command.push(b' ');
            shell_word(
                &qualified(&graph.node(b"CMakeFiles/rcmake-clean"), Some(config)),
                &mut command,
            );
        }
        ninja.edge(
            b"CLEAN",
            Edge {
                outputs: &[qualified(&graph.node(b"clean"), Some(name))],
                ..Edge::default()
            },
        );
        ninja.binding(b"COMMAND", &command);
        // Default aliases can run multiple clean edges that share outputs.
        ninja.binding(b"pool", b"console");
    };
    for config in included {
        emit(config, &mut std::iter::once(config));
    }
    let mut cross = cross.peekable();
    if cross.peek().is_some() {
        emit(b"all", &mut cross);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_aliases_use_the_cross_set_for_all() -> Result<(), GenError> {
        let configs = [ConfigId("Release".into()), ConfigId("Debug".into())];
        let selection = Selection::new(&configs, Path::new("/build"), |name| match name {
            b"CMAKE_DEFAULT_BUILD_TYPE" => Some("".into()),
            b"CMAKE_CROSS_CONFIGS" => Some("Debug".into()),
            b"CMAKE_DEFAULT_CONFIGS" => Some("all".into()),
            _ => None,
        })?;
        assert_eq!(selection.default, 0);
        assert_eq!(selection.cross, [1]);
        assert_eq!(selection.defaults, [1]);
        Ok(())
    }

    #[test]
    fn default_alias_must_match_the_entry_manifest_without_cross_configs() {
        let configs = [ConfigId("Release".into()), ConfigId("Debug".into())];
        let select = |name| {
            Selection::new(&configs, Path::new("/build"), |setting| {
                (setting == b"CMAKE_DEFAULT_CONFIGS").then(|| BString::from(name))
            })
        };
        assert!(select("Release").is_ok());
        assert!(select("Debug").is_err());
    }
}
