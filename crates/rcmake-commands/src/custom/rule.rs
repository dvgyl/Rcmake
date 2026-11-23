use rcmake_base::BString;
use rcmake_eval::{CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_json::{Value, write_compact};
use rcmake_model::ProjectModel;

use super::status;

pub(super) fn add(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some((name, args)) = args.split_first() else {
        return Err(status("called with incorrect number of arguments."));
    };
    let name = &name.value;
    if name.is_empty()
        || !name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(byte))
    {
        return Err(status(format!("invalid name for RULE: {name}.")));
    }
    if name.iter().any(u8::is_ascii_uppercase)
        && !name.iter().any(u8::is_ascii_lowercase)
        && !name.iter().any(u8::is_ascii_digit)
    {
        return Err(status("names in all uppercase are reserved for CMake."));
    }

    let mut has_output = false;
    let mut has_command = false;
    let mut base = None;
    let mut global = false;
    let mut configurator = false;
    let mut args_iter = args.iter().peekable();
    while let Some(arg) = args_iter.next() {
        match arg.value.as_slice() {
            b"OUTPUT" => has_output |= args_iter.peek().is_some_and(|arg| !keyword(&arg.value)),
            b"COMMAND" => has_command |= args_iter.peek().is_some_and(|arg| !keyword(&arg.value)),
            b"FROM_RULE" => base = args_iter.next().map(|arg| &arg.value),
            b"GLOBAL" => global = true,
            b"CONFIGURATOR" => configurator = true,
            b"FOR_FILE_SET" | b"FOR_SOURCE" if configurator => {
                if let Some(function) = args_iter.next()
                    && !cx.command_exists(&function.value)
                {
                    return Err(status(format!(
                        "command specified for \"{}\" does not exist: {}.",
                        arg.value, function.value
                    )));
                }
            }
            _ => {}
        }
    }
    if base.is_none() && !(has_output && has_command) {
        return Err(status(format!(
            "cannot create RULE \"{name}\" because the mandatory options \"COMMAND\" or \"OUTPUT\" are missing."
        )));
    }

    let dir = cx.current_dir_id();
    let project = cx.project();
    let key = rule_key(name);
    if project.dir(dir).props.get(&key).is_some() || project.global.props.get(&key).is_some() {
        return Err(status(format!(
            "cannot create RULE \"{name}\" because another RULE with the same name already exists."
        )));
    }
    if let Some(base) = base
        && !rule_visible(project, dir, base)
    {
        return Err(status(format!(
            "cannot create RULE \"{name}\" because the RULE \"{base}\" does not exist or is not accessible."
        )));
    }

    // Keep the template arguments intact for the generator and file set commands.
    let definition = Value::Array(
        args.iter()
            .map(|arg| Value::string(arg.value.clone()))
            .collect(),
    );
    let definition = write_compact(&definition);
    if global {
        project.global.props.set(key, definition);
    } else {
        project.dir_mut(dir).props.set(key, definition);
    }
    Ok(Flow::Normal)
}

fn keyword(value: &[u8]) -> bool {
    matches!(
        value,
        b"OUTPUT"
            | b"COMMAND"
            | b"DEPENDS"
            | b"BYPRODUCTS"
            | b"DEPFILE"
            | b"CONFIGURATOR"
            | b"FOR_FILE_SET"
            | b"FOR_SOURCE"
            | b"FROM_RULE"
            | b"GLOBAL"
    )
}

fn rule_key(name: &BString) -> BString {
    [b"_RCMAKE_CUSTOM_RULE_", name.as_slice()].concat().into()
}

fn rule_visible(project: &ProjectModel, mut dir: rcmake_model::DirId, name: &BString) -> bool {
    let key = rule_key(name);
    if project.global.props.get(&key).is_some() {
        return true;
    }
    loop {
        let directory = project.dir(dir);
        if directory.props.get(&key).is_some() {
            return true;
        }
        let Some(parent) = directory.parent else {
            return false;
        };
        dir = parent;
    }
}
