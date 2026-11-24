//! Collect per-process resource use through the platform time utility.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::BString;
use rcmake_host::{
    FileKind, Follow, Host, HostResult, ReadRange, RunOutput, RunRequest, Stream, WriteMode, path,
};

use crate::snippet::ProcessMetrics;

static NEXT_REPORT: AtomicU64 = AtomicU64::new(0);

/// Run a process and collect resource metrics when the platform provides them.
///
/// # Errors
///
/// Returns a host error if starting or waiting for the process fails.
pub fn run(
    host: &dyn Host,
    request: &RunRequest,
    scratch: &Path,
    collect: bool,
    on_output: &mut dyn FnMut(Stream, &[u8]),
) -> HostResult<(RunOutput, Option<ProcessMetrics>)> {
    let direct = |on_output: &mut dyn FnMut(Stream, &[u8])| {
        host.run(request, on_output).map(|result| (result, None))
    };
    let [command] = request.pipeline.as_slice() else {
        return direct(on_output);
    };
    let time = Path::new("/usr/bin/time");
    if !collect
        || !host
            .metadata(time, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return direct(on_output);
    }
    let system = host.system();
    let format = match system.sysname.as_slice() {
        b"Darwin" => Format::Darwin,
        b"Linux" => Format::Linux,
        _ => return direct(on_output),
    };
    let sequence = NEXT_REPORT.fetch_add(1, Ordering::Relaxed);
    let report = scratch.join(format!(".metrics-{}-{sequence}", system.pid));
    if host
        .create_dir_all(scratch)
        .and_then(|()| host.write(&report, b"", WriteMode::CreateNew))
        .is_err()
    {
        return direct(on_output);
    }
    let mut args = match format {
        Format::Darwin => vec!["-l".into(), "-o".into(), path::from_native(&report)],
        Format::Linux => vec![
            "-f".into(),
            "%M %U %S".into(),
            "-o".into(),
            path::from_native(&report),
        ],
    };
    args.extend([BString::from("--"), command.program.clone()]);
    args.extend(command.args.iter().cloned());
    let wrapped = RunRequest {
        pipeline: vec![rcmake_host::CommandSpec {
            program: path::from_native(time),
            args,
        }],
        ..request.clone()
    };
    let run = host.run(&wrapped, on_output);
    let metrics = host
        .read(&report, ReadRange::All { limit: 8192 })
        .ok()
        .and_then(|data| format.parse(&data));
    let _ = host.remove_file(&report);
    run.map(|output| (output, metrics))
}

enum Format {
    Darwin,
    Linux,
}

impl Format {
    fn parse(self, bytes: &[u8]) -> Option<ProcessMetrics> {
        let report = std::str::from_utf8(bytes).ok()?;
        let seconds = |text: &str| {
            let seconds: f64 = text.replace(',', ".").parse().ok()?;
            (seconds.is_finite() && seconds >= 0.0)
                .then_some((seconds * 1_000_000.0).round() as u64)
        };
        let mut lines = report.lines();
        match self {
            Self::Linux => {
                let mut fields = lines.next_back()?.split_ascii_whitespace();
                Some(ProcessMetrics {
                    max_rss: fields.next()?.parse().ok()?,
                    user_time: seconds(fields.next()?)?,
                    system_time: seconds(fields.next()?)?,
                })
            }
            Self::Darwin => {
                let mut fields = lines
                    .find(|line| line.contains(" real "))?
                    .split_ascii_whitespace();
                let user_time = seconds(fields.nth(2)?)?;
                let system_time = seconds(fields.nth(1)?)?;
                let rss: u64 = lines
                    .find(|line| line.ends_with("maximum resident set size"))?
                    .split_ascii_whitespace()
                    .next()?
                    .parse()
                    .ok()?;
                Some(ProcessMetrics {
                    max_rss: rss / 1024,
                    user_time,
                    system_time,
                })
            }
        }
    }
}
