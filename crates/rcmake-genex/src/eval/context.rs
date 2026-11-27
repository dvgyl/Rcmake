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
                if let Some(trace) = self.trace.as_deref_mut() {
                    trace.cross_config(call.name);
                }
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
        // Only the first name must be valid. CMake warns about the others for compatibility.
        if values.first().is_some_and(|v| !valid_id(v)) {
            return Err(call.error("Expression syntax not recognized."));
        }
        for value in values.iter().skip(1).filter(|v| !valid_id(v)) {
            let body = format!(
                "Warning evaluating generator expression:\n  {}\nThe config name of \"{value}\" is invalid",
                crate::parse::render_expr(call.expr)
            );
            self.host.warning(body.into(), call.ctx);
        }
        if let Some(configs) = call
            .ctx
            .current
            .and_then(|target| self.host.imported_config_matches_ctx(target, call.ctx))
        {
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
            && let Some(mapped) =
                with_upper_prefixed_key(b"MAP_IMPORTED_CONFIG_", &call.ctx.config.0, |key| {
                    self.host.raw_property(target, key)
                })
        {
            return Ok(boolean(
                list::iter(&mapped, list::Empty::Keep)
                    .any(|cfg| values.iter().any(|v| cfg.eq_ignore_ascii_case(v))),
            ));
        }
        Ok(boolean(false))
    }

    pub(super) fn compiler(&mut self, call: Call<'_>) -> Value {
        self.flags.context_sensitive = true;
        let arguments = self.arguments(call)?;
        let is_version = call.name.ends_with(b"_VERSION");
        if is_version && call.params() > 1 {
            return Err(call.error(format!(
                "$<{}> expression requires one or zero parameters.",
                BString::from(call.name)
            )));
        }
        if call.ctx.head.is_none() {
            let spacing = if call.name.ends_with(b"_COMPILER_LINKER_ID")
                || call.name.ends_with(b"_COMPILER_LINKER_FRONTEND_VARIANT")
            {
                " "
            } else {
                "  "
            };
            return Err(call.error(format!(
                "$<{}> may only be used with binary targets.{spacing}It may not be used with add_custom_command or add_custom_target.",
                BString::from(call.name)
            )));
        }
        let value = with_prefixed_key(b"CMAKE_", call.name, b"", |key| self.host.definition(key))
            .unwrap_or_default();
        if !is_version {
            return compiler_id(call, value, &arguments);
        }
        let Some(wanted) = arguments.first() else {
            return Ok(value);
        };
        if !wanted
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b'.')
        {
            return Err(call.error("Expression syntax not recognized."));
        }
        Ok(boolean(if value.is_empty() {
            wanted.is_empty()
        } else {
            version::compare(&value, wanted).is_eq()
        }))
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
        let role = EvaluationRole::active();
        let language = if role.supports_link_language() {
            call.ctx.link_language.as_ref()
        } else {
            call.ctx.language.as_ref()
        };
        let linking = call.name.starts_with(b"LINK_");
        if linking {
            if call.ctx.head.is_none() || !role.supports_link_language() {
                let expression = if and_id {
                    "LINK_LANG_AND_ID:lang,id"
                } else {
                    "LINK_LANGUAGE:..."
                };
                return Err(call.error(format!("$<{expression}> may only be used with binary targets to specify link libraries, link directories, link options, link depends, and link wrappers.")));
            }
            if role.is_link_libraries() {
                if arguments.is_empty() {
                    return Err(call
                        .error("$<LINK_LANGUAGE> is not supported in link libraries expression."));
                }
                self.flags.head_sensitive = true;
                self.flags.link_language_sensitive = true;
            }
        } else if (language.is_none_or(|value| value.is_empty()) && role != EvaluationRole::Compile)
            || (and_id && call.ctx.head.is_none())
        {
            return Err(call.error(if and_id {
                "$<COMPILE_LANG_AND_ID:lang,id> may only be used with binary targets to specify include directories, compile definitions, compile options, and compile wrappers.  It may not be used with the add_custom_command, add_custom_target, or file(GENERATE) commands."
            } else {
                "$<COMPILE_LANGUAGE:...> may only be used to specify include directories, compile definitions, compile options, compile wrappers, and to evaluate components of the file(GENERATE) command."
            }));
        }
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
        let value = with_prefixed_key(b"CMAKE_", lang, b"_COMPILER_ID", |key| {
            self.host.definition(key)
        })
        .unwrap_or_default();
        compiler_id(call, value, ids)
    }

    fn compile_features(&mut self, call: Call<'_>) -> Value {
        self.flags.head_sensitive = true;
        let binary = call.ctx.head.is_some_and(|head| {
            !self
                .host
                .raw_property(head, b"TYPE")
                .is_some_and(|kind| matches!(kind.as_slice(), b"UTILITY" | b"GLOBAL_TARGET"))
        });
        if !binary {
            return Err(call.error(
                "$<COMPILE_FEATURE> may only be used with binary targets.  It may not be used with add_custom_command or add_custom_target.",
            ));
        }
        if call.params() == 0 {
            return Err(call.arity(1));
        }
        let args = self.arguments(call)?;
        let mut available = true;
        for feature in args {
            let Some(lang) = LANGUAGES.iter().copied().find(|lang| {
                feature.len() > lang.len()
                    && feature.get(..lang.len()).is_some_and(|prefix| {
                        prefix
                            .iter()
                            .zip(lang.iter())
                            .all(|(&f, &l)| f == l.to_ascii_lowercase())
                    })
                    && feature.get(lang.len()) == Some(&b'_')
            }) else {
                return Err(call.error(format!("specified unknown feature \"{feature}\"")));
            };
            if self.host.language_enabled(lang) == Some(false) {
                return Err(call.error(format!(
                    "cannot use features from non-enabled language {}",
                    BString::from(lang)
                )));
            }
            let features = with_prefixed_key(b"CMAKE_", lang, b"_COMPILE_FEATURES", |key| {
                self.host.definition(key)
            })
            .unwrap_or_default();
            available &= list::iter(&features, list::Empty::Drop)
                .any(|item| item.as_ref() == feature.as_slice());
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
        let active = if let Some(target) = ctx.head {
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
        let ast = parse(value)?;
        let previous = self.in_target_expression;
        self.in_target_expression |= active.is_some();
        let result = self.nodes(&ast.nodes, ctx);
        self.in_target_expression = previous;
        result
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
            b"LINK_ONLY" | b"COMPILE_ONLY" => self.usage_only(call),
            b"DEVICE_LINK" | b"HOST_LINK" => self.link_step(call),
            _ => Err(unknown(call)),
        }
    }

    fn usage_only(&mut self, call: Call<'_>) -> Value {
        self.flags.context_sensitive = true;
        let [value] = self.fixed(call)?;
        let property = DAGChecker::with_root_property(|property| {
            property.map(|prop| (is_link_libraries_property(prop), prop == b"LINK_LIBRARIES"))
        });
        if property.is_none() && !self.in_target_expression {
            return Err(call.error(format!(
                "$<{}:...> may only be used for linking",
                BString::from(call.name)
            )));
        }
        let (is_link_libs, is_direct_link_libs) = property.unwrap_or((false, false));
        let compile_usage = call.ctx.traversal == UsageTraversal::Compile && is_link_libs;
        let include = if call.name == b"COMPILE_ONLY" {
            compile_usage
        } else if is_direct_link_libs {
            !compile_usage
                || call
                    .ctx
                    .current
                    .or(call.ctx.head)
                    .and_then(|target| self.host.target_policy(target, b"CMP0131"))
                    != Some(true)
        } else {
            !compile_usage
        };
        Ok(if include { value } else { BString::default() })
    }

    fn link_step(&mut self, call: Call<'_>) -> Value {
        self.flags.context_sensitive = true;
        let args = self.arguments(call)?;
        if call.ctx.head.is_none() || EvaluationRole::active() != EvaluationRole::LinkOptions {
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
}

pub(super) fn link_features_allowed(ctx: &EvalContext) -> bool {
    ctx.head.is_some() && EvaluationRole::active() == EvaluationRole::LinkLibraries
}

pub(super) fn is_link_libraries_property(property: &[u8]) -> bool {
    EvaluationRole::for_property(property).is_link_libraries()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EvaluationRole {
    Compile,
    Link,
    LinkOptions,
    LinkLibraries,
    LinkLibrariesExclude,
    Other,
}

impl EvaluationRole {
    fn active() -> Self {
        DAGChecker::with_root_property(|property| property.map_or(Self::Other, Self::for_property))
    }

    fn for_property(property: &[u8]) -> Self {
        // The resolver uses interface property names for inherited usage.
        match property {
            b"INCLUDE_DIRECTORIES"
            | b"INTERFACE_INCLUDE_DIRECTORIES"
            | b"COMPILE_DEFINITIONS"
            | b"INTERFACE_COMPILE_DEFINITIONS"
            | b"COMPILE_OPTIONS"
            | b"INTERFACE_COMPILE_OPTIONS"
            | b"COMPILE_WRAPPERS"
            | b"INTERFACE_COMPILE_WRAPPERS" => Self::Compile,
            b"LINK_OPTIONS" | b"INTERFACE_LINK_OPTIONS" | b"LINKER_TYPE" => Self::LinkOptions,
            b"LINK_DIRECTORIES"
            | b"INTERFACE_LINK_DIRECTORIES"
            | b"LINK_DEPENDS"
            | b"INTERFACE_LINK_DEPENDS"
            | b"LINK_LIBRARY_OVERRIDE"
            | b"LINK_WRAPPERS"
            | b"INTERFACE_LINK_WRAPPERS" => Self::Link,
            b"LINK_LIBRARIES"
            | b"INTERFACE_LINK_LIBRARIES"
            | b"INTERFACE_LINK_LIBRARIES_DIRECT"
            | b"LINK_INTERFACE_LIBRARIES"
            | b"IMPORTED_LINK_INTERFACE_LIBRARIES" => Self::LinkLibraries,
            b"INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE" => Self::LinkLibrariesExclude,
            name if name.len() > b"_LINKER_LAUNCHER".len()
                && name.ends_with(b"_LINKER_LAUNCHER") =>
            {
                Self::Link
            }
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
            Self::Link | Self::LinkOptions | Self::LinkLibraries | Self::LinkLibrariesExclude
        )
    }

    fn is_link_libraries(self) -> bool {
        matches!(self, Self::LinkLibraries | Self::LinkLibrariesExclude)
    }
}

fn compiler_id(call: Call<'_>, value: BString, arguments: &[BString]) -> Value {
    let Some(first) = arguments.first() else {
        return Ok(value);
    };
    if value.is_empty() {
        return Ok(boolean(first.is_empty()));
    }
    for argument in arguments {
        if !valid_id(argument) {
            return Err(call.error("Expression syntax not recognized."));
        }
        if argument == &value {
            return Ok(boolean(true));
        }
    }
    Ok(boolean(false))
}

fn valid_id(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn with_prefixed_key<R>(
    prefix: &[u8],
    middle: &[u8],
    suffix: &[u8],
    f: impl FnOnce(&[u8]) -> R,
) -> R {
    let total = prefix.len() + middle.len() + suffix.len();
    let mut buf = [0_u8; 96];
    if let Some(slot) = buf.get_mut(..total) {
        let p = prefix.len();
        let pm = p + middle.len();
        if let Some(dst) = slot.get_mut(..p) {
            dst.copy_from_slice(prefix);
        }
        if let Some(dst) = slot.get_mut(p..pm) {
            dst.copy_from_slice(middle);
        }
        if let Some(dst) = slot.get_mut(pm..total) {
            dst.copy_from_slice(suffix);
        }
        f(slot)
    } else {
        let mut key = Vec::with_capacity(total);
        key.extend_from_slice(prefix);
        key.extend_from_slice(middle);
        key.extend_from_slice(suffix);
        f(&key)
    }
}

fn with_upper_prefixed_key<R>(prefix: &[u8], tail: &[u8], f: impl FnOnce(&[u8]) -> R) -> R {
    let total = prefix.len() + tail.len();
    let mut buf = [0_u8; 96];
    if let Some(slot) = buf.get_mut(..total) {
        let p = prefix.len();
        if let Some(dst) = slot.get_mut(..p) {
            dst.copy_from_slice(prefix);
        }
        if let Some(dst) = slot.get_mut(p..total) {
            for (d, &b) in dst.iter_mut().zip(tail.iter()) {
                *d = b.to_ascii_uppercase();
            }
        }
        f(slot)
    } else {
        let mut key = Vec::with_capacity(total);
        key.extend_from_slice(prefix);
        key.extend(tail.iter().map(u8::to_ascii_uppercase));
        f(&key)
    }
}
