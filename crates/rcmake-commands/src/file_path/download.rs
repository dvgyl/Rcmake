use std::time::Duration;

use rcmake_base::{BString, hash::HashAlgo, truth};
use rcmake_eval::{Cx, Flow};
use rcmake_host::{CommandSpec, Input, Output, ProcessResult, RunRequest, Stream, WriteMode, path};

use super::{Result, error, warning};

#[derive(Default)]
struct Download<'a> {
    destination: Option<&'a [u8]>,
    status: Option<&'a [u8]>,
    log: Option<&'a [u8]>,
    expected: Option<ExpectedHash>,
    ranges: Vec<(&'a [u8], Option<&'a [u8]>)>,
    flags: Vec<BString>,
    input: BString,
    timeout: Option<Duration>,
    progress: bool,
    default_verify: bool,
}

struct ExpectedHash {
    algorithm: HashAlgo,
    label: String,
    value: String,
}

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let [url, options @ ..] = args else {
        return Err(error(
            "DOWNLOAD must be called with at least two arguments.",
        ));
    };
    let mut download = Download::parse(cx, options)?;
    let cwd = cx.host().initial_cwd();
    let destination = download
        .destination
        .map(|file| cwd.join(path::to_native(file)));
    if let Some(expected) = &download.expected {
        let file = destination
            .as_deref()
            .ok_or_else(|| error("DOWNLOAD cannot calculate hash if file is not saved."))?;
        if path::hash_file(cx.host(), file, expected.algorithm)
            .is_ok_and(|actual| actual == expected.value)
        {
            set_status(
                cx,
                download.status,
                0,
                &format!(
                    "skipping download as file already exists with expected {}",
                    expected.label
                ),
            );
            return Ok(Flow::Normal);
        }
    }
    let transfer_url = std::str::from_utf8(url).unwrap_or("<invalid URL>");
    #[cfg(all(feature = "native", any(unix, windows)))]
    let fetch = if cx.host().supports_process_execution() {
        crate::fetch_observe::prepare_archive(cx, transfer_url, options)?
    } else {
        None
    };
    #[cfg(all(feature = "native", any(unix, windows)))]
    if let (Some(fetch), Some(file)) = (&fetch, &destination)
        && fetch.restore(cx, file)?
    {
        set_status(cx, download.status, 0, "No error");
        if let Some(log) = download.log {
            cx.set_var(log, BString::default());
        }
        if download.progress {
            cx.host()
                .write_out(Stream::Stdout, b"-- [download 100% complete]\n");
        }
        return Ok(Flow::Normal);
    }
    if cx.host().supports_process_execution() {
        #[cfg(all(feature = "native", any(unix, windows)))]
        if let Some(fetch) = &fetch {
            fetch.check_network(cx)?;
        } else {
            crate::fetch_observe::check_unmatched_url(cx, url)?;
            crate::hermetic::check_network(cx, transfer_url, download.expected.is_some())?;
        }
        #[cfg(not(all(feature = "native", any(unix, windows))))]
        crate::hermetic::check_network(cx, transfer_url, download.expected.is_some())?;
    }
    if let Some(file) = &destination {
        if let Some(parent) = file.parent() {
            cx.host().create_dir_all(parent).map_err(|_| error(format!(
                "DOWNLOAD error: cannot create directory '{}' - Specify file by full path name and verify that you have directory creation and file write privileges.",
                parent.display(),
            )))?;
        }
        cx.host()
            .write(file, b"", WriteMode::Truncate)
            .map_err(|_| {
                error(format!(
                    "DOWNLOAD cannot open file for write\n  file: \"{}\"",
                    BString::from(download.destination.unwrap_or_default()),
                ))
            })?;
        download
            .flags
            .extend(["--output".into(), path::from_native(file)]);
    }
    if let Some((start, end)) = download.ranges.last() {
        let mut range = BString::from(*start);
        range.push(b'-');
        range.extend_from_slice(end.unwrap_or_default());
        download.flags.extend(["--range".into(), range]);
    }
    if download.log.is_some() {
        download.flags.push("--verbose".into());
    }
    download.flags.extend(["--url".into(), BString::from(*url)]);
    let timeout = match (download.timeout, cx.limits().wall_budget) {
        (Some(timeout), Some(budget)) => Some(timeout.min(budget)),
        (timeout, budget) => timeout.or(budget),
    };
    let result = cx
        .host()
        .run_bounded(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: "curl".into(),
                    args: download.flags,
                }],
                cwd,
                env: cx.host().env_all(),
                stdin: if download.input.is_empty() {
                    Input::Null
                } else {
                    Input::Bytes(download.input.into())
                },
                stdout: Output::Null,
                stderr: Output::Capture,
                timeout,
                tee: false,
            },
            cx.limits().max_value_bytes,
        )
        .map_err(|failure| error(format!("DOWNLOAD error initializing curl: {failure}")))?;
    let code = match result.results.first() {
        Some(ProcessResult::Exited(code)) => *code,
        Some(ProcessResult::TimedOut) => 28,
        Some(ProcessResult::SpawnFailed(reason)) => {
            return Err(error(format!("DOWNLOAD error initializing curl: {reason}")));
        }
        _ => 2,
    };
    #[cfg(all(feature = "native", any(unix, windows)))]
    let fetch_mismatch = if let (0, Some(fetch), Some(file)) = (code, &fetch, &destination) {
        !fetch.complete(cx, file)?
    } else {
        false
    };
    #[cfg(not(all(feature = "native", any(unix, windows))))]
    let fetch_mismatch = false;
    let (code, message) = if fetch_mismatch {
        (
            1,
            "Downloaded archive does not match the required content.".into(),
        )
    } else {
        (
            code,
            status_message(code, &result.stderr, download.default_verify),
        )
    };
    set_status(cx, download.status, code, &message);
    if let Some(log) = download.log {
        cx.set_var(log, result.stderr.into());
    }
    if code == 0 && download.progress {
        cx.host()
            .write_out(Stream::Stdout, b"-- [download 100% complete]\n");
    }

    if let (Some(expected), Some(file)) = (download.expected, destination) {
        if code != 0 {
            return Err(error(format!(
                "DOWNLOAD cannot compute hash on failed download\n  from url: \"{}\"\n  status: [{code};\"{message}\"]",
                BString::from(*url),
            )));
        }
        let actual = path::hash_file(cx.host(), &file, expected.algorithm).map_err(|_| {
            error(format!(
                "DOWNLOAD cannot compute hash on download\n  for file: \"{}\"",
                file.display(),
            ))
        })?;
        if actual != expected.value {
            if let Some(status) = download.status {
                cx.set_var(
                    status,
                    format!(
                        "1;HASH mismatch: expected: {} actual: {actual}",
                        expected.value
                    )
                    .into(),
                );
            }
            return Err(error(format!(
                "DOWNLOAD HASH mismatch\n  for file: \"{}\"\n    expected hash: \"{}\"\n      actual hash: \"{actual}\"\n",
                file.display(),
                expected.value,
            )));
        }
    }
    Ok(Flow::Normal)
}

impl<'a> Download<'a> {
    fn parse(cx: &mut Cx<'_>, args: &[&'a [u8]]) -> Result<Self> {
        let mut download = Self {
            flags: [
                "--disable",
                "--globoff",
                "--silent",
                "--show-error",
                "--location",
                "--fail",
            ]
            .map(BString::from)
            .to_vec(),
            ..Self::default()
        };
        let mut verify = setting(cx, b"CMAKE_TLS_VERIFY");
        let mut version = setting(cx, b"CMAKE_TLS_VERSION");
        let mut cainfo = cx.var_or_empty(b"CMAKE_TLS_CAINFO");
        let mut netrc = cx.var_or_empty(b"CMAKE_NETRC");
        let mut netrc_file = cx.var_or_empty(b"CMAKE_NETRC_FILE");
        let mut args = args.iter().copied();
        while let Some(option) = args.next() {
            let label = match option {
                b"TIMEOUT" | b"INACTIVITY_TIMEOUT" => "time",
                b"STATUS" | b"LOG" => "VAR",
                b"TLS_VERSION" | b"RANGE_START" | b"RANGE_END" => "value",
                b"TLS_VERIFY" => "bool value",
                b"TLS_CAINFO" | b"NETRC_FILE" => "file value",
                b"NETRC" => "level value",
                b"EXPECTED_MD5" => "sum value",
                b"EXPECTED_HASH" => "ALGO=value",
                b"HTTPHEADER" | b"USERPWD" => "string",
                b"SHOW_PROGRESS" => {
                    download.progress = true;
                    continue;
                }
                _ => {
                    if download.destination.is_none() {
                        download.destination = (!option.is_empty()).then_some(option);
                    } else {
                        warning(cx, [b"Unexpected argument: ".as_slice(), option].concat());
                    }
                    continue;
                }
            };
            let value = args.next().ok_or_else(|| {
                error(format!(
                    "DOWNLOAD missing {label} for {}.",
                    BString::from(option)
                ))
            })?;
            match option {
                b"LOG" => download.log = Some(value),
                b"STATUS" => download.status = Some(value),
                b"TLS_VERIFY" => verify = Some(value.into()),
                b"TLS_VERSION" => version = Some(value.into()),
                b"TLS_CAINFO" => cainfo = value.into(),
                b"NETRC" => netrc = value.into(),
                b"NETRC_FILE" => netrc_file = value.into(),
                b"HTTPHEADER" if value.starts_with(b"@") => {
                    return Err(error("DOWNLOAD HTTPHEADER cannot start with '@'."));
                }
                b"HTTPHEADER" => download.flags.extend(["--header".into(), value.into()]),
                b"USERPWD" => download.input = credentials(value),
                b"EXPECTED_MD5" => {
                    download.expected = Some(ExpectedHash {
                        algorithm: HashAlgo::Md5,
                        label: "MD5 sum".into(),
                        value: BString::from(value).to_string().to_ascii_lowercase(),
                    })
                }
                b"EXPECTED_HASH" => download.expected = Some(ExpectedHash::parse(value)?),
                b"RANGE_START" => download.ranges.push((value, None)),
                b"RANGE_END" => match download.ranges.last_mut() {
                    None => download.ranges.push((b"0", Some(value))),
                    Some((_, end @ None)) => *end = Some(value),
                    _ => {
                        return Err(error(
                            "Multiple RANGE_END values is provided without the corresponding RANGE_START.",
                        ));
                    }
                },
                b"TIMEOUT" | b"INACTIVITY_TIMEOUT" => {
                    if let Some(seconds) = std::str::from_utf8(value)
                        .ok()
                        .and_then(|text| text.parse::<u64>().ok())
                        .filter(|&seconds| seconds > 0)
                    {
                        if option == b"TIMEOUT" {
                            download.timeout = Some(Duration::from_secs(seconds));
                            download
                                .flags
                                .extend(["--max-time".into(), seconds.to_string().into()]);
                        } else {
                            download.flags.extend([
                                "--speed-limit".into(),
                                "1".into(),
                                "--speed-time".into(),
                                seconds.to_string().into(),
                            ]);
                        }
                    }
                }
                _ => {}
            }
        }
        if !download.input.is_empty() {
            download.flags.extend(["--config".into(), "-".into()]);
        }
        download.default_verify = verify.is_none();
        if verify.is_some_and(|verify| !truth::is_on(&verify)) {
            download.flags.push("--insecure".into());
        }
        if let Some(version) = version {
            let flag = match version.as_slice() {
                b"1.0" => "--tlsv1.0",
                b"1.1" => "--tlsv1.1",
                b"1.2" => "--tlsv1.2",
                b"1.3" => "--tlsv1.3",
                _ => {
                    return Err(error(format!(
                        "DOWNLOAD given unknown TLS/SSL version {version}"
                    )));
                }
            };
            download.flags.push(flag.into());
        }
        if !cainfo.is_empty() {
            download.flags.extend(["--cacert".into(), cainfo]);
        }
        match netrc.to_ascii_uppercase().as_slice() {
            b"" | b"IGNORED" => {}
            level @ (b"OPTIONAL" | b"REQUIRED") => {
                let flag = if level == b"OPTIONAL" {
                    "--netrc-optional"
                } else {
                    "--netrc"
                };
                download.flags.push(flag.into());
                if !netrc_file.is_empty() {
                    download.flags.extend(["--netrc-file".into(), netrc_file]);
                }
            }
            _ => {
                return Err(error(format!(
                    "NETRC accepts OPTIONAL, IGNORED or REQUIRED but got: {netrc}"
                )));
            }
        }
        Ok(download)
    }
}

impl ExpectedHash {
    fn parse(value: &[u8]) -> Result<Self> {
        let mut fields = value.splitn(2, |&byte| byte == b'=');
        let name = fields.next().unwrap_or_default();
        let value = fields.next().ok_or_else(|| {
            error(format!(
                "DOWNLOAD EXPECTED_HASH expects ALGO=value but got: {}",
                BString::from(value),
            ))
        })?;
        let algorithm = HashAlgo::parse(name).ok_or_else(|| {
            error(format!(
                "DOWNLOAD EXPECTED_HASH given unknown ALGO: {}",
                BString::from(name),
            ))
        })?;
        Ok(Self {
            algorithm,
            label: format!("{} hash", BString::from(name)),
            value: BString::from(value).to_string().to_ascii_lowercase(),
        })
    }
}

fn credentials(value: &[u8]) -> BString {
    let mut config = BString::from("user = \"");
    for &byte in value {
        match byte {
            b'\r' => config.extend_from_slice(b"\\r"),
            b'\n' => config.extend_from_slice(b"\\n"),
            b'"' | b'\\' => config.extend([b'\\', byte]),
            _ => config.push(byte),
        }
    }
    config.extend_from_slice(b"\"\n");
    config
}

fn setting(cx: &mut Cx<'_>, name: &[u8]) -> Option<BString> {
    cx.var(name).or_else(|| cx.host().env_get(name))
}

fn set_status(cx: &mut Cx<'_>, name: Option<&[u8]>, code: i32, message: &str) {
    if let Some(name) = name.filter(|name| !name.is_empty()) {
        cx.set_var(name, format!("{code};\"{message}\"").into());
    }
}

fn status_message(code: i32, stderr: &[u8], default_verify: bool) -> String {
    let message = match code {
        0 => "No error",
        3 => "URL using bad/illegal format or missing URL",
        5 => "Could not resolve proxy",
        6 => "Could not resolve hostname",
        7 => "Could not connect to server",
        22 => "HTTP response code said error",
        23 => "Failed writing received data to disk/application",
        28 => "Timeout was reached",
        35 => "SSL connect error",
        37 => "Could not read a file:// file",
        60 => "SSL peer certificate or SSH remote key was not OK",
        _ => {
            let text = String::from_utf8_lossy(stderr);
            return text
                .lines()
                .last()
                .unwrap_or("File transfer failed")
                .to_owned();
        }
    };
    if default_verify && matches!(code, 35 | 60) {
        format!(
            "{message}.  If this is due to https certificate verification failure, one may set environment variable CMAKE_TLS_VERIFY=0 to suppress it."
        )
    } else {
        message.into()
    }
}
