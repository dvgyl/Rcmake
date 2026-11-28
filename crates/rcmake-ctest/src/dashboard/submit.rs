use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::{
    BString,
    curl::{CurlConfig, CurlOption},
    hash::HashAlgo,
    list, truth,
};
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest, Stream, path,
};

use crate::cli::Invocation;
use crate::dart::{Configuration, Tag};
use crate::junit::write_file;
use rcmake_json::Value;

use super::{Dashboard, definition};

pub(crate) const PARTS: &[&str] = &[
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

pub(super) fn run(
    host: &dyn Host,
    invocation: &Invocation,
    dashboard: &Dashboard,
) -> Result<i32, String> {
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
    let Dashboard {
        build: dir,
        config,
        tag,
        ..
    } = dashboard;
    let log = invocation.options.verbose && !invocation.options.quiet;
    let mut transport = Transport::new(
        false,
        false,
        definition(invocation, b"CTEST_SUBMIT_INACTIVITY_TIMEOUT").or(config
            .submit_inactivity_timeout
            .as_ref()
            .map(|value| value.as_slice())),
        None,
    );
    tls(
        host,
        config,
        |name| definition(invocation, name).map(BString::from),
        log,
        &mut transport.flags,
    )?;
    let job = Submission {
        url: submission_url(config),
        config,
        parts: requested,
        files: Vec::new(),
        subproject: None,
    };
    submit(host, dir, tag, &transport, &job).map(|result| if result.code == 0 { 0 } else { 64 })
}

pub(crate) fn submission_url(config: &Configuration) -> String {
    if config.submit_url.is_empty() {
        format!(
            "{}://{}{}",
            config.drop_method, config.drop_site, config.drop_location
        )
    } else {
        config.submit_url.to_string()
    }
}

pub(crate) struct Submission<'a> {
    pub url: String,
    pub config: &'a Configuration,
    pub parts: Option<Vec<BString>>,
    pub files: Vec<PathBuf>,
    pub subproject: Option<BString>,
}

#[derive(Default)]
pub(crate) struct SubmitResult {
    pub code: i32,
    pub build_id: BString,
}

pub(crate) struct Transport {
    pub flags: Vec<BString>,
    config: CurlConfig,
    pub retries: u32,
    pub delay: Duration,
    pub log: bool,
    pub verbose: bool,
    pub debug: bool,
    timeout: Option<Duration>,
}

impl Transport {
    pub(crate) fn new(
        log: bool,
        verbose: bool,
        inactivity: Option<&[u8]>,
        timeout: Option<Duration>,
    ) -> Self {
        let seconds = inactivity
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(120);
        Self {
            flags: [
                "--disable",
                "--silent",
                "--show-error",
                "--fail",
                "--proto",
                "=http,https",
                "--globoff",
                "--speed-limit",
                "1",
                "--speed-time",
            ]
            .map(BString::from)
            .into_iter()
            .chain([seconds.to_string().into()])
            .collect(),
            config: CurlConfig::default(),
            retries: 0,
            delay: Duration::ZERO,
            log,
            verbose,
            debug: false,
            timeout,
        }
    }

    pub(crate) fn header(&mut self, host: &dyn Host, header: BString) -> Result<(), String> {
        if header.starts_with(b"@") {
            return Err("ctest_submit HTTPHEADER cannot start with '@'.".into());
        }
        self.config
            .push_header(header.as_slice())
            .map_err(|error| format!("Invalid ctest_submit HTTPHEADER: {error}"))?;
        if self.verbose {
            let display = if self.debug {
                String::from_utf8_lossy(header.as_slice()).into_owned()
            } else {
                redact_sensitive_header(header.as_slice())
            };
            host.write_out(
                Stream::Stdout,
                format!("Add HTTP Header: \"{display}\"\n").as_bytes(),
            );
        }
        Ok(())
    }

    fn request(
        &self,
        host: &dyn Host,
        dir: &Path,
        transfer: Transfer<'_>,
    ) -> Result<rcmake_host::RunOutput, String> {
        let curl = path::executable_on_absolute_path(host, "curl")
            .ok_or_else(|| "Cannot find curl for HTTP submission.".to_owned())?;
        let mut args = self.flags.clone();
        let mut post_body: Option<&[u8]> = None;
        let url = match transfer {
            Transfer::Put(file, url) => {
                args.extend(["--upload-file".into(), path::from_native(file)]);
                url
            }
            Transfer::Post(fields, url) => {
                post_body = Some(fields.as_bytes());
                url
            }
        };
        let mut config = self.config.clone();
        config
            .push(CurlOption::Url, &[url.as_bytes()])
            .map_err(|error| format!("Invalid ctest_submit URL: {error}"))?;
        let mut stdin_bytes = config.into_bytes();
        if let Some(fields) = post_body {
            push_curl_config_option(&mut stdin_bytes, b"data-raw", fields)
                .map_err(|byte| format!("Invalid ctest_submit POST data: byte 0x{byte:02x}"))?;
        }
        args.extend(["--config".into(), "-".into()]);
        host.run_bounded(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: path::from_native(&curl),
                    args,
                }],
                cwd: dir.to_path_buf(),
                env: host.env_all(),
                stdin: Input::Bytes(stdin_bytes),
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: self.timeout,
                tee: false,
                default_error_mode: false,
            },
            1024 * 1024,
        )
        .map_err(|error| error.to_string())
    }

    fn attempt(
        &self,
        host: &dyn Host,
        dir: &Path,
        stage: &str,
        transfer: Transfer<'_>,
    ) -> Result<rcmake_host::RunOutput, String> {
        let mut result = self.request(host, dir, transfer)?;
        for retry in 1..=self.retries {
            if success(&result) {
                break;
            }
            self.retry(host, stage, retry);
            result = self.request(host, dir, transfer)?;
        }
        Ok(result)
    }

    fn retry(&self, host: &dyn Host, stage: &str, retry: u32) {
        if self.log {
            let name = match stage {
                "upload" => "Upload",
                "request" => "Request",
                _ => "Submission",
            };
            host.write_out(Stream::Stdout, format!(
                "   {name} failed, waiting {} seconds...\n   Retry {stage}: Attempt {retry} of {}\n",
                self.delay.as_secs(), self.retries,
            ).as_bytes());
        }
        host.sleep(self.delay);
    }
}

#[derive(Clone, Copy)]
enum Transfer<'a> {
    Put(&'a Path, &'a str),
    Post(&'a str, &'a str),
}

fn success(result: &rcmake_host::RunOutput) -> bool {
    matches!(result.results.as_slice(), [ProcessResult::Exited(0)])
}

pub(crate) fn submit(
    host: &dyn Host,
    dir: &Path,
    tag: &Tag,
    transport: &Transport,
    job: &Submission<'_>,
) -> Result<SubmitResult, String> {
    let url = &job.url;
    if !http_url(url) {
        return Err("A dashboard submission requires an HTTP or HTTPS URL.".into());
    }
    let reports = dir.join("Testing").join(tag.stamp());
    let stamp = format!("{}-{}", tag.stamp(), tag.track);
    let mut files = Vec::new();
    for part in PARTS.iter().copied().filter(|part| {
        job.parts
            .as_ref()
            .is_none_or(|parts| parts.iter().any(|name| name == *part))
    }) {
        let name = if part == "MemCheck" {
            "DynamicAnalysis"
        } else {
            part
        };
        let report = reports.join(format!("{name}.xml"));
        if part == "Done" {
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Done><buildId></buildId><time>{}</time></Done>\n",
                host.now().secs
            );
            write_file(host, &report, xml.as_bytes()).map_err(|error| error.to_string())?;
        }
        if host
            .metadata(&report, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            files.push((report, true));
        }
    }
    files.extend(job.files.iter().cloned().map(|file| (file, false)));
    let mut result = SubmitResult::default();
    let config = job.config;
    for (file, report) in files {
        let Some(name) = file.file_name() else {
            continue;
        };
        let remote = format!(
            "{}___{}___{stamp}___XML___{}",
            config.site,
            config.build_name,
            name.to_string_lossy()
        )
        .replace(
            ['\\', ':', '*', '?', '"', '<', '>', '|', '\n', '\r', '\t'],
            "",
        );
        let md5 = path::hash_file(host, &file, HashAlgo::Md5).map_err(|error| error.to_string())?;
        let separator = if url.contains('?') { '&' } else { '?' };
        let mut destination = format!("{url}{separator}FileName={}", encode(remote.as_bytes()));
        if report {
            destination.push_str(&format!(
                "&build={}&site={}&stamp={}",
                encode(&config.build_name),
                encode(&config.site),
                encode(stamp.as_bytes())
            ));
            if let Some(project) = &job.subproject {
                destination.push_str(&format!("&subproject={}", encode(project)));
            }
        }
        destination.push_str(&format!("&MD5={md5}"));
        if transport.verbose {
            let display_url = redact_debug_url(&destination);
            host.write_out(
                Stream::Stdout,
                format!(
                    "Upload file: {} to {display_url} Size: {}\n",
                    path::from_native_cmake(&file),
                    host.metadata(&file, Follow::Yes)
                        .map_err(|error| error.to_string())?
                        .len
                )
                .as_bytes(),
            );
        }
        let output =
            transport.attempt(host, dir, "submission", Transfer::Put(&file, &destination))?;
        if !success(&output) {
            let message = String::from_utf8_lossy(&output.stderr);
            let message = message
                .trim()
                .split_once(") ")
                .map_or(message.trim(), |(_, detail)| detail);
            host.write_out(Stream::Stderr, format!(
                "Error when uploading file: {}\nError message was: {message}\n   Problems when submitting via HTTP\n",
                path::from_native_cmake(&file),
            ).as_bytes());
            result.code = -1;
            break;
        }
        let body = String::from_utf8_lossy(&output.stdout);
        if let Some((_, id)) = body.split_once("<buildId>")
            && let Some((id, _)) = id.split_once("</buildId>")
        {
            result.build_id = id.into();
        }
    }
    Ok(result)
}

pub(crate) fn tls(
    host: &dyn Host,
    config: &Configuration,
    definition: impl Fn(&[u8]) -> Option<BString>,
    log: bool,
    args: &mut Vec<BString>,
) -> Result<(), String> {
    let version = definition(b"CTEST_TLS_VERSION")
        .or_else(|| config.tls_version.clone())
        .or_else(|| definition(b"CMAKE_TLS_VERSION"))
        .or_else(|| host.env_get(b"CMAKE_TLS_VERSION"));
    let verify = definition(b"CTEST_TLS_VERIFY")
        .or_else(|| config.tls_verify.clone())
        .or_else(|| definition(b"CMAKE_TLS_VERIFY"))
        .or_else(|| host.env_get(b"CMAKE_TLS_VERIFY"));
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

pub(crate) struct CDashUpload<'a> {
    pub job: &'a Submission<'a>,
    pub file: &'a Path,
    pub kind: &'a [u8],
    pub checksum_test: bool,
}

pub(crate) fn cdash_upload(
    host: &dyn Host,
    dir: &Path,
    tag: &Tag,
    transport: &Transport,
    upload: CDashUpload<'_>,
) -> Result<SubmitResult, String> {
    let job = upload.job;
    if !http_url(&job.url) {
        return Err("Only http and https are supported for CDASH_UPLOAD".into());
    }
    let md5 =
        path::hash_file(host, upload.file, HashAlgo::Md5).map_err(|error| error.to_string())?;
    let (url, query) = job.url.split_once('?').unwrap_or((&job.url, ""));
    let stamp = encode(format!("{}-{}", tag.stamp(), tag.track).as_bytes());
    let group = encode(tag.track.as_bytes());
    let config = job.config;
    let build = encode(&config.build_name);
    let site = encode(&config.site);
    let kind = encode(upload.kind);
    let now = host.now().secs;
    let mut fields = String::from(query);
    if !fields.is_empty() {
        fields.push('&');
    }
    if let Some(project) = &job.subproject {
        fields.push_str(&format!("subproject={}&", encode(project)));
    }
    fields.push_str(&format!("stamp={stamp}&model={group}&build={build}&site={site}&group={group}&track={group}&starttime={now}&endtime={now}&datafilesmd5[0]={md5}&type={kind}"));
    if upload.checksum_test {
        for retry in 1..=transport.retries {
            transport.retry(host, "upload", retry);
        }
        return Err(format!(
            "error uploading to CDash. {}",
            upload.file.display()
        ));
    }
    let response = transport.attempt(host, dir, "request", Transfer::Post(&fields, url))?;
    if !success(&response) {
        return Err(format!(
            "Error in HttpRequest\n{}",
            String::from_utf8_lossy(&response.stderr)
        ));
    }
    let info = cdash_response(&response.stdout)?;
    let build_id = match info.get(b"buildid".as_slice()) {
        Some(Value::String(id)) => id.clone(),
        Some(Value::Int(id)) => id.to_string().into(),
        Some(Value::UInt(id)) => id.to_string().into(),
        _ => BString::default(),
    };
    let Some(Value::Array(hash_status)) = info.get(b"datafilesmd5".as_slice()) else {
        return Err("CDash response has no datafilesmd5 array".into());
    };
    if !matches!(hash_status.first(), Some(Value::Int(1) | Value::UInt(1))) {
        let file = encode(
            upload
                .file
                .file_name()
                .map_or_else(BString::default, |name| path::from_native(Path::new(name)))
                .as_slice(),
        );
        let id = encode(&build_id);
        let url = format!("{url}?type={kind}&md5={md5}&filename={file}&buildid={id}");
        let response = transport.attempt(host, dir, "upload", Transfer::Put(upload.file, &url))?;
        if !success(&response) {
            return Err(format!(
                "error uploading to CDash. {}",
                upload.file.display()
            ));
        }
        cdash_response(&response.stdout)?;
    }
    Ok(SubmitResult { code: 0, build_id })
}

fn cdash_response(bytes: &[u8]) -> Result<std::collections::BTreeMap<BString, Value>, String> {
    let Value::Object(mut data) = rcmake_json::parse(bytes, &Default::default())
        .map_err(|error| format!("error parsing json string: {error}"))?
    else {
        return Err("error parsing json string: expected an object".into());
    };
    if let Some(status) = data.remove(b"status".as_slice())
        && !matches!(status, Value::Int(0) | Value::UInt(0))
    {
        return Err("Bad status returned from CDash".into());
    }
    Ok(data)
}

fn http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn push_curl_config_option(buf: &mut Vec<u8>, option: &[u8], value: &[u8]) -> Result<(), u8> {
    if let Some(&bad) = value.iter().find(|&&byte| matches!(byte, 0x00 | 0x1A)) {
        return Err(bad);
    }
    buf.extend_from_slice(option);
    buf.extend_from_slice(b" = \"");
    for &byte in value {
        match byte {
            b'\\' => buf.extend_from_slice(b"\\\\"),
            b'"' => buf.extend_from_slice(b"\\\""),
            b'\t' => buf.extend_from_slice(b"\\t"),
            b'\n' => buf.extend_from_slice(b"\\n"),
            b'\r' => buf.extend_from_slice(b"\\r"),
            0x0B => buf.extend_from_slice(b"\\v"),
            _ => buf.push(byte),
        }
    }
    buf.extend_from_slice(b"\"\n");
    Ok(())
}

fn is_sensitive_header_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "x-api-key"
    ) || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("api-key")
        || lower.contains("apikey")
}

fn redact_sensitive_header(header: &[u8]) -> String {
    let text = String::from_utf8_lossy(header);
    match text.split_once(':') {
        Some((name, _)) if is_sensitive_header_name(name) => {
            format!("{}: <redacted>", name.trim())
        }
        Some(_) => text.into_owned(),
        None => "<redacted>".to_owned(),
    }
}

fn is_public_cdash_param(key: &str) -> bool {
    matches!(
        key,
        "FileName"
            | "build"
            | "site"
            | "stamp"
            | "subproject"
            | "MD5"
            | "project"
            | "type"
            | "filename"
            | "buildid"
    )
}

fn redact_debug_url(url: &str) -> String {
    let (without_frag, frag) = match url.split_once('#') {
        Some((before, after)) => (before, Some(after)),
        None => (url, None),
    };
    let (base, query) = match without_frag.split_once('?') {
        Some((before, after)) => (before, Some(after)),
        None => (without_frag, None),
    };
    let mut out = if let Some((scheme, rest)) = base.split_once("://") {
        let (authority, path_part) = match rest.split_once('/') {
            Some((auth, path)) => (auth, Some(path)),
            None => (rest, None),
        };
        if let Some((_, host_port)) = authority.rsplit_once('@') {
            match path_part {
                Some(path) => format!("{scheme}://<redacted>@{host_port}/{path}"),
                None => format!("{scheme}://<redacted>@{host_port}"),
            }
        } else {
            base.to_owned()
        }
    } else {
        base.to_owned()
    };
    if let Some(query) = query {
        out.push('?');
        for (index, param) in query.split('&').enumerate() {
            if index > 0 {
                out.push('&');
            }
            if let Some((key, value)) = param.split_once('=') {
                out.push_str(key);
                out.push('=');
                if is_public_cdash_param(key) {
                    out.push_str(value);
                } else {
                    out.push_str("<redacted>");
                }
            } else if !param.is_empty() {
                out.push_str("<redacted>");
            }
        }
    }
    if frag.is_some() {
        out.push_str("#<redacted>");
    }
    out
}

pub(crate) fn redact_url(url: &str) -> String {
    let bytes = url.as_bytes();
    let valid_scheme = bytes
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"http://"))
        || bytes
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"https://"));
    if !valid_scheme {
        return "<redacted-invalid-url>".to_owned();
    }
    let (without_frag, frag) = match url.split_once('#') {
        Some((before, after)) => (before, Some(after)),
        None => (url, None),
    };
    let (base, query) = match without_frag.split_once('?') {
        Some((before, after)) => (before, Some(after)),
        None => (without_frag, None),
    };
    let mut out = if let Some((scheme, rest)) = base.split_once("://") {
        let (authority, path_part) = match rest.split_once('/') {
            Some((auth, path)) => (auth, Some(path)),
            None => (rest, None),
        };
        if let Some((_, host_port)) = authority.rsplit_once('@') {
            match path_part {
                Some(path) => format!("{scheme}://<redacted>@{host_port}/{path}"),
                None => format!("{scheme}://<redacted>@{host_port}"),
            }
        } else {
            base.to_owned()
        }
    } else {
        base.to_owned()
    };
    if let Some(query) = query {
        out.push('?');
        for (index, param) in query.split('&').enumerate() {
            if index > 0 {
                out.push('&');
            }
            if let Some((key, _)) = param.split_once('=') {
                out.push_str(key);
                out.push_str("=<redacted>");
            } else if !param.is_empty() {
                out.push_str("<redacted>");
            }
        }
    }
    if frag.is_some() {
        out.push_str("#<redacted>");
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::{HostError, PureHost, RunOutput};

    #[test]
    fn rejects_header_file_arguments() -> Result<(), String> {
        let host = PureHost::default();
        let mut transport = Transport::new(false, false, None, None);
        assert!(transport.header(&host, "@secret".into()).is_err());
        assert!(!transport.flags.iter().any(|flag| flag == "--header"));
        assert!(transport.config.as_bytes().is_empty());
        transport.header(&host, "X-Example: value".into())?;
        assert!(!transport.flags.iter().any(|flag| flag == "--header"));
        assert_eq!(
            transport.config.as_bytes(),
            b"header = \"X-Example: value\"\n"
        );
        Ok(())
    }

    #[test]
    fn keeps_submit_credentials_and_headers_off_argv() -> Result<(), Box<dyn std::error::Error>> {
        let dir = if cfg!(windows) { "C:/work" } else { "/work" };
        let host = PureHost::new(dir.into(), dir.into());
        let curl = Path::new(dir).join(format!("curl{}", std::env::consts::EXE_SUFFIX));
        host.seed_file(&curl, b"")?;
        host.set_permissions(&curl, 0o755)?;
        host.env_set(b"PATH", Some(dir.as_bytes()));
        let report = Path::new(dir).join("Testing/20261126-0000/Test.xml");
        host.seed_file(&report, b"<Site/>")?;
        host.seed_file(
            &Path::new(dir).join("Testing/TAG"),
            b"20261126-0000\nExperimental\nExperimental\n",
        )?;
        host.set_command_handler(|_, request, _| {
            let args = &request
                .pipeline
                .first()
                .ok_or_else(|| HostError::unsupported("Missing submission command"))?
                .args;
            assert!(args.windows(2).any(|pair| pair == ["--config", "-"]));
            for arg in args {
                let text = String::from_utf8_lossy(arg);
                assert!(!text.contains("pw"), "password leaked in argv: {text}");
                assert!(!text.contains("t0ken"), "token leaked in argv: {text}");
            }
            let Input::Bytes(stdin) = &request.stdin else {
                panic!("Expected curl config bytes on stdin.");
            };
            let stdin = String::from_utf8_lossy(stdin);
            assert!(stdin.contains("header = \"Authorization: Bearer t0ken\"\n"));
            assert!(stdin.contains("url = \"https://u:pw@cdash.invalid/submit.php?project=P&"));
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        let mut transport = Transport::new(false, false, None, None);
        transport.header(&host, "Authorization: Bearer t0ken".into())?;
        let config = Configuration {
            site: "site".into(),
            build_name: "build".into(),
            submit_url: "https://u:pw@cdash.invalid/submit.php?project=P".into(),
            ..Configuration::default()
        };
        let tag = Tag::read(&host, Path::new(dir))?;
        let job = Submission {
            url: submission_url(&config),
            config: &config,
            parts: Some(vec!["Test".into()]),
            files: Vec::new(),
            subproject: None,
        };
        let outcome = submit(&host, Path::new(dir), &tag, &transport, &job)?;
        assert_eq!(outcome.code, 0);
        Ok(())
    }

    #[test]
    fn sends_literal_post_data_and_applies_the_inactivity_timeout()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = if cfg!(windows) { "C:/work" } else { "/work" };
        for (configured, speed_time, timeout) in [
            (None, "120", None),
            (Some(b"45".as_slice()), "45", Some(Duration::from_secs(9))),
        ] {
            let host = PureHost::new(dir.into(), dir.into());
            let curl = Path::new(dir).join(format!("curl{}", std::env::consts::EXE_SUFFIX));
            host.seed_file(&curl, b"")?;
            host.set_permissions(&curl, 0o755)?;
            host.env_set(b"PATH", Some(dir.as_bytes()));
            host.set_command_handler(move |_, request, _| {
                let args = &request
                    .pipeline
                    .first()
                    .ok_or_else(|| HostError::unsupported("Missing submission command"))?
                    .args;
                assert_eq!(request.timeout, timeout);
                assert!(args.windows(2).any(|pair| pair == ["--speed-limit", "1"]));
                assert!(
                    args.windows(2)
                        .any(|pair| pair == ["--speed-time", speed_time])
                );
                assert!(
                    !args.iter().any(|arg| arg == "--data-raw"
                        || arg.as_slice().windows(7).any(|w| w == b"literal")),
                    "POST body must stay off curl argv: {args:?}"
                );
                let Input::Bytes(stdin) = &request.stdin else {
                    panic!("Expected curl config bytes on stdin.");
                };
                let stdin = String::from_utf8_lossy(stdin);
                assert!(
                    stdin.contains("data-raw = \"@literal&key=value\"\n"),
                    "{stdin}"
                );
                Ok(RunOutput::default())
            })?;
            Transport::new(false, false, configured, timeout).request(
                &host,
                Path::new(dir),
                Transfer::Post("@literal&key=value", "https://example.invalid/"),
            )?;
        }
        Ok(())
    }

    #[test]
    fn rejects_crlf_headers_before_verbose_logging() {
        let host = PureHost::default();
        let mut transport = Transport::new(false, true, None, None);
        for bad in [
            "X-Test: a\r\nX-Evil: 1",
            "Authorization: Bearer tok\nX-Evil: 1",
            "Proxy-Authorization: Basic abc\rX-Evil: 1",
        ] {
            let err = transport
                .header(&host, bad.into())
                .expect_err("CRLF header must be rejected");
            assert!(err.contains("Invalid ctest_submit HTTPHEADER"), "{err}");
        }
        assert!(transport.config.as_bytes().is_empty());
        assert!(host.stream_bytes(Stream::Stdout).is_empty());
    }

    #[test]
    fn verbose_redacts_sensitive_headers_and_url_credentials()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = if cfg!(windows) { "C:/work" } else { "/work" };
        let host = PureHost::new(dir.into(), dir.into());
        let curl = Path::new(dir).join(format!("curl{}", std::env::consts::EXE_SUFFIX));
        host.seed_file(&curl, b"")?;
        host.set_permissions(&curl, 0o755)?;
        host.env_set(b"PATH", Some(dir.as_bytes()));
        let report = Path::new(dir).join("Testing/20261126-0000/Test.xml");
        host.seed_file(&report, b"<Site/>")?;
        host.seed_file(
            &Path::new(dir).join("Testing/TAG"),
            b"20261126-0000\nExperimental\nExperimental\n",
        )?;
        host.set_command_handler(|_, request, _| {
            let Input::Bytes(stdin) = &request.stdin else {
                panic!("Expected curl config bytes on stdin.");
            };
            let stdin = String::from_utf8_lossy(stdin);
            assert!(stdin.contains("header = \"Authorization: Bearer secret_tok\"\n"));
            assert!(stdin.contains("header = \"X-Example: safe_value\"\n"));
            assert!(stdin.contains(
                "url = \"https://u:p4ss@cdash.invalid/submit.php?project=P&foo=secret_foo&%74oken=qtok&bare_secret&"
            ));
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        let mut transport = Transport::new(false, true, None, None);
        for header in [
            "Authorization: Bearer secret_tok",
            "Proxy-Authorization: Basic cHJveHk=",
            "Cookie: sid=cookie_secret",
            "X-Api-Key: key_secret",
            "X-Custom-Token: custom_tok",
            "X-Example: safe_value",
            "NoColonSecret",
        ] {
            transport.header(&host, header.into())?;
        }
        let config = Configuration {
            site: "site".into(),
            build_name: "build".into(),
            submit_url: "https://u:p4ss@cdash.invalid/submit.php?project=P&foo=secret_foo&%74oken=qtok&bare_secret"
                .into(),
            ..Configuration::default()
        };
        let tag = Tag::read(&host, Path::new(dir))?;
        let job = Submission {
            url: submission_url(&config),
            config: &config,
            parts: Some(vec!["Test".into()]),
            files: Vec::new(),
            subproject: None,
        };
        let outcome = submit(&host, Path::new(dir), &tag, &transport, &job)?;
        assert_eq!(outcome.code, 0);
        let stdout = host.stream_bytes(Stream::Stdout).to_string();
        for secret in [
            "secret_tok",
            "cHJveHk=",
            "cookie_secret",
            "key_secret",
            "custom_tok",
            "NoColonSecret",
            "u:p4ss",
            "p4ss",
            "secret_foo",
            "qtok",
            "bare_secret",
        ] {
            assert!(
                !stdout.contains(secret),
                "secret leaked in verbose stdout: {stdout}"
            );
        }
        assert!(
            stdout.contains("Add HTTP Header: \"Authorization: <redacted>\""),
            "{stdout}"
        );
        assert!(
            stdout.contains("Add HTTP Header: \"Proxy-Authorization: <redacted>\""),
            "{stdout}"
        );
        assert!(
            stdout.contains("Add HTTP Header: \"Cookie: <redacted>\""),
            "{stdout}"
        );
        assert!(
            stdout.contains("Add HTTP Header: \"X-Api-Key: <redacted>\""),
            "{stdout}"
        );
        assert!(
            stdout.contains("Add HTTP Header: \"X-Custom-Token: <redacted>\""),
            "{stdout}"
        );
        assert!(
            stdout.contains("Add HTTP Header: \"X-Example: safe_value\""),
            "{stdout}"
        );
        assert!(
            stdout.contains("Add HTTP Header: \"<redacted>\""),
            "{stdout}"
        );
        assert!(
            stdout.contains("https://<redacted>@cdash.invalid/submit.php?project=P&foo=<redacted>&%74oken=<redacted>&<redacted>&FileName="),
            "{stdout}"
        );
        assert_eq!(
            redact_url("https://user:pw@example.invalid/p?a=1&%74oken=xyz#frag_secret"),
            "https://<redacted>@example.invalid/p?a=<redacted>&%74oken=<redacted>#<redacted>"
        );
        for invalid in [
            "//user:secret@cdash.example/submit.php?token=xyz",
            "user:secret@cdash.example/submit.php",
            "ftp://user:secret@cdash.example/submit.php?token=xyz",
            "not-a-url",
        ] {
            assert_eq!(redact_url(invalid), "<redacted-invalid-url>", "{invalid}");
        }
        Ok(())
    }

    #[test]
    fn debug_mode_matches_ctest_submit_headers_and_keeps_cdash_upload_post_off_argv()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = if cfg!(windows) { "C:/work" } else { "/work" };
        let host = PureHost::new(dir.into(), dir.into());
        let curl = Path::new(dir).join(format!("curl{}", std::env::consts::EXE_SUFFIX));
        host.seed_file(&curl, b"")?;
        host.set_permissions(&curl, 0o755)?;
        host.env_set(b"PATH", Some(dir.as_bytes()));
        let upload_file = Path::new(dir).join("artifact.bin");
        host.seed_file(&upload_file, b"payload")?;
        host.seed_file(
            &Path::new(dir).join("Testing/TAG"),
            b"20261126-0000\nExperimental\nExperimental\n",
        )?;
        host.set_command_handler(|_, request, _| {
            let args = &request
                .pipeline
                .first()
                .ok_or_else(|| HostError::unsupported("Missing submission command"))?
                .args;
            for arg in args {
                let text = String::from_utf8_lossy(arg);
                assert!(
                    !text.contains("qsecret"),
                    "POST secret leaked in argv: {text}"
                );
                assert!(
                    !text.contains("asdf"),
                    "header secret leaked in argv: {text}"
                );
            }
            let Input::Bytes(stdin) = &request.stdin else {
                panic!("Expected curl config bytes on stdin.");
            };
            let stdin = String::from_utf8_lossy(stdin);
            assert!(
                stdin.contains("header = \"Authorization: Bearer asdf\"\n"),
                "{stdin}"
            );
            assert!(stdin.contains("data-raw = \"token=qsecret&"), "{stdin}");
            Ok(RunOutput {
                stdout: br#"{"status":0,"buildid":"42","datafilesmd5":[1]}"#.to_vec(),
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        let mut transport = Transport::new(false, true, None, None);
        transport.debug = true;
        transport.header(&host, "Authorization: Bearer asdf".into())?;
        let config = Configuration {
            site: "test-site".into(),
            build_name: "test-build-name".into(),
            submit_url: "https://cdash.invalid/submit.php?token=qsecret".into(),
            ..Configuration::default()
        };
        let tag = Tag::read(&host, Path::new(dir))?;
        let job = Submission {
            url: submission_url(&config),
            config: &config,
            parts: Some(Vec::new()),
            files: Vec::new(),
            subproject: None,
        };
        let outcome = cdash_upload(
            &host,
            Path::new(dir),
            &tag,
            &transport,
            CDashUpload {
                job: &job,
                file: &upload_file,
                kind: b"foo",
                checksum_test: false,
            },
        )?;
        assert_eq!(outcome.code, 0);
        let stdout = host.stream_bytes(Stream::Stdout).to_string();
        assert!(
            stdout.contains("Add HTTP Header: \"Authorization: Bearer asdf\""),
            "{stdout}"
        );
        Ok(())
    }
}
