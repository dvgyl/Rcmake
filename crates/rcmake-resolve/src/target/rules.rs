//! Instantiate custom rules for members of a target's file sets.

use indexmap::IndexMap;
use rcmake_base::{BString, ByteSlice, list, path_lex::Lexical, truth};
use rcmake_diag::{Location, MessageKind};
use rcmake_model::{
    BT, CustomCommand, CustomCommandKind, DirId, FileSetKind, ProjectModel, PropertyMap,
};

use crate::{
    ResolveError, lower,
    sources::{SourcePaths, absolute_path},
};

pub(crate) fn instantiate_rules(
    model: &ProjectModel,
    host: &dyn rcmake_host::Host,
) -> Result<Option<ProjectModel>, ResolveError> {
    let selected = model
        .targets()
        .flat_map(|(_, target)| {
            target.file_sets.iter().filter_map(move |id| {
                let set = model.file_set(*id)?;
                let name = set.props.get(b"_RCMAKE_RULE")?;
                let dir = model.file_set_rule_dir(*id).unwrap_or(target.dir);
                Some((*id, target, set, name, dir, model.custom_rule(dir, name)?))
            })
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok(None);
    }
    let mut prepared = model.clone();
    let diagnostics = SourcePaths::new(host);
    let mut instances = 0_usize;
    for (id, target, set, name, dir, rule) in selected {
        let directory = model.dir(dir);
        let mut replacements = IndexMap::from_iter([
            ("RULE".into(), name.clone()),
            ("TARGET".into(), target.name.clone()),
            ("FILE_SET".into(), set.name.clone()),
            (
                "SOURCE_DIR".into(),
                model.dir(DirId::ROOT).source_dir.clone(),
            ),
            (
                "BINARY_DIR".into(),
                model.dir(DirId::ROOT).binary_dir.clone(),
            ),
            (
                "OUTPUT_CURRENT_SOURCE_DIR".into(),
                model.dir(target.dir).source_dir.clone(),
            ),
        ]);
        let output_set = rule
            .get(b"OUTPUT_FILE_SET")
            .map_or_else(Vec::new, |value| list::split(value, list::Empty::Keep));
        let [output_name, output_type] = output_set.as_slice() else {
            continue;
        };
        let output_name = expand_rule_patterns(output_name, &replacements, true);
        let output_kind =
            FileSetKind::from_name(output_type).unwrap_or(if output_type == b"SYCL_HEADERS" {
                FileSetKind::Headers
            } else {
                FileSetKind::Sources
            });
        let existing = target.file_sets.iter().copied().find(|&id| {
            model
                .file_set(id)
                .is_some_and(|set| set.name == output_name)
        });
        let invalid = if output_name.contains(&b'<') || output_name.contains(&b'>') {
            Some(format!(
                "Output File Set name, for rule \"{name}\", did not expand correctly:\n  \"{output_name}\""
            ))
        } else {
            existing.and_then(|id| model.file_set(id)).filter(|set| set.kind != output_kind).map(|set| {
                format!("The output file set \"{output_name}\", for the target \"{}\", has the type \"{}\" instead of \"{output_type}\", as specified by the rule \"{name}\".", target.name, set.kind.as_str())
            })
        };
        if let Some(body) = invalid {
            let body = BString::from(body);
            diagnostics.emit(
                model,
                MessageKind::Error,
                Location::FileLine {
                    file: lower::join(&directory.source_dir, b"CMakeLists.txt")
                        .as_slice()
                        .into(),
                    line: 0,
                },
                body.clone(),
            );
            return Err(ResolveError::Reported(body));
        }
        replacements.insert("OUTPUT_FILE_SET".into(), output_name.clone());
        replacements.insert("CURRENT_BINARY_DIR".into(), directory.binary_dir.clone());
        replacements.insert("CURRENT_SOURCE_DIR".into(), directory.source_dir.clone());
        let output_dir = &model.dir(target.dir).binary_dir;
        replacements.insert("OUTPUT_CURRENT_BINARY_DIR".into(), output_dir.clone());
        let mut generated = Vec::new();
        for source in &set.files {
            let absolute = absolute_path(&directory.source_dir, &source.value);
            let lexical = Lexical::new(&absolute);
            let filename = lexical.filename();
            let stem = filename
                .rsplit_once_str(b".")
                .map_or(filename, |(base, _)| base);
            let mut replacements = replacements.clone();
            replacements.extend([
                ("SOURCE".into(), absolute.clone()),
                ("INPUT_DIR".into(), lexical.parent().into()),
                ("FILE_NAME".into(), filename.into()),
                ("BASE_NAME".into(), stem.into()),
            ]);
            let source_props = directory.source_props.get(&absolute);
            for key in [
                b"INCLUDE_DIRECTORIES".as_slice(),
                b"COMPILE_OPTIONS",
                b"COMPILE_DEFINITIONS",
            ] {
                let mut values = [
                    rule.get(key),
                    source_props.and_then(|props| props.get(key)),
                    set.props.get(key),
                ];
                if key == b"INCLUDE_DIRECTORIES" {
                    values.swap(0, 2);
                }
                let values = values
                    .into_iter()
                    .flatten()
                    .filter(|value| !value.is_empty());
                replacements.insert(key.into(), list::join(values));
            }
            let source_key = [name.as_slice(), b"_PATTERNS"].concat();
            let reserved = replacements
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let patterns = set
                .props
                .get(b"RULE_PATTERNS")
                .into_iter()
                .chain(
                    directory
                        .source_props
                        .get(&absolute)
                        .and_then(|props| props.get(&source_key)),
                )
                .chain(set.props.get(b"_RCMAKE_RULE_FILE_SET_PATTERNS"))
                .chain(
                    set.props
                        .get(&[b"_RCMAKE_RULE_PATTERNS_".as_slice(), &absolute].concat()),
                );
            for pattern in patterns.flat_map(|value| list::split(value, list::Empty::Drop)) {
                if let Some((key, value)) = pattern.split_once_str(b"=")
                    && key.len() >= 2
                    && key.first().is_some_and(u8::is_ascii_uppercase)
                    && key
                        .iter()
                        .all(|key| key.is_ascii_uppercase() || key.is_ascii_digit() || *key == b'_')
                    && !reserved.contains(key)
                {
                    replacements.insert(key.into(), value.into());
                }
            }
            // `$<RULE_PROPERTY:<RULE>,prop>` reads the properties of this
            // instance of the rule, with its patterns expanded.
            instances = instances.saturating_add(1);
            let instance: BString = format!("{name}_{instances}").into();
            for value in replacements.values_mut() {
                *value = bind_rule_instance(value, &instance);
            }
            let expand = |value: &[u8]| {
                expand_rule_patterns(&bind_rule_instance(value, &instance), &replacements, false)
            };
            let mut instance_props = PropertyMap::default();
            for key in rule.names() {
                if let Some(value) = rule.get(&key) {
                    let value = if key == b"NAME" {
                        value.clone()
                    } else {
                        expand(value)
                    };
                    instance_props.set(key, value);
                }
            }
            prepared.store_custom_rule(None, instance.clone(), instance_props);
            let words = |key: &[u8]| {
                rule.get(key)
                    .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop))
            };
            let paths = |key: &[u8]| {
                words(key)
                    .iter()
                    .map(|path| {
                        BT::new(
                            crate::sources::absolute_path(&directory.binary_dir, &expand(path)),
                            source.backtrace.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let mut command = CustomCommand::new(
                target.dir,
                CustomCommandKind::Output,
                source.backtrace.clone(),
                target.policies.policies,
            );
            command.outputs = paths(b"OUTPUT");
            command.byproducts = paths(b"BYPRODUCTS");
            command.depends = words(b"DEPENDS")
                .iter()
                .map(|value| BT::new(expand(value), source.backtrace.clone()))
                .collect();
            command
                .depends
                .push(BT::new(absolute, source.backtrace.clone()));
            command.depfile = rule
                .get(b"DEPFILE")
                .filter(|value| !value.is_empty())
                .map(|value| expand(value));
            command.comment = rule.get(b"COMMENT").map(|value| expand(value));
            command.verbatim = rule
                .get(b"VERBATIM")
                .is_some_and(|value| truth::is_on(value));
            command.command_expand_lists = rule
                .get(b"COMMAND_EXPAND_LISTS")
                .is_some_and(|value| truth::is_on(value));
            command.command_lines = (0..)
                .map_while(|index| {
                    rule.get(format!("COMMAND_{index}").as_bytes())
                        .map(|value| {
                            list::split(value, list::Empty::Keep)
                                .iter()
                                .map(|arg| expand(arg))
                                .collect()
                        })
                })
                .collect();
            generated.extend(command.outputs.iter().cloned());
            prepared
                .add_command(command)
                .map_err(|error| ResolveError::Message(error.to_string().into()))?;
        }
        if let Some(input) = prepared.file_set_mut(id) {
            input.files.clear();
            if existing.is_none() {
                input.name = output_name;
                input.kind = output_kind;
                input.base_dirs.clear();
            }
        }
        if let Some(output) = prepared.file_set_mut(existing.unwrap_or(id)) {
            output.files.extend(generated);
            if output.base_dirs.is_empty() {
                output.base_dirs.push(BT::new(
                    directory.binary_dir.clone(),
                    target.backtrace.clone(),
                ));
            }
        }
    }
    Ok(Some(prepared))
}

/// Replace the `<RULE>` pattern in a `$<RULE_PROPERTY:<RULE>,prop>`
/// expression with the name of a rule instance.
fn bind_rule_instance(value: &[u8], instance: &[u8]) -> BString {
    const PATTERN: &[u8] = b"$<RULE_PROPERTY:<RULE>,";
    if value.find(PATTERN).is_none() {
        return value.into();
    }
    let replacement = [b"$<RULE_PROPERTY:".as_slice(), instance, b","].concat();
    value.replace(PATTERN, &replacement).into()
}

fn expand_rule_patterns(
    value: &[u8],
    patterns: &IndexMap<BString, BString>,
    preserve_unknown: bool,
) -> BString {
    let mut value = BString::from(value);
    for _ in 0..patterns.len().min(32) {
        let mut next = value.clone();
        for (key, replacement) in patterns {
            let pattern = [b"<", key.as_slice(), b">"].concat();
            let extra = replacement.len().saturating_sub(pattern.len());
            let growth = next.find_iter(&pattern).count().saturating_mul(extra);
            if next.len().saturating_add(growth) > 65_536 {
                return value;
            }
            next = next.replace(&pattern, replacement).into();
        }
        if value == next {
            break;
        }
        value = next;
    }
    if preserve_unknown {
        return value;
    }
    let mut expanded = BString::default();
    let mut remaining = value.as_slice();
    while let Some((before, after)) = remaining.split_once_str(b"<")
        && let Some((name, tail)) = after.split_once_str(b">")
    {
        expanded.extend_from_slice(before);
        let placeholder = !before.ends_with(b"$")
            && !name.is_empty()
            && name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        let (open, close) = if placeholder {
            (b'{', b'}')
        } else {
            (b'<', b'>')
        };
        expanded.push(open);
        expanded.extend_from_slice(name);
        expanded.push(close);
        remaining = tail;
    }
    expanded.extend_from_slice(remaining);
    expanded
}

#[cfg(test)]
mod tests {
    use super::bind_rule_instance;

    #[test]
    fn rule_property_expressions_bind_to_the_rule_instance() {
        assert_eq!(
            bind_rule_instance(b"NAME=$<RULE_PROPERTY:<RULE>,NAME>;<RULE>", b"gen_3"),
            "NAME=$<RULE_PROPERTY:gen_3,NAME>;<RULE>"
        );
        assert_eq!(bind_rule_instance(b"<RULE>.txt", b"gen_3"), "<RULE>.txt");
    }
}
