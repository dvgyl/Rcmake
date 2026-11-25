use rcmake_model::TargetKind;
use rcmake_resolve::ide::{IdeFacts, IdeSource};

use super::{SourceConfig, flags_table, msbuild, string, windows};

pub(super) fn configure_source(
    item: &mut SourceConfig,
    source: &IdeSource,
    ide: &IdeFacts,
    language: &[u8],
) {
    let pch = ide
        .pch
        .iter()
        .find(|pch| pch.language == language && pch.companion.is_some());
    let Some(pch) = pch else {
        if ide.pch.iter().any(|pch| pch.companion.is_some()) && matches!(language, b"C" | b"CXX") {
            item.compile
                .get_or_insert_with(Default::default)
                .set("PrecompiledHeader", "NotUsing");
        }
        return;
    };
    let settings = item.compile.get_or_insert_with(Default::default);
    if source.skip_precompile_headers {
        settings.set("PrecompiledHeader", "NotUsing");
        return;
    }
    let create = pch.reuse_from.is_none() && source.path == pch.source;
    settings.set("PrecompiledHeader", if create { "Create" } else { "Use" });
    let header = msbuild(&windows(&string(&pch.header)));
    settings.set("PrecompiledHeaderFile", header.clone());
    settings.set(
        "PrecompiledHeaderOutputFile",
        msbuild(&windows(&string(&pch.artifact))),
    );
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
