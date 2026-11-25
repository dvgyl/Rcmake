use std::time::Duration;

use rcmake_base::{BString, truth};
use rcmake_eval::{Cx, Flow};
use rcmake_host::{CommandSpec, FileKind, Follow, Input, Output, ProcessResult, RunRequest, path};

use super::{Result, error, warning};

struct Upload {
    options: Vec<BString>,
    credentials: BString,
    log: Option<BString>,
    status: Option<BString>,
    timeout: Option<Duration>,
    progress: bool,
}

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let [file, url, options @ ..] = args else {
        return Err(error(
            "UPLOAD must be called with at least three arguments.",
        ));
    };
    let mut upload = Upload::parse(cx, options)?;
    let host = cx.host();
    let cwd = host.initial_cwd();
    let source = cwd.join(path::to_native(file));
    if !host
        .metadata(&source, Follow::Yes)
        .is_ok_and(|metadata| metadata.kind == FileKind::File)
    {
        return Err(error(format!(
            "UPLOAD cannot open file for reading: {}",
            BString::from(*file)
        )));
    }
    if upload.log.is_some() {
        upload.options.push("--verbose".into());
    }
    if !upload.progress {
        upload.options.push("--silent".into());
    }
    upload.options.extend([
        "--show-error".into(),
        "--upload-file".into(),
        path::from_native(&source),
        "--url".into(),
        super::curl_compat_file_url(url),
    ]);
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: "curl".into(),
            args: upload.options,
        }],
        cwd,
        env: host.env_all(),
        stdin: if upload.credentials.is_empty() {
            Input::Null
        } else {
            Input::Bytes(upload.credentials.into())
        },
        stdout: if upload.log.is_some() {
            Output::Capture
        } else {
            Output::Null
        },
        stderr: Output::Capture,
        timeout: upload
            .timeout
            .or(cx.limits().wall_budget)
            .or(Some(Duration::from_secs(600))),
        tee: upload.progress,
    };
    let result = host
        .run(&request, &mut |stream, bytes| host.write_out(stream, bytes))
        .map_err(|reason| error(format!("UPLOAD error initializing curl: {reason}")))?;
    let status = match result.results.first() {
        Some(ProcessResult::Exited(code)) => *code,
        Some(ProcessResult::TimedOut) => 28,
        Some(ProcessResult::SpawnFailed(reason)) => {
            return Err(error(format!("UPLOAD error initializing curl: {reason}")));
        }
        _ => 2,
    };
    if let Some(var) = upload.status {
        let message = if status == 0 {
            "No error"
        } else if status == 28 {
            "Timeout was reached"
        } else {
            "File transfer failed"
        };
        cx.set_var(&var, format!("{status};\"{message}\"").into());
    }
    if let Some(var) = upload.log {
        let mut log = BString::default();
        for (label, contents) in [
            (b"Response:\n".as_slice(), result.stdout),
            (b"Debug:\n", result.stderr),
        ] {
            if !contents.is_empty() {
                log.extend_from_slice(label);
                log.extend_from_slice(&contents);
                log.push(b'\n');
            }
        }
        cx.set_var(&var, log);
    }
    Ok(Flow::Normal)
}

impl Upload {
    fn parse(cx: &mut Cx<'_>, options: &[&[u8]]) -> Result<Self> {
        let mut args = options.iter().copied();
        let mut upload = Self {
            options: ["--disable", "--globoff", "--location", "--fail"]
                .map(BString::from)
                .to_vec(),
            credentials: BString::default(),
            log: None,
            status: None,
            timeout: None,
            progress: false,
        };
        let mut verify = setting(cx, b"CMAKE_TLS_VERIFY");
        let mut tls = setting(cx, b"CMAKE_TLS_VERSION");
        let mut cainfo = cx.var_or_empty(b"CMAKE_TLS_CAINFO");
        let mut netrc = cx.var_or_empty(b"CMAKE_NETRC");
        let mut netrc_file = cx.var_or_empty(b"CMAKE_NETRC_FILE");
        while let Some(option) = args.next() {
            let missing = match option {
                b"TIMEOUT" | b"INACTIVITY_TIMEOUT" => "time",
                b"STATUS" | b"LOG" => "VAR",
                b"TLS_VERSION" => "value",
                b"TLS_VERIFY" => "bool value",
                b"TLS_CAINFO" | b"NETRC_FILE" => "file value",
                b"NETRC" => "level value",
                b"HTTPHEADER" | b"USERPWD" => "string",
                b"SHOW_PROGRESS" => {
                    upload.progress = true;
                    continue;
                }
                _ => {
                    warning(cx, [b"Unexpected argument: ".as_slice(), option].concat());
                    continue;
                }
            };
            let value = args.next().ok_or_else(|| {
                error(format!(
                    "UPLOAD missing {missing} for {}.",
                    BString::from(option)
                ))
            })?;
            match option {
                b"LOG" => upload.log = Some(value.into()),
                b"STATUS" => upload.status = Some(value.into()),
                b"TLS_VERIFY" => verify = Some(value.into()),
                b"TLS_VERSION" => tls = Some(value.into()),
                b"TLS_CAINFO" => cainfo = value.into(),
                b"NETRC" => netrc = value.into(),
                b"NETRC_FILE" => netrc_file = value.into(),
                b"HTTPHEADER" => upload.options.extend(["--header".into(), value.into()]),
                b"USERPWD" => upload.credentials = user_config(value),
                b"TIMEOUT" | b"INACTIVITY_TIMEOUT" => upload.set_timeout(option, value),
                _ => {}
            }
        }
        if !upload.credentials.is_empty() {
            upload.options.extend(["--config".into(), "-".into()]);
        }
        if let Some(verify) = verify
            && !truth::is_on(&verify)
        {
            upload.options.push("--insecure".into());
        }
        if let Some(version) = tls {
            let option = match version.as_slice() {
                b"1.0" => "--tlsv1.0",
                b"1.1" => "--tlsv1.1",
                b"1.2" => "--tlsv1.2",
                b"1.3" => "--tlsv1.3",
                _ => {
                    return Err(error(format!(
                        "UPLOAD given unknown TLS/SSL version {version}"
                    )));
                }
            };
            upload.options.push(option.into());
        }
        if !cainfo.is_empty() {
            upload.options.extend(["--cacert".into(), cainfo]);
        }
        match netrc.to_ascii_uppercase().as_slice() {
            b"" | b"IGNORED" => {}
            b"OPTIONAL" => upload.options.push("--netrc-optional".into()),
            b"REQUIRED" => upload.options.push("--netrc".into()),
            _ => {
                return Err(error(format!(
                    "NETRC accepts OPTIONAL, IGNORED or REQUIRED but got: {netrc}"
                )));
            }
        }
        if !netrc_file.is_empty() && !netrc.eq_ignore_ascii_case(b"IGNORED") {
            upload.options.extend(["--netrc-file".into(), netrc_file]);
        }
        Ok(upload)
    }

    fn set_timeout(&mut self, option: &[u8], value: &[u8]) {
        let Some(seconds) = std::str::from_utf8(value)
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
            .filter(|&seconds| seconds > 0)
        else {
            return;
        };
        if option == b"TIMEOUT" {
            self.timeout = Some(Duration::from_secs(seconds));
            self.options
                .extend(["--max-time".into(), seconds.to_string().into()]);
        } else {
            self.options.extend([
                "--speed-limit".into(),
                "1".into(),
                "--speed-time".into(),
                seconds.to_string().into(),
            ]);
        }
    }
}

fn setting(cx: &mut Cx<'_>, name: &[u8]) -> Option<BString> {
    cx.var(name).or_else(|| cx.host().env_get(name))
}

fn user_config(value: &[u8]) -> BString {
    let mut config = BString::from("user = \"");
    for &byte in value {
        match byte {
            b'\r' => config.extend_from_slice(b"\\r"),
            b'\n' => config.extend_from_slice(b"\\n"),
            b'"' | b'\\' => {
                config.push(b'\\');
                config.push(byte);
            }
            _ => config.push(byte),
        }
    }
    config.extend_from_slice(b"\"\n");
    config
}
