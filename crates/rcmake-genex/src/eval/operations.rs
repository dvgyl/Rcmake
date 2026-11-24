use rcmake_base::BString;

use super::Call;
use crate::GenexError;

pub(super) fn operation_arity(
    call: Call<'_>,
    operation: &BString,
    modifier: Option<&'static str>,
    constraint: &str,
    count: usize,
) -> GenexError {
    let name = BString::from(call.name);
    let modifier = modifier.map_or_else(String::new, |value| format!(",{value}"));
    let number = match count {
        1 => "one".into(),
        2 => "two".into(),
        3 => "three".into(),
        4 => "four".into(),
        _ => count.to_string(),
    };
    let suffix = if count == 1 { "" } else { "s" };
    let verb = if constraint == "at most" {
        "expects"
    } else {
        "requires"
    };
    call.error(format!(
        "$<{name}:{operation}{modifier}> expression {verb} {constraint} {number} parameter{suffix}."
    ))
}
