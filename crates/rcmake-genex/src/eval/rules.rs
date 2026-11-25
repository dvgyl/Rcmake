//! `$<RULE_PROPERTY:rule,prop>` reads a property of a custom rule.

use rcmake_base::BString;

use super::{Call, Evaluator, Value};
use crate::parse::parse;

impl Evaluator<'_> {
    pub(super) fn rule_property(&mut self, call: Call<'_>) -> Value {
        let arguments = self.arguments(call)?;
        let [rule, property] = arguments.as_slice() else {
            return Err(call.error(format!(
                "$<RULE_PROPERTY> expression requires 2 comma separated parameters, but got {} instead.",
                call.params()
            )));
        };
        if rule.is_empty() {
            return Err(
                call.error("$<RULE_PROPERTY:rule,prop> expression requires a non-empty rule name.")
            );
        }
        if property.is_empty() {
            return Err(call.error(
                "$<RULE_PROPERTY:src,prop> expression requires a non-empty property name.",
            ));
        }
        self.flags.context_sensitive = true;
        let found = self
            .host
            .rule_property(rule, property)
            .ok_or_else(|| call.error(format!("Rule \"{rule}\" not found.")))?;
        let key = (rule.clone(), property.clone());
        if self.active_rules.contains(&key) {
            return Err(call
                .error(format!("Self reference on rule \"{}\".", found.rule_name))
                .without_location());
        }
        let Some(value) = found.value.filter(|value| !value.is_empty()) else {
            return Ok(BString::default());
        };
        let ast = parse(&value)?;
        self.active_rules.push(key);
        let result = self.nodes(&ast.nodes, call.ctx);
        self.active_rules.pop();
        result
    }
}
