use std::fmt::Write as _;
use std::io::ErrorKind;

use rcmake_host::{FileKind, Host, ReadRange, path};

use super::Dashboard;
use crate::junit::{escape, write_file};

pub(super) fn run(host: &dyn Host, dashboard: &Dashboard) -> Result<i32, String> {
    let testing = dashboard.build.join("Testing");
    let dir = testing.join("Notes");
    let mut notes = match host.read_dir(&dir) {
        Ok(notes) => notes,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.to_string()),
    };
    notes.retain(|entry| entry.kind == FileKind::File);
    if notes.is_empty() {
        return Ok(0);
    }
    notes.sort_by(|left, right| left.name.cmp(&right.name));
    let config = &dashboard.config;
    let build = escape(&config.build_name, true);
    let site = escape(&config.site, true);
    let tag = &dashboard.tag;
    let stamp = escape(format!("{}-{}", tag.stamp(), tag.track).as_bytes(), true);
    let version = rcmake_base::CMAKE_VERSION_STR;
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Site BuildName=\"{build}\" Name=\"{site}\" BuildStamp=\"{stamp}\" Generator=\"ctest-{version}\">\n<Notes>\n"
    );
    let mut limit = 16 * 1024 * 1024;
    for entry in notes {
        let file = dir.join(path::to_native(&entry.name));
        let content = host
            .read(&file, ReadRange::All { limit })
            .map_err(|error| error.to_string())?;
        limit = limit.saturating_sub(content.len() as u64);
        let name = escape(&path::from_native_cmake(&file), true);
        let text = escape(&content, false);
        let now = host.now();
        let date = crate::reporting::format_date(host, now, true, "%b %d %H:%M GMT");
        let _ = writeln!(
            xml,
            "<Note Name=\"{name}\"><Time>{}</Time><DateTime>{date}</DateTime><Text>{text}</Text></Note>",
            now.secs
        );
    }
    xml.push_str("</Notes>\n</Site>\n");
    write_file(
        host,
        &testing.join(tag.stamp()).join("Notes.xml"),
        xml.as_bytes(),
    )
    .map_err(|error| error.to_string())?;
    Ok(0)
}
