use rcmake_base::{BString, list, truth, version};

use super::{
    Call, Evaluator, Value, boolean,
    targets::{DAGChecker, DAGEntry, valid_target},
    unknown,
};
use crate::{EvalContext, GenexError, UsageTraversal, parse};

const LANGUAGES: &[&[u8]] = &[
    b"C", b"CXX", b"CUDA", b"OBJC", b"OBJCXX", b"Fortran", b"HIP", b"ISPC", b"SYCL",
];

pub(super) fn compiler_operator(name: &[u8]) -> bool {
    LANGUAGES.iter().any(|lang| {
        matches!(
            name.strip_prefix(*lang),
            Some(
                b"_COMPILER_ID"
                    | b"_COMPILER_VERSION"
                    | b"_COMPILER_FRONTEND_VARIANT"
                    | b"_COMPILER_LINKER_ID"
                    | b"_COMPILER_LINKER_FRONTEND_VARIANT"
            )
        )
    })
}

impl Evaluator<'_> {
    pub(super) fn context_value(&mut self, call: Call<'_>) -> Value {
        self.flags.context_sensitive = true;
        let ctx = call.ctx;
        match call.name {
            b"CONFIG" => self.configuration(call),
            b"CONFIGURATION" => {
                let [] = self.fixed(call)?;
                Ok(ctx.config.0.clone())
            }
            b"OUTPUT_CONFIG" | b"COMMAND_CONFIG" => {
                let config = if call.name == b"OUTPUT_CONFIG" {
                    &ctx.output_config
                } else {
                    &ctx.command_config
                };
                let Some(config) = config else {
                    return Err(unknown(call));
                };
                if self.depth != 1 {
                    return Err(unknown(call));
                }
                let switched = EvalContext {
                    config: config.clone(),
                    ..ctx.clone()
                };
                self.content(Call {
                    ctx: &switched,
                    ..call
                })
            }
            b"PLATFORM_ID" => {
                let value = self
                    .host
                    .definition(b"CMAKE_SYSTEM_NAME")
                    .unwrap_or_default();
                self.id_value(call, value)
            }
            b"COMPILE_LANGUAGE"
            | b"LINK_LANGUAGE"
            | b"COMPILE_LANG_AND_ID"
            | b"LINK_LANG_AND_ID" => self.language(call),
            b"COMPILE_FEATURES" => self.compile_features(call),
            _ => Err(unknown(call)),
        }
    }

    fn configuration(&mut self, call: Call<'_>) -> Value {
        if call.params() == 0 {
            return Ok(call.ctx.config.0.clone());
        }
        let values = self.arguments(call)?;
        if values.iter().any(|v| !valid_id(v)) {
            return Err(call.error("Expression syntax not recognized."));
        }
        if let Some(configs) = call.ctx.current.and_then(|target| {
            self.host
                .imported_config_matches(target, call.ctx.head, &call.ctx.config)
        }) {
            return Ok(boolean(configs.iter().any(|config| {
                values
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(config))
            })));
        }
        if values.iter().any(|v| call.ctx.config.matches(v)) {
            return Ok(boolean(true));
        }
        if let Some(target) = call.ctx.current
            && self
                .host
                .raw_property(target, b"IMPORTED")
                .is_some_and(|v| truth::is_on(&v))
        {
            let mut key = b"MAP_IMPORTED_CONFIG_".to_vec();
            key.extend_from_slice(&call.ctx.config.0.to_ascii_uppercase());
            if let Some(mapped) = self.host.raw_property(target, &key) {
                return Ok(boolean(
                    list::split(&mapped, list::Empty::Keep)
                        .iter()
                        .any(|cfg| values.iter().any(|v| cfg.eq_ignore_ascii_case(v))),
                ));
            }
        }
        Ok(boolean(false))
    }

    pub(super) fn compiler(&mut self, call: Call<'_>) -> Value {
        self.flags.context_sensitive = true;
        if call.ctx.head.is_none() {
            return Err(call.error(format!(
                "$<{}> may only be used with binary targets.  It may not be used with add_custom_command or add_custom_target.",
                BString::from(call.name)
            )));
        }
        let mut key = b"CMAKE_".to_vec();
        key.extend_from_slice(call.name);
        let value = self.host.definition(&key).unwrap_or_default();
        if call.name.ends_with(b"_VERSION") {
            return match call.params() {
                0 => Ok(value),
                1 => {
                    let [wanted] = self.fixed(call)?;
                    if !wanted
                        .iter()
                        .all(|byte| byte.is_ascii_digit() || *byte == b'.')
                    {
                        return Err(call.error("Expression syntax not recognized."));
                    }
                    Ok(boolean(if value.is_empty() || wanted.is_empty() {
                        value == wanted
                    } else {
                        version::compare(&value, &wanted).is_eq()
                    }))
                }
                _ => Err(call.error(format!(
                    "$<{}> expression requires one or zero parameters.",
                    BString::from(call.name)
                ))),
            };
        }
        self.id_value(call, value)
    }

    fn id_value(&mut self, call: Call<'_>, value: BString) -> Value {
        if call.params() == 0 {
            return Ok(value);
        }
        let arguments = self.arguments(call)?;
        Ok(boolean(arguments.contains(&value)))
    }

    fn language(&mut self, call: Call<'_>) -> Value {
        let arguments = self.arguments(call)?;
        let and_id = call.name.ends_with(b"_AND_ID");
        if and_id && call.params() < 2 {
            return Err(call.error(format!(
                "$<{}> expression requires at least two parameters.",
                BString::from(call.name)
            )));
        }
        let role = EvaluationRole::of(call.ctx);
        let linking = call.name.starts_with(b"LINK_");
        let language = if linking {
            if call.ctx.head.is_none() || !role.supports_link_language() {
                let expression = if and_id {
                    "LINK_LANG_AND_ID:lang,id"
                } else {
                    "LINK_LANGUAGE:..."
                };
                return Err(call.error(format!("$<{expression}> may only be used with binary targets to specify link libraries, link directories, link options, link depends, and link wrappers.")));
            }
            if role == EvaluationRole::LinkLibraries {
                if arguments.is_empty() {
                    return Err(call
                        .error("$<LINK_LANGUAGE> is not supported in link libraries expression."));
                }
                self.flags.head_sensitive = true;
            }
            self.flags.link_language_sensitive = true;
            call.ctx.link_language.as_ref()
        } else {
            if (call
                .ctx
                .language
                .as_ref()
                .is_none_or(|value| value.is_empty())
                && role != EvaluationRole::Compile)
                || (and_id && call.ctx.head.is_none())
            {
                return Err(call.error(if and_id {
                    "$<COMPILE_LANG_AND_ID:lang,id> may only be used with binary targets to specify include directories, compile definitions, compile options, and compile wrappers.  It may not be used with the add_custom_command, add_custom_target, or file(GENERATE) commands."
                } else {
                    "$<COMPILE_LANGUAGE:...> may only be used to specify include directories, compile definitions, compile options, compile wrappers, and to evaluate components of the file(GENERATE) command."
                }));
            }
            call.ctx.language.as_ref()
        };
        let lang = language.map_or(b"".as_slice(), |value| value.as_slice());
        if !and_id {
            if arguments.is_empty() {
                return Ok(lang.into());
            }
            return Ok(boolean(arguments.iter().any(|v| v == lang)));
        }

        let [wanted, ids @ ..] = arguments.as_slice() else {
            return Ok(boolean(false));
        };
        if wanted != lang {
            return Ok(boolean(false));
        }
        let mut key = b"CMAKE_".to_vec();
        key.extend_from_slice(lang);
        key.extend_from_slice(b"_COMPILER_ID");
        let id = self.host.definition(&key).unwrap_or_default();
        if id.is_empty() {
            return Ok(boolean(
                ids.first().is_some_and(|argument| argument.is_empty()),
            ));
        }
        for argument in ids {
            if !valid_id(argument) {
                return Err(call.error("Expression syntax not recognized."));
            }
            if argument == &id {
                return Ok(boolean(true));
            }
        }
        Ok(boolean(false))
    }

    fn compile_features(&mut self, call: Call<'_>) -> Value {
        self.flags.head_sensitive = true;
        if call.ctx.head.is_none() {
            return Err(call.error("$<COMPILE_FEATURES:...> may only be used with binary targets."));
        }
        if call.params() == 0 {
            return Err(call.arity(1));
        }
        let args = self.arguments(call)?;
        let mut available = true;
        for feature in args {
            let Some(lang) = LANGUAGES.iter().copied().find(|lang| {
                feature
                    .strip_prefix(lang.to_ascii_lowercase().as_slice())
                    .is_some_and(|suffix| suffix.starts_with(b"_"))
            }) else {
                return Err(call.error(format!("specified unknown feature \"{feature}\"")));
            };
            if self.host.language_enabled(lang) == Some(false) {
                return Err(call.error(format!(
                    "cannot use features from non-enabled language {}",
                    BString::from(lang)
                )));
            }
            let mut key = b"CMAKE_".to_vec();
            key.extend_from_slice(lang);
            key.extend_from_slice(b"_COMPILE_FEATURES");
            let features = self.host.definition(&key).unwrap_or_default();
            available &= list::split(&features, list::Empty::Drop).contains(&feature);
        }
        Ok(boolean(available))
    }

    pub(super) fn reevaluate(&mut self, call: Call<'_>) -> Value {
        if call.name == b"GENEX_EVAL" {
            let value = self.content(call)?;
            return self.second_pass(call, &value, call.ctx);
        }
        if call.params() == 0 {
            return Err(call.arity(2));
        }
        let name = self.parameter(call, 0)?;
        let name_args = self.expand_argument(name)?;
        let value_args = self.arbitrary_content(call, 1)?;
        let mut arguments = name_args.into_iter().chain(value_args);
        let name = arguments.next().unwrap_or_default();
        let value = arguments.next().unwrap_or_default();
        if !valid_target(&name) {
            return Err(call.error(
                "$<TARGET_GENEX_EVAL:tgt, ...> expression requires a non-empty valid target name.",
            ));
        }
        self.flags.context_sensitive = true;
        let target = self.host.find_target(&name).ok_or_else(|| {
            call.error(format!(
                "$<TARGET_GENEX_EVAL:tgt, ...> target \"{name}\" not found."
            ))
        })?;
        let parent_flags = self.flags;
        self.flags = Default::default();
        let ctx = EvalContext {
            head: Some(target),
            current: Some(target),
            ..call.ctx.clone()
        };
        let result = self.second_pass(call, &value, &ctx);
        self.flags.head_sensitive = false;
        self.flags = parent_flags.union(self.flags);
        result
    }

    fn second_pass(&mut self, call: Call<'_>, value: &[u8], ctx: &EvalContext) -> Value {
        if value.is_empty() {
            return Ok(BString::default());
        }
        let _active = if let Some(target) = ctx.head {
            let mut key = BString::from(call.name);
            self.append(&mut key, b":")?;
            self.append(&mut key, value)?;
            let expression = crate::parse::render_expr(call.expr);
            match DAGChecker::enter_expression(
                target,
                &key,
                expression.clone(),
                self.limits.max_recursion,
            )? {
                DAGEntry::Entered(active) => Some(active),
                DAGEntry::SelfReference | DAGEntry::CyclicReference => {
                    let steps = DAGChecker::loop_steps(&expression);
                    if steps.len() == 1 {
                        let error = self.self_reference(Some(target));
                        return Err(if call.ctx.site.backtrace.is_empty() {
                            error
                        } else {
                            call.error(format!("{error}\n"))
                        });
                    }
                    return Err(GenexError::DependencyLoop { expression, steps });
                }
            }
        } else {
            None
        };
        self.nodes(&parse(value)?.nodes, ctx)
    }

    pub(super) fn interface(&mut self, call: Call<'_>) -> Value {
        match call.name {
            b"BUILD_INTERFACE" | b"BUILD_LOCAL_INTERFACE" => self.content(call),
            b"INSTALL_INTERFACE" => {
                if call.params() == 0 {
                    return Err(call.arity(1));
                }
                Ok(BString::default())
            }
            b"INSTALL_PREFIX" => {
                let [] = self.fixed(call)?;
                self.flags.context_sensitive = true;
                self.host.install_prefix(call.ctx).ok_or_else(|| {
                    call.error("INSTALL_PREFIX is a marker for install(EXPORT) only.  It should never be evaluated.")
                })
            }
            b"LINK_ONLY" | b"COMPILE_ONLY" => {
                self.flags.context_sensitive = true;
                if call.params() != 1 {
                    return Err(call.arity(1));
                }
                let [value] = self.fixed(call)?;
                let link = call.ctx.traversal == UsageTraversal::Link;
                if link == (call.name == b"LINK_ONLY") {
                    Ok(value)
                } else {
                    Ok(BString::default())
                }
            }
            b"DEVICE_LINK" | b"HOST_LINK" => self.link_step(call),
            _ => self.link_feature(call),
        }
    }

    fn link_step(&mut self, call: Call<'_>) -> Value {
        self.flags.context_sensitive = true;
        let args = self.arguments(call)?;
        let role = EvaluationRole::of(call.ctx);
        if call.ctx.head.is_none()
            || !(role == EvaluationRole::LinkOptions
                || (role == EvaluationRole::ExplicitLanguages
                    && call.ctx.traversal == UsageTraversal::Link))
        {
            return Err(call.error(format!(
                "$<{}:...> may only be used with binary targets to specify link options.",
                BString::from(call.name)
            )));
        }
        let device = self.host.is_device_link(call.ctx);
        match (call.name, device) {
            (b"HOST_LINK", false) => self.join(args, b";"),
            (b"DEVICE_LINK", true) => self.join(
                [BString::from("<DEVICE_LINK>")]
                    .into_iter()
                    .chain(
                        args.iter()
                            .flat_map(|arg| list::split(arg, list::Empty::Drop))
                            .filter(|arg| {
                                !matches!(arg.as_slice(), b"<DEVICE_LINK>" | b"</DEVICE_LINK>")
                            }),
                    )
                    .chain([BString::from("</DEVICE_LINK>")]),
                b";",
            ),
            _ => Ok(BString::default()),
        }
    }

    fn link_feature(&mut self, call: Call<'_>) -> Value {
        self.flags.context_sensitive = true;
        if call.ctx.traversal != UsageTraversal::Link {
            return Err(call.error(format!("$<{}:...> may only be used with binary targets to specify link libraries through 'LINK_LIBRARIES', 'INTERFACE_LINK_LIBRARIES', and 'INTERFACE_LINK_LIBRARIES_DIRECT' properties.", BString::from(call.name))));
        }
        let arguments = self.arguments(call)?;
        let [feature, items @ ..] = arguments.as_slice() else {
            return Err(call.arity(2));
        };
        if items.is_empty() {
            return Err(call.arity(2));
        }
        if feature.is_empty() || !valid_id(feature) {
            return Err(call.error(format!("The feature name '{feature}' is invalid.")));
        }
        let name = BString::from(call.name);
        let members: Vec<_> = items
            .iter()
            .flat_map(|item| list::split(item, list::Empty::Drop))
            .collect();
        if members.is_empty() {
            return Ok(BString::default());
        }
        let mut output = if name == "LINK_GROUP" {
            let encoded = self.join(members.iter().cloned(), b"|")?;
            BString::from(format!("<{name}:{feature}:{encoded}>"))
        } else {
            BString::from(format!("<{name}:{feature}>"))
        };
        for item in members {
            self.append(&mut output, b";")?;
            self.append(&mut output, &item)?;
        }
        self.append(&mut output, format!(";</{name}:{feature}>").as_bytes())?;
        Ok(output)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EvaluationRole {
    Compile,
    Link,
    LinkOptions,
    LinkLibraries,
    ExplicitLanguages,
    Other,
}

impl EvaluationRole {
    fn of(ctx: &EvalContext) -> Self {
        let root = DAGChecker::root_property();
        let property = root.as_ref().or(ctx.site.property.as_ref());
        let Some(property) = property else {
            return if ctx.language.is_some() && ctx.link_language.is_some() {
                Self::ExplicitLanguages
            } else {
                Self::Other
            };
        };
        let property = property.strip_prefix(b"INTERFACE_").unwrap_or(property);
        match property {
            b"INCLUDE_DIRECTORIES"
            | b"SYSTEM_INCLUDE_DIRECTORIES"
            | b"COMPILE_DEFINITIONS"
            | b"COMPILE_OPTIONS"
            | b"COMPILE_WRAPPERS" => Self::Compile,
            b"LINK_OPTIONS" | b"LINKER_TYPE" => Self::LinkOptions,
            b"LINK_DIRECTORIES" | b"LINK_DEPENDS" | b"LINK_LIBRARY_OVERRIDE" | b"LINK_WRAPPERS" => {
                Self::Link
            }
            b"LINK_LIBRARIES"
            | b"LINK_LIBRARIES_DIRECT"
            | b"LINK_LIBRARIES_DIRECT_EXCLUDE"
            | b"LINK_INTERFACE_LIBRARIES"
            | b"IMPORTED_LINK_INTERFACE_LIBRARIES" => Self::LinkLibraries,
            name if name.ends_with(b"_LINKER_LAUNCHER") => Self::Link,
            name if name.ends_with(b"_COMPILER_LAUNCHER") => Self::Compile,
            name if name.starts_with(b"LINK_INTERFACE_LIBRARIES_")
                || name.starts_with(b"IMPORTED_LINK_INTERFACE_LIBRARIES_") =>
            {
                Self::LinkLibraries
            }
            _ => Self::Other,
        }
    }

    fn supports_link_language(self) -> bool {
        matches!(
            self,
            Self::Link | Self::LinkOptions | Self::LinkLibraries | Self::ExplicitLanguages
        )
    }
}

fn valid_id(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}
