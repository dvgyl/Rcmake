use std::collections::BTreeMap;

use rcmake_base::{BString, version};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::path;
use rcmake_model::{CacheEntry, CacheType, DirId};

use super::{quoted, values};

const METADATA: [&str; 5] = [
    "VERSION",
    "COMPAT_VERSION",
    "SPDX_LICENSE",
    "DESCRIPTION",
    "HOMEPAGE_URL",
];

pub(super) fn project(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let result = configure_project(cx, args);
    crate::configure::finish_probe_prelude(cx);
    result
}

fn configure_project(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let Some((name, tail)) = args.split_first() else {
        return Err(CmdError::status(
            "PROJECT called with incorrect number of arguments",
        ));
    };
    // `cmMakefile::Configure` adds this marker to the `project()` call that it injects.
    let tail = match tail {
        [b"__CMAKE_INJECTED_PROJECT_COMMAND__", rest @ ..] => rest,
        _ => tail,
    };
    let top_level = cx.current_dir_id() == DirId::ROOT;
    if name == &b"LANGUAGES".as_slice() || is_metadata(name) {
        author_warning(
            cx,
            quoted(
                "project() called with '",
                name,
                "' as first argument. The first parameter should be the project name, not a keyword argument. See the cmake-commands(7) manual for correct usage of the project() command.",
            ),
        );
    }
    if top_level && !cx.var_defined(b"CMAKE_MINIMUM_REQUIRED_VERSION") {
        author_warning(cx, "cmake_minimum_required() should be called prior to this top-level project() call. Please see the cmake-commands(7) manual for usage documentation of both commands.".into());
    }
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
        return Err(crate::command_error(
            cx,
            "project with COMPAT_VERSION must also provide VERSION.",
        ));
    }
    for (key, value) in [("VERSION", version), ("COMPAT_VERSION", compat)] {
        if let Some(value) = value
            && !valid_version(value)
        {
            return Err(crate::command_error(
                cx,
                quoted(&format!("{key} \""), value, "\" format invalid."),
            ));
        }
    }
    let normalized;
    let version = if cx.policy(PolicyId::new(96)) == PolicyStatus::New {
        version
    } else {
        normalized = version.map(normalize_version);
        normalized.as_deref()
    };
    if let (Some(version), Some(compat)) = (version, compat)
        && version::compare(compat, version).is_gt()
    {
        return Err(crate::command_error(
            cx,
            "COMPAT_VERSION must be less than or equal to VERSION",
        ));
    }

    let source = path::from_native(&cx.current_source_dir());
    let binary = path::from_native(&cx.current_binary_dir());
    cx.set_var(b"PROJECT_NAME", (*name).into());
    cx.directory_props()
        .set("_RCMAKE_PROJECT_NAME".into(), (*name).into());
    set_top_level(cx, b"CMAKE_PROJECT_NAME", name, top_level);
    let top_level_value = if top_level { b"ON".as_slice() } else { b"OFF" };
    for (key, value) in [
        (b"_SOURCE_DIR".as_slice(), source.as_slice()),
        (b"_BINARY_DIR", binary.as_slice()),
        (b"_IS_TOP_LEVEL", top_level_value),
    ] {
        cx.set_var(&field(b"PROJECT", key), value.into());
        let var = field(name, key);
        if cx.policy(PolicyId::new(180)) == PolicyStatus::New || cx.var_normal_only(&var).is_some()
        {
            cx.set_var(&var, value.into());
        }
        set_static(cx, &var, value);
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
            if version.is_some() || !cx.var_or_empty(&var).is_empty() {
                cx.set_var(&var, value.into());
            }
        }
        let var = field(b"CMAKE_PROJECT", key);
        if version.is_some() {
            set_top_level(cx, &var, value, top_level);
        } else if top_level && !cx.var_or_empty(&var).is_empty() {
            cx.set_var(&var, BString::default());
        }
    }

    for key in METADATA.into_iter().filter(|key| *key != "VERSION") {
        let value = metadata.get(key.as_bytes()).copied().unwrap_or_default();
        let suffix = format!("_{key}");
        cx.set_var(&field(b"PROJECT", suffix.as_bytes()), value.into());
        cx.set_var(&field(name, suffix.as_bytes()), value.into());
        set_top_level(
            cx,
            &field(b"CMAKE_PROJECT", suffix.as_bytes()),
            value,
            top_level,
        );
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
    let mut seen = std::collections::BTreeSet::new();
    let mut missing = Vec::new();
    let mut languages_seen = false;
    let mut in_languages = false;
    let mut bare_languages = Vec::new();
    let mut languages = Vec::new();
    while let Some(arg) = args.next() {
        if (arg == b"LANGUAGES" || is_metadata(arg)) && !seen.insert(arg) {
            return Err(crate::command_error(
                cx,
                quoted("", arg, " may be specified at most once."),
            ));
        }
        if arg == b"LANGUAGES" {
            languages_seen = true;
            in_languages = true;
        } else if is_metadata(arg) {
            in_languages = false;
            if let Some(value) = args.next_if(|next| *next != b"LANGUAGES" && !is_metadata(next)) {
                metadata.insert(arg, value);
            } else {
                if matches!(arg, b"VERSION" | b"COMPAT_VERSION") {
                    metadata.insert(arg, b"");
                }
                missing.push(arg);
            }
        } else if in_languages {
            languages.push(arg);
        } else {
            bare_languages.push(arg);
        }
    }
    let has_metadata = [b"VERSION".as_slice(), b"DESCRIPTION", b"HOMEPAGE_URL"]
        .iter()
        .any(|key| metadata.contains_key(key));
    for arg in missing {
        warn(
            cx,
            quoted(
                "",
                arg,
                " keyword not followed by a value or was followed by a value that expanded to nothing.",
            ),
        );
    }
    if !bare_languages.is_empty() {
        if languages_seen {
            warn(
                cx,
                quoted(
                    "the following parameters must be specified after LANGUAGES keyword: ",
                    &bare_languages.join(b", ".as_slice()),
                    ".",
                ),
            );
        } else if has_metadata {
            return Err(crate::command_error(
                cx,
                "project with VERSION, DESCRIPTION or HOMEPAGE_URL must use LANGUAGES before language names.",
            ));
        }
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

fn author_warning(cx: &mut Cx<'_>, body: BString) {
    cx.issue(
        MessageKind::AuthorWarning,
        Effect::None,
        Some(DiagCategory::Author),
        body,
    );
}

fn normalize_version(version: &[u8]) -> Vec<u8> {
    if version.is_empty() {
        return Vec::new();
    }
    version
        .split(|&byte| byte == b'.')
        .map(|part| {
            let number = part.iter().fold(0_u32, |value, &digit| {
                value.wrapping_mul(10).wrapping_add(u32::from(digit - b'0'))
            });
            number.to_string()
        })
        .collect::<Vec<_>>()
        .join(".")
        .into_bytes()
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

fn set_top_level(cx: &mut Cx<'_>, name: &[u8], value: &[u8], top_level: bool) {
    if top_level || !cx.var_defined(name) {
        if cx.var_normal_only(name).is_some() {
            cx.unset_var(name);
        }
        set_static(cx, name, value);
    }
}

fn set_static(cx: &mut Cx<'_>, name: &[u8], value: &[u8]) {
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
