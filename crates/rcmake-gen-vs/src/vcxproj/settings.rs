//! Source item types and the `VS_SETTINGS` metadata of source items.

use std::collections::BTreeMap;

use rcmake_base::{BString, list};
use rcmake_model::Target;
use rcmake_resolve::usage::UsageResolver;

use super::{Context, Kind, Plan, gen_error};
use crate::{GenError, string, xml::is_name};

/// One metadata element of a source item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Setting {
    pub(crate) name: String,
    /// The configuration of the element, or `None` when the value is the
    /// same in all configurations.
    pub(crate) config: Option<String>,
    pub(crate) value: String,
}

/// The settings of each configuration, by metadata name.
type ConfigSettings = BTreeMap<String, BTreeMap<String, String>>;

impl<'a> Context<'a> {
    /// Apply the source properties that select the item type and add
    /// metadata, as CMake does.
    ///
    /// * `VS_TOOL_OVERRIDE` replaces the item type of a source.
    /// * A source that CMake does not compile gets an item type from its
    ///   extension, for example `FXCompile` for `.hlsl` (see [`extra_tool`]).
    /// * `VS_SETTINGS` is a list of `name=value` metadata. It can use
    ///   generator expressions, so each configuration has its own value.
    /// * `VS_SOURCE_SETTINGS_<tool>` of the target adds metadata to each item
    ///   of that type that CMake does not compile.
    ///
    /// An item type or a metadata name that is not a valid XML name would
    /// make the project file invalid. Such a value is an error in
    /// [`Plan::errors`], and the generator does not use it.
    pub(super) fn source_settings(
        &self,
        plan: &mut Plan,
        target: &Target,
        usage: &mut Option<UsageResolver<'a>>,
    ) -> Result<(), GenError> {
        let dir = self.model.dir(target.dir);
        let mut tool_settings: BTreeMap<String, ConfigSettings> = BTreeMap::new();
        let mut errors = Vec::new();
        for item in plan.items.values_mut() {
            if item.synthetic || item.kind == Kind::Custom {
                continue;
            }
            let props = dir.source_props.get(item.path.as_bytes());
            let property = |key: &[u8]| props.and_then(|props| props.get(key));
            let mut extra = item.kind == Kind::Other;
            if let Some(tool) = property(b"VS_TOOL_OVERRIDE").filter(|value| !value.is_empty()) {
                let tool = string(tool);
                if is_name(&tool) {
                    item.tool = Some(tool);
                    extra = true;
                } else {
                    errors.push(format!(
                        "VS_TOOL_OVERRIDE value \"{tool}\" of source \"{}\" is not a valid MSBuild item type.",
                        item.path
                    ));
                }
            }
            if item.tool.is_none() && extra {
                item.tool = extra_tool(&item.path).map(str::to_owned);
            }
            let mut settings = ConfigSettings::new();
            for config in &self.settings.configs {
                settings.entry(config.clone()).or_default();
            }
            if let Some(value) = property(b"VS_SETTINGS") {
                let origin = format!("VS_SETTINGS of source \"{}\"", item.path);
                self.parse_settings(value, &mut settings, usage, (origin.as_str(), &mut errors))?;
            }
            if extra {
                let tool = item.tag().to_owned();
                if !tool_settings.contains_key(&tool) {
                    let mut parsed = ConfigSettings::new();
                    let key = format!("VS_SOURCE_SETTINGS_{tool}");
                    if let Some(value) = target.props.get(key.as_bytes()) {
                        let origin = format!("{key} of target \"{}\"", string(&target.name));
                        self.parse_settings(
                            value,
                            &mut parsed,
                            usage,
                            (origin.as_str(), &mut errors),
                        )?;
                    }
                    tool_settings.insert(tool.clone(), parsed);
                }
                for (config, values) in tool_settings.get(&tool).into_iter().flatten() {
                    settings.entry(config.clone()).or_default().extend(
                        values
                            .iter()
                            .map(|(name, value)| (name.clone(), value.clone())),
                    );
                }
            }
            item.settings = finish(&settings);
        }
        for error in errors {
            if !plan.errors.contains(&error) {
                plan.errors.push(error);
            }
        }
        Ok(())
    }

    /// Evaluate a settings list for each configuration and keep the
    /// `name=value` items with a value.
    ///
    /// A name that is not a valid XML name gives an error in `errors`, which
    /// names the property with `origin`.
    fn parse_settings(
        &self,
        value: &BString,
        settings: &mut ConfigSettings,
        usage: &mut Option<UsageResolver<'a>>,
        (origin, errors): (&str, &mut Vec<String>),
    ) -> Result<(), GenError> {
        if value.is_empty() {
            return Ok(());
        }
        let Some(id) = self.project.target else {
            return Ok(());
        };
        let resolver = usage.get_or_insert_with(|| UsageResolver::new(self.model));
        for config in &self.settings.configs {
            // A property name that no target has makes the resolver evaluate
            // the given value in the context of the target.
            let evaluated = resolver
                .target_toolchain_property(id, config.as_bytes(), b"", b"", Some(value))
                .map_err(gen_error)?
                .unwrap_or_default();
            for entry in list::split(&evaluated, list::Empty::Drop) {
                let entry = string(&entry);
                let Some((name, value)) = entry.split_once('=') else {
                    continue;
                };
                if value.is_empty() {
                    continue;
                }
                if !is_name(name) {
                    let error = format!(
                        "{origin} names \"{name}\", which is not a valid MSBuild metadata name."
                    );
                    if !errors.contains(&error) {
                        errors.push(error);
                    }
                    continue;
                }
                settings
                    .entry(config.clone())
                    .or_default()
                    .insert(name.to_owned(), value.to_owned());
            }
        }
        Ok(())
    }
}

impl<'a> Context<'a> {
    /// Assign the source groups whose `source_group(FILES)` paths use
    /// generator expressions.
    ///
    /// As CMake does in each directory before it writes the projects, the
    /// expressions are evaluated without a configuration. A file in such a
    /// group goes in the group. For example, the file `$<$<BOOL:1>:a.h>`
    /// names `a.h`.
    pub(super) fn source_group_genex(
        &self,
        plan: &mut Plan,
        target: &Target,
        usage: &mut Option<UsageResolver<'a>>,
    ) -> Result<(), GenError> {
        let Some(id) = self.project.target else {
            return Ok(());
        };
        let mut pending: Vec<(String, &rcmake_model::SourceGroupDef)> = self
            .model
            .dir(target.dir)
            .source_groups
            .iter()
            .map(|group| (string(&group.name), group))
            .collect();
        let mut groups = BTreeMap::<String, String>::new();
        while let Some((name, group)) = pending.pop() {
            for file in group
                .files
                .iter()
                .filter(|file| file.windows(2).any(|pair| pair == b"$<"))
            {
                let evaluated = usage
                    .get_or_insert_with(|| UsageResolver::new(self.model))
                    .target_toolchain_property(id, b"", b"", b"", Some(file))
                    .map_err(gen_error)?
                    .unwrap_or_default();
                for path in list::split(&evaluated, list::Empty::Drop) {
                    groups.insert(string(&path), name.clone());
                }
            }
            pending.extend(
                group
                    .children
                    .iter()
                    .map(|child| (format!("{name}\\{}", string(&child.name)), child)),
            );
        }
        for item in plan.items.values_mut() {
            if let Some(group) = groups.get(&item.path) {
                item.group.clone_from(group);
            }
        }
        Ok(())
    }
}

/// The item type of a source that CMake does not compile, from its file
/// extension. Other files are `None` items.
fn extra_tool(path: &str) -> Option<&'static str> {
    let extension = path.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match extension.as_str() {
        "hlsl" => "FXCompile",
        "jpg" | "png" => "Image",
        "resw" => "PRIResource",
        "xml" => "XML",
        "natvis" => "Natvis",
        _ => return None,
    })
}

/// Order the metadata elements as CMake writes them: configurations in
/// name order, and one element without a condition for a value that is the
/// same in all configurations.
fn finish(settings: &ConfigSettings) -> Vec<Setting> {
    let mut out = Vec::new();
    let mut written = Vec::<&str>::new();
    let same = |name: &str| {
        let mut values = settings.values().map(|values| values.get(name));
        let first = values.next().flatten();
        first.is_some() && values.all(|value| value == first)
    };
    for (config, values) in settings {
        for (name, value) in values {
            if written.contains(&name.as_str()) {
                continue;
            }
            if same(name) {
                written.push(name);
                out.push(Setting {
                    name: name.clone(),
                    config: None,
                    value: value.clone(),
                });
            } else {
                out.push(Setting {
                    name: name.clone(),
                    config: Some(config.clone()),
                    value: value.clone(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{ConfigSettings, Setting, extra_tool, finish};

    #[test]
    fn values_in_all_configurations_have_no_condition() {
        let mut settings = ConfigSettings::new();
        for config in ["Debug", "Release"] {
            let values = settings.entry(config.into()).or_default();
            values.insert("Shared".into(), "1".into());
        }
        settings
            .entry("Debug".into())
            .or_default()
            .insert("Only".into(), "x".into());
        assert_eq!(
            finish(&settings),
            [
                Setting {
                    name: "Only".into(),
                    config: Some("Debug".into()),
                    value: "x".into()
                },
                Setting {
                    name: "Shared".into(),
                    config: None,
                    value: "1".into()
                },
            ]
        );
    }

    #[test]
    fn extensions_select_item_types() {
        assert_eq!(extra_tool("/s/shader.HLSL"), Some("FXCompile"));
        assert_eq!(extra_tool("/s/icon.png"), Some("Image"));
        assert_eq!(extra_tool("/s/notes.txt"), None);
    }
}
