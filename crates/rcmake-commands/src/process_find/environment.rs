use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_eval::args::Parsed;
use rcmake_host::Host;

use super::cat;

pub(super) fn child_environment(
    host: &dyn Host,
    parsed: &Parsed,
) -> Result<Vec<(BString, BString)>, BString> {
    let mut env: BTreeMap<_, _> = host.env_all().into_iter().collect();
    for arg in parsed.multi("ENVIRONMENT") {
        if let Some((name, value)) = split(arg, b'=')
            && !name.is_empty()
        {
            env.insert(name.into(), value.into());
        }
    }
    let modifications = parsed.multi("ENVIRONMENT_MODIFICATION");
    if modifications.is_empty() {
        return Ok(env.into_iter().collect());
    }

    let mut original = BTreeMap::new();
    for arg in modifications {
        let (name, operation) = split(arg, b'=')
            .ok_or_else(|| error(b"Missing `=` after the variable name in: ", arg))?;
        let (operation, value) = split(operation, b':')
            .ok_or_else(|| error(b"Missing `:` after the operation in: ", arg))?;
        let previous = env.get(name);
        let initial = original
            .entry(BString::from(name))
            .or_insert_with(|| previous.cloned());
        let path_separator: &[u8] = if cfg!(windows) { b";" } else { b":" };
        let next = match operation {
            b"reset" => initial.clone(),
            b"set" => Some(value.into()),
            b"unset" => None,
            b"string_append" => Some(extend(previous, value, b"", false)),
            b"string_prepend" => Some(extend(previous, value, b"", true)),
            b"path_list_append" => Some(extend(previous, value, path_separator, false)),
            b"path_list_prepend" => Some(extend(previous, value, path_separator, true)),
            b"cmake_list_append" => Some(extend(previous, value, b";", false)),
            b"cmake_list_prepend" => Some(extend(previous, value, b";", true)),
            invalid => {
                return Err(error(
                    b"Unrecognized environment manipulation argument: ",
                    invalid,
                ));
            }
        };
        if let Some(next) = next {
            env.insert(name.into(), next);
        } else {
            env.remove(name);
        }
    }
    Ok(env.into_iter().collect())
}

fn extend(previous: Option<&BString>, value: &[u8], separator: &[u8], prepend: bool) -> BString {
    match previous.filter(|previous| !previous.is_empty()) {
        Some(previous) if prepend => cat(&[value, separator, previous]),
        Some(previous) => cat(&[previous, separator, value]),
        None => value.into(),
    }
}

fn split(value: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    let mut parts = value.splitn(2, |byte| *byte == separator);
    Some((parts.next()?, parts.next()?))
}

fn error(message: &[u8], argument: &[u8]) -> BString {
    cat(&[b"Error: ", message, argument])
}
