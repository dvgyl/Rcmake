use std::path::Path;
use std::time::Duration;

use rcmake_base::{BString, hash::HashAlgo, list, truth};
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, ReadRange, RunRequest,
    Stream, path,
};

use crate::cli::Invocation;
use crate::dart::{Configuration, Model, Tag};
use crate::junit::write_file;

use super::definition;

const PARTS: &[&str] = &[
    "Start",
    "Update",
    "Configure",
    "Build",
    "Test",
    "Coverage",
    "MemCheck",
    "Notes",
    "ExtraFiles",
    "Upload",
    "Submit",
    "Done",
];

pub(super) fn run(host: &dyn Host, invocation: &Invocation) -> Result<i32, String> {
    let requested = definition(invocation, b"CTEST_SUBMIT_PARTS")
        .map(|value| list::split(value, list::Empty::Drop));
    if let Some(part) = requested
        .as_ref()
        .into_iter()
        .flatten()
        .find(|part| !PARTS.iter().any(|name| part.as_slice() == name.as_bytes()))
    {
        host.write_out(
            Stream::Stderr,
            format!("Part name \"{part}\" is invalid.\n").as_bytes(),
        );
        return Ok(64);
    }
    let selected = |part: &str| {
        requested
            .as_ref()
            .is_none_or(|parts| parts.iter().any(|name| name.as_slice() == part.as_bytes()))
    };
    let dir = &invocation.options.test_dir;
    let config = Configuration::load(host, dir, &invocation.overwrite_configuration_file)
        .map_err(|error| error.to_string())?;
    let url = if config.submit_url.is_empty() {
        format!(
            "{}://{}{}",
            config.drop_method, config.drop_site, config.drop_location
        )
    } else {
        config.submit_url.to_string()
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("A dashboard submission requires an HTTP or HTTPS URL.".into());
    }

    let create_tag = || {
        Tag::create(
            host,
            dir,
            host.now(),
            invocation.dashboard_model.unwrap_or(Model::Experimental),
            None,
            &config.nightly_start,
        )
    };
    let tag = if invocation.actions.iter().any(|action| action == "start") {
        create_tag()
    } else {
        Tag::read(host, dir).or_else(|_| create_tag())
    }
    .map_err(|error| error.to_string())?;
    let reports = dir.join("Testing").join(tag.stamp());
    let mut args = vec![
        "--disable".into(),
        "--silent".into(),
        "--show-error".into(),
        "--fail".into(),
        "--proto".into(),
        "=http,https".into(),
        "--globoff".into(),
    ];
    tls(host, invocation, &config, &mut args)?;
    let stamp = format!("{}-{}", tag.stamp(), tag.track);
    for part in PARTS.iter().copied().filter(|part| selected(part)) {
        let file_name = if part == "MemCheck" {
            "DynamicAnalysis"
        } else {
            part
        };
        let report = reports.join(format!("{file_name}.xml"));
        if part == "Done" {
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Done><buildId></buildId><time>{}</time></Done>\n",
                host.now().secs
            );
            write_file(host, &report, xml.as_bytes()).map_err(|error| error.to_string())?;
        } else if !host
            .metadata(&report, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            continue;
        }
        let remote = format!(
            "{}___{}___{stamp}___XML___{file_name}.xml",
            config.site, config.build_name
        );
        let remote = remote.replace(
            ['\\', ':', '*', '?', '"', '<', '>', '|', '\n', '\r', '\t'],
            "",
        );
        let payload = host
            .read(
                &report,
                ReadRange::All {
                    limit: rcmake_base::Limits::default().max_file_read,
                },
            )
            .map_err(|error| error.to_string())?;
        let separator = if url.contains('?') { '&' } else { '?' };
        let destination = format!(
            "{url}{separator}FileName={}&build={}&site={}&stamp={}&MD5={}",
            encode(remote.as_bytes()),
            encode(&config.build_name),
            encode(&config.site),
            encode(stamp.as_bytes()),
            HashAlgo::Md5.hex_digest(&payload),
        );
        let code = upload(host, dir, &args, &report, &destination)?;
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

fn tls(
    host: &dyn Host,
    invocation: &Invocation,
    config: &Configuration,
    args: &mut Vec<BString>,
) -> Result<(), String> {
    let version = definition(invocation, b"CTEST_TLS_VERSION")
        .map(BString::from)
        .or_else(|| config.tls_version.clone())
        .or_else(|| definition(invocation, b"CMAKE_TLS_VERSION").map(BString::from))
        .or_else(|| host.env_get(b"CMAKE_TLS_VERSION"));
    let verify = definition(invocation, b"CTEST_TLS_VERIFY")
        .map(BString::from)
        .or_else(|| config.tls_verify.clone())
        .or_else(|| definition(invocation, b"CMAKE_TLS_VERIFY").map(BString::from))
        .or_else(|| host.env_get(b"CMAKE_TLS_VERIFY"));
    let log = invocation.options.verbose && !invocation.options.quiet;
    if let Some(version) = version.filter(|version| !version.is_empty()) {
        let (flag, name) = match version.as_slice() {
            b"1.0" => ("--tlsv1.0", "TLSv1_0"),
            b"1.1" => ("--tlsv1.1", "TLSv1_1"),
            b"1.2" => ("--tlsv1.2", "TLSv1_2"),
            b"1.3" => ("--tlsv1.3", "TLSv1_3"),
            _ => return Err(format!("Unsupported TLS version \"{version}\".")),
        };
        args.push(flag.into());
        if log {
            host.write_out(
                Stream::Stdout,
                format!("  Set CURLOPT_SSLVERSION to CURL_SSLVERSION_{name}\n").as_bytes(),
            );
        }
    }
    if let Some(verify) = verify.filter(|verify| !verify.is_empty()) {
        let verify = !truth::is_off(&verify);
        if !verify {
            args.push("--insecure".into());
        }
        if log {
            let value = if verify { "on" } else { "off" };
            host.write_out(
                Stream::Stdout,
                format!("  Set CURLOPT_SSL_VERIFYPEER to {value}\n").as_bytes(),
            );
        }
    }
    Ok(())
}

fn upload(
    host: &dyn Host,
    dir: &Path,
    options: &[BString],
    file: &Path,
    url: &str,
) -> Result<i32, String> {
    let curl = path::executable_on_absolute_path(host, "curl")
        .ok_or_else(|| "Cannot find curl for HTTP submission.".to_owned())?;
    let mut args = options.to_vec();
    args.extend([
        "--upload-file".into(),
        path::from_native(file),
        "--url".into(),
        url.into(),
    ]);
    let output = host
        .run_bounded(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: path::from_native(&curl),
                    args,
                }],
                cwd: dir.to_path_buf(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(30)),
                tee: false,
            },
            1024 * 1024,
        )
        .map_err(|error| error.to_string())?;
    if !matches!(output.results.as_slice(), [ProcessResult::Exited(0)]) {
        let message = String::from_utf8_lossy(&output.stderr);
        let message = message
            .trim()
            .split_once(") ")
            .map_or(message.trim(), |(_, detail)| detail);
        host.write_out(Stream::Stderr, format!(
            "Error when uploading file: {}\nError message was: {message}\n   Problems when submitting via HTTP\n",
            file.display(),
        ).as_bytes());
        return Ok(64);
    }
    Ok(0)
}

fn encode(value: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut escaped = String::new();
    for &byte in value {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            escaped.push(char::from(byte));
        } else {
            let _ = write!(escaped, "%{byte:02X}");
        }
    }
    escaped
}
