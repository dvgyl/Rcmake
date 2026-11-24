use std::collections::BTreeSet;

use rcmake_base::{BString, list};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, UserCommandKind};
use rcmake_expr::ExpandedArg;
use rcmake_json::{Value, write_compact};
use rcmake_model::PropertyMap;

const OPTIONS: &[&[u8]] = &[
    b"COMMAND",
    b"OUTPUT",
    b"DEPENDS",
    b"BYPRODUCTS",
    b"DEPFILE",
    b"GLOBAL",
    b"CONFIGURATOR",
    b"FROM_RULE",
    b"FOR_FILE_SET",
    b"FOR_SOURCE",
];

pub(super) fn add(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some((name, args)) = args.split_first() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let name = &name.value;
    let error = if name.is_empty() || OPTIONS.contains(&name.as_slice()) {
        Some("rule name is missing.".to_owned())
    } else if !name
        .iter()
        .all(|ch| ch.is_ascii_alphanumeric() || b"_.+-".contains(ch))
    {
        Some(format!("invalid name for RULE: {name}."))
    } else if !name
        .iter()
        .any(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        Some("names in all uppercase are reserved for CMake.".to_owned())
    } else {
        None
    };
    if let Some(error) = error {
        cx.issue(
            MessageKind::Error,
            Effect::SetError,
            None,
            format!("add_custom_rule {error}").into(),
        );
        return Ok(Flow::Normal);
    }

    let dir = cx.current_dir_id();
    let values = args
        .iter()
        .map(|arg| arg.value.as_slice())
        .collect::<Vec<_>>();
    if cx.project().custom_rule(dir, name).is_some() {
        return Err(CmdError::status(format!(
            "cannot create RULE \"{name}\" because another RULE with the same name already exists."
        )));
    }
    let derived = values.contains(&b"FROM_RULE".as_slice());
    let mut parsed = Declaration::default();
    parsed
        .parse(cx, name, &values, derived)
        .map_err(|error| error.issue(cx))?;
    let mut props = if let Some(base) = parsed.base {
        let parent = cx.project().custom_rule(dir, base).cloned();
        parent.ok_or_else(|| {
            RuleError::fatal(format!(
                "cannot create RULE \"{name}\" because the RULE \"{}\" does not exist or is not accessible.",
                BString::from(base)
            ))
            .issue(cx)
        })?
    } else {
        parsed.properties(name).map_err(|error| error.issue(cx))?
    };
    props.set("NAME".into(), name.clone());
    props.set("PARENT_RULE".into(), parsed.base.unwrap_or_default().into());
    props.set(
        "GLOBAL".into(),
        if parsed.global { "1" } else { "0" }.into(),
    );
    for (key, configurator) in [
        (b"FILE_SET_CONFIGURATORS".as_slice(), parsed.file_set),
        (b"SOURCE_CONFIGURATORS".as_slice(), parsed.source),
    ] {
        let previous = props.get(key).filter(|text| !text.is_empty());
        if let Some((next, chain)) = configurator {
            let value = match previous.filter(|_| chain) {
                Some(previous) => list::join([previous.as_slice(), next]),
                None => next.into(),
            };
            props.set(key.into(), value);
        }
    }
    let project = cx.project();
    project.store_custom_rule((!parsed.global).then_some(dir), name.clone(), props);
    let declaration = Value::Array(
        values
            .into_iter()
            .map(|value| Value::string(BString::from(value)))
            .collect(),
    );
    let key = [b"_RCMAKE_CUSTOM_RULE_", name.as_slice()].concat().into();
    let scope = if parsed.global {
        &mut project.global.props
    } else {
        &mut project.dir_mut(dir).props
    };
    scope.set(key, write_compact(&declaration));
    Ok(Flow::Normal)
}

enum RuleError {
    Status(&'static str),
    Fatal(BString),
}

impl RuleError {
    fn fatal(message: impl Into<BString>) -> Self {
        Self::Fatal(message.into())
    }

    fn issue(self, cx: &mut Cx<'_>) -> CmdError {
        match self {
            Self::Status(message) => CmdError::status(message),
            Self::Fatal(message) => {
                cx.issue(
                    MessageKind::Error,
                    Effect::Fatal,
                    None,
                    [b"add_custom_rule ", message.as_slice()].concat().into(),
                );
                CmdError::Reported
            }
        }
    }
}

fn is_configurator_option(key: &[u8]) -> bool {
    matches!(key, b"FOR_FILE_SET" | b"FOR_SOURCE")
}

#[derive(Default)]
struct Declaration<'a> {
    commands: Vec<BString>,
    outputs: Vec<&'a [u8]>,
    depends: Vec<&'a [u8]>,
    byproducts: Vec<&'a [u8]>,
    depfile: Option<&'a [u8]>,
    base: Option<&'a [u8]>,
    global: bool,
    file_set: Option<(&'a [u8], bool)>,
    source: Option<(&'a [u8], bool)>,
}

impl<'a> Declaration<'a> {
    fn parse(
        &mut self,
        cx: &Cx<'_>,
        name: &[u8],
        mut args: &[&'a [u8]],
        derived: bool,
    ) -> Result<(), RuleError> {
        let singleton_options = if derived {
            (
                b"FROM_RULE".as_slice(),
                b"CONFIGURATOR".as_slice(),
                "only one occurrence of \"FROM_RULE\" or \"CONFIGURATOR\" options is allowed.",
            )
        } else {
            (
                b"DEPFILE".as_slice(),
                b"CONFIGURATOR".as_slice(),
                "only one occurrence of \"DEPFILE\" or \"CONFIGURATOR\" options is allowed.",
            )
        };
        let (first, second, message) = singleton_options;
        if [first, second]
            .iter()
            .any(|key| args.iter().filter(|word| *word == key).count() > 1)
        {
            return Err(RuleError::Status(message));
        }
        let mut seen = BTreeSet::new();
        let mut missing = BTreeSet::new();
        let mut configuring = false;
        while let Some((&key, rest)) = args.split_first() {
            let end = rest
                .iter()
                .position(|arg| {
                    OPTIONS.contains(arg)
                        && (!is_configurator_option(arg)
                            || key == b"CONFIGURATOR"
                            || (configuring && is_configurator_option(key)))
                })
                .unwrap_or(rest.len());
            let (values, tail) = rest.split_at(end);
            args = tail;
            if !is_configurator_option(key) {
                configuring = key == b"CONFIGURATOR";
            }
            if !seen.insert(key) && configuring && is_configurator_option(key) {
                return Err(RuleError::Status(
                    "only one occurrence of \"FOR_FILE_SET\" or \"FOR_SOURCE\" sub-options of \"CONFIGURATOR\" option is allowed.",
                ));
            }
            match key {
                b"CONFIGURATOR" => {
                    if args.first().is_none_or(|arg| !is_configurator_option(arg))
                        && values.is_empty()
                    {
                        missing.insert(key);
                    }
                }
                b"GLOBAL" => self.global = true,
                b"FOR_FILE_SET" | b"FOR_SOURCE" if configuring => {
                    if let Some((&function, mode)) = values.split_first() {
                        let issue = if !cx.command_exists(function) {
                            Some("does not exist")
                        } else if cx.user_command_kind(function) != Some(UserCommandKind::Function)
                        {
                            Some("is not a function")
                        } else {
                            None
                        };
                        if let Some(issue) = issue {
                            return Err(RuleError::fatal(format!(
                                "command specified for \"{}\" {issue}: {}.",
                                BString::from(key),
                                BString::from(function)
                            )));
                        }
                        let chain = mode.contains(&b"CHAIN".as_slice());
                        let override_ = mode.contains(&b"OVERRIDE".as_slice());
                        if chain && override_ {
                            return Err(RuleError::fatal(format!(
                                "cannot create RULE \"{}\" because the \"CHAIN\" and \"OVERRIDE\" options of CONFIGURATOR \"{}\" are mutually exclusive.",
                                BString::from(name),
                                BString::from(function)
                            )));
                        }
                        if let Some(extra) = mode
                            .iter()
                            .find(|arg| !derived || !matches!(**arg, b"CHAIN" | b"OVERRIDE"))
                        {
                            return Err(unknown(extra));
                        }
                        let entry = if key == b"FOR_FILE_SET" {
                            &mut self.file_set
                        } else {
                            &mut self.source
                        };
                        *entry = Some((function, chain));
                    } else {
                        missing.insert(key);
                    }
                    continue;
                }
                b"FROM_RULE" | b"DEPFILE" if key == b"FROM_RULE" || !derived => {
                    let slot = if key == b"FROM_RULE" {
                        &mut self.base
                    } else {
                        &mut self.depfile
                    };
                    *slot = values.first().copied();
                    if slot.is_none() {
                        missing.insert(key);
                    }
                    if let Some(extra) = values.get(1) {
                        return Err(unknown(extra));
                    }
                    continue;
                }
                b"OUTPUT" | b"COMMAND" | b"BYPRODUCTS" | b"DEPENDS" if !derived => {
                    if values.is_empty() && matches!(key, b"OUTPUT" | b"COMMAND") {
                        missing.insert(key);
                    }
                    match key {
                        b"OUTPUT" => self.outputs.extend(values),
                        b"COMMAND" => self.commands.push(list::join(values)),
                        b"DEPENDS" => self.depends.extend(values),
                        _ => self.byproducts.extend(values),
                    }
                    continue;
                }
                _ => return Err(unknown(key)),
            }
            if let Some(extra) = values.first() {
                return Err(unknown(extra));
            }
        }
        if !missing.is_empty() {
            let entries = missing
                .iter()
                .map(|key| format!("  {}: missing required value", BString::from(*key)))
                .collect::<Vec<_>>();
            let count = if entries.len() == 1 {
                "argument"
            } else {
                "arguments"
            };
            return Err(RuleError::fatal(format!(
                "given invalid {count}:\n\n{}",
                entries.join("\n")
            )));
        }
        Ok(())
    }

    fn properties(&self, name: &[u8]) -> Result<PropertyMap, RuleError> {
        if self.commands.is_empty() || self.outputs.is_empty() {
            return Err(RuleError::fatal(format!(
                "cannot create RULE \"{}\" because the mandatory options \"COMMAND\" or \"OUTPUT\" are missing.",
                BString::from(name)
            )));
        }
        let mut props = PropertyMap::default();
        props.set("OUTPUT".into(), list::join(&self.outputs));
        props.set("DEPENDS".into(), list::join(&self.depends));
        props.set("BYPRODUCTS".into(), list::join(&self.byproducts));
        props.set("DEPFILE".into(), self.depfile.unwrap_or_default().into());
        if let Some(command) = self.commands.first() {
            props.set("COMMAND".into(), command.clone());
        }
        props.set(
            "COMMAND_COUNT".into(),
            self.commands.len().to_string().into(),
        );
        for (index, command) in self.commands.iter().enumerate() {
            props.set(format!("COMMAND_{index}").into(), command.clone());
        }
        props.set("COMMAND_EXPAND_LISTS".into(), "1".into());
        props.set("VERBATIM".into(), "1".into());
        props.set(
            "OUTPUT_FILE_SET".into(),
            "__cmake_rule_<RULE>_<TARGET>_<FILE_SET>_outputs;SOURCES".into(),
        );
        Ok(props)
    }
}

fn unknown(key: &[u8]) -> RuleError {
    RuleError::fatal(format!(
        "given unknown argument: \"{}\".",
        BString::from(key)
    ))
}
