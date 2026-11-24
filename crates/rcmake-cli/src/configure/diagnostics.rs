use rcmake_base::{BString, truth::is_on};
use rcmake_diag::{DiagAction, DiagCategory, DiagState};
use rcmake_host::{Host, Stream};
use rcmake_model::{Cache, CacheType};

use crate::{apply_warning_flag, cache};

const DIAG_INIT: &[u8] = b"CMAKE_DIAGNOSTIC_INIT";

pub(crate) fn initialize_diagnostics(
    host: &dyn Host,
    cache: &mut Cache,
    preset: &[BString],
    cli: &[BString],
    configure: bool,
) -> DiagState {
    let mut state = DiagState::default();
    if configure {
        state.set(DiagCategory::UnusedCli, DiagAction::Warn, false);
    }
    if let Some(entry) = cache.get(DIAG_INIT) {
        for part in rcmake_base::list::split(&entry.value, rcmake_base::list::Empty::Drop) {
            let Some(eq) = part.iter().position(|b| *b == b'=') else {
                continue;
            };
            if let (Some(cat), Some(action)) = (
                DiagCategory::parse(part.get(..eq).unwrap_or_default()),
                DiagAction::parse(part.get(eq + 1..).unwrap_or_default()),
            ) {
                state.set(cat, action, false);
            }
        }
    } else {
        for (name, cat, action, when) in [
            (
                b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS".as_slice(),
                DiagCategory::Author,
                DiagAction::Ignore,
                true,
            ),
            (
                b"CMAKE_SUPPRESS_DEVELOPER_ERRORS",
                DiagCategory::Author,
                DiagAction::SendError,
                false,
            ),
        ] {
            if cache
                .get(name)
                .is_some_and(|entry| is_on(&entry.value) == when)
            {
                state.set(cat, action, false);
            }
        }
    }
    for (name, on, off) in [
        (
            b"CMAKE_WARN_DEPRECATED".as_slice(),
            DiagAction::Warn,
            DiagAction::Ignore,
        ),
        (
            b"CMAKE_ERROR_DEPRECATED",
            DiagAction::SendError,
            DiagAction::Warn,
        ),
    ] {
        if let Some(entry) = cache.get(name) {
            let current = state.get(DiagCategory::Deprecated);
            let next = if is_on(&entry.value) {
                current.max(on)
            } else {
                current.min(off)
            };
            state.set(DiagCategory::Deprecated, next, false);
        }
    }
    let initial = state;
    for flag in preset.iter().chain(cli) {
        if let Some(prefix) = flag
            .strip_prefix(b"-W")
            .and_then(|v| v.strip_suffix(b"dev"))
            && matches!(prefix, b"" | b"no-" | b"error=" | b"no-error=")
        {
            let prefix = String::from_utf8_lossy(prefix);
            host.write_out(
                Stream::Stderr,
                format!("The {prefix}dev option is deprecated.  Use -W{prefix}author instead.\n")
                    .as_bytes(),
            );
        }
        apply_warning_flag(flag, &mut state);
    }
    if configure {
        let mut serialized = String::new();
        for cat in DiagCategory::ALL {
            if !serialized.is_empty() {
                serialized.push(';');
            }
            serialized.push_str(cat.as_str());
            serialized.push('=');
            serialized.push_str(state.get(cat).as_str());
        }
        cache::insert(
            cache,
            DIAG_INIT,
            serialized.into(),
            Some(CacheType::Internal),
        );
    }
    for (cat, vars) in [
        (
            DiagCategory::Author,
            [
                b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS".as_slice(),
                b"CMAKE_SUPPRESS_DEVELOPER_ERRORS",
            ],
        ),
        (
            DiagCategory::Deprecated,
            [
                b"CMAKE_WARN_DEPRECATED".as_slice(),
                b"CMAKE_ERROR_DEPRECATED",
            ],
        ),
    ] {
        let action = state.get(cat);
        if (configure && cat == DiagCategory::Deprecated)
            || initial.get(cat) != action
            || preset.iter().chain(cli).any(|f| {
                f.ends_with(b"dev")
                    || f.ends_with(b"author")
                    || (cat == DiagCategory::Deprecated && f.ends_with(b"deprecated"))
            })
        {
            let enabled = if cat == DiagCategory::Author {
                [action == DiagAction::Ignore, action < DiagAction::SendError]
            } else {
                [action >= DiagAction::Warn, action >= DiagAction::SendError]
            };
            for (name, value) in vars.into_iter().zip(enabled) {
                cache::insert(
                    cache,
                    name,
                    (if configure && cat == DiagCategory::Deprecated {
                        if value { "ON" } else { "OFF" }
                    } else if value {
                        "TRUE"
                    } else {
                        "FALSE"
                    })
                    .into(),
                    Some(CacheType::Internal),
                );
                if configure
                    && let Some(option) = legacy_warning_option(name)
                    && let Some(entry) = cache.get_mut(name)
                {
                    entry.help = format!("Deprecated.  Use {option} instead.").into();
                }
            }
        }
    }
    state
}

fn legacy_warning_option(name: &[u8]) -> Option<&'static str> {
    match name {
        b"CMAKE_WARN_DEPRECATED" => Some("-W[no-]deprecated"),
        b"CMAKE_ERROR_DEPRECATED" => Some("-W[no-]error=deprecated"),
        _ => None,
    }
}

pub(super) fn warn_legacy_define(host: &dyn Host, name: &[u8]) {
    if let Some(option) = legacy_warning_option(name) {
        let variable = BString::from(name);
        host.write_out(
            Stream::Stderr,
            format!("The {variable} variable is deprecated.  Use {option} instead.\n").as_bytes(),
        );
    }
}
