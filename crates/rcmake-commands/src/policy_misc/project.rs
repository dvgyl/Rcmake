use std::collections::BTreeMap;

use rcmake_base::{BString, version};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::path;
use rcmake_model::{CacheEntry, CacheType};

use super::{quoted, status, values};

const METADATA: [&str; 5] = [
    "VERSION",
    "COMPAT_VERSION",
    "SPDX_LICENSE",
    "DESCRIPTION",
    "HOMEPAGE_URL",
];

pub(super) fn project(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let Some((name, tail)) = args.split_first() else {
        return Err(status("PROJECT called with incorrect number of arguments"));
    };
    crate::configure::project_include_hook(cx, b"CMAKE_PROJECT_INCLUDE_BEFORE")?;
    crate::configure::project_include_hook(
        cx,
        &field(b"CMAKE_PROJECT_", &field(name, b"_INCLUDE_BEFORE")),
    )?;
    let Arguments {
        metadata,
        unparsed,
        languages,
    } = parse_metadata(cx, tail)?;
    let version = metadata.get(b"VERSION".as_slice()).copied();
    let compat = metadata.get(b"COMPAT_VERSION".as_slice()).copied();
    if compat.is_some() && version.is_none() {
        return Err(status(
            "project with COMPAT_VERSION must also provide VERSION.",
        ));
    }
    for (key, value) in [("VERSION", version), ("COMPAT_VERSION", compat)] {
        if let Some(value) = value
            && !valid_version(value)
        {
            return Err(status(quoted(
                &format!("{key} \""),
                value,
                "\" format invalid.",
            )));
        }
    }
    if let (Some(version), Some(compat)) = (version, compat)
        && version::compare(compat, version).is_gt()
    {
        return Err(status(
            "COMPAT_VERSION must be less than or equal to VERSION",
        ));
    }

    let source = path::from_native(&cx.current_source_dir());
    let binary = path::from_native(&cx.current_binary_dir());
    let top_level = source == cx.var_or_empty(b"CMAKE_SOURCE_DIR");
    cx.set_var(b"PROJECT_NAME", (*name).into());
    cx.directory_props()
        .set("_RCMAKE_PROJECT_NAME".into(), (*name).into());
    if top_level {
        set_static(cx, b"CMAKE_PROJECT_NAME", name);
    }
    let top_level_value = if top_level { b"ON".as_slice() } else { b"OFF" };
    for (key, value) in [
        (b"_SOURCE_DIR".as_slice(), source.as_slice()),
        (b"_BINARY_DIR", binary.as_slice()),
        (b"_IS_TOP_LEVEL", top_level_value),
    ] {
        cx.set_var(&field(b"PROJECT", key), value.into());
        set_static(cx, &field(name, key), value);
    }

    let mut components = version.unwrap_or_default().split(|byte| *byte == b'.');
    for (key, value) in [
        (b"_VERSION".as_slice(), version.unwrap_or_default()),
        (b"_VERSION_MAJOR", components.next().unwrap_or_default()),
        (b"_VERSION_MINOR", components.next().unwrap_or_default()),
        (b"_VERSION_PATCH", components.next().unwrap_or_default()),
        (b"_VERSION_TWEAK", components.next().unwrap_or_default()),
    ] {
        for prefix in [b"PROJECT".as_slice(), name] {
            let var = field(prefix, key);
            if version.is_some() || cx.var_defined(&var) {
                cx.set_var(&var, value.into());
            }
        }
        if top_level {
            let var = field(b"CMAKE_PROJECT", key);
            if version.is_some() {
                set_static(cx, &var, value);
            } else if cx.var_defined(&var) {
                cx.set_var(&var, BString::default());
            }
        }
    }

    for key in METADATA.into_iter().filter(|key| *key != "VERSION") {
        let value = metadata.get(key.as_bytes()).copied().unwrap_or_default();
        let suffix = format!("_{key}");
        cx.set_var(&field(b"PROJECT", suffix.as_bytes()), value.into());
        cx.set_var(&field(name, suffix.as_bytes()), value.into());
        if top_level {
            set_static(cx, &field(b"CMAKE_PROJECT", suffix.as_bytes()), value);
        }
    }

    if cx.running_deferred() {
        cx.issue(
            MessageKind::Error,
            Effect::SetError,
            None,
            "Languages may not be enabled during deferred execution.".into(),
        );
        return Err(CmdError::Reported);
    }
    for group in language_groups(&unparsed, languages.as_deref()) {
        crate::configure::enable_languages(cx, &group, false)?;
    }

    crate::configure::project_include_hook(cx, b"CMAKE_PROJECT_INCLUDE")?;
    crate::configure::project_include_hook(
        cx,
        &field(b"CMAKE_PROJECT_", &field(name, b"_INCLUDE")),
    )?;
    Ok(Flow::Normal)
}

/// The `enable_languages` calls of `project()`: `C` and `CXX` without language
/// arguments, `NONE` for an empty `LANGUAGES` list, otherwise the legacy language names
/// and then the `LANGUAGES` list.
fn language_groups(unparsed: &[&[u8]], languages: Option<&[&[u8]]>) -> Vec<Vec<BString>> {
    let owned = |list: &[&[u8]]| list.iter().map(|lang| BString::from(*lang)).collect();
    match languages {
        None if unparsed.is_empty() => vec![vec!["C".into(), "CXX".into()]],
        Some([]) if unparsed.is_empty() => vec![vec!["NONE".into()]],
        languages => [unparsed, languages.unwrap_or_default()]
            .into_iter()
            .filter(|list| !list.is_empty())
            .map(owned)
            .collect(),
    }
}

/// Parsed `project()` arguments after the project name.
struct Arguments<'a> {
    metadata: BTreeMap<&'a [u8], &'a [u8]>,
    /// Arguments outside every keyword: legacy language names.
    unparsed: Vec<&'a [u8]>,
    /// The `LANGUAGES` list, when the keyword is present.
    languages: Option<Vec<&'a [u8]>>,
}

fn parse_metadata<'a>(cx: &mut Cx<'_>, args: &[&'a [u8]]) -> Result<Arguments<'a>, CmdError> {
    let mut metadata = BTreeMap::new();
    let mut args = args.iter().copied().peekable();
    let mut languages_seen = false;
    let mut in_languages = false;
    let mut bare_languages = Vec::new();
    let mut languages = Vec::new();
    while let Some(arg) = args.next() {
        if arg == b"LANGUAGES" {
            if languages_seen {
                return Err(status("LANGUAGES may be specified at most once."));
            }
            languages_seen = true;
            in_languages = true;
        } else if is_metadata(arg) {
            in_languages = false;
            if metadata.contains_key(arg) {
                return Err(status(quoted("", arg, " may be specified at most once.")));
            }
            let value = args.next_if(|next| *next != b"LANGUAGES" && !is_metadata(next));
            if value.is_none() {
                warn(
                    cx,
                    quoted(
                        "",
                        arg,
                        " keyword not followed by a value or was followed by a value that expanded to nothing.",
                    ),
                );
            }
            metadata.insert(arg, value.unwrap_or_default());
        } else if in_languages {
            languages.push(arg);
        } else {
            bare_languages.push(arg);
        }
    }
    let has_metadata = [b"VERSION".as_slice(), b"DESCRIPTION", b"HOMEPAGE_URL"]
        .iter()
        .any(|key| metadata.contains_key(key));
    if !bare_languages.is_empty() && has_metadata {
        if !languages_seen {
            return Err(status(
                "with VERSION, DESCRIPTION or HOMEPAGE_URL must use LANGUAGES before language names.",
            ));
        }
        warn(
            cx,
            quoted(
                "the following parameters must be specified after LANGUAGES keyword: ",
                &bare_languages.join(b";".as_slice()),
                ".",
            ),
        );
    }
    Ok(Arguments {
        metadata,
        unparsed: bare_languages,
        languages: languages_seen.then_some(languages),
    })
}

fn is_metadata(arg: &[u8]) -> bool {
    METADATA.iter().any(|key| key.as_bytes() == arg)
}

fn warn(cx: &mut Cx<'_>, body: BString) {
    cx.issue(MessageKind::Warning, Effect::None, None, body);
}

fn valid_version(version: &[u8]) -> bool {
    let parts = version.split(|byte| *byte == b'.');
    version.is_empty()
        || (parts.clone().count() <= 4
            && parts
                .into_iter()
                .all(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit)))
}

fn field(prefix: &[u8], suffix: &[u8]) -> BString {
    [prefix, suffix].concat().into()
}

fn set_static(cx: &mut Cx<'_>, name: &[u8], value: &[u8]) {
    cx.set_var(name, value.into());
    cx.cache().insert(
        name.into(),
        CacheEntry {
            value: value.into(),
            ty: CacheType::Static,
            help: "Value Computed by CMake".into(),
            advanced: false,
            props: Default::default(),
        },
    );
}
