use std::time::Duration;

use rcmake_base::{BString, list};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, Stream};

use crate::dart::Tag;
use crate::dashboard::submit::{self, CDashUpload, PARTS, Submission, SubmitResult, Transport};

use super::{
    HandlerResult, absolute, configuration, debug, directory, error, extra_verbose, prior_start,
    verbose, with_capture,
};

pub(super) fn ctest_submit(cx: &mut Cx<'_>, input: &[ExpandedArg]) -> CmdResult {
    let upload = input.first().is_some_and(|arg| arg.value == "CDASH_UPLOAD");
    let mut keys = vec![
        b"BUILD_ID".as_slice(),
        b"HTTPHEADER",
        b"RETRY_COUNT",
        b"RETRY_DELAY",
        b"SUBMIT_URL",
    ];
    if upload {
        keys.extend([b"CDASH_UPLOAD".as_slice(), b"CDASH_UPLOAD_TYPE"]);
    } else {
        keys.extend([b"FILES".as_slice(), b"PARTS"]);
    }
    let result = with_capture(
        cx,
        input,
        &keys,
        &[b"INTERNAL_TEST_CHECKSUM"],
        |cx, args| {
            let cwd = cx.host().initial_cwd();
            let files = args.labels(b"FILES").into_iter().map(|file| {
            let resolved = absolute(&cwd, &file);
            if !is_file(cx, &resolved) {
                return Err(report(cx, format!("File \"{file}\" does not exist.  Cannot submit a non-existent file.")));
            }
            Ok(resolved)
        }).collect::<Result<Vec<_>, _>>()?;
            let parts = if args.flag(b"PARTS") {
                Some(args.labels(b"PARTS"))
            } else if upload || args.flag(b"FILES") {
                Some(Vec::new())
            } else {
                cx.var(b"CTEST_SUBMIT_PARTS")
                    .map(|parts| list::split(&parts, list::Empty::Drop))
                    .filter(|parts| !parts.is_empty())
            };
            if let Some(invalid) = parts
                .iter()
                .flatten()
                .find(|arg| !PARTS.iter().any(|name| *arg == *name))
            {
                return Err(report(cx, format!("Part name \"{invalid}\" is invalid.")));
            }
            let start = prior_start();
            let build = directory(
                cx,
                args.value(b"BUILD"),
                b"CTEST_BINARY_DIRECTORY",
                start.as_ref().map(|start| start.build.as_path()),
            )?;
            let mut config = configuration(
                cx,
                &build,
                start.as_ref().map(|start| start.source.as_path()),
            )?;
            for (key, value) in [
                (b"CTEST_DROP_METHOD".as_slice(), &mut config.drop_method),
                (b"CTEST_DROP_SITE", &mut config.drop_site),
                (b"CTEST_DROP_LOCATION", &mut config.drop_location),
                (b"CTEST_SUBMIT_URL", &mut config.submit_url),
            ] {
                if let Some(setting) = cx.var(key) {
                    *value = setting;
                }
            }
            let url = args
                .value(b"SUBMIT_URL")
                .filter(|value| !value.is_empty())
                .map_or_else(|| submit::submission_url(&config), ToString::to_string);
            let subproject = cx.global().props.get(b"SubProject").cloned();
            let tag = Tag::read(cx.host(), &build).map_err(|failure| error(failure.to_string()))?;
            let job = Submission {
                url,
                config: &config,
                parts,
                files,
                subproject,
            };
            let quiet = args.flag(b"QUIET");
            let inactivity = cx.var(b"CTEST_SUBMIT_INACTIVITY_TIMEOUT");
            let mut transport = Transport::new(
                verbose() && !quiet,
                (extra_verbose() || debug()) && !quiet,
                inactivity
                    .as_ref()
                    .or(config.submit_inactivity_timeout.as_ref())
                    .map(|value| value.as_slice()),
                cx.limits().wall_budget,
            );
            transport.retries =
                retry_option(cx, args.value(b"RETRY_COUNT"), b"CTEST_SUBMIT_RETRY_COUNT").min(10);
            transport.delay = Duration::from_secs(u64::from(
                retry_option(cx, args.value(b"RETRY_DELAY"), b"CTEST_SUBMIT_RETRY_DELAY").min(30),
            ));
            let host = cx.host();
            tls_log(cx, transport.verbose);
            if transport.log && !upload {
                host.write_out(
                    Stream::Stdout,
                    format!("Submit files\n   SubmitURL: {}\n", job.url).as_bytes(),
                );
            }
            submit::tls(
                host,
                &config,
                |name| read_var(cx, name),
                transport.verbose,
                &mut transport.flags,
            )
            .map_err(error)?;
            for header in args.labels(b"HTTPHEADER") {
                transport.header(host, header).map_err(error)?;
            }
            let result = if let Some(file) = args.value(b"CDASH_UPLOAD") {
                let path = absolute(&cwd, file);
                if !is_file(cx, &path) {
                    Err(format!("Upload file not found: '{file}'"))
                } else {
                    submit::cdash_upload(
                        host,
                        &build,
                        &tag,
                        &transport,
                        CDashUpload {
                            job: &job,
                            file: &path,
                            kind: args
                                .value(b"CDASH_UPLOAD_TYPE")
                                .map_or(b"", |kind| kind.as_slice()),
                            checksum_test: args.flag(b"INTERNAL_TEST_CHECKSUM"),
                        },
                    )
                }
            } else {
                submit::submit(host, &build, &tag, &transport, &job)
            };
            let result = result.unwrap_or_else(|failure| {
                host.write_out(Stream::Stderr, format!("{failure}\n").as_bytes());
                SubmitResult {
                    code: -1,
                    ..SubmitResult::default()
                }
            });
            args.set(cx, b"BUILD_ID", result.build_id);
            Ok(HandlerResult {
                code: result.code,
                capture_error: result.code != 0,
            })
        },
    );
    match result {
        Err(CmdError::Status(mut message))
            if message.starts_with(b"called with unknown argument ") =>
        {
            message.push(b'.');
            Err(CmdError::Status(message))
        }
        result => result,
    }
}

fn is_file(cx: &Cx<'_>, file: &std::path::Path) -> bool {
    cx.host()
        .metadata(file, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
}

fn report(cx: &mut Cx<'_>, text: String) -> CmdError {
    cx.issue(MessageKind::Error, Effect::Fatal, None, text.into());
    CmdError::Reported
}

fn retry_option(cx: &mut Cx<'_>, value: Option<&BString>, name: &[u8]) -> u32 {
    value
        .cloned()
        .or_else(|| cx.var(name))
        .and_then(|value| value.to_string().parse().ok())
        .unwrap_or_default()
}

fn tls_log(cx: &Cx<'_>, log: bool) {
    if !log {
        return;
    }
    let host = cx.host();
    for (setting, ctest, cmake) in [
        (
            "TLSVersion",
            b"CTEST_TLS_VERSION".as_slice(),
            b"CMAKE_TLS_VERSION".as_slice(),
        ),
        ("TLSVerify", b"CTEST_TLS_VERIFY", b"CMAKE_TLS_VERIFY"),
    ] {
        let mode = BString::from(cmake);
        let entry = if let Some(value) = read_var(cx, ctest) {
            let key = BString::from(ctest);
            format!(
                "SetCTestConfigurationFromCMakeVariable:{setting}:{key}\nSetCTestConfiguration:{setting}:{value}\n"
            )
        } else if let Some(value) = read_var(cx, cmake) {
            format!(
                "SetCTestConfiguration from {mode}:{setting}:{value}\nSetCTestConfiguration:{setting}:{value}\n"
            )
        } else if let Some(value) = host.env_get(cmake) {
            format!(
                "SetCTestConfiguration from ENV{{{mode}}}:{setting}:{value}\nSetCTestConfiguration:{setting}:{value}\n"
            )
        } else {
            continue;
        };
        host.write_out(Stream::Stdout, entry.as_bytes());
    }
}

fn read_var(cx: &Cx<'_>, name: &[u8]) -> Option<BString> {
    cx.var_defined(name).then(|| cx.var_or_empty(name))
}
