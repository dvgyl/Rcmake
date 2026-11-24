use rcmake_base::BString;
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::DefinedProperty;

use super::{quoted, values};

pub(in crate::policy_misc) fn define(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [scope, b"PROPERTY", name, tail @ ..] = args.as_slice() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    if !matches!(
        *scope,
        b"GLOBAL"
            | b"DIRECTORY"
            | b"TARGET"
            | b"SOURCE"
            | b"TEST"
            | b"VARIABLE"
            | b"CACHED_VARIABLE"
    ) {
        return Err(CmdError::status(quoted("given invalid scope ", scope, ".")));
    }
    let mut entry = DefinedProperty {
        scope: (*scope).into(),
        name: (*name).into(),
        inherited: false,
        brief_docs: BString::default(),
        full_docs: BString::default(),
        initialize_from_variable: None,
    };
    let mut docs = None;
    let mut tail = tail.iter().copied();
    while let Some(arg) = tail.next() {
        match arg {
            b"INHERITED" => {
                entry.inherited = true;
                docs = None;
            }
            b"BRIEF_DOCS" => docs = Some(&mut entry.brief_docs),
            b"FULL_DOCS" => docs = Some(&mut entry.full_docs),
            b"INITIALIZE_FROM_VARIABLE" => {
                let variable = tail.next().ok_or_else(|| {
                    CmdError::status("missing variable name after INITIALIZE_FROM_VARIABLE")
                })?;
                validate_initializer(scope, name, variable)?;
                entry.initialize_from_variable = Some(variable.into());
                docs = None;
            }
            other => {
                let Some(target) = docs.as_deref_mut() else {
                    return Err(CmdError::status(quoted(
                        "given invalid argument \"",
                        other,
                        "\".",
                    )));
                };
                target.extend_from_slice(other);
            }
        }
    }
    let definitions = &mut cx.global().defined_props;
    if !definitions
        .iter()
        .any(|existing| existing.scope == entry.scope && existing.name == entry.name)
    {
        definitions.push(entry);
    }
    Ok(Flow::Normal)
}

fn validate_initializer(scope: &[u8], property: &[u8], variable: &[u8]) -> Result<(), CmdError> {
    if scope != b"TARGET" {
        return Err(CmdError::status(
            "Scope must be TARGET if INITIALIZE_FROM_VARIABLE is specified",
        ));
    }
    if variable.starts_with(b"CMAKE_") || variable.starts_with(b"_CMAKE_") {
        return Err(CmdError::status(quoted(
            "variable name \"",
            variable,
            "\" is reserved",
        )));
    }
    if !variable.ends_with(property) {
        return Err(CmdError::status(
            [
                b"Variable name \"".as_slice(),
                variable,
                b"\" does not end with property name \"",
                property,
                b"\"",
            ]
            .concat(),
        ));
    }
    if !property.contains(&b'_') {
        return Err(CmdError::status(quoted(
            "Property name \"",
            property,
            "\" defined with INITIALIZE_FROM_VARIABLE does not contain underscore",
        )));
    }
    Ok(())
}
