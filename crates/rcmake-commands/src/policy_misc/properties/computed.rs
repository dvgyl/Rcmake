use rcmake_base::{BString, list, truth};
use rcmake_diag::FrameKind;
use rcmake_eval::{Cx, RunMode, UserCommandKind};
use rcmake_model::{CacheType, DirId, GeneratorKind, PropScope, UsageField};

use super::{known_features, model_value};

pub(super) fn source_value(
    cx: &mut Cx<'_>,
    scope: &PropScope,
    name: &[u8],
    inherited: bool,
) -> Option<BString> {
    let PropScope::Source { path: source, .. } = scope else {
        return None;
    };
    model_value(cx, scope, name, inherited).or_else(|| match name {
        b"GENERATED" => Some("0".into()),
        b"LOCATION" => Some(source.clone()),
        _ => None,
    })
}

pub(super) fn directory_value(
    cx: &mut Cx<'_>,
    dir: DirId,
    name: &[u8],
    inherited: bool,
) -> Option<BString> {
    match name {
        b"VARIABLES" if dir == cx.current_dir_id() => Some(list::join(cx.variable_names())),
        b"CACHE_VARIABLES" => Some(cache_names(cx)),
        b"RULES" => Some(list::join(cx.project().directory_rules(dir))),
        b"MACROS" => Some(
            cx.project()
                .dir(dir)
                .props
                .get(name)
                .cloned()
                .unwrap_or_default(),
        ),
        b"LISTFILE_STACK" => Some(listfile_stack(cx, dir)),
        _ => model_value(cx, &PropScope::Directory(dir), name, inherited).or_else(|| {
            UsageField::from_name(name)
                .filter(|field| field.on_directory())
                .map(|_| BString::default())
        }),
    }
}

fn listfile_stack(cx: &mut Cx<'_>, dir: DirId) -> BString {
    let entry = cx.project().dir(dir).listfiles.first().cloned();
    let backtrace = cx.backtrace();
    let mut frames: Vec<_> = backtrace.frames().collect();
    frames.reverse();
    let mut files = Vec::new();
    let mut function = false;
    for frame in frames {
        if frame.kind == FrameKind::ModuleInclude {
            if entry
                .as_ref()
                .is_some_and(|entry| entry.as_slice() == &*frame.file)
            {
                files.clear();
            }
            files.push(&*frame.file);
        } else if function {
            files.push(&*frame.file);
        }
        function = frame
            .name
            .as_deref()
            .is_some_and(|name| cx.user_command_kind(name) == Some(UserCommandKind::Function));
    }
    if dir != cx.current_dir_id() {
        return entry.unwrap_or_default();
    }
    list::join(files)
}

pub(super) fn global_value(cx: &mut Cx<'_>, name: &[u8]) -> Option<BString> {
    match name {
        b"CMAKE_ROLE" => Some(
            match cx.mode() {
                RunMode::Script => "SCRIPT",
                RunMode::Configure => "PROJECT",
            }
            .into(),
        ),
        b"GENERATOR_IS_MULTI_CONFIG" => Some(boolean(generator_is_multi_config(cx))),
        b"IN_TRY_COMPILE" => Some(boolean(
            cx.global()
                .props
                .get(name)
                .is_some_and(|value| truth::is_on(value)),
        )),
        b"ENABLED_LANGUAGES" => Some(cx.global().props.get(name).cloned().unwrap_or_default()),
        b"COMMANDS" => Some(list::join(crate::compat::command_names(cx))),
        _ => known_features(name)
            .map(BString::from)
            .or_else(|| cx.global().props.get(name).cloned()),
    }
}

fn generator_is_multi_config(cx: &mut Cx<'_>) -> bool {
    if cx.mode() == RunMode::Script {
        return false;
    }
    if let Some(generator) = cx.project().generator {
        return generator.multi_config();
    }
    // Preload scripts and generators without a model kind use the cache entry.
    cx.cache().get(b"CMAKE_GENERATOR").is_some_and(|entry| {
        entry.ty == CacheType::Internal
            && (GeneratorKind::from_name(&entry.value).is_some_and(GeneratorKind::multi_config)
                || entry.value == b"Xcode"
                || entry.value.starts_with(b"Visual Studio "))
    })
}

fn cache_names(cx: &mut Cx<'_>) -> BString {
    let mut names: Vec<_> = cx.cache().iter().map(|(name, _)| name).collect();
    names.sort();
    list::join(names)
}

pub(super) fn boolean(value: bool) -> BString {
    if value { "1" } else { "0" }.into()
}
