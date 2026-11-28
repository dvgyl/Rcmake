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
            let name = format!("SG_Filter_{group}");
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
            let path = crate::msbuild_item(&windows(&item.path));
            let attrs = [("Include", path.as_str())];
            if item.group.is_empty() {
                xml.empty(item.tag(), &attrs);
            } else {
                xml.start(item.tag(), &attrs);
                xml.text("Filter", &[], &msbuild(&item.group));
                xml.end();
            }
        }
        xml.end();
    }
    if let Some(props) = ctx
        .project
        .target
        .and_then(|id| ctx.model.target(id))
        .and_then(|target| target.props.get(b"VS_FILTER_PROPS"))
        .filter(|value| !value.is_empty())
    {
        crate::vcxproj::write_props_import(&mut xml, &windows(&crate::string(props)));
    }
    xml.finish()
}
