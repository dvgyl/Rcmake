use rcmake_base::{
    BString, list,
    path_lex::{self, Lexical},
};

use super::{Call, Evaluator, Value, boolean};

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
        let [op, rest @ ..] = args.as_slice() else {
            return Err(call.error("$<PATH> expression requires at least one parameter."));
        };
        let arity = |n: usize| {
            let suffix = if n == 1 {
                "one parameter".into()
            } else {
                format!("{n} parameters")
            };
            call.error(format!(
                "$<PATH:{op}> expression requires exactly {suffix}."
            ))
        };
        let (option, rest) = match rest {
            [option, rest @ ..]
                if (option == "NORMALIZE"
                    && matches!(
                        op.as_slice(),
                        b"IS_PREFIX" | b"CMAKE_PATH" | b"NATIVE_PATH" | b"ABSOLUTE_PATH"
                    ))
                    || (option == "LAST_ONLY"
                        && matches!(
                            op.as_slice(),
                            b"GET_EXTENSION"
                                | b"GET_STEM"
                                | b"REMOVE_EXTENSION"
                                | b"REPLACE_EXTENSION"
                        )) =>
            {
                (true, rest)
            }
            _ => (false, rest),
        };

        let [path, tail @ ..] = rest else {
            return Err(arity(1));
        };
        let lexical = Lexical::new(path);
        if let Some(component) = op.strip_prefix(b"HAS_") {
            if !tail.is_empty() {
                return Err(arity(1));
            }
            let has = component_value(lexical, component, false)
                .ok_or_else(|| call.error("Invalid path component."))?;
            return Ok(boolean(!has.is_empty()));
        }
        match op.as_slice() {
            b"IS_ABSOLUTE" | b"IS_RELATIVE" => {
                if !tail.is_empty() {
                    return Err(arity(1));
                }
                return Ok(boolean(lexical.absolute() == (op == "IS_ABSOLUTE")));
            }
            b"IS_PREFIX" => {
                let [input] = tail else {
                    return Err(arity(2));
                };
                return Ok(boolean(
                    !path.is_empty()
                        && if option {
                            Lexical::new(&lexical.normalize())
                                .prefix(Lexical::new(&Lexical::new(input).normalize()))
                        } else {
                            lexical.prefix(Lexical::new(input))
                        },
                ));
            }
            _ => {}
        }

        let replacement = match op.as_slice() {
            b"REPLACE_FILENAME" | b"REPLACE_EXTENSION" | b"ABSOLUTE_PATH" | b"RELATIVE_PATH" => {
                let [value] = tail else {
                    return Err(arity(2));
                };
                value.as_slice()
            }
            b"APPEND" => b"",
            _ => {
                if !tail.is_empty() {
                    return Err(arity(1));
                }
                b""
            }
        };
        let paths = split_paths(path);
        let mut out = BString::default();
        for (index, item) in paths.iter().enumerate() {
            let path = Lexical::new(item);
            let transformed = match op.as_slice() {
                name if name.starts_with(b"GET_") => {
                    let component = name.get(4..).unwrap_or_default();
                    component_value(path, component, option)
                        .ok_or_else(|| call.error("Invalid path component."))?
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
                _ => return Err(call.error("Invalid path operation.")),
            };
            if index != 0 {
                self.append(&mut out, b";")?;
            }
            self.append(&mut out, &transformed)?;
        }
        Ok(out)
    }
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
