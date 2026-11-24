use rcmake_base::{BString, list};
use rcmake_eval::{CmdError, Cx};
use rcmake_model::PropScope;

use super::computed::{directory_value, global_value, source_value};
use super::scope::{alias_value, check_directory, find_target, source_scope};
use super::{is_inherited, model_value, quoted};

pub(in crate::policy_misc) fn print_value(
    cx: &mut Cx<'_>,
    scope: &[u8],
    object: &[u8],
    name: &[u8],
) -> Result<Option<Option<BString>>, CmdError> {
    let dir = cx.current_dir_id();
    let inherited = is_inherited(cx, scope, name);
    let value = match scope {
        b"TARGET" => {
            let id = find_target(cx, object)?;
            if let Some(config) = location_config(name) {
                let model = cx.project();
                if !model
                    .target(id)
                    .is_some_and(rcmake_model::Target::is_imported)
                {
                    cx.issue(
                        rcmake_diag::MessageKind::Warning,
                        rcmake_diag::Effect::None,
                        None,
                        format!(
                            "The {} property may not be read from non-imported target \"{}\"; skipping.",
                            BString::from(name),
                            BString::from(object)
                        ).into(),
                    );
                    return Ok(None);
                }
                let owner = PropScope::Target(id);
                let configuration = config
                    .filter(|name| !name.is_empty())
                    .map(|name| name.to_ascii_uppercase())
                    .or_else(|| {
                        let value = cx.var_or_empty(b"CMAKE_BUILD_TYPE");
                        (!value.is_empty()).then(|| value.to_ascii_uppercase())
                    });
                let model = cx.project();
                configuration
                    .and_then(|config| {
                        model.get_prop(&owner, &[b"IMPORTED_LOCATION_", config.as_slice()].concat())
                    })
                    .or_else(|| model.get_prop(&owner, b"IMPORTED_LOCATION"))
                    .or_else(|| {
                        let configurations = model.get_prop(&owner, b"IMPORTED_CONFIGURATIONS")?;
                        list::split(&configurations, list::Empty::Drop)
                            .into_iter()
                            .find_map(|config| {
                                model.get_prop(
                                    &owner,
                                    &[
                                        b"IMPORTED_LOCATION_".as_slice(),
                                        &config.to_ascii_uppercase(),
                                    ]
                                    .concat(),
                                )
                            })
                    })
            } else {
                alias_value(cx, object, name)
                    .or_else(|| model_value(cx, &PropScope::Target(id), name, inherited))
            }
        }
        b"DIRECTORY" => {
            let id = check_directory(cx, &[object])?;
            directory_value(cx, id, name, inherited)
        }
        b"SOURCE" => {
            let owner = source_scope(cx, dir, object);
            source_value(cx, &owner, name, inherited).or_else(|| {
                (name == b"LANGUAGE").then(|| {
                    let extension = object.rsplit(|&b| b == b'.').next().unwrap_or_default();
                    let languages = global_value(cx, b"ENABLED_LANGUAGES").unwrap_or_default();
                    for language in list::split(&languages, list::Empty::Drop) {
                        let key =
                            [b"CMAKE_", language.as_slice(), b"_SOURCE_FILE_EXTENSIONS"].concat();
                        if list::split(&cx.var_or_empty(&key), list::Empty::Drop)
                            .iter()
                            .any(|item| item == extension)
                        {
                            return language;
                        }
                    }
                    BString::default()
                })
            })
        }
        b"TEST" => {
            if cx.project().find_test(dir, object).is_none() {
                return Err(CmdError::status(quoted(
                    "given TEST name that does not exist: ",
                    object,
                    "",
                )));
            }
            model_value(
                cx,
                &PropScope::Test {
                    dir,
                    name: object.into(),
                },
                name,
                inherited,
            )
        }
        b"CACHE" => cx
            .project()
            .get_prop(&PropScope::Cache(object.into()), name),
        _ => None,
    };
    Ok(Some(value))
}

// The suffix form excludes the IMPORTED_LOCATION property.
fn location_config(name: &[u8]) -> Option<Option<&[u8]>> {
    if name == b"LOCATION" {
        Some(None)
    } else if let Some(config) = name.strip_prefix(b"LOCATION_") {
        Some(Some(config))
    } else if let Some(config) = name
        .strip_suffix(b"_LOCATION")
        .filter(|&p| p != b"IMPORTED")
    {
        Some(Some(config))
    } else {
        None
    }
}
