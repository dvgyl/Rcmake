use std::fmt::Write as _;
use std::path::Path;

use rcmake_base::{BString, truth};
use rcmake_host::{FileTime, Host, ProcessResult, path};
use rcmake_presets::{EnvSnapshot, PresetKind};

use crate::cli::Invocation;
use crate::dart::{Configuration, Model, Tag};
use crate::junit::{escape, write_file};

use super::definition;

pub(super) fn run(host: &dyn Host, invocation: &Invocation) -> Result<i32, String> {
    let cwd = host.initial_cwd();
    let initial = &invocation.options.test_dir;
    let preliminary = Configuration::load(host, initial, &invocation.overwrite_configuration_file)
        .map_err(|error| error.to_string())?;
    let source = invocation.source_dir.as_ref().map_or_else(
        || initial.join(path::to_native(&preliminary.source_directory)),
        |source| cwd.join(source),
    );
    let mut build = initial.clone();
    if invocation.build_dir.is_none() {
        if !preliminary.build_directory.is_empty() {
            build = cwd.join(path::to_native(&preliminary.build_directory));
        } else if let Some(preset) = definition(invocation, b"CTEST_CONFIGURE_PRESET")
            .or_else(|| definition(invocation, b"CTEST_PRESET"))
        {
            let set = match &invocation.presets_file {
                Some(file) => rcmake_presets::load_file(&source, file, host),
                None => rcmake_presets::load(&source, host),
            }
            .map_err(|error| error.message)?;
            let preset = set
                .resolve(PresetKind::Configure, preset, &EnvSnapshot::from_host(host))
                .map_err(|error| error.message)?;
            if let Some(dir) = preset.binary_dir {
                build = source.join(path::to_native(&dir));
            }
        }
    }
    let config = Configuration::load(host, &build, &invocation.overwrite_configuration_file)
        .map_err(|error| error.to_string())?;
    let tag = Tag::read(host, &build)
        .or_else(|_| {
            Tag::create(
                host,
                &build,
                host.now(),
                invocation.dashboard_model.unwrap_or(Model::Experimental),
                None,
                &config.nightly_start,
            )
        })
        .map_err(|error| error.to_string())?;
    let started = host.now();
    let program = definition(invocation, b"CTEST_UPDATE_COMMAND").unwrap_or(&config.update_command);
    let version_only =
        definition(invocation, b"CTEST_UPDATE_VERSION_ONLY").is_some_and(truth::is_on);
    let output = if program.is_empty() {
        None
    } else {
        let args = if version_only {
            vec!["--version".into()]
        } else {
            Vec::new()
        };
        Some(
            crate::script::run_process(host, source, program.into(), args, host.env_all(), false)
                .map_err(|error| error.to_string())?,
        )
    };
    let succeeded = output
        .as_ref()
        .is_none_or(|output| matches!(output.results.as_slice(), [ProcessResult::Exited(0)]));
    let status = if succeeded {
        ""
    } else {
        "Update command failed."
    };
    write_report(
        host,
        &build,
        &tag,
        &config,
        Report {
            started,
            command: program,
            repository: "Unknown",
            revision: definition(invocation, b"CTEST_UPDATE_VERSION_OVERRIDE"),
            status,
        },
    )?;
    Ok(if succeeded { 0 } else { 1 })
}

pub(crate) struct Report<'a> {
    pub(crate) started: FileTime,
    pub(crate) command: &'a [u8],
    pub(crate) repository: &'a str,
    pub(crate) revision: Option<&'a [u8]>,
    pub(crate) status: &'a str,
}

pub(crate) fn write_report(
    host: &dyn Host,
    build: &Path,
    tag: &Tag,
    config: &Configuration,
    report: Report<'_>,
) -> Result<(), String> {
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Update mode=\"Client\" Generator=\"ctest-{}\">\n",
        rcmake_base::CMAKE_VERSION_STR
    );
    let ended = host.now();
    let date = |time| crate::reporting::format_date(host, time, true, "%b %d %H:%M GMT");
    for (name, value) in [
        ("Site", config.site.clone()),
        ("BuildName", config.build_name.clone()),
        (
            "BuildStamp",
            format!("{}-{}", tag.stamp(), tag.track).into(),
        ),
        ("StartDateTime", date(report.started).into()),
        ("StartTime", report.started.secs.to_string().into()),
        ("UpdateCommand", report.command.into()),
        ("UpdateType", report.repository.into()),
    ] {
        let _ = writeln!(xml, "<{name}>{}</{name}>", escape(&value, false));
    }
    if let Some(revision) = report.revision {
        let _ = writeln!(xml, "<Revision>{}</Revision>", escape(revision, false));
    }
    if !config.change_id.is_empty() {
        let _ = writeln!(
            xml,
            "<ChangeId>{}</ChangeId>",
            escape(&config.change_id, false)
        );
    }
    let minutes = ended.secs.saturating_sub(report.started.secs).max(0) / 60;
    let values: [(&str, BString); 4] = [
        ("EndDateTime", date(ended).into()),
        ("EndTime", ended.secs.to_string().into()),
        ("ElapsedMinutes", minutes.to_string().into()),
        ("UpdateReturnStatus", report.status.into()),
    ];
    for (name, value) in values {
        let _ = writeln!(xml, "<{name}>{}</{name}>", escape(&value, false));
    }
    xml.push_str("</Update>\n");
    write_file(
        host,
        &build.join("Testing").join(tag.stamp()).join("Update.xml"),
        xml.as_bytes(),
    )
    .map_err(|error| error.to_string())
}
