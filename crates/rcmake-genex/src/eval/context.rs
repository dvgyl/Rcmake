use rcmake_base::{BString, list, truth, version};

use super::{
    Call, Evaluator, Value, boolean,
    targets::{DAGChecker, DAGEntry, FrameKind},
    unknown,
};
use crate::{EvalContext, UsageTraversal, parse};

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
        if let Some(configs) = call
            .ctx
            .current
            .and_then(|target| self.host.imported_config_matches(target, &call.ctx.config))
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
        let linking = call.name.starts_with(b"LINK_");
        let language = if linking {
            self.flags.link_language_sensitive = true;
            if call.ctx.traversal != UsageTraversal::Link && call.ctx.link_language.is_none() {
                return Err(call.error(format!("$<{}:...> may only be used with binary targets to specify link libraries, link directories, link options, link depends, and link wrappers.", BString::from(call.name))));
            }
            call.ctx.link_language.as_ref()
        } else {
            if call.ctx.head.is_none() && call.ctx.language.is_none() {
                return Err(call.error(format!("$<{}:...> may only be used with binary targets to specify include directories, compile definitions, compile options, and to evaluate components of the file(GENERATE) command.", BString::from(call.name))));
            }
            call.ctx.language.as_ref()
        };
        let lang = language.map_or(b"".as_slice(), |value| value.as_slice());
        if !call.name.ends_with(b"_AND_ID") {
            if call.params() == 0 {
                return Ok(lang.into());
            }
            return Ok(boolean(self.arguments(call)?.iter().any(|v| v == lang)));
        }

        let args = self.arguments(call)?;
        let [wanted, ids @ ..] = args.as_slice() else {
            return Err(call.arity(2));
        };
        if ids.is_empty() {
            return Err(call.arity(2));
        }
        if wanted != lang {
            return Ok(boolean(false));
        }
        let mut key = b"CMAKE_".to_vec();
        key.extend_from_slice(lang);
        key.extend_from_slice(b"_COMPILER_ID");
        let id = self.host.definition(&key).unwrap_or_default();
        Ok(boolean(ids.contains(&id)))
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
            return self.second_pass(&value, call.ctx);
        }
        if call.params() < 2 {
            return Err(call.arity(2));
        }
        let name = self.parameter(call, 0)?;
        let target = self.require_target(call, &name)?;
        let mut value = BString::default();
        for index in 1..call.params() {
            if index != 1 {
                self.append(&mut value, b",")?;
            }
            let parameter = self.parameter(call, index)?;
            self.append(&mut value, &parameter)?;
        }
        let parent_flags = self.flags;
        self.flags = Default::default();
        let ctx = EvalContext {
            head: Some(target),
            current: Some(target),
            ..call.ctx.clone()
        };
        let result = self.second_pass(&value, &ctx);
        self.flags.head_sensitive = false;
        self.flags.context_sensitive = true;
        self.flags = parent_flags.union(self.flags);
        result
    }

    fn second_pass(&mut self, value: &[u8], ctx: &EvalContext) -> Value {
        let target = ctx.current.or(ctx.head);
        let _active = match DAGChecker::enter(
            target,
            FrameKind::Expression,
            value,
            self.limits.max_recursion,
        )? {
            DAGEntry::Entered(active) => active,
            DAGEntry::SelfReference => return Err(self.self_reference(target)),
            DAGEntry::CyclicReference => return Ok(BString::default()),
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
        if call.ctx.traversal != UsageTraversal::Link {
            return Err(call.error(format!(
                "$<{}:...> may only be used with binary targets to specify link options.",
                BString::from(call.name)
            )));
        }
        if call.params() == 0 {
            return Err(call.arity(1));
        }
        let args = self.arguments(call)?;
        let device = self.host.is_device_link(call.ctx);
        match (call.name, device) {
            (b"HOST_LINK", false) => self.join(args, b";"),
            (b"DEVICE_LINK", true) => self.join(
                [BString::from("<DEVICE_LINK>")]
                    .into_iter()
                    .chain(args)
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

fn valid_id(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}
