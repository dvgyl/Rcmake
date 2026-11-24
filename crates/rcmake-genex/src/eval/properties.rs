use rcmake_base::{BString, list};

use super::{
    Call, Evaluator, Value, boolean,
    targets::{DAGChecker, DAGEntry, FrameKind},
};
use crate::{EvalContext, GenexError, TargetHandle, parse};

impl Evaluator<'_> {
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

pub(super) fn validate_property(call: Call<'_>, name: &[u8]) -> Result<(), GenexError> {
    if name
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_:".contains(byte))
    {
        Ok(())
    } else {
        Err(call.error("Property name not supported."))
    }
}
