use std::fmt::Write as _;

use rcmake_base::{BString, Limits};
use rcmake_eval::{CmdError, CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Follow, ReadRange, Stream, path};

use super::{
    Arguments, HandlerResult, configuration, directory, error, prior_start, process_code,
    run_process, verbose, with_capture,
};
use crate::dart::Tag;
use crate::junit::{escape, write_file};

pub(super) fn ctest_coverage(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    with_capture(cx, args, &[b"LABELS"], &[b"APPEND"], |cx, args| {
        if verbose() && !args.flag(b"QUIET") {
            cx.host().write_out(
                Stream::Stdout,
                b"Performing coverage\n Cannot find any coverage files. Ignoring Coverage request.\n",
            );
        }
        Ok(HandlerResult {
            code: 0,
            capture_error: false,
        })
    })
}

pub(super) fn ctest_update(cx: &mut Cx<'_>, input: &[ExpandedArg]) -> CmdResult {
    with_capture(cx, input, &[b"SOURCE"], &[], |cx, args| {
        let start = prior_start();
        let source = directory(
            cx,
            args.value(b"SOURCE"),
            b"CTEST_SOURCE_DIRECTORY",
            start.as_ref().map(|start| start.source.as_path()),
        )?;
        let build = directory(
            cx,
            args.value(b"BUILD"),
            b"CTEST_BINARY_DIRECTORY",
            start.as_ref().map(|start| start.build.as_path()),
        )?;
        let host = cx.host();
        let config = configuration(cx, &build, Some(&source))?;
        let tag = Tag::read(host, &build).map_err(|failure| error(failure.to_string()))?;
        let is_git = host.metadata(&source.join(".git"), Follow::Yes).is_ok();
        let repository = if is_git { "GIT" } else { "Unknown" };
        if verbose() && !args.flag(b"QUIET") {
            host.write_out(
                Stream::Stdout,
                format!(
                    "   Updating {repository} repository: {}\n",
                    source.display()
                )
                .as_bytes(),
            );
        }
        let started = host.now();
        let version = cx.var_or_empty(b"CTEST_UPDATE_VERSION_OVERRIDE");
        let mut command = BString::default();
        let mut revision = (!version.is_empty()).then_some(version);
        let mut succeeded = true;
        if is_git {
            let program = cx
                .var(b"CTEST_GIT_COMMAND")
                .or_else(|| cx.var(b"CTEST_UPDATE_COMMAND"))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "git".into());
            let version_only = cx.var_is_on(b"CTEST_UPDATE_VERSION_ONLY");
            if !version_only {
                let args = vec!["pull".into(), "--ff-only".into()];
                command = format!("{program} pull --ff-only").into();
                let output = run_process(
                    host,
                    source.clone(),
                    program.clone(),
                    args,
                    host.env_all(),
                    false,
                )
                .map_err(|failure| error(failure.to_string()))?;
                succeeded = process_code(&output) == Some(0);
            }
            if revision.is_none() {
                let output = run_process(
                    host,
                    source,
                    program,
                    vec!["rev-parse".into(), "HEAD".into()],
                    host.env_all(),
                    false,
                )
                .map_err(|failure| error(failure.to_string()))?;
                if process_code(&output) == Some(0) {
                    revision = Some(output.stdout.trim_ascii().into());
                } else {
                    succeeded = false;
                }
            }
        }
        let status = if succeeded {
            ""
        } else {
            "Update command failed."
        };
        crate::dashboard::update::write_report(
            host,
            &build,
            &tag,
            &config,
            crate::dashboard::update::Report {
                started,
                command: &command,
                repository,
                revision: revision.as_ref().map(|value| value.as_slice()),
                status,
            },
        )
        .map_err(error)?;
        Ok(HandlerResult {
            code: if succeeded { 0 } else { -1 },
            capture_error: !succeeded,
        })
    })
}

pub(super) fn ctest_upload(cx: &mut Cx<'_>, input: &[ExpandedArg]) -> CmdResult {
    with_capture(cx, input, &[b"FILES"], &[], upload)
}

fn upload(cx: &mut Cx<'_>, args: &Arguments<'_>) -> Result<HandlerResult, CmdError> {
    let start = prior_start();
    let build = directory(
        cx,
        args.value(b"BUILD"),
        b"CTEST_BINARY_DIRECTORY",
        start.as_ref().map(|start| start.build.as_path()),
    )?;
    let host = cx.host();
    let source = start.as_ref().map(|start| start.source.as_path());
    let config = configuration(cx, &build, source)?;
    let tag = Tag::read(host, &build).map_err(|failure| error(failure.to_string()))?;
    let build_name = escape(&config.build_name, true);
    let site = escape(&config.site, true);
    let stamp = escape(format!("{}-{}", tag.stamp(), tag.track).as_bytes(), true);
    let version = rcmake_base::CMAKE_VERSION_STR;
    let time = host.now().secs;
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Site BuildName=\"{build_name}\" BuildStamp=\"{stamp}\" Name=\"{site}\" Generator=\"ctest-{version}\">\n<Upload>\n<Time>{time}</Time>\n"
    );
    let files: std::collections::BTreeSet<_> = args.labels(b"FILES").into_iter().collect();
    let cwd = host.initial_cwd();
    let mut limit = Limits::default().max_file_read;
    for file in files {
        let absolute = super::absolute(&cwd, &file);
        let content = host
            .read(&absolute, ReadRange::All { limit })
            .map_err(|failure| error(failure.to_string()))?;
        limit = limit.saturating_sub(content.len() as u64);
        let name = escape(&path::from_native_cmake(&absolute), true);
        let contents = crate::measurements::base64(&content);
        let _ = writeln!(
            xml,
            "<File filename=\"{name}\"><Content encoding=\"base64\">{contents}</Content></File>"
        );
    }
    xml.push_str("</Upload>\n</Site>\n");
    let target = build.join("Testing").join(tag.stamp()).join("Upload.xml");
    write_file(host, &target, xml.as_bytes()).map_err(|failure| error(failure.to_string()))?;
    if verbose() && !args.flag(b"QUIET") {
        host.write_out(Stream::Stdout, b"Upload files\n");
    }
    Ok(HandlerResult {
        code: 0,
        capture_error: false,
    })
}
