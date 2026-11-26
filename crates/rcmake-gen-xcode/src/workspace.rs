//! Xcode workspace descriptors and shared settings.

use std::path::Path;

use rcmake_gen_common::GenError;
use rcmake_host::Host;

use crate::write_file;

pub(crate) const XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";
pub(crate) const PLIST: &str = "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n";

pub(crate) fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut start = 0;
    for (i, ch) in value.char_indices() {
        let repl = match ch {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' => "&quot;",
            '\'' => "&apos;",
            '\n' => "&#10;",
            '\r' => "&#13;",
            '\t' => "&#9;",
            _ => continue,
        };
        if start < i {
            out.push_str(value.get(start..i).unwrap_or_default());
        }
        out.push_str(repl);
        start = i + ch.len_utf8();
    }
    if start < value.len() {
        out.push_str(value.get(start..).unwrap_or_default());
    }
    out
}

pub(crate) fn write(host: &dyn Host, project: &Path) -> Result<(), GenError> {
    let workspace = project.join("project.xcworkspace");
    write_file(
        host,
        &workspace.join("contents.xcworkspacedata"),
        &format!(
            "{XML}<Workspace version=\"1.0\">\n   <FileRef location=\"self:\"></FileRef>\n</Workspace>\n"
        ),
    )?;
    write_file(
        host,
        &workspace.join("xcshareddata/WorkspaceSettings.xcsettings"),
        &format!(
            "{XML}{PLIST}<dict>\n\t<key>BuildSystemType</key>\n\t<string>Latest</string>\n\t<key>IDEWorkspaceSharedSettings_AutocreateContextsIfNeeded</key>\n\t<false/>\n</dict>\n</plist>\n"
        ),
    )
}
