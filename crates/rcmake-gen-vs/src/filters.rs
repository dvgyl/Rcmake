//! Source group definitions and file assignments for Solution Explorer.

use std::collections::BTreeSet;

use crate::{
    MSBUILD_NS, guid, msbuild,
    vcxproj::{Context, Plan},
    windows,
    xml::Writer,
};

pub(crate) fn render(ctx: &Context<'_>, plan: &Plan) -> String {
    let mut xml = Writer::new(true, "utf-8");
    xml.start(
        "Project",
        &[
            ("ToolsVersion", ctx.settings.version.tools_version()),
            ("xmlns", MSBUILD_NS),
        ],
    );
    let mut groups = BTreeSet::new();
    for item in plan.items.values() {
        let mut group = item.group.as_str();
        while !group.is_empty() && groups.insert(group) {
            group = group.rsplit_once('\\').map_or("", |(parent, _)| parent);
        }
    }
    if !groups.is_empty() {
        xml.start("ItemGroup", &[]);
        for group in groups {
            xml.start("Filter", &[("Include", &msbuild(group))]);
            let name = format!("{}|{group}", ctx.project.name);
            xml.text(
                "UniqueIdentifier",
                &[],
                &guid::for_name(ctx.model, &ctx.settings.bin, &name),
            );
            xml.end();
        }
        xml.end();
    }
    if !plan.items.is_empty() {
        xml.start("ItemGroup", &[]);
        for item in plan.items.values() {
            let path = msbuild(&windows(&item.path));
            let attrs = [("Include", path.as_str())];
            if item.group.is_empty() {
                xml.empty(item.kind.tag(), &attrs);
            } else {
                xml.start(item.kind.tag(), &attrs);
                xml.text("Filter", &[], &msbuild(&item.group));
                xml.end();
            }
        }
        xml.end();
    }
    xml.finish()
}
