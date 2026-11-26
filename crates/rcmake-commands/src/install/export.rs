//! The `export()` command.

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::{from_native, to_native};

use super::{
    check_file, export_options, fail, keyword, metadata, package_dependencies, record_request,
    target_ids, unknown, value,
};

pub(super) fn command(cx: &mut Cx<'_>, input: &[ExpandedArg]) -> CmdResult {
    let input: Vec<_> = input.iter().map(|arg| arg.value.as_slice()).collect();
    let [signature, rest @ ..] = input.as_slice() else {
        return Err(fail("called with no arguments."));
    };
    if matches!(*signature, b"PACKAGE_INFO" | b"SBOM") {
        metadata::export(cx, signature, rest)?;
        return Ok(Flow::Normal);
    }
    if *signature == b"PACKAGE" {
        let [name] = rest else {
            return Err(fail("PACKAGE requires one package name."));
        };
        if name.is_empty()
            || !name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(byte))
        {
            return Err(fail(
                [b"PACKAGE given invalid package name \"", *name, b"\"."].concat(),
            ));
        }
        let enabled = if cx.policy(PolicyId::new(90)) == PolicyStatus::New {
            cx.var_is_on(b"CMAKE_EXPORT_PACKAGE_REGISTRY")
        } else {
            !cx.var_is_on(b"CMAKE_EXPORT_NO_PACKAGE_REGISTRY")
        };
        if enabled {
            record_request(cx, "_RCMAKE_PACKAGE_EXPORT", &input);
        }
        return Ok(Flow::Normal);
    }
    if *signature == b"SETUP" {
        return package_dependencies::setup(cx, rest);
    }
    let packages = *signature == b"EXPORT" && package_dependencies::enabled(cx);
    let normalized;
    let input = if *signature == b"EXPORT" {
        normalized = export_options::normalize(cx, &input)?;
        normalized.as_slice()
    } else {
        input.as_slice()
    };
    let rest = input.get(1..).unwrap_or_default();
    let mut args = rest.iter().copied().peekable();
    let name = match *signature {
        b"EXPORT" => Some(value(&mut args, b"EXPORT")?),
        b"TARGETS" => {
            let names =
                std::iter::from_fn(|| args.next_if(|token| !export_keyword(token))).collect();
            target_ids(cx, names, false)?;
            None
        }
        _ => return Err(unknown(signature)),
    };
    let mut file = None;
    let mut android = None;
    let mut append = false;
    while let Some(key) = args.next() {
        match key {
            b"FILE" => {
                let name = value(&mut args, key)?;
                file = Some(BString::from(name));
            }
            b"ANDROID_MK" if name.is_none() => {
                android = args.next_if(|token| !export_keyword(token) && !token.is_empty());
                if android.is_none() {
                    return Err(fail("ANDROID_MK given no file name."));
                }
            }
            b"NAMESPACE" => {
                if args.next_if(|token| !export_keyword(token)).is_none() {
                    return Err(fail(
                        [
                            *signature,
                            b" given invalid argument:\n  NAMESPACE: missing required value",
                        ]
                        .concat(),
                    ));
                }
            }
            b"CXX_MODULES_DIRECTORY" => {
                value(&mut args, key)?;
            }
            b"EXPORT_PACKAGE_DEPENDENCIES" if name.is_some() && packages => {}
            b"APPEND" if name.is_none() => append = true,
            b"EXPORT_LINK_INTERFACE_LIBRARIES" if name.is_none() => {}
            _ if name.is_some() => {
                return Err(fail(
                    [b"EXPORT given unknown argument: \"", key, b"\"."].concat(),
                ));
            }
            _ => return Err(unknown(key)),
        }
    }
    if let Some(name) = name
        && !cx.project().exports.contains_key(name)
    {
        return Err(fail([b"Export set \"", name, b"\" not found."].concat()));
    }
    if android.is_none()
        && let Some(file) = &file
    {
        check_file(file)?;
    }
    let file = android
        .map(BString::from)
        .or(file)
        .or_else(|| name.map(|name| [name, b".cmake"].concat().into()));
    if let Some(file) = file {
        let path = to_native(&file);
        if path.is_absolute() && !crate::compat::can_write_project_file(cx, &path) {
            cx.set_fatal();
            return Err(fail(format!(
                r#"attempted to write the file "{file}" into a source directory."#
            )));
        }
        let output = Lexical::new(&from_native(
            &cx.current_binary_dir().join(to_native(&file)),
        ))
        .normalize();
        let key = [b"_RCMAKE_EXPORT_OUTPUT_".as_slice(), output.as_slice()].concat();
        if !append && cx.global().props.get(&key).is_some() {
            export_options::repeat(cx, &file)?;
        }
        if *signature == b"TARGETS" {
            let target_key = [b"_RCMAKE_TARGET_EXPORT_OUTPUT_".as_slice(), &output].concat();
            cx.global().props.set(target_key.into(), "1".into());
        }
        cx.global().props.set(key.into(), b"1".as_slice().into());
        record_request(cx, "_RCMAKE_BUILD_EXPORT", input);
    }
    Ok(Flow::Normal)
}

fn export_keyword(token: &[u8]) -> bool {
    token == b"ANDROID_MK" || keyword(token)
}
