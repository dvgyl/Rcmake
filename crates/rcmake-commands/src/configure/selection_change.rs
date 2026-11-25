// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::sync::Mutex;

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_eval::Cx;
use rcmake_host::path;

const TOOLCHAIN_FILE: &[u8] = b"CMAKE_TOOLCHAIN_FILE";

/// Platform selections that changed during a warm configure.
pub struct ConfigureSelection {
    changes: Mutex<BTreeMap<BString, BString>>,
}

impl ConfigureSelection {
    /// Start tracking changes to the configure selections.
    #[must_use]
    pub fn new(_previous_toolchain: BString) -> Self {
        Self {
            changes: Mutex::default(),
        }
    }

    /// Take the changed cache variables in name order.
    pub fn take_changes(&self) -> BTreeMap<BString, BString> {
        std::mem::take(
            &mut *self
                .changes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Check for pending changes before the configure result is published.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self
            .changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    fn record(&self, key: &[u8], value: BString) {
        self.changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.into(), value);
    }
}

pub(super) fn toolchain_changed(cx: &mut Cx<'_>) -> bool {
    let Some(selection) = cx
        .session_data::<std::sync::Arc<ConfigureSelection>>()
        .cloned()
    else {
        return false;
    };
    let Some(selected) = cx.var(TOOLCHAIN_FILE) else {
        return false;
    };
    if [
        b"_CMAKE_INPUT_TOOLCHAIN_FILE".as_slice(),
        b"_CMAKE_SYSTEM_TOOLCHAIN_FILE",
    ]
    .iter()
    .any(|name| cx.var(name).as_ref() == Some(&selected))
    {
        return false;
    }
    selection.record(TOOLCHAIN_FILE, selected);
    true
}

pub(super) fn record_compiler(
    cx: &mut Cx<'_>,
    compiler: &[u8],
    arg_key: &[u8],
    requested: Option<BString>,
    args: Option<BString>,
) {
    let (Some(requested), Some(selection)) = (
        requested,
        cx.session_data::<std::sync::Arc<ConfigureSelection>>()
            .cloned(),
    ) else {
        return;
    };
    let loaded = cx.var_or_empty(compiler);
    let loaded_args = cx.var_or_empty(arg_key);
    let values = list::split(&requested, list::Empty::Drop);
    let path = values
        .first()
        .map_or(requested.as_slice(), |path| path.as_slice());
    let norm = |path: &[u8]| {
        if path.is_empty() {
            BString::default()
        } else {
            Lexical::new(path).normalize()
        }
    };
    let native = path::to_native(path);
    let resolved = (!path.is_empty() && native.file_name() == Some(native.as_os_str()))
        .then(|| crate::vars_list::separate::find_program(cx, path))
        .flatten();
    let path = norm(resolved.as_deref().map_or(path, |value| value.as_slice()));
    let requested_args = match values.as_slice() {
        [_, args @ ..] if !args.is_empty() => Some(
            args.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ")
                .into(),
        ),
        _ => args,
    };
    if path != norm(&loaded)
        || requested_args
            .as_ref()
            .is_some_and(|args| args != &loaded_args)
    {
        selection.record(compiler, requested);
        if let Some(args) = requested_args {
            selection.record(arg_key, args);
        }
    }
}
