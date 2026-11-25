use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use rcmake_base::{BString, truth};
use rcmake_host::{CommandSpec, Follow, Host, Input, Output, ProcessResult, RunRequest, Stream};

use crate::cli::Invocation;
use crate::dart::{Configuration, Tag};
use crate::junit::{escape, write_file};

use super::{Dashboard, definition};

pub(super) fn run(
    host: &dyn Host,
    invocation: &Invocation,
    dashboard: &Dashboard,
) -> Result<i32, String> {
    let Dashboard {
        source,
        build,
        tag,
        config,
        ..
    } = dashboard;
    let setting = |name, key| {
        definition(invocation, name).map_or_else(
            || dashboard.value(host, invocation, key),
            |value| Ok(BString::from(value)),
        )
    };
    let program = definition(invocation, b"CTEST_UPDATE_COMMAND")
        .filter(|value| !value.is_empty())
        .unwrap_or(&config.update_command);
    let update_type = setting(b"CTEST_UPDATE_TYPE", b"UpdateType")?;
    let git = setting(b"CTEST_GIT_COMMAND", b"GITCommand")?;
    let version_only = setting(b"CTEST_UPDATE_VERSION_ONLY", b"UpdateVersionOnly")?;
    let revision = setting(b"CTEST_UPDATE_VERSION_OVERRIDE", b"UpdateVersionOverride")?;
    update(
        host,
        source,
        build,
        tag,
        config,
        Options {
            program,
            git: &git,
            update_type: &update_type,
            version_only: truth::is_on(&version_only),
            revision: &revision,
            log: !invocation.options.quiet,
        },
    )
    .map(|count| i32::from(count < 0))
}

pub(crate) struct Options<'a> {
    pub(crate) program: &'a [u8],
    pub(crate) git: &'a [u8],
    pub(crate) update_type: &'a [u8],
    pub(crate) version_only: bool,
    pub(crate) revision: &'a [u8],
    pub(crate) log: bool,
}

pub(crate) fn update(
    host: &dyn Host,
    source: &Path,
    build: &Path,
    tag: &Tag,
    config: &Configuration,
    options: Options<'_>,
) -> Result<i32, String> {
    let started = host.now();
    let repository = repository(host, source, options.update_type, options.program);
    if options.log {
        host.write_out(
            Stream::Stdout,
            format!(
                "   Updating {repository} repository: {}\n",
                source.display()
            )
            .as_bytes(),
        );
    }
    let mut report = Report {
        command: BString::default(),
        repository,
        revision: options.revision.into(),
        prior_revision: BString::default(),
        updated: BTreeSet::new(),
        status: String::new(),
    };
    if report.revision.is_empty() {
        let status = match repository {
            "GIT" => {
                let program = if !options.program.is_empty() {
                    options.program
                } else if !options.git.is_empty() {
                    options.git
                } else {
                    b"git"
                };
                collect(host, source, program, options.version_only, &mut report)
            }
            "Unknown" if options.program.is_empty() => {
                return Err("Update command is not specified.".into());
            }
            "Unknown" => Ok(()),
            _ => Err(format!("The {repository} update backend is unavailable.")),
        };
        if let Err(message) = status {
            host.write_out(Stream::Stderr, format!("   {message}\n").as_bytes());
            report.status = message;
        }
    }
    write_report(host, build, tag, config, &report, started)?;
    if report.status.is_empty() {
        let count = report.updated.len();
        if options.log && count != 0 {
            host.write_out(
                Stream::Stdout,
                format!("   Found {count} updated files\n").as_bytes(),
            );
        }
        i32::try_from(count).map_err(|failure| failure.to_string())
    } else {
        Ok(-1)
    }
}

fn repository(host: &dyn Host, source: &Path, update_type: &[u8], program: &[u8]) -> &'static str {
    const TOOLS: [(&[u8], &str, &str); 6] = [
        (b"git", "GIT", ".git"),
        (b"svn", "SVN", ".svn"),
        (b"cvs", "CVS", "CVS"),
        (b"bzr", "BZR", ".bzr"),
        (b"hg", "HG", ".hg"),
        (b"p4", "P4", ""),
    ];
    let filename = program
        .rsplit(|&byte| matches!(byte, b'/' | b'\\'))
        .next()
        .unwrap_or_default();
    let filename = filename.strip_suffix(b".exe").unwrap_or(filename);
    let select = |value: &[u8]| {
        TOOLS
            .iter()
            .find(|(tool, _, _)| value.eq_ignore_ascii_case(tool))
            .map(|(_, name, _)| *name)
    };
    select(update_type)
        .or_else(|| {
            TOOLS
                .iter()
                .find(|(_, _, marker)| {
                    !marker.is_empty() && host.metadata(&source.join(marker), Follow::Yes).is_ok()
                })
                .map(|(_, name, _)| *name)
        })
        .or_else(|| select(filename))
        .unwrap_or("Unknown")
}

fn collect(
    host: &dyn Host,
    source: &Path,
    program: &[u8],
    version_only: bool,
    report: &mut Report,
) -> Result<(), String> {
    let run = |args| {
        let output = host
            .run_bounded(
                &RunRequest {
                    pipeline: vec![CommandSpec {
                        program: program.into(),
                        args,
                    }],
                    cwd: source.to_path_buf(),
                    env: host.env_all(),
                    stdin: Input::Null,
                    stdout: Output::Capture,
                    stderr: Output::Capture,
                    timeout: Some(Duration::from_secs(30 * 60)),
                    tee: false,
                },
                16 * 1024 * 1024,
            )
            .map_err(|failure| failure.to_string())?;
        if matches!(output.results.as_slice(), [ProcessResult::Exited(0)]) {
            Ok(output.stdout)
        } else {
            Err("Update command failed.".to_owned())
        }
    };
    let revision = || -> Result<BString, String> {
        let output = run(vec!["rev-parse".into(), "HEAD".into()])?;
        let hash = output.trim_ascii();
        if matches!(hash.len(), 40 | 64) && hash.iter().all(u8::is_ascii_hexdigit) {
            Ok(BString::from(hash))
        } else {
            Err("Git did not return a commit ID.".into())
        }
    };
    if version_only {
        report.revision = revision()?;
        return Ok(());
    }
    report.prior_revision = revision()?;
    report.command = format!("{} pull --ff-only", BString::from(program)).into();
    run(vec!["pull".into(), "--ff-only".into()])?;
    report.revision = revision()?;
    if report.revision != report.prior_revision {
        let range = format!("{}..{}", report.prior_revision, report.revision);
        let paths = run([
            "log",
            "--format=",
            "--name-only",
            "--no-renames",
            "-z",
            &range,
            "--",
        ]
        .into_iter()
        .map(BString::from)
        .collect())?;
        report.updated = paths
            .split(|&byte| byte == 0)
            .filter(|name| !name.is_empty())
            .map(BString::from)
            .collect();
    }
    Ok(())
}

struct Report {
    command: BString,
    repository: &'static str,
    revision: BString,
    prior_revision: BString,
    updated: BTreeSet<BString>,
    status: String,
}

fn write_report(
    host: &dyn Host,
    build: &Path,
    tag: &Tag,
    config: &Configuration,
    report: &Report,
    started: rcmake_host::FileTime,
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
        ("StartDateTime", date(started).into()),
        ("StartTime", started.secs.to_string().into()),
        ("UpdateCommand", report.command.clone()),
        ("UpdateType", report.repository.into()),
    ] {
        let _ = writeln!(xml, "<{name}>{}</{name}>", escape(&value, false));
    }
    let revision = escape(&report.revision, false);
    let prior_revision = escape(&report.prior_revision, false);
    if !revision.is_empty() {
        let _ = writeln!(xml, "<Revision>{revision}</Revision>");
    }
    if !prior_revision.is_empty() && prior_revision != revision {
        let _ = writeln!(xml, "<PriorRevision>{prior_revision}</PriorRevision>");
    }
    for file in &report.updated {
        let (directory, name) = file
            .iter()
            .rposition(|&byte| byte == b'/')
            .and_then(|pos| file.get(..pos).zip(file.get(pos + 1..)))
            .unwrap_or((b"".as_slice(), file.as_slice()));
        let full = escape(file, false);
        let directory = escape(directory, false);
        let name = escape(name, false);
        let _ = writeln!(
            xml,
            "<Directory><Name>{directory}</Name><Updated><File>{name}</File><Directory>{directory}</Directory><FullName>{full}</FullName><Revision>{revision}</Revision><PriorRevision>{prior_revision}</PriorRevision></Updated></Directory>"
        );
    }
    if !config.change_id.is_empty() {
        let _ = writeln!(
            xml,
            "<ChangeId>{}</ChangeId>",
            escape(&config.change_id, false)
        );
    }
    let minutes = ended.secs.saturating_sub(started.secs).max(0) / 60;
    let values: [(&str, BString); 4] = [
        ("EndDateTime", date(ended).into()),
        ("EndTime", ended.secs.to_string().into()),
        ("ElapsedMinutes", minutes.to_string().into()),
        ("UpdateReturnStatus", report.status.as_str().into()),
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
