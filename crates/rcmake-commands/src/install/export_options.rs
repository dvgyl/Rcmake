use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::Cx;

use super::{Result, fail, keyword};

const CMP0208: PolicyId = PolicyId::new(208);
const CMP0103: PolicyId = PolicyId::new(103);

pub(super) fn normalize<'a>(cx: &mut Cx<'_>, args: &[&'a [u8]]) -> Result<Vec<&'a [u8]>> {
    let mut parsed = Vec::with_capacity(args.len() + 1);
    let mut missing = Vec::new();
    let mut words = args.iter().copied().peekable();
    while let Some(key) = words.next() {
        if !matches!(
            key,
            b"EXPORT" | b"FILE" | b"NAMESPACE" | b"CXX_MODULES_DIRECTORY"
        ) {
            parsed.push(key);
            continue;
        }
        let value = words.next_if(|word| !keyword(word));
        match value {
            Some(value) if key == b"NAMESPACE" || !value.is_empty() => {
                parsed.extend([key, value]);
            }
            invalid => {
                let reason = if invalid.is_some() {
                    "empty value is not allowed"
                } else {
                    "missing required value"
                };
                missing.push(format!("  {}: {reason}", String::from_utf8_lossy(key)));
                if key == b"EXPORT" {
                    parsed.extend([key, b""]);
                }
            }
        }
    }
    if !missing.is_empty() {
        let message = format!("EXPORT given invalid argument:\n{}", missing.join("\n"));
        match cx.policy(CMP0208) {
            PolicyStatus::New => return Err(fail(message)),
            PolicyStatus::Warn => {
                crate::author_warning(cx, BString::from(format!("export {message}")));
                cx.issue_policy_warning(CMP0208, b"", b"");
            }
            PolicyStatus::Old => {}
        }
    }
    Ok(parsed)
}

pub(super) fn repeat(cx: &mut Cx<'_>, file: &BString) -> Result<()> {
    let message =
        format!("command already specified for the file\n  {file}\nDid you miss 'APPEND' keyword?");
    match cx.policy(CMP0103) {
        PolicyStatus::New => Err(fail(message)),
        PolicyStatus::Warn => {
            let warning = format!("export() {message}");
            cx.issue_policy_warning(CMP0103, b"", warning.as_bytes());
            Ok(())
        }
        PolicyStatus::Old => Ok(()),
    }
}
