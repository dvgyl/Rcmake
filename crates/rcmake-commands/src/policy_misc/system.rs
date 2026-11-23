use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::{BString, ByteSlice, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{CommandSpec, Input, Output, ProcessResult, ReadRange, RunRequest, SystemInfo};
use rcmake_model::{CacheEntry, CacheType};

use super::{quoted, status, values};

const MIB: u64 = 1024 * 1024;

pub(super) fn site_name(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [arg] = args else {
        return Err(status("called with incorrect number of arguments"));
    };
    if !cx.var_defined(&arg.value) {
        let hostname = cx.host().system().hostname;
        cx.cache().insert(
            arg.value.clone(),
            CacheEntry {
                value: hostname,
                ty: CacheType::String,
                help: "Name of the computer/site where compile is being run".into(),
                advanced: false,
                props: Default::default(),
            },
        );
    }
    Ok(Flow::Normal)
}

pub(super) fn variable_watch(_: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some(var) = args.first() else {
        return Err(status("must be called with at least one argument."));
    };
    if var.value == b"CMAKE_CURRENT_LIST_FILE" {
        return Err(status(
            "cannot be set on the variable: CMAKE_CURRENT_LIST_FILE",
        ));
    }
    // Variable access callbacks need evaluator support.
    Ok(Flow::Normal)
}

pub(super) fn query(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [b"RESULT", out, b"QUERY", keys @ ..] = args.as_slice() else {
        return Err(status("called with incorrect number of arguments"));
    };
    if let [b"WINDOWS_REGISTRY", _, options @ ..] = keys {
        if let Some([_, error_var]) = options
            .windows(2)
            .find(|pair| pair.first() == Some(&b"ERROR_VARIABLE".as_slice()))
        {
            cx.set_var(error_var, "No Registry on this platform.".into());
        }
        cx.set_var(out, BString::default());
        return Ok(Flow::Normal);
    }
    let (requested_root, keys) = query_options(keys)?;
    if keys.is_empty() {
        return Err(status("missing QUERY specification"));
    }
    let policy = PolicyId::new(221);
    let sysroot = requested_root.unwrap_or_else(|| cx.policy(policy) != PolicyStatus::New);
    if requested_root.is_none()
        && cx.policy(policy) == PolicyStatus::Warn
        && !cx.var_or_empty(b"CMAKE_SYSROOT").is_empty()
        && keys.iter().any(|key| key.starts_with(b"DISTRIB_"))
    {
        cx.issue_policy_warning(policy, b"", b"");
        if cx.fatal_occurred() {
            return Err(CmdError::Reported);
        }
    }
    let host = cx.host().system();
    let mut distro = None;
    let mut facts = HostFacts::default();
    let mut result = BString::default();
    for key in keys {
        let item = if let Some(name) = key.strip_prefix(b"DISTRIB_") {
            let fields = distro.get_or_insert_with(|| distribution(cx, sysroot));
            if name == b"INFO" {
                let mut variables = Vec::with_capacity(fields.len());
                for (key, value) in fields.iter() {
                    let var: BString = [*out, b"_", key.as_slice()].concat().into();
                    cx.set_var(&var, value.clone());
                    variables.push(var);
                }
                rcmake_base::list::join(variables)
            } else {
                fields.get(name).cloned().unwrap_or_default()
            }
        } else {
            facts
                .get(cx, &host, key)
                .ok_or_else(|| status(quoted("does not recognize <key> ", key, "")))?
        };
        if !result.is_empty() {
            result.push(b';');
        }
        result.extend_from_slice(&item);
    }
    cx.set_var(out, result);
    Ok(Flow::Normal)
}

fn query_options<'a>(args: &[&'a [u8]]) -> Result<(Option<bool>, Vec<&'a [u8]>), CmdError> {
    let mut keys = Vec::new();
    let mut root = None;
    let mut args = args.iter().copied();
    while let Some(key) = args.next() {
        if key != b"FROM_SYSROOT" {
            keys.push(key);
            continue;
        }
        if root.is_some() {
            return Err(status("FROM_SYSROOT may be given at most once."));
        }
        let flag = args
            .next()
            .ok_or_else(|| status("FROM_SYSROOT requires a boolean value."))?;
        root = Some(if truth::is_on(flag) {
            true
        } else if truth::is_off(flag) {
            false
        } else {
            return Err(status(quoted(
                "FROM_SYSROOT requires a boolean value, got \"",
                flag,
                "\".",
            )));
        });
    }
    Ok((root, keys))
}

#[derive(Default)]
struct HostFacts {
    cpu: Option<Vec<BTreeMap<BString, BString>>>,
    memory: Option<BTreeMap<BString, BString>>,
}

impl HostFacts {
    fn get(&mut self, cx: &Cx<'_>, host: &SystemInfo, key: &[u8]) -> Option<BString> {
        let darwin = host.sysname == b"Darwin";
        let machine64 = matches!(
            host.machine.as_slice(),
            b"aarch64" | b"arm64" | b"amd64" | b"AMD64" | b"s390x" | b"ia64"
        ) || host.machine.windows(2).any(|bytes| bytes == b"64");
        let text = match key {
            b"NUMBER_OF_LOGICAL_CORES" => host.cpu_count.to_string().into(),
            b"HOSTNAME" | b"FQDN" => host.hostname.clone(),
            b"OS_NAME" => {
                if darwin {
                    run(cx, b"/usr/bin/sw_vers", &[b"-productName"])
                        .unwrap_or_else(|| host.sysname.clone())
                } else {
                    host.sysname.clone()
                }
            }
            b"OS_RELEASE" => {
                if darwin {
                    run(cx, b"/usr/bin/sw_vers", &[b"-productVersion"])
                        .unwrap_or_else(|| host.release.clone())
                } else {
                    host.release.clone()
                }
            }
            b"OS_VERSION" => {
                if darwin {
                    run(cx, b"/usr/bin/sw_vers", &[b"-buildVersion"])
                        .unwrap_or_else(|| host.version.clone())
                } else {
                    host.version.clone()
                }
            }
            b"OS_PLATFORM" => host.machine.clone(),
            b"IS_64BIT" => boolean(machine64),
            b"LOCALE_CHARSET" if host.sysname == b"Windows" => "UTF-8".into(),
            b"LOCALE_CHARSET" => locale_charset(cx),
            b"MSYSTEM_PREFIX" if host.sysname == b"Windows" => {
                cx.host().env_get(b"MSYSTEM_PREFIX").unwrap_or_default()
            }
            b"TOTAL_PHYSICAL_MEMORY"
            | b"AVAILABLE_PHYSICAL_MEMORY"
            | b"TOTAL_VIRTUAL_MEMORY"
            | b"AVAILABLE_VIRTUAL_MEMORY" => {
                if darwin {
                    return Some(darwin_memory(cx, key));
                }
                let memory = self.memory.get_or_insert_with(|| {
                    fields(read(cx, Path::new("/proc/meminfo")).lines(), b':')
                });
                let field: &[u8] = match key {
                    b"TOTAL_PHYSICAL_MEMORY" => b"MemTotal",
                    b"AVAILABLE_PHYSICAL_MEMORY" => b"MemAvailable",
                    b"TOTAL_VIRTUAL_MEMORY" => b"SwapTotal",
                    _ => b"SwapFree",
                };
                let kib = memory
                    .get(field)
                    .and_then(|value| number(value))
                    .unwrap_or(0);
                (kib / 1024).to_string().into()
            }
            _ => return self.cpu_value(cx, host, key),
        };
        Some(text)
    }

    fn cpu_value(&mut self, cx: &Cx<'_>, host: &SystemInfo, key: &[u8]) -> Option<BString> {
        let darwin = host.sysname == b"Darwin";
        let apple = |key: &[u8]| {
            darwin
                .then(|| run(cx, b"/usr/sbin/sysctl", &[b"-n", key]))
                .flatten()
        };
        let brand = || apple(b"machdep.cpu.brand_string");
        let cpu = self.cpu.get_or_insert_with(|| {
            if host.sysname != b"Linux" {
                return Vec::new();
            }
            read(cx, Path::new("/proc/cpuinfo"))
                .split(|&byte| byte == b'\n')
                .collect::<Vec<_>>()
                .split(|line| line.is_empty())
                .filter(|lines| !lines.is_empty())
                .map(|lines| fields(lines.iter().copied(), b':'))
                .collect()
        });
        let cpu_field = |name: &[u8]| cpu.first().and_then(|cpu| cpu.get(name)).cloned();
        let flags = cpu_field(b"flags")
            .or_else(|| cpu_field(b"Features"))
            .unwrap_or_default();
        let feature = |flag: &[u8]| {
            flags
                .split(|byte| byte.is_ascii_whitespace())
                .any(|word| word == flag)
        };
        let physical = || {
            let ids: BTreeSet<_> = cpu
                .iter()
                .filter_map(|info| {
                    Some((
                        info.get(b"physical id".as_slice())?,
                        info.get(b"core id".as_slice())?,
                    ))
                })
                .collect();
            apple(b"hw.physicalcpu")
                .or_else(|| (!ids.is_empty()).then(|| ids.len().to_string().into()))
                .unwrap_or_else(|| host.cpu_count.to_string().into())
        };
        let value = match key {
            b"NUMBER_OF_PHYSICAL_CORES" => physical(),
            b"PROCESSOR_NAME" | b"MODEL_NAME" => brand()
                .or_else(|| cpu_field(b"model name"))
                .or_else(|| cpu_field(b"Processor"))
                .unwrap_or_else(|| host.machine.clone()),
            b"PROCESSOR_DESCRIPTION" => {
                let name = brand()
                    .or_else(|| cpu_field(b"model name"))
                    .unwrap_or_else(|| host.machine.clone());
                quoted(&format!("{} core ", physical()), &name, "")
            }
            b"PROCESSOR_SERIAL_NUMBER" => cpu_field(b"Serial").unwrap_or_default(),
            b"FAMILY_ID" => apple(b"machdep.cpu.family")
                .or_else(|| cpu_field(b"cpu family"))
                .unwrap_or_default(),
            b"MODEL_ID" => apple(b"machdep.cpu.model")
                .or_else(|| cpu_field(b"model"))
                .unwrap_or_default(),
            b"PROCESSOR_APIC_ID" => cpu_field(b"apicid").unwrap_or_default(),
            b"PROCESSOR_CACHE_SIZE" => apple(b"hw.l1dcachesize")
                .and_then(|value| number(&value))
                .map(|bytes| (bytes / 1024).to_string().into())
                .or_else(|| {
                    cpu_field(b"cache size")
                        .and_then(|text| number(&text))
                        .map(|num| num.to_string().into())
                })
                .unwrap_or_default(),
            b"PROCESSOR_CLOCK_FREQUENCY" => cpu_field(b"cpu MHz").unwrap_or_default(),
            b"VENDOR_ID" | b"VENDOR_STRING" => apple(b"machdep.cpu.vendor")
                .or_else(|| cpu_field(b"vendor_id"))
                .unwrap_or_default(),
            b"HAS_FPU" => boolean(feature(b"fpu") || feature(b"fp") || darwin),
            b"HAS_MMX" => boolean(feature(b"mmx")),
            b"HAS_MMX_PLUS" => boolean(feature(b"mmxext")),
            b"HAS_SSE" | b"HAS_SSE_FP" => boolean(feature(b"sse")),
            b"HAS_SSE2" => boolean(feature(b"sse2")),
            b"HAS_SSE_MMX" => boolean(feature(b"mmxext") || feature(b"sse")),
            b"HAS_AMD_3DNOW" => boolean(feature(b"3dnow")),
            b"HAS_AMD_3DNOW_PLUS" => boolean(feature(b"3dnowext")),
            b"HAS_IA64" => boolean(host.machine == b"ia64"),
            b"HAS_SERIAL_NUMBER" => boolean(cpu_field(b"Serial").is_some()),
            b"HAS_APIC" => boolean(feature(b"apic")),
            b"HAS_L1_CACHE" => boolean(
                apple(b"hw.l1dcachesize").is_some_and(|size| number(&size).is_some_and(|n| n > 0))
                    || cpu_field(b"cache size").is_some(),
            ),
            _ => return None,
        };
        Some(value)
    }
}

fn darwin_memory(cx: &Cx<'_>, key: &[u8]) -> BString {
    let stat = |key| run(cx, b"/usr/sbin/sysctl", &[b"-n", key]);
    let mib = match key {
        b"TOTAL_PHYSICAL_MEMORY" => stat(b"hw.memsize")
            .and_then(|value| number(&value))
            .map(|bytes| bytes / MIB),
        b"AVAILABLE_PHYSICAL_MEMORY" => {
            let page_size = stat(b"hw.pagesize").and_then(|value| number(&value));
            let vm = run(cx, b"/usr/bin/vm_stat", &[]).unwrap_or_default();
            let pages = fields(vm.lines(), b':');
            let free = [
                b"Pages free".as_slice(),
                b"Pages inactive",
                b"Pages speculative",
            ]
            .into_iter()
            .filter_map(|key| pages.get(key).and_then(|value| number(value)))
            .fold(0_u64, u64::saturating_add);
            page_size.map(|size| free.saturating_mul(size) / MIB)
        }
        _ => {
            let label = if key == b"TOTAL_VIRTUAL_MEMORY" {
                b"total".as_slice()
            } else {
                b"free"
            };
            let swap = stat(b"vm.swapusage").unwrap_or_default();
            let mut tokens = swap.fields_with(char::is_whitespace);
            tokens.find(|token| *token == label);
            tokens.nth(1).and_then(number)
        }
    };
    mib.unwrap_or(0).to_string().into()
}

fn locale_charset(cx: &Cx<'_>) -> BString {
    for var in [b"LC_ALL".as_slice(), b"LC_CTYPE", b"LANG"] {
        if let Some(value) = cx.host().env_get(var).filter(|value| !value.is_empty()) {
            return if let Some(dot) = value.find_byte(b'.') {
                value
                    .get(dot + 1..)
                    .unwrap_or_default()
                    .split(|byte| *byte == b'@')
                    .next()
                    .unwrap_or_default()
                    .into()
            } else if matches!(value.as_slice(), b"C" | b"POSIX") {
                "US-ASCII".into()
            } else {
                BString::default()
            };
        }
    }
    "US-ASCII".into()
}

fn distribution(cx: &Cx<'_>, sysroot: bool) -> BTreeMap<BString, BString> {
    let root = cx.var_or_empty(b"CMAKE_SYSROOT");
    let root = if sysroot && !root.is_empty() {
        cx.resolve_path(&root)
    } else {
        PathBuf::from("/")
    };
    for name in ["etc/os-release", "usr/lib/os-release"] {
        let input = read(cx, &root.join(name));
        if !input.is_empty() {
            return fields(
                input
                    .lines()
                    .filter(|line| !line.trim_ascii_start().starts_with(b"#")),
                b'=',
            )
            .into_iter()
            .map(|(key, value)| (key, unquote(&value)))
            .collect();
        }
    }
    BTreeMap::new()
}

fn unquote(value: &[u8]) -> BString {
    if let Some(inner) = value
        .strip_prefix(b"'")
        .and_then(|value| value.strip_suffix(b"'"))
    {
        return inner.into();
    }
    let Some(inner) = value
        .strip_prefix(b"\"")
        .and_then(|value| value.strip_suffix(b"\""))
    else {
        return value.into();
    };
    let mut bytes = inner.iter().copied().peekable();
    let mut result = BString::default();
    while let Some(byte) = bytes.next() {
        if byte == b'\\'
            && let Some(escaped) = bytes.next_if(|byte| matches!(byte, b'\\' | b'"' | b'$' | b'`'))
        {
            result.push(escaped);
        } else {
            result.push(byte);
        }
    }
    result
}

fn read(cx: &Cx<'_>, path: &Path) -> Vec<u8> {
    cx.host()
        .read(
            path,
            ReadRange::All {
                limit: cx.limits().max_file_read.min(MIB),
            },
        )
        .unwrap_or_default()
}

fn fields<'a>(lines: impl IntoIterator<Item = &'a [u8]>, sep: u8) -> BTreeMap<BString, BString> {
    lines
        .into_iter()
        .filter_map(|line| {
            let mut fields = line.splitn(2, |byte| *byte == sep);
            let key = fields.next()?;
            let value = fields.next()?;
            Some((key.trim_ascii().into(), value.trim_ascii().into()))
        })
        .collect()
}

fn number(text: &[u8]) -> Option<u64> {
    let digits = text.iter().take_while(|byte| byte.is_ascii_digit()).count();
    core::str::from_utf8(text.get(..digits)?).ok()?.parse().ok()
}

fn boolean(value: bool) -> BString {
    u8::from(value).to_string().into()
}

fn run(cx: &Cx<'_>, program: &[u8], args: &[&[u8]]) -> Option<BString> {
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: program.into(),
            args: args.iter().copied().map(BString::from).collect(),
        }],
        cwd: cx.current_source_dir(),
        env: Vec::new(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Null,
        timeout: Some(Duration::from_secs(2)),
        tee: false,
    };
    let result = cx.host().run(&request, &mut |_, _| {}).ok()?;
    matches!(result.results.as_slice(), [ProcessResult::Exited(0)])
        .then(|| result.stdout.trim_ascii().into())
}
