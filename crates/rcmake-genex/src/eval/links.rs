use rcmake_base::{BString, list, truth};

use super::{
    Call, Evaluator, Value,
    context::{is_link_libraries_property, link_features_allowed},
};
use crate::{EvalContext, GenexError, UsageTraversal};

impl Evaluator<'_> {
    pub(super) fn link_feature(&mut self, call: Call<'_>) -> Value {
        let arguments = self.arguments(call)?;
        let name = BString::from(call.name);
        if arguments.is_empty() {
            return Err(call.error(format!(
                "$<{name}> expression requires at least one parameter."
            )));
        }
        let group = call.name == b"LINK_GROUP";
        if !link_feature_site(call.ctx) {
            let items = if group {
                "group of link libraries"
            } else {
                "link libraries"
            };
            return Err(call.error(format!(
                "$<{name}:...> may only be used with binary targets to specify {items} through 'LINK_LIBRARIES', 'INTERFACE_LINK_LIBRARIES', and 'INTERFACE_LINK_LIBRARIES_DIRECT' properties."
            )));
        }
        self.flags.context_sensitive = true;

        let items: Vec<_> = arguments
            .iter()
            .flat_map(|argument| list::split(argument, list::Empty::Drop))
            .collect();
        if group && items.iter().any(|item| item.starts_with(b"<LINK_GROUP")) {
            return Err(call.error("$<LINK_GROUP:...> cannot be nested."));
        }
        let [feature, members @ ..] = items.as_slice() else {
            return Err(call.error(format!(
                "$<{name}:...> expects a feature name as first argument."
            )));
        };
        if members.is_empty() {
            return Ok(BString::default());
        }
        if !valid_feature(feature) {
            return Err(call.error(format!(
                "The feature name '{feature}' contains invalid characters."
            )));
        }

        let end = BString::from(format!("</{name}:{feature}>"));
        let mut start = BString::from(format!("<{name}:{feature}"));
        if group {
            self.append(&mut start, b":")?;
            for (index, member) in members.iter().enumerate() {
                if index != 0 {
                    self.append(&mut start, b"|")?;
                }
                self.append(&mut start, member)?;
            }
        }
        self.append(&mut start, b">")?;

        if !group {
            if members
                .iter()
                .any(|item| item.starts_with(b"<LINK_LIBRARY:") && item != &start)
            {
                return Err(
                    call.error("$<LINK_LIBRARY:...> with different features cannot be nested.")
                );
            }
            if members.iter().any(|item| item.starts_with(b"<LINK_GROUP:")) {
                return Err(call.error(
                    "$<LINK_GROUP:...> cannot be nested inside a $<LINK_LIBRARY:...> expression.",
                ));
            }
        }

        let library_start = (!group).then(|| start.clone());
        let mut output = start;
        for item in members.iter().filter(|item| {
            library_start
                .as_ref()
                .is_none_or(|start| *item != start && *item != &end)
        }) {
            self.append(&mut output, b";")?;
            self.append(&mut output, item)?;
        }
        self.append(&mut output, b";")?;
        self.append(&mut output, &end)?;
        self.emitted_link_feature = true;
        Ok(output)
    }

    pub(super) fn validate_link_feature_output(
        &mut self,
        ctx: &EvalContext,
        value: &[u8],
    ) -> Result<(), GenexError> {
        if !self.emitted_link_feature || !link_feature_site(ctx) {
            return Ok(());
        }
        let Some(owner) = ctx.current.or(ctx.head) else {
            return Ok(());
        };
        let only_targets = self
            .host
            .raw_property(owner, b"LINK_LIBRARIES_ONLY_TARGETS")
            .is_some_and(|value| truth::is_on(&value));
        let implementation = ctx
            .site
            .property
            .as_ref()
            .is_some_and(|name| name == "LINK_LIBRARIES");
        let language = ctx.link_language.as_ref().filter(|language| {
            implementation
                && ctx.traversal == UsageTraversal::Link
                && Some(owner) == ctx.head
                && !language.is_empty()
        });
        let language = language.filter(|_| {
            self.host.raw_property(owner, b"TYPE").is_none_or(|kind| {
                matches!(
                    kind.as_slice(),
                    b"EXECUTABLE" | b"SHARED_LIBRARY" | b"MODULE_LIBRARY"
                )
            })
        });
        if !only_targets && language.is_none() {
            return Ok(());
        }

        // Check only the link items that remain after the outer expression runs.
        let mut in_library = false;
        let mut in_group = false;
        for item in list::iter(value, list::Empty::Drop) {
            let name = item.trim_ascii();
            if let Some(feature) = name
                .strip_prefix(b"<LINK_LIBRARY:")
                .and_then(|name| name.strip_suffix(b">"))
            {
                in_library = true;
                if let Some(language) = language
                    && feature != b"DEFAULT"
                {
                    self.validate_library_attributes(language, feature)?;
                }
            } else if name.starts_with(b"</LINK_LIBRARY:") && name.ends_with(b">") {
                in_library = false;
            } else if name.starts_with(b"<LINK_GROUP:") && name.ends_with(b">") {
                in_group = true;
            } else if name.starts_with(b"</LINK_GROUP:") && name.ends_with(b">") {
                in_group = false;
            } else if only_targets
                && (in_library || in_group)
                && possible_target_name(name)
                && self.host.find_target_in_context(name, ctx).is_none()
            {
                return Err(not_a_target(
                    self.host.target_name(owner),
                    name,
                    implementation,
                ));
            }
        }
        Ok(())
    }

    fn validate_library_attributes(
        &mut self,
        language: &BString,
        feature: &[u8],
    ) -> Result<(), GenexError> {
        let feature = BString::from(feature);
        let specific = format!("CMAKE_{language}_LINK_LIBRARY_{feature}_ATTRIBUTES");
        let generic = format!("CMAKE_LINK_LIBRARY_{feature}_ATTRIBUTES");
        let selected = [specific, generic].into_iter().find_map(|name| {
            self.host
                .definition(name.as_bytes())
                .filter(|value| !value.is_empty())
                .map(|value| (name, value))
        });
        let Some((name, value)) = selected else {
            return Ok(());
        };
        self.charge(value.len())?;
        let mut body = BString::default();
        for option in list::iter(&value, list::Empty::Drop) {
            if !valid_attribute(&option) {
                self.append(&mut body, b"  ")?;
                self.append(&mut body, &option)?;
                self.append(&mut body, b"\n")?;
            }
        }
        if body.is_empty() {
            return Ok(());
        }
        Err(GenexError::Message {
            expression: BString::default(),
            message: format!("Erroneous option(s) for '{name}':\n{body}").into(),
        }
        .without_location())
    }
}

fn link_feature_site(ctx: &EvalContext) -> bool {
    // A property read can evaluate a link list beneath a different root property.
    if let Some(property) = &ctx.site.property {
        return ctx.head.is_some()
            && property != "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE"
            && is_link_libraries_property(property);
    }
    link_features_allowed(ctx)
}

fn possible_target_name(name: &[u8]) -> bool {
    name.first()
        .is_some_and(|&byte| !matches!(byte, b'-' | b'$' | b'`'))
        && !name.iter().any(|&byte| matches!(byte, b'/' | b'\\'))
}

fn not_a_target(target: BString, name: &[u8], implementation: bool) -> GenexError {
    let name = BString::from(name);
    let message = if implementation {
        "it links to"
    } else {
        "its link interface contains"
    };
    GenexError::Message {
        expression: BString::default(),
        message: format!(
            "Target \"{target}\" has LINK_LIBRARIES_ONLY_TARGETS enabled, but {message}:\n  {name}\nwhich is not a target.  Possible reasons include:\n  * There is a typo in the target name.\n  * A find_package call is missing for an IMPORTED target.\n  * An ALIAS target is missing.\n"
        )
        .into(),
    }
}

fn valid_feature(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn valid_attribute(option: &[u8]) -> bool {
    let mut parts = option.splitn(2, |byte| *byte == b'=');
    let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
        return false;
    };
    match key {
        b"LIBRARY_TYPE" => value
            .split(|byte| *byte == b',')
            .all(|kind| matches!(kind, b"STATIC" | b"SHARED" | b"MODULE" | b"EXECUTABLE")),
        b"DEDUPLICATION" => matches!(value, b"YES" | b"NO" | b"DEFAULT"),
        b"OVERRIDE" => value.split(|byte| *byte == b',').all(valid_feature),
        _ => false,
    }
}
