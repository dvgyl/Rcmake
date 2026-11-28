use rcmake_model::TargetKind;
use rcmake_resolve::ide::{IdeFacts, IdePch, IdeSource};

use super::{SourceConfig, flags_table, msbuild, string, windows};

/// The precompiled header of `language` that MSVC builds, if any.
fn msvc_pch<'i>(ide: &'i IdeFacts, language: &[u8]) -> Option<&'i IdePch> {
    ide.pch
        .iter()
        .find(|pch| pch.language == language && pch.companion.is_some())
}

/// Set the target-level precompiled header settings, as CMake does.
///
/// The header of the linker language is used with `Use`. Without one, the
/// target settings are `NotUsing`. CMake writes these paths with `/`.
pub(super) fn configure_target(settings: &mut flags_table::Settings, ide: &IdeFacts, link: &[u8]) {
    match msvc_pch(ide, link) {
        Some(pch) => {
            settings.set("PrecompiledHeader", "Use");
            settings.set("PrecompiledHeaderFile", string(&pch.header));
            settings.set("PrecompiledHeaderOutputFile", string(&pch.artifact));
        }
        None => settings.set("PrecompiledHeader", "NotUsing"),
    }
}

/// Set the precompiled header settings of one source, as CMake does.
///
/// * The source that makes the header gets `Create`.
/// * A source of the linker language includes the header of the target
///   settings (`ForcedIncludeFiles`).
/// * A source of another language uses the header of its own language.
/// * A source that skips precompiled headers gets `NotUsing` when the target
///   settings use a header.
pub(super) fn configure_source(
    item: &mut SourceConfig,
    source: &IdeSource,
    ide: &IdeFacts,
    language: &[u8],
    link: &[u8],
) {
    let pch = msvc_pch(ide, language);
    let skip = pch.is_none() || source.skip_precompile_headers;
    let settings = item.compile.get_or_insert_with(Default::default);
    if source.skip_precompile_headers {
        settings.set("PrecompiledHeader", "NotUsing");
    }
    let Some(pch) = pch.filter(|_| !skip) else {
        if language != link && msvc_pch(ide, link).is_some() {
            settings.set("PrecompiledHeader", "NotUsing");
        }
        if settings.is_empty() {
            item.compile = None;
        }
        return;
    };
    let header = string(&pch.header);
    let create = pch.reuse_from.is_none() && source.path == pch.source;
    if create || language != link {
        settings.set("PrecompiledHeader", if create { "Create" } else { "Use" });
        settings.set("PrecompiledHeaderFile", header.clone());
        settings.set("PrecompiledHeaderOutputFile", string(&pch.artifact));
    }
    settings.list_unique("ForcedIncludeFiles", header);
    if let Some(path) = &pch.compile_pdb {
        settings.set("ProgramDataBaseFileName", msbuild(&windows(&string(path))));
    }
    if create && let Some(object) = &pch.companion {
        item.object = windows(&string(object));
    }
}

pub(super) fn link_reused(kind: TargetKind, ide: &IdeFacts, settings: &mut flags_table::Settings) {
    if kind == TargetKind::ObjectLibrary {
        return;
    }
    for pch in &ide.pch {
        if pch.reuse_from.is_some()
            && let Some(object) = &pch.companion
        {
            settings.list_unique("AdditionalDependencies", msbuild(&windows(&string(object))));
        }
    }
}
