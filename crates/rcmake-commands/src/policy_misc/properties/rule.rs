use rcmake_base::{BString, list, truth};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;

use super::values;

pub(in crate::policy_misc) fn set(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if args.first().is_none_or(|scope| scope.value != b"RULE") {
        return super::set::set(cx, args);
    }
    let args = values(args);
    let mut parts = args
        .get(1..)
        .unwrap_or_default()
        .splitn(2, |key| *key == b"PROPERTY");
    let rules = parts.next().unwrap_or_default();
    let Some([name, values @ ..]) = parts.next() else {
        return Err(CmdError::status("not given a PROPERTY <name> argument."));
    };
    let append_string = rules.contains(&b"APPEND_STRING".as_slice());
    let append = rules.contains(&b"APPEND".as_slice());
    let dir = cx.current_dir_id();
    for &rule in rules
        .iter()
        .filter(|rule| !matches!(**rule, b"APPEND" | b"APPEND_STRING"))
    {
        if cx
            .global()
            .props
            .get(b"\0RCMAKE_IN_RULE_CONFIGURATOR")
            .is_some()
        {
            let message = if matches!(
                *name,
                b"COMMENT" | b"VERBATIM" | b"COMMAND_EXPAND_LISTS" | b"OUTPUT_FILE_SET" | b"GLOBAL"
            ) {
                format!(
                    "\"{}\" property can't be set during generation for rules (\"{}\")\n",
                    BString::from(*name),
                    BString::from(rule)
                )
            } else {
                format!(
                    "rule properties cannot be changed during generation (\"{}\")\n",
                    BString::from(rule)
                )
            };
            cx.issue(MessageKind::Error, Effect::SetError, None, message.into());
            return Err(CmdError::Reported);
        }
        let project = cx.project();
        let Some(owner) = project.custom_rule_owner(dir, rule) else {
            return Err(CmdError::status(format!(
                "could not find RULE {}.  Perhaps it has not yet been created.",
                BString::from(rule)
            )));
        };
        let Some(mut properties) = project.custom_rule(dir, rule).cloned() else {
            continue;
        };
        let mut value = list::join(values);
        if let Some(old) = properties.get(name).filter(|_| append || append_string) {
            let separator = if append && !old.is_empty() && !value.is_empty() {
                b";".as_slice()
            } else {
                b""
            };
            value = [old.as_slice(), separator, &value].concat().into();
        }
        let mut new_owner = owner;
        let mut cross_directory_promotion = false;
        let error = match *name {
            b"GLOBAL" if !truth::is_on(&value) => Some(format!(
                "GLOBAL property can't be set to FALSE on rules (\"{}\")",
                BString::from(rule)
            )),
            b"GLOBAL" => {
                cross_directory_promotion = owner.is_some_and(|owner| owner != dir);
                new_owner = None;
                value = "1".into();
                None
            }
            b"OUTPUT_FILE_SET" => {
                let entries = list::split(&value, list::Empty::Keep);
                match entries.as_slice() {
                    [_, kind] if rcmake_model::FileSetKind::from_name(kind).is_some() || matches!(kind.as_slice(), b"SYCL" | b"SYCL_HEADERS") => None,
                    [_, _] => Some("specified file set type is erroneous.  The supported types are: HEADERS, SOURCES, SYCL, SYCL_HEADERS, CXX_MODULES.".into()),
                    _ => Some(
                        "OUTPUT_FILE_SET property require a list of 2 elements:\n  \"name;type\""
                            .into(),
                    ),
                }
            }
            b"NAME"
            | b"PARENT_RULE"
            | b"COMMAND"
            | b"COMMAND_COUNT"
            | b"OUTPUT"
            | b"DEPENDS"
            | b"BYPRODUCTS"
            | b"DEPFILE"
            | b"FILE_SET_CONFIGURATORS"
            | b"SOURCE_CONFIGURATORS" => Some(read_only(name, rule)),
            _ if command_index(name).is_some() => Some(read_only(name, rule)),
            _ => None,
        };
        if let Some(error) = error {
            cx.issue(MessageKind::Error, Effect::SetError, None, error.into());
            continue;
        }
        if values.is_empty() && !append && !append_string {
            properties.unset(name);
        } else {
            properties.set((*name).into(), value);
        }
        if owner != new_owner {
            project.remove_custom_rule(owner, rule);
        }
        project.store_custom_rule(new_owner, rule.into(), properties);
        if cross_directory_promotion {
            cx.issue(
                MessageKind::Error,
                Effect::SetError,
                None,
                format!(
                    "Attempt to promote rule \"{}\" to global scope (by setting GLOBAL) which is not created in this directory.",
                    BString::from(rule)
                )
                .into(),
            );
        }
    }
    Ok(Flow::Normal)
}

fn read_only(name: &[u8], rule: &[u8]) -> String {
    format!(
        "\"{}\" property is read-only for rules (\"{}\")",
        BString::from(name),
        BString::from(rule)
    )
}

pub(super) fn command_index(property: &[u8]) -> Option<&[u8]> {
    let index = property.strip_prefix(b"COMMAND_")?;
    (!index.is_empty() && index.iter().all(u8::is_ascii_digit)).then_some(index)
}
