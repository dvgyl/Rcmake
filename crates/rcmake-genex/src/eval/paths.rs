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
        let operation =
            PathOp::parse(op).ok_or_else(|| call.error(format!("{op}: invalid option.")))?;
        let count = operation.arity();
        let allowed_modifier = operation.modifier();
        let (modifier, rest) = match (allowed_modifier, rest) {
            (Some(modifier), [item, rest @ ..]) if item == modifier => (Some(modifier), rest),
            _ => (None, rest),
        };
        let variadic = operation == PathOp::Append;
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
        match operation {
            PathOp::Has(component) => {
                return Ok(boolean(!component.value(lexical, false).is_empty()));
            }
            PathOp::IsAbsolute => return Ok(boolean(lexical.absolute())),
            PathOp::IsRelative => return Ok(boolean(!lexical.absolute())),
            PathOp::IsPrefix => {
                let [input] = tail else {
                    return Err(arity());
                };
                if path.is_empty() {
                    self.flags.context_sensitive = true;
                    let policy = rcmake_diag::PolicyId::new(223);
                    let status = self.host.directory_policy_status(policy, call.ctx);
                    if status == Some(rcmake_diag::PolicyStatus::Warn) {
                        self.host.policy_warning(policy, b"", call.ctx);
                    }
                    return Ok(boolean(matches!(
                        status,
                        Some(rcmake_diag::PolicyStatus::Old | rcmake_diag::PolicyStatus::Warn)
                    )));
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
            let transformed = match operation {
                PathOp::Get(component) => component.value(path, option),
                PathOp::CmakePath => {
                    let cmake = path_lex::cmake_style(item);
                    if option {
                        Lexical::new(&cmake).normalize()
                    } else {
                        cmake
                    }
                }
                PathOp::NativePath => {
                    if option {
                        path_lex::native_style(&path.normalize())
                    } else {
                        path_lex::native_style(item)
                    }
                }
                PathOp::Append => {
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
                PathOp::RemoveFilename => path.without_filename().into(),
                PathOp::ReplaceFilename => {
                    if path.filename().is_empty() {
                        item.clone()
                    } else {
                        Lexical::new(path.without_filename()).append(replacement)
                    }
                }
                PathOp::RemoveExtension | PathOp::ReplaceExtension => {
                    let extension = path.stem_extension(option).1;
                    let prefix = item.get(..item.len() - extension.len()).unwrap_or_default();
                    let mut value = BString::from(prefix);
                    if !replacement.is_empty() && !replacement.starts_with(b".") {
                        self.append(&mut value, b".")?;
                    }
                    self.append(&mut value, replacement)?;
                    value
                }
                PathOp::NormalPath => path.normalize(),
                PathOp::RelativePath => {
                    path.relative(Lexical::new(replacement)).unwrap_or_default()
                }
                PathOp::AbsolutePath => {
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
                PathOp::Has(_) | PathOp::IsAbsolute | PathOp::IsRelative | PathOp::IsPrefix => {
                    unreachable!("Path predicates return before transforming the list");
                }
            };
            if index != 0 {
                self.append(&mut out, b";")?;
            }
            self.append(&mut out, &transformed)?;
        }
        Ok(out)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathOp {
    Get(Component),
    Has(Component),
    IsPrefix,
    IsAbsolute,
    IsRelative,
    CmakePath,
    NativePath,
    Append,
    RemoveFilename,
    ReplaceFilename,
    RemoveExtension,
    ReplaceExtension,
    NormalPath,
    RelativePath,
    AbsolutePath,
}

impl PathOp {
    fn parse(name: &[u8]) -> Option<Self> {
        if let Some(component) = name.strip_prefix(b"GET_") {
            return Component::parse(component).map(Self::Get);
        }
        if let Some(component) = name.strip_prefix(b"HAS_") {
            return Component::parse(component).map(Self::Has);
        }
        Some(match name {
            b"IS_PREFIX" => Self::IsPrefix,
            b"IS_ABSOLUTE" => Self::IsAbsolute,
            b"IS_RELATIVE" => Self::IsRelative,
            b"CMAKE_PATH" => Self::CmakePath,
            b"NATIVE_PATH" => Self::NativePath,
            b"APPEND" => Self::Append,
            b"REMOVE_FILENAME" => Self::RemoveFilename,
            b"REPLACE_FILENAME" => Self::ReplaceFilename,
            b"REMOVE_EXTENSION" => Self::RemoveExtension,
            b"REPLACE_EXTENSION" => Self::ReplaceExtension,
            b"NORMAL_PATH" => Self::NormalPath,
            b"RELATIVE_PATH" => Self::RelativePath,
            b"ABSOLUTE_PATH" => Self::AbsolutePath,
            _ => return None,
        })
    }

    fn arity(self) -> usize {
        match self {
            Self::IsPrefix
            | Self::AbsolutePath
            | Self::ReplaceExtension
            | Self::ReplaceFilename
            | Self::RelativePath => 2,
            _ => 1,
        }
    }

    fn modifier(self) -> Option<&'static str> {
        match self {
            Self::IsPrefix | Self::AbsolutePath | Self::CmakePath | Self::NativePath => {
                Some("NORMALIZE")
            }
            Self::Get(Component::Extension | Component::Stem)
            | Self::RemoveExtension
            | Self::ReplaceExtension => Some("LAST_ONLY"),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Component {
    RootName,
    RootDirectory,
    RootPath,
    Filename,
    Extension,
    Stem,
    RelativePart,
    ParentPath,
}

impl Component {
    fn parse(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"ROOT_NAME" => Self::RootName,
            b"ROOT_DIRECTORY" => Self::RootDirectory,
            b"ROOT_PATH" => Self::RootPath,
            b"FILENAME" => Self::Filename,
            b"EXTENSION" => Self::Extension,
            b"STEM" => Self::Stem,
            b"RELATIVE_PART" => Self::RelativePart,
            b"PARENT_PATH" => Self::ParentPath,
            _ => return None,
        })
    }

    fn value(self, path: Lexical<'_>, last: bool) -> BString {
        match self {
            Self::RootName => path.root_name().into(),
            Self::RootDirectory => path.root_directory().into(),
            Self::RootPath => path.root_path(),
            Self::Filename => path.filename().into(),
            Self::Extension => path.stem_extension(last).1.into(),
            Self::Stem => path.stem_extension(last).0.into(),
            Self::RelativePart => path.relative_part().into(),
            Self::ParentPath => path.parent().into(),
        }
    }
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
