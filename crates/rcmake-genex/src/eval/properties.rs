use rcmake_base::{BString, list};

use super::{
    Call, Evaluator, Value, boolean,
    context::is_link_libraries_property,
    targets::{DAGChecker, DAGEntry, FrameKind, valid_target},
};
use crate::{EvalContext, GenexError, TargetHandle, parse};

const HEAD_PROPERTY_REQUIRED: &str = "$<TARGET_PROPERTY:prop> may only be used with binary targets.  It may not be used with add_custom_command or add_custom_target.  Specify the target to read a property from using the $<TARGET_PROPERTY:tgt,prop> signature instead.";

impl Evaluator<'_> {
    /// Check target property arguments before the evaluator reads the property.
    pub(super) fn validate_target_property_arguments(
        &self,
        call: Call<'_>,
        arguments: &[BString],
    ) -> Result<(), GenexError> {
        let message = match arguments {
            [] => "$<TARGET_PROPERTY> expression requires at least one parameter.",
            [_] if call.ctx.head.is_none() => HEAD_PROPERTY_REQUIRED,
            [target, property] if target.is_empty() => {
                if property.is_empty() {
                    "$<TARGET_PROPERTY:tgt,prop> expression requires a non-empty target name and property name."
                } else {
                    "$<TARGET_PROPERTY:tgt,prop> expression requires a non-empty target name."
                }
            }
            [_] | [_, _] => return Ok(()),
            _ => "$<TARGET_PROPERTY:...> expression requires one or two parameters",
        };
        Err(call.error(message))
    }

    pub(super) fn property(&mut self, call: Call<'_>) -> Value {
        let arguments = self.arguments(call)?;
        let (target, property) = match arguments.as_slice() {
            [property] => {
                self.flags.head_sensitive = true;
                let Some(target) = call.ctx.head else {
                    return Err(call.error(HEAD_PROPERTY_REQUIRED));
                };
                if call
                    .ctx
                    .site
                    .backtrace
                    .top()
                    .and_then(|frame| frame.name.as_deref())
                    .is_some_and(|name| name.eq_ignore_ascii_case(b"add_custom_target"))
                    && self
                        .host
                        .raw_property(target, b"TYPE")
                        .is_some_and(|kind| kind == "UTILITY")
                {
                    return Err(call.error(HEAD_PROPERTY_REQUIRED));
                }
                (target, property)
            }
            [name, property] => {
                if !valid_target(name) {
                    let message = if property.is_empty() || !valid_property(property) {
                        "Target name and property name not supported."
                    } else {
                        "Target name not supported."
                    };
                    return Err(call.error(message));
                }
                let target = self.require_target(call, name)?;
                self.record_lookup(name, target);
                if let Some(value) = self.host.alias_property(name, property, call.ctx) {
                    return Ok(value);
                }
                (target, property)
            }
            _ => {
                return Err(
                    call.error("$<TARGET_PROPERTY:...> expression requires one or two parameters")
                );
            }
        };
        if property.is_empty() {
            return Err(
                call.error("$<TARGET_PROPERTY:...> expression requires a non-empty property name.")
            );
        }
        validate_property(call, property)?;

        let link_evaluation = !self.in_target_expression
            && call
                .ctx
                .site
                .property
                .as_ref()
                .is_some_and(|name| is_link_libraries_property(name));
        if property == b"LINKER_LANGUAGE"
            && (link_evaluation
                || matches!(
                    call.ctx.site.property.as_deref().map(Vec::as_slice),
                    Some(b"SOURCES" | b"INTERFACE_SOURCES")
                ))
            && self
                .host
                .raw_property(target, b"TYPE")
                .is_some_and(|kind| kind == b"STATIC_LIBRARY")
        {
            return Err(call.error(
                "LINKER_LANGUAGE target property can not be used while evaluating link libraries for a static library",
            ));
        }
        let built_in = requires_link_closure(property);
        if link_evaluation && built_in {
            return Err(call.error(
                "$<TARGET_PROPERTY:...> expression in link libraries evaluation depends on target property which is transitive over the link libraries, creating a recursion.",
            ));
        }
        let link_list = matches!(
            property.as_slice(),
            b"LINK_LIBRARIES" | b"INTERFACE_LINK_LIBRARIES"
        );
        let transitive = if link_list {
            self.host.directory_policy(b"CMP0189", call.ctx) != Some(false)
        } else {
            built_in || (!link_evaluation && self.custom_transitive(target, property))
        };
        if !transitive {
            let view = self.host.scalar_property(target, property, call.ctx)?;
            self.flags = self.flags.union(view.flags);
            return Ok(view.value);
        }

        let _active = match DAGChecker::enter(
            Some(target),
            FrameKind::Property,
            property,
            self.limits.max_recursion,
        )? {
            DAGEntry::Entered(active) => active,
            DAGEntry::SelfReference
                if call.ctx.site.property.is_none()
                    || (call.ctx.current == Some(target)
                        && call.ctx.site.property.as_ref() == Some(property)) =>
            {
                let error = call.error(self.self_reference(Some(target)).to_string());
                return Err(if link_list {
                    error
                } else {
                    error.without_location()
                });
            }
            DAGEntry::SelfReference | DAGEntry::CyclicReference => return Ok(BString::default()),
        };
        let context = EvalContext {
            head: call.ctx.head.or(Some(target)),
            current: Some(target),
            ..call.ctx.clone()
        };
        let view = self.host.transitive_property(target, property, &context)?;
        self.flags = self.flags.union(view.flags);
        Ok(view.value)
    }

    fn custom_transitive(&self, target: TargetHandle, property: &[u8]) -> bool {
        let base = property.strip_prefix(b"INTERFACE_").unwrap_or(property);
        [
            b"TRANSITIVE_COMPILE_PROPERTIES".as_slice(),
            b"TRANSITIVE_LINK_PROPERTIES",
        ]
        .iter()
        .filter_map(|key| self.host.raw_property(target, key))
        .any(|names| list::iter(&names, list::Empty::Drop).any(|name| name.as_ref() == base))
    }

    pub(super) fn scoped_property(&mut self, call: Call<'_>) -> Value {
        self.flags.context_sensitive = true;
        match call.name {
            b"SOURCE_EXISTS" => self.source_exists(call),
            b"FILE_SET_EXISTS" => self.file_set_exists(call),
            b"FILE_SET_PROPERTY" => self.file_set_property(call),
            b"GLOBAL_PROPERTY" => {
                let [property] = self.fixed(call)?;
                if property.is_empty() {
                    return Err(call.error(
                        "$<GLOBAL_PROPERTY:property> expression requires a non-empty property name.",
                    ));
                }
                validate_property(call, &property)?;
                Ok(self.host.global_property(&property).unwrap_or_default())
            }
            _ => {
                let [directory, property] = self.fixed(call)?;
                if directory.is_empty() {
                    return Err(call.error(
                        "$<DIRECTORY_PROPERTY:directory,property> expression requires a non-empty directory name.",
                    ));
                }
                if property.is_empty() {
                    return Err(call.error(
                        "$<DIRECTORY_PROPERTY:directory,property> expression requires a non-empty property name.",
                    ));
                }
                validate_property(call, &property)?;
                self.host
                    .directory_property(&directory, &property, call.ctx)
                    .map(Option::unwrap_or_default)
                    .map_err(|error| call.error(error))
            }
        }
    }

    fn source_exists(&mut self, call: Call<'_>) -> Value {
        let arguments = self.arguments(call)?;
        let (source, option) = match arguments.as_slice() {
            [source] => (source, None),
            [source, option] => (source, Some(option)),
            [] => {
                return Err(
                    call.error("$<SOURCE_EXISTS> expression requires at least one parameter.")
                );
            }
            _ => {
                return Err(
                    call.error("$<SOURCE_EXISTS:...> expression requires at most two parameters.")
                );
            }
        };
        if source.is_empty() {
            return Err(
                call.error("$<SOURCE_EXISTS:src> expression requires a non-empty source name.")
            );
        }
        let scope = option
            .map(|option| source_scope(call, option))
            .transpose()?;
        self.host
            .source_exists(source, scope, call.ctx)
            .map(boolean)
            .map_err(|error| call.error(error))
    }

    fn file_set_exists(&mut self, call: Call<'_>) -> Value {
        let [name, option] = self.fixed(call)?;
        if name.is_empty() {
            return Err(call.error(
                "$<FILE_SET_EXISTS:fileset,TARGET:tgt> expression requires a non-empty FILE_SET name.",
            ));
        }
        let target = self.file_set_target(call, &option)?;
        Ok(boolean(self.host.file_set_exists(target, &name)))
    }

    fn file_set_property(&mut self, call: Call<'_>) -> Value {
        let [name, option, property] = self.fixed(call)?;
        let missing = match (name.is_empty(), property.is_empty()) {
            (true, true) => Some("FILE_SET name and property name"),
            (true, false) => Some("FILE_SET name"),
            (false, true) => Some("property name"),
            (false, false) => None,
        };
        if let Some(missing) = missing {
            return Err(call.error(format!(
                "$<FILE_SET_PROPERTY:fileset,TARGET:tgt,prop> expression requires a non-empty {missing}."
            )));
        }
        validate_property(call, &property)?;
        let target = self.file_set_target(call, &option)?;
        if !self.host.file_set_exists(target, &name) {
            return Err(call.error(format!("FILE_SET \"{name}\" is not known from CMake.")));
        }
        let value = self
            .host
            .file_set_property(target, &name, &property)
            .unwrap_or_default();
        if !matches!(
            property.as_slice(),
            b"BASE_DIRS"
                | b"SOURCES"
                | b"INCLUDE_DIRECTORIES"
                | b"COMPILE_OPTIONS"
                | b"COMPILE_DEFINITIONS"
                | b"INTERFACE_SOURCES"
                | b"INTERFACE_INCLUDE_DIRECTORIES"
                | b"INTERFACE_COMPILE_OPTIONS"
                | b"INTERFACE_COMPILE_DEFINITIONS"
        ) {
            return Ok(value);
        }
        let _active = match DAGChecker::enter(
            Some(target),
            FrameKind::Property,
            &property,
            self.limits.max_recursion,
        )? {
            DAGEntry::Entered(active) => active,
            DAGEntry::SelfReference => {
                return Err(call.error(self.self_reference(Some(target)).to_string()));
            }
            DAGEntry::CyclicReference => return Ok(BString::default()),
        };
        let context = EvalContext {
            current: Some(target),
            head: Some(target),
            ..call.ctx.clone()
        };
        let value = self.nodes(&parse(&value)?.nodes, &context)?;
        Ok(list::join(list::split(&value, list::Empty::Drop)))
    }

    fn file_set_target(
        &mut self,
        call: Call<'_>,
        option: &[u8],
    ) -> Result<TargetHandle, GenexError> {
        let Some(name) = option.strip_prefix(b"TARGET:") else {
            return Err(call.error("Invalid option.  TARGET: expected."));
        };
        if name.is_empty() {
            return Err(call.error("No value provided for the TARGET: option."));
        }
        self.host
            .find_target_in_context(name, call.ctx)
            .ok_or_else(|| call.error(format!("Non-existent target: {}", BString::from(name))))
    }
}

pub(super) fn source_scope<'a>(
    call: Call<'_>,
    option: &'a [u8],
) -> Result<(&'static [u8], &'a [u8]), GenexError> {
    for (key, prefix) in [
        (b"DIRECTORY".as_slice(), b"DIRECTORY:".as_slice()),
        (
            b"TARGET_DIRECTORY".as_slice(),
            b"TARGET_DIRECTORY:".as_slice(),
        ),
    ] {
        if let Some(value) = option.strip_prefix(prefix) {
            if value.is_empty() {
                return Err(call.error(format!(
                    "No value provided for the {} option.",
                    BString::from(prefix)
                )));
            }
            return Ok((key, value));
        }
    }
    Err(call.error("Invalid option.  DIRECTORY: or TARGET_DIRECTORY: expected."))
}

fn requires_link_closure(property: &[u8]) -> bool {
    // Link evaluation checks built-in usage properties. It allows link lists.
    let base = property.strip_prefix(b"INTERFACE_").unwrap_or(property);
    matches!(
        base,
        b"AUTOMOC_MACRO_NAMES"
            | b"AUTOUIC_OPTIONS"
            | b"COMPILE_DEFINITIONS"
            | b"COMPILE_FEATURES"
            | b"COMPILE_OPTIONS"
            | b"COMPILE_WRAPPERS"
            | b"INCLUDE_DIRECTORIES"
            | b"LINK_DEPENDS"
            | b"LINK_DIRECTORIES"
            | b"LINK_OPTIONS"
            | b"LINK_WRAPPERS"
            | b"PRECOMPILE_HEADERS"
            | b"SOURCES"
            | b"SYSTEM_INCLUDE_DIRECTORIES"
    )
}

fn valid_property(name: &[u8]) -> bool {
    name.iter()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_:".contains(byte))
}

pub(super) fn validate_property(call: Call<'_>, name: &[u8]) -> Result<(), GenexError> {
    if valid_property(name) {
        Ok(())
    } else {
        Err(call.error("Property name not supported."))
    }
}
