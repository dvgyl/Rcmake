use std::time::Duration;

use rcmake_base::{
    BString,
    curl::{CurlConfig, CurlOption},
    truth,
};
use rcmake_eval::{Cx, Flow};
use rcmake_host::{CommandSpec, FileKind, Follow, Input, Output, ProcessResult, RunRequest, path};

use super::{Result, error, warning};

struct Upload {
    options: Vec<BString>,
    config: CurlConfig,
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
    let cwd = cx.host().initial_cwd();
    let source = cwd.join(path::to_native(file));
    if !cx
        .host()
        .metadata(&source, Follow::Yes)
        .is_ok_and(|metadata| metadata.kind == FileKind::File)
    {
        return Err(error(format!(
            "UPLOAD cannot open file for reading: {}",
            BString::from(*file)
        )));
    }
    let transfer_url = std::str::from_utf8(url).unwrap_or("<invalid URL>");
    if cx.host().supports_process_execution() {
        crate::hermetic::check_network(cx, transfer_url, false)?;
    }
    let host = cx.host();
    if upload.log.is_some() {
        upload.options.push("--verbose".into());
    }
    if !upload.progress {
        upload.options.push("--silent".into());
    }
    let compat_url = super::curl_compat_file_url(url);
    upload
        .config
        .push(CurlOption::Url, &[compat_url.as_slice()])
        .map_err(|reason| error(format!("UPLOAD error initializing curl: {reason}")))?;
    upload.options.extend([
        "--show-error".into(),
        "--upload-file".into(),
        path::from_native(&source),
        "--config".into(),
        "-".into(),
    ]);
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: "curl".into(),
            args: upload.options,
        }],
        cwd,
        env: host.env_all(),
        stdin: Input::Bytes(upload.config.into_bytes()),
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
        default_error_mode: false,
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
            config: CurlConfig::default(),
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
                b"HTTPHEADER" if value.starts_with(b"@") => {
                    return Err(error("UPLOAD HTTPHEADER cannot start with '@'."));
                }
                b"HTTPHEADER" => {
                    upload.config.push_header(value).map_err(|reason| {
                        error(format!("UPLOAD error initializing curl: {reason}"))
                    })?;
                }
                b"USERPWD" => {
                    upload
                        .config
                        .push(CurlOption::User, &[value])
                        .map_err(|reason| {
                            error(format!("UPLOAD error initializing curl: {reason}"))
                        })?;
                }
                b"TIMEOUT" | b"INACTIVITY_TIMEOUT" => upload.set_timeout(option, value),
                _ => {}
            }
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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use rcmake_eval::{Interp, InterpConfig, RunMode};
    use rcmake_host::{Input, ProcessResult, PureHost, RunOutput, Stream};

    #[test]
    fn upload_keeps_secrets_off_argv_and_rejects_at_headers()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/work/payload.bin"), b"data")?;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        host.set_command_handler(move |_, request, _| {
            sink.lock()
                .expect("lock capture")
                .push((request.pipeline.clone(), request.stdin.clone()));
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        let script = Path::new("/work/upload.cmake");
        host.seed_file(
            script,
            br#"file(UPLOAD "/work/payload.bin" "https://u:s3cret@example.invalid/f"
  HTTPHEADER "Authorization: Bearer t0ken"
  USERPWD "a:pw"
  STATUS st)"#,
        )?;
        let make_interp = |host: Arc<PureHost>| {
            Interp::new(
                InterpConfig {
                    host,
                    mode: RunMode::Script,
                    cmake_root: "/cmake".into(),
                    cmake_command: "/cmake/bin/cmake".into(),
                    cwd: "/work".into(),
                    limits: Default::default(),
                    diag: Default::default(),
                    services: Default::default(),
                    defines: Vec::new(),
                    script_args: Vec::new(),
                },
                crate::register_all,
            )
        };
        let mut interp = make_interp(host.clone());
        assert_eq!(interp.run_script(script).exit_code, 0);
        let calls = captured.lock().expect("lock capture");
        assert_eq!(calls.len(), 1);
        let (pipeline, stdin) = &calls[0];
        let [command] = pipeline.as_slice() else {
            panic!("Expected one curl invocation.");
        };
        assert_eq!(
            command.args.first().map(AsRef::as_ref),
            Some(b"--disable".as_slice())
        );
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| pair == ["--config", "-"])
        );
        for arg in &command.args {
            let text = String::from_utf8_lossy(arg);
            for secret in ["s3cret", "t0ken", "a:pw"] {
                assert!(!text.contains(secret), "secret leaked in argv: {text}");
            }
        }
        assert_eq!(
            stdin,
            &Input::Bytes(
                b"header = \"Authorization: Bearer t0ken\"\nuser = \"a:pw\"\nurl = \"https://u:s3cret@example.invalid/f\"\n"
                    .as_slice()
                    .into()
            )
        );

        let rejected_host = Arc::new(PureHost::default());
        rejected_host.seed_file(Path::new("/work/payload.bin"), b"data")?;
        rejected_host
            .set_command_handler(|_, _, _| panic!("Rejected upload header reached subprocess."))?;
        let bad_script = Path::new("/work/bad_upload.cmake");
        rejected_host.seed_file(
            bad_script,
            br#"file(UPLOAD "/work/payload.bin" "https://example.invalid/f" HTTPHEADER "@/etc/passwd")"#,
        )?;
        let mut interp = make_interp(rejected_host.clone());
        assert_eq!(interp.run_script(bad_script).exit_code, 1);
        let stderr = rejected_host.stream_bytes(Stream::Stderr).to_string();
        assert!(
            stderr.contains("UPLOAD HTTPHEADER cannot start with '@'."),
            "{stderr}"
        );

        let crlf_host = Arc::new(PureHost::default());
        crlf_host.seed_file(Path::new("/work/payload.bin"), b"data")?;
        crlf_host
            .set_command_handler(|_, _, _| panic!("Rejected upload header reached subprocess."))?;
        let crlf_script = Path::new("/work/bad_crlf_upload.cmake");
        crlf_host.seed_file(
            crlf_script,
            b"file(UPLOAD \"/work/payload.bin\" \"https://example.invalid/f\" HTTPHEADER \"Authorization: Bearer tok\nX-Evil: 1\")",
        )?;
        let mut interp = make_interp(crlf_host.clone());
        assert_eq!(interp.run_script(crlf_script).exit_code, 1);
        let stderr = crlf_host.stream_bytes(Stream::Stderr).to_string();
        assert!(
            stderr.contains("UPLOAD error initializing curl"),
            "{stderr}"
        );
        Ok(())
    }
}
