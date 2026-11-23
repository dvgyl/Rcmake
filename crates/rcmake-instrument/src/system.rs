//! System information available through the host.

use std::path::Path;
use std::time::Duration;

use rcmake_host::{CommandSpec, Host, Input, Output, ProcessResult, RunRequest};
use rcmake_json::Value;

use crate::Object;
use crate::files::read_bytes;

pub(crate) fn static_information(host: &dyn Host) -> Value {
    let info = host.system();
    let mut fields: Object = [
        "familyId",
        "modelId",
        "modelName",
        "numberOfPhysicalCPU",
        "processorAPICID",
        "processorCacheSize",
        "processorClockFrequency",
        "processorName",
        "totalPhysicalMemory",
        "totalVirtualMemory",
        "vendorID",
        "vendorString",
    ]
    .into_iter()
    .map(|key| (key.into(), Value::Null))
    .collect();
    fields.extend([
        ("OSName".into(), Value::String(info.sysname)),
        ("OSPlatform".into(), Value::String(info.machine)),
        ("OSRelease".into(), Value::String(info.release)),
        ("OSVersion".into(), Value::String(info.version)),
        ("hostname".into(), Value::String(info.hostname)),
        (
            "is64Bits".into(),
            Value::Bool(cfg!(target_pointer_width = "64")),
        ),
        (
            "numberOfLogicalCPU".into(),
            Value::UInt(u64::from(info.cpu_count)),
        ),
    ]);
    if let Some(info) = meminfo(host) {
        for (field, key) in [
            ("totalPhysicalMemory", b"MemTotal:".as_slice()),
            ("totalVirtualMemory", b"SwapTotal:"),
        ] {
            if let Some(value) = memory_value(&info, key)
                .map(|kib| kib / 1024)
                .filter(|mib| *mib > 0)
            {
                fields.insert(field.into(), Value::UInt(value));
            }
        }
    }
    Value::Object(fields)
}

/// A point-in-time sample of host memory use and CPU load.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SystemSample {
    /// Host memory in use, in KiB. Zero indicates that the sample is unavailable.
    pub memory_used: u64,
    /// One-minute load average, if the host reports it.
    pub cpu_load: Option<f64>,
}

fn memory_value(info: &[u8], key: &[u8]) -> Option<u64> {
    let value = info
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(key))?;
    std::str::from_utf8(value)
        .ok()?
        .split_ascii_whitespace()
        .next()?
        .trim_end_matches('.')
        .parse()
        .ok()
}

fn meminfo(host: &dyn Host) -> Option<Vec<u8>> {
    read_bytes(host, Path::new("/proc/meminfo")).ok()
}

fn linux_memory(host: &dyn Host) -> Option<u64> {
    let info = meminfo(host)?;
    let total = memory_value(&info, b"MemTotal:")?;
    let available = memory_value(&info, b"MemAvailable:").or_else(|| {
        Some(
            memory_value(&info, b"MemFree:")?
                .saturating_add(memory_value(&info, b"Buffers:")?)
                .saturating_add(memory_value(&info, b"Cached:")?),
        )
    })?;
    Some(total.saturating_sub(available))
}

fn probe(host: &dyn Host, program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: program.into(),
            args: args.iter().copied().map(Into::into).collect(),
        }],
        cwd: host.initial_cwd(),
        env: host.env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Null,
        timeout: Some(Duration::from_secs(2)),
        tee: false,
    };
    let output = host.run(&request, &mut |_, _| {}).ok()?;
    matches!(output.results.as_slice(), [ProcessResult::Exited(0)]).then_some(output.stdout)
}

fn darwin_memory(host: &dyn Host) -> Option<u64> {
    let stats = probe(host, "/usr/bin/vm_stat", &[])?;
    let header = std::str::from_utf8(&stats).ok()?.lines().next()?;
    let page_kib = header
        .split_once("page size of ")?
        .1
        .split_ascii_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?
        / 1024;
    let active = memory_value(&stats, b"Pages active:")?;
    let wired = memory_value(&stats, b"Pages wired down:")?;
    Some(active.saturating_add(wired).saturating_mul(page_kib))
}

fn load_average(bytes: &[u8]) -> Option<f64> {
    let load = std::str::from_utf8(bytes)
        .ok()?
        .trim_start_matches(['{', ' '])
        .split_ascii_whitespace()
        .next()?
        .parse::<f64>()
        .ok()?;
    (load.is_finite() && load > 0.0).then_some(load)
}

impl SystemSample {
    /// Read the current host sample.
    #[must_use]
    pub fn collect(host: &dyn Host) -> Self {
        let (memory, load) = if host.system().sysname.as_slice() == b"Darwin" {
            (
                darwin_memory(host),
                probe(host, "/usr/sbin/sysctl", &["-n", "vm.loadavg"]),
            )
        } else {
            (
                linux_memory(host),
                read_bytes(host, Path::new("/proc/loadavg")).ok(),
            )
        };
        Self {
            memory_used: memory.unwrap_or_default(),
            cpu_load: load.as_deref().and_then(load_average),
        }
    }
}

pub(crate) fn dynamic_information(before: SystemSample, after: SystemSample) -> Value {
    let load = |value: Option<f64>| {
        value
            .filter(|n| n.is_finite())
            .map_or(Value::Null, Value::Real)
    };
    Value::Object(Object::from([
        (
            "beforeHostMemoryUsed".into(),
            Value::UInt(before.memory_used),
        ),
        ("afterHostMemoryUsed".into(), Value::UInt(after.memory_used)),
        ("beforeCPULoadAverage".into(), load(before.cpu_load)),
        ("afterCPULoadAverage".into(), load(after.cpu_load)),
    ]))
}
