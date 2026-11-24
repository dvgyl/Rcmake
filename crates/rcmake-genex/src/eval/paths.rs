use rcmake_base::{
    BString, list,
    path_lex::{self, Lexical},
};

use super::{Call, Evaluator, Value, boolean, operations::operation_arity};

impl Evaluator<'_> {
    pub(super) fn path(&mut self, call: Call<'_>) -> Value {
        if call.name == b"SHELL_PATH" {
            let [value] = self.fixed(call)?;
            let mut out = BString::default();
            let paths = split_paths(&value);
            for (index, path) in paths.iter().enumerate() {
                if !Lexical::new(path).absolute() {
                    return Err(call.error(format!("\"{path}\" is not an absolute path.")));
                }
                if index != 0 {
                    self.append(&mut out, &[path_lex::NATIVE_LIST_SEPARATOR])?;
                }
                self.append(&mut out, &path_lex::native_style(path))?;
            }
            return Ok(out);
        }

        let args = self.arguments(call)?;
        if call.params() < 2 {
            return Err(call.error("$<PATH> expression requires at least two parameters."));
        }
        let [op, rest @ ..] = args.as_slice() else {
            return Err(call.error("$<PATH> expression requires at least two parameters."));
        };
        let component = path_component(op);
        let (count, allowed_modifier) = match op.as_slice() {
            b"IS_PREFIX" | b"ABSOLUTE_PATH" => (2, Some("NORMALIZE")),
            b"CMAKE_PATH" | b"NATIVE_PATH" => (1, Some("NORMALIZE")),
            b"GET_EXTENSION" | b"GET_STEM" | b"REMOVE_EXTENSION" => (1, Some("LAST_ONLY")),
            b"REPLACE_EXTENSION" => (2, Some("LAST_ONLY")),
            b"REPLACE_FILENAME" | b"RELATIVE_PATH" => (2, None),
            b"IS_ABSOLUTE" | b"IS_RELATIVE" | b"APPEND" | b"NORMAL_PATH" | b"REMOVE_FILENAME" => {
                (1, None)
            }
            _ if component.is_some() => (1, None),
            _ => return Err(call.error(format!("{op}: invalid option."))),
        };
        let (modifier, rest) = match (allowed_modifier, rest) {
            (Some(modifier), [item, rest @ ..]) if item == modifier => (Some(modifier), rest),
            _ => (None, rest),
        };
        let variadic = op == "APPEND";
        let arity = || {
            operation_arity(
                call,
                op,
                modifier,
                if variadic { "at least" } else { "exactly" },
                count,
            )
        };
        if rest.len() < count || (!variadic && rest.len() > count) {
            return Err(arity());
        }
        let [path, tail @ ..] = rest else {
            return Err(arity());
        };
        let option = modifier.is_some();
        let lexical = Lexical::new(path);
        if let Some(component) = component.filter(|_| op.starts_with(b"HAS_")) {
            let has = component_value(lexical, component, false)
                .ok_or_else(|| call.error(format!("{op}: invalid option.")))?;
            return Ok(boolean(!has.is_empty()));
        }
        match op.as_slice() {
            b"IS_ABSOLUTE" | b"IS_RELATIVE" => {
                return Ok(boolean(lexical.absolute() == (op == "IS_ABSOLUTE")));
            }
            b"IS_PREFIX" => {
                let [input] = tail else {
                    return Err(arity());
                };
                if path.is_empty() {
                    self.flags.context_sensitive = true;
                    return Ok(boolean(
                        self.host.directory_policy(b"CMP0223", call.ctx) == Some(false),
                    ));
                }
                return Ok(boolean(if option {
                    Lexical::new(&lexical.normalize())
                        .prefix(Lexical::new(&Lexical::new(input).normalize()))
                } else {
                    lexical.prefix(Lexical::new(input))
                }));
            }
            _ => {}
        }

        let replacement = tail
            .first()
            .map_or(b"".as_slice(), |value| value.as_slice());
        let paths = split_paths(path);
        let mut out = BString::default();
        for (index, item) in paths.iter().enumerate() {
            let path = Lexical::new(item);
            let transformed = match op.as_slice() {
                name if name.starts_with(b"GET_") => {
                    let component = component.unwrap_or_default();
                    component_value(path, component, option)
                        .ok_or_else(|| call.error(format!("{op}: invalid option.")))?
                }
                b"CMAKE_PATH" => {
                    let cmake = path_lex::cmake_style(item);
                    if option {
                        Lexical::new(&cmake).normalize()
                    } else {
                        cmake
                    }
                }
                b"NATIVE_PATH" => {
                    if option {
                        path_lex::native_style(&path.normalize())
                    } else {
                        path_lex::native_style(item)
                    }
                }
                b"APPEND" => {
                    let mut value = item.clone();
                    for suffix in tail {
                        value = Lexical::new(&value).append(suffix);
                        if value.len() > self.limits.max_value_bytes {
                            return Err(Self::limit_error(
                                "Generator expression exceeds the value size limit.",
                            ));
                        }
                    }
                    value
                }
                b"REMOVE_FILENAME" => path.without_filename().into(),
                b"REPLACE_FILENAME" => {
                    if path.filename().is_empty() {
                        item.clone()
                    } else {
                        Lexical::new(path.without_filename()).append(replacement)
                    }
                }
                b"REMOVE_EXTENSION" | b"REPLACE_EXTENSION" => {
                    let extension = path.stem_extension(option).1;
                    let prefix = item.get(..item.len() - extension.len()).unwrap_or_default();
                    let mut value = BString::from(prefix);
                    if !replacement.is_empty() && !replacement.starts_with(b".") {
                        self.append(&mut value, b".")?;
                    }
                    self.append(&mut value, replacement)?;
                    value
                }
                b"NORMAL_PATH" => path.normalize(),
                b"RELATIVE_PATH" => path.relative(Lexical::new(replacement)).unwrap_or_default(),
                b"ABSOLUTE_PATH" => {
                    let value = if path.absolute() {
                        item.clone()
                    } else {
                        Lexical::new(replacement).append(item)
                    };
                    if option {
                        Lexical::new(&value).normalize()
                    } else {
                        value
                    }
                }
                _ => return Err(call.error(format!("{op}: invalid option."))),
            };
            if index != 0 {
                self.append(&mut out, b";")?;
            }
            self.append(&mut out, &transformed)?;
        }
        Ok(out)
    }
}

fn path_component(operation: &[u8]) -> Option<&[u8]> {
    let component = operation
        .strip_prefix(b"GET_")
        .or_else(|| operation.strip_prefix(b"HAS_"))?;
    matches!(
        component,
        b"ROOT_NAME"
            | b"ROOT_DIRECTORY"
            | b"ROOT_PATH"
            | b"FILENAME"
            | b"EXTENSION"
            | b"STEM"
            | b"RELATIVE_PART"
            | b"PARENT_PATH"
    )
    .then_some(component)
}

fn component_value(path: Lexical<'_>, component: &[u8], last: bool) -> Option<BString> {
    Some(match component {
        b"ROOT_NAME" => path.root_name().into(),
        b"ROOT_DIRECTORY" => path.root_directory().into(),
        b"ROOT_PATH" => path.root_path(),
        b"FILENAME" => path.filename().into(),
        b"EXTENSION" => path.stem_extension(last).1.into(),
        b"STEM" => path.stem_extension(last).0.into(),
        b"RELATIVE_PART" => path.relative_part().into(),
        b"PARENT_PATH" => path.parent().into(),
        _ => return None,
    })
}

fn split_paths(value: &[u8]) -> Vec<BString> {
    let mut paths = Vec::new();
    list::split_with_empty_input(
        value,
        list::Empty::Keep,
        list::EmptyInput::OneEmptyElement,
        &mut paths,
    );
    paths
}
