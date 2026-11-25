use std::fmt::Write as _;

use rcmake_base::Limits;
use rcmake_eval::{CmdError, CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{ReadRange, Stream, path};

use super::{
    Arguments, HandlerResult, configuration, directory, error, prior_start, verbose, with_capture,
};
use crate::dart::Tag;
use crate::junit::{escape, write_file};

pub(super) fn ctest_coverage(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    use crate::dashboard::coverage::{Options, collect};

    with_capture(cx, args, &[b"LABELS"], &[b"APPEND"], |cx, args| {
        let start = prior_start();
        let build = directory(
            cx,
            args.value(b"BUILD"),
            b"CTEST_BINARY_DIRECTORY",
            start.as_ref().map(|start| start.build.as_path()),
        )?;
        let source = directory(
            cx,
            None,
            b"CTEST_SOURCE_DIRECTORY",
            start
                .as_ref()
                .map(|start| start.source.as_path())
                .or(Some(&build)),
        )?;
        let config = configuration(cx, &build, Some(&source))?;
        let mut setting = |key: &[u8], variable: &[u8]| {
            if let Some(value) = cx.var(variable) {
                Ok(value)
            } else {
                let overrides = super::SESSION.with_borrow(|session| {
                    session
                        .as_ref()
                        .map(|session| session.settings.overrides.clone())
                        .unwrap_or_default()
                });
                crate::dashboard::configuration_value(cx.host(), &build, &overrides, key)
                    .map_err(error)
            }
        };
        let command = setting(b"CoverageCommand", b"CTEST_COVERAGE_COMMAND")?;
        let flags = setting(b"CoverageExtraFlags", b"CTEST_COVERAGE_EXTRA_FLAGS")?;
        let flags = if flags.is_empty() {
            Vec::new()
        } else {
            super::split_command(cx, &flags)?
        };
        let code = collect(
            cx.host(),
            &config,
            &Options {
                source: &source,
                build: &build,
                command: &command,
                flags: &flags,
                labels: &args.labels(b"LABELS"),
                append: args.flag(b"APPEND"),
                log: verbose() && !args.flag(b"QUIET"),
            },
        )
        .map_err(error)?;
        Ok(HandlerResult {
            code,
            capture_error: code != 0,
        })
    })
}

pub(super) fn ctest_update(cx: &mut Cx<'_>, input: &[ExpandedArg]) -> CmdResult {
    use crate::dashboard::update::{Options, update};

    with_capture(
        cx,
        input,
        &[b"SOURCE", b"VERSION_OVERRIDE"],
        &[b"VERSION_ONLY"],
        |cx, args| {
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
            let program = cx.var_or_empty(b"CTEST_UPDATE_COMMAND");
            let git = cx.var_or_empty(b"CTEST_GIT_COMMAND");
            let update_type = cx.var_or_empty(b"CTEST_UPDATE_TYPE");
            let revision = args
                .value(b"VERSION_OVERRIDE")
                .filter(|value| !value.is_empty())
                .cloned()
                .unwrap_or_else(|| cx.var_or_empty(b"CTEST_UPDATE_VERSION_OVERRIDE"));
            let code = update(
                host,
                &source,
                &build,
                &tag,
                &config,
                Options {
                    program: &program,
                    git: &git,
                    update_type: &update_type,
                    version_only: args.flag(b"VERSION_ONLY")
                        || cx.var_is_on(b"CTEST_UPDATE_VERSION_ONLY"),
                    revision: &revision,
                    log: verbose() && !args.flag(b"QUIET"),
                },
            )
            .map_err(error)?;
            Ok(HandlerResult {
                code,
                capture_error: false,
            })
        },
    )
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
