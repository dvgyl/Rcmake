use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::{BString, ByteSlice, truth};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::args::{KeywordParser, Slot};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, IncludeOptions};
use rcmake_expr::ExpandedArg;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Input, Output, ProcessResult, ReadRange, RegistryRoot,
    RegistryView, RunRequest, SystemInfo, path,
};
use rcmake_model::{CacheEntry, CacheType};

use super::{quoted, values};

const MIB: u64 = 1024 * 1024;

pub(super) fn site_name(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [arg] = args else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    if !cx.var_defined(&arg.value) {
        let hostname = cx.host().system().hostname;
        cx.insert_cache_entry_unrecorded(
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

pub(super) fn variable_watch(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some(var) = args.first() else {
        return Err(CmdError::status(
            "must be called with at least one argument.",
        ));
    };
    if var.value == b"CMAKE_CURRENT_LIST_FILE" {
        return Err(CmdError::status(
            "cannot be set on the variable: CMAKE_CURRENT_LIST_FILE",
        ));
    }
    cx.add_variable_watch(&var.value, args.get(1).map(|a| a.value.as_slice()));
    Ok(Flow::Normal)
}

pub(super) fn query(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [result, out, args @ ..] = args else {
        return Err(CmdError::status("missing RESULT specification."));
    };
    if result.value != b"RESULT" {
        return Err(CmdError::status("missing RESULT specification."));
    }
    let [query, keys @ ..] = args else {
        return Err(CmdError::status("missing QUERY specification"));
    };
    if query.value != b"QUERY" {
        return Err(CmdError::status("missing QUERY specification"));
    }
    let out = out.value.as_slice();
    if let Some((query, args)) = keys.split_first()
        && query.value == b"WINDOWS_REGISTRY"
    {
        return registry(cx, out, args);
    }
    let keys = values(keys);
    let (requested_root, keys) = query_options(&keys)?;
    if keys.is_empty() {
        return Err(CmdError::status("missing QUERY specification"));
    }
    let policy = PolicyId::new(221);
    let sysroot = requested_root.unwrap_or_else(|| cx.policy(policy) != PolicyStatus::New);
    if requested_root.is_none()
        && cx.policy(policy) == PolicyStatus::Warn
        && !cx.var_or_empty(b"CMAKE_SYSROOT").is_empty()
        && keys.iter().any(|key| key.starts_with(b"DISTRIB_"))
    {
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Policy),
            "Policy CMP0221 is not set: cmake_host_system_information() DISTRIB_* queries read the host os-release.  Run \"cmake --help-policy CMP0221\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.".into(),
        );
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
            if distro.is_none() {
                distro = Some(distribution(cx, sysroot)?);
            }
            let Some(fields) = &distro else { continue };
            if name == b"INFO" {
                let mut variables = Vec::with_capacity(fields.len());
                for (key, value) in fields.iter() {
                    let var: BString = [out, b"_", key.as_slice()].concat().into();
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
                .ok_or_else(|| CmdError::status(quoted("does not recognize <key> ", key, "")))?
        };
        rcmake_base::list::push(&mut result, &item);
    }
    cx.set_var(out, result);
    Ok(Flow::Normal)
}

fn registry(cx: &mut Cx<'_>, out: &[u8], args: &[ExpandedArg]) -> CmdResult {
    let [key, options @ ..] = args else {
        return Err(CmdError::status("missing <key> specification."));
    };
    let parsed = KeywordParser::new()
        .keyword("VALUE", Slot::Single)
        .keyword("VIEW", Slot::Single)
        .keyword("SEPARATOR", Slot::Single)
        .keyword("ERROR_VARIABLE", Slot::Single)
        .keyword("VALUE_NAMES", Slot::Flag)
        .keyword("SUBKEYS", Slot::Flag)
        .parse(options);
    if !parsed.unparsed().is_empty() {
        return Err(CmdError::status(quoted(
            "given invalid argument(s) \"",
            &parsed.unparsed().join(b";".as_slice()),
            "\".",
        )));
    }
    if let Some(error) = parsed.report_error_text() {
        return Err(crate::command_error(cx, error));
    }
    if let Some(view) = parsed.single("VIEW")
        && !matches!(
            view.as_slice(),
            b"32" | b"64" | b"32_64" | b"64_32" | b"HOST" | b"TARGET" | b"BOTH"
        )
    {
        return Err(CmdError::status(quoted(
            "given invalid value for VIEW: ",
            view,
            ".",
        )));
    }
    let error_var = parsed.single("ERROR_VARIABLE").map(|v| v.as_slice());
    let Some(host_view) = cx.host().registry_host_view() else {
        if let Some(error) = error_var {
            cx.set_var(error, "No Registry on this platform.".into());
        }
        cx.set_var(out, BString::default());
        return Ok(Flow::Normal);
    };
    let (root, subkey) = match parse_registry_key(&key.value) {
        Ok(parsed_key) => parsed_key,
        Err(message) => {
            if let Some(error) = error_var {
                cx.set_var(error, message);
            }
            cx.set_var(out, BString::default());
            return Ok(Flow::Normal);
        }
    };
    let sizeof_void_p = match cx.var_or_empty(b"CMAKE_SIZEOF_VOID_P").as_slice() {
        b"8" => Some(RegistryView::Bits64),
        b"4" => Some(RegistryView::Bits32),
        _ => None,
    };
    let views = registry_views(
        parsed.single("VIEW").map(|v| v.as_slice()),
        host_view,
        sizeof_void_p,
    );
    let mode_count = usize::from(parsed.seen("VALUE"))
        + usize::from(parsed.flag("VALUE_NAMES"))
        + usize::from(parsed.flag("SUBKEYS"));
    if mode_count > 1 {
        return Err(CmdError::status(
            "VALUE, VALUE_NAMES, and SUBKEYS are mutually exclusive.",
        ));
    }
    if parsed.flag("VALUE_NAMES") || parsed.flag("SUBKEYS") {
        let value_names = parsed.flag("VALUE_NAMES");
        let mut items = BTreeSet::new();
        let mut query_successful = false;
        let mut last_error = BString::default();
        for view in views {
            if view == RegistryView::Bits64 && host_view == RegistryView::Bits32 {
                last_error = "No 64bit registry on Windows32.".into();
                continue;
            }
            let result = if value_names {
                cx.host().registry_value_names(root, subkey, view)
            } else {
                cx.host().registry_subkeys(root, subkey, view)
            };
            match result {
                Ok(Some(entries)) => {
                    items.extend(entries);
                    query_successful = true;
                }
                Ok(None) => {}
                Err(err) => {
                    last_error = err.to_string().into();
                }
            }
        }
        if query_successful {
            if let Some(error) = error_var {
                cx.set_var(error, BString::default());
            }
            cx.set_var(out, rcmake_base::list::join(items));
        } else {
            if let Some(error) = error_var {
                cx.set_var(error, last_error);
            }
            cx.set_var(out, BString::default());
        }
        return Ok(Flow::Normal);
    }
    let name = parsed
        .single("VALUE")
        .map(|v| v.as_slice())
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case(b"(default)"));
    let separator = parsed.single("SEPARATOR").map(|v| v.as_slice());
    let mut found: Option<BString> = None;
    let mut last_error = BString::default();
    for view in views {
        if view == RegistryView::Bits64 && host_view == RegistryView::Bits32 {
            last_error = "No 64bit registry on Windows32.".into();
            continue;
        }
        match cx
            .host()
            .registry_value_with_separator(root, subkey, name, view, separator)
        {
            Ok(Some(value)) => {
                found = Some(value);
                last_error.clear();
                break;
            }
            Ok(None) => {}
            Err(err) => {
                last_error = err.to_string().into();
            }
        }
    }
    if let Some(value) = found {
        if let Some(error) = error_var {
            cx.set_var(error, BString::default());
        }
        cx.set_var(out, value);
    } else {
        if let Some(error) = error_var {
            cx.set_var(error, last_error);
        }
        cx.set_var(out, BString::default());
    }
    Ok(Flow::Normal)
}

fn parse_registry_key(key: &[u8]) -> Result<(RegistryRoot, &[u8]), BString> {
    let (root_bytes, subkey) = match key.iter().position(|&b| b == b'/' || b == b'\\') {
        Some(pos) => (
            key.get(..pos).unwrap_or_default(),
            key.get(pos + 1..).unwrap_or_default(),
        ),
        None => (key, b"".as_slice()),
    };
    let root = match root_bytes {
        b"HKLM" | b"HKEY_LOCAL_MACHINE" => RegistryRoot::LocalMachine,
        b"HKCU" | b"HKEY_CURRENT_USER" => RegistryRoot::CurrentUser,
        b"HKCR" | b"HKEY_CLASSES_ROOT" => RegistryRoot::ClassesRoot,
        b"HKCC" | b"HKEY_CURRENT_CONFIG" => RegistryRoot::CurrentConfig,
        b"HKU" | b"HKEY_USERS" => RegistryRoot::Users,
        _ => return Err([root_bytes, b": invalid root key."].concat().into()),
    };
    Ok((root, subkey))
}

fn registry_views(
    view: Option<&[u8]>,
    host_view: RegistryView,
    sizeof_void_p: Option<RegistryView>,
) -> Vec<RegistryView> {
    let is_64_host = host_view == RegistryView::Bits64;
    let target_view = sizeof_void_p.unwrap_or(host_view);
    let both = || match sizeof_void_p {
        Some(RegistryView::Bits64) => vec![RegistryView::Bits64, RegistryView::Bits32],
        Some(RegistryView::Bits32) => {
            if is_64_host {
                vec![RegistryView::Bits32, RegistryView::Bits64]
            } else {
                vec![RegistryView::Bits32]
            }
        }
        None => {
            if is_64_host {
                vec![RegistryView::Bits64, RegistryView::Bits32]
            } else {
                vec![RegistryView::Bits32]
            }
        }
    };
    match view {
        Some(b"32") => vec![RegistryView::Bits32],
        Some(b"64") => vec![RegistryView::Bits64],
        Some(b"32_64") => {
            if is_64_host {
                vec![RegistryView::Bits32, RegistryView::Bits64]
            } else {
                vec![RegistryView::Bits32]
            }
        }
        Some(b"64_32") => {
            if is_64_host {
                vec![RegistryView::Bits64, RegistryView::Bits32]
            } else {
                vec![RegistryView::Bits32]
            }
        }
        Some(b"HOST") => vec![host_view],
        Some(b"TARGET") => vec![target_view],
        Some(b"BOTH") | None => both(),
        _ => both(),
    }
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
            return Err(CmdError::status("FROM_SYSROOT may be given at most once."));
        }
        let flag = args
            .next()
            .ok_or_else(|| CmdError::status("FROM_SYSROOT requires a boolean value."))?;
        root = Some(if truth::is_on(flag) {
            true
        } else if truth::is_off(flag) {
            false
        } else {
            return Err(CmdError::status(quoted(
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
            b"VS_15_DIR" => vs_install_dir(cx, host, 15),
            b"VS_16_DIR" => vs_install_dir(cx, host, 16),
            b"VS_17_DIR" => vs_install_dir(cx, host, 17),
            b"VS_18_DIR" => vs_install_dir(cx, host, 18),
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
            b"PROCESSOR_CLOCK_FREQUENCY" => {
                let frequency =
                    |value: BString| std::str::from_utf8(&value).ok()?.parse::<f64>().ok();
                let mhz = apple(b"hw.cpufrequency")
                    .and_then(frequency)
                    .map(|hz| hz / 1_000_000.0)
                    .or_else(|| cpu_field(b"cpu MHz").and_then(frequency))
                    .unwrap_or_default();
                format!("{mhz:.6}").into()
            }
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

fn distribution(cx: &mut Cx<'_>, sysroot: bool) -> Result<BTreeMap<BString, BString>, CmdError> {
    let root = cx.var_or_empty(b"CMAKE_SYSROOT");
    let root = if sysroot && !root.is_empty() {
        cx.resolve_path(&root)
    } else {
        PathBuf::from("/")
    };
    for name in ["etc/os-release", "usr/lib/os-release"] {
        let input = read(cx, &root.join(name));
        if !input.is_empty() {
            return Ok(fields(input.lines(), b'=')
                .into_iter()
                .filter_map(|(key, value)| {
                    if key.is_empty()
                        || !key
                            .iter()
                            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                    {
                        return None;
                    }
                    release_value(&value).map(|value| (key, value))
                })
                .collect());
        }
    }
    distribution_fallback(cx, &root)
}

fn release_value(value: &[u8]) -> Option<BString> {
    let Some(&quote @ (b'"' | b'\'')) = value.first() else {
        let unquoted = value.split(|&byte| byte == b'#').next()?.trim_ascii_end();
        return (!unquoted
            .iter()
            .any(|byte| matches!(byte, b'\\' | b'\'' | b'"')))
        .then(|| unquoted.into());
    };
    let mut bytes = value.get(1..)?.iter().copied().peekable();
    let mut result = BString::default();
    while let Some(byte) = bytes.next() {
        if byte == quote {
            let tail = bytes.collect::<Vec<_>>();
            let tail = tail.trim_ascii_start();
            return (tail.is_empty() || tail.starts_with(b"#")).then_some(result);
        }
        if byte == b'\\'
            && let Some(escaped) =
                bytes.next_if(|byte| *byte == quote || matches!(byte, b'\\' | b'$' | b'`'))
        {
            result.push(escaped);
        } else {
            result.push(byte);
        }
    }
    None
}

fn distribution_fallback(
    cx: &mut Cx<'_>,
    root: &Path,
) -> Result<BTreeMap<BString, BString>, CmdError> {
    const PREFIX: &[u8] = b"CMAKE_GET_OS_RELEASE_FALLBACK_RESULT_";
    let builtins =
        path::to_native(&cx.var_or_empty(b"CMAKE_ROOT")).join("Modules/Internal/OSRelease");
    let mut scripts: Vec<_> = cx
        .host()
        .read_dir(&builtins)
        .unwrap_or_default()
        .iter()
        .filter(|entry| entry.name.ends_with(b".cmake"))
        .map(|entry| builtins.join(path::to_native(&entry.name)))
        .collect();
    scripts.extend(
        rcmake_base::list::split(
            &cx.var_or_empty(b"CMAKE_GET_OS_RELEASE_FALLBACK_SCRIPTS"),
            rcmake_base::list::Empty::Drop,
        )
        .iter()
        .map(|script| cx.resolve_path(script)),
    );
    scripts.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for script in scripts {
        let mut fields = cx.with_variable_scope(|cx| {
            cx.set_var(b"CMAKE_SYSROOT", path::from_native(root));
            let result = b"CMAKE_GET_OS_RELEASE_FALLBACK_RESULT";
            cx.unset_var(result);
            cx.include_file(
                &script,
                IncludeOptions {
                    optional: true,
                    ..IncludeOptions::default()
                },
            )?;
            let names =
                rcmake_base::list::split(&cx.var_or_empty(result), rcmake_base::list::Empty::Drop);
            Ok::<BTreeMap<_, _>, CmdError>(
                names
                    .into_iter()
                    .filter_map(|name| {
                        let field = name.strip_prefix(PREFIX)?;
                        Some((BString::from(field), cx.var_or_empty(&name)))
                    })
                    .collect(),
            )
        })?;
        if cx.fatal_occurred() {
            return Err(CmdError::Reported);
        }
        if !fields.is_empty() {
            fields.insert(
                "USED_FALLBACK_SCRIPT".into(),
                path::from_native_cmake(&script),
            );
            return Ok(fields);
        }
    }
    Ok(BTreeMap::new())
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
        default_error_mode: false,
    };
    let result = cx.host().run(&request, &mut |_, _| {}).ok()?;
    matches!(result.results.as_slice(), [ProcessResult::Exited(0)])
        .then(|| result.stdout.trim_ascii().into())
}

fn vs_install_dir(cx: &Cx<'_>, host_info: &SystemInfo, major: u8) -> BString {
    if host_info.sysname != b"Windows" && !cfg!(windows) {
        return BString::default();
    }
    let host = cx.host();
    let expected_prefix = format!("{major}.");
    if host
        .env_get(b"VisualStudioVersion")
        .is_some_and(|version| version.starts_with(expected_prefix.as_bytes()))
        && let Some(dir) = host.env_get(b"VSINSTALLDIR").filter(|dir| !dir.is_empty())
        && host
            .metadata(&path::to_native(&dir), Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        return normalize_vs_dir(&dir);
    }
    let locator = [b"ProgramFiles(x86)".as_slice(), b"ProgramFiles"]
        .into_iter()
        .filter_map(|var| host.env_get(var).filter(|dir| !dir.is_empty()))
        .map(|base| path::to_native(&base).join("Microsoft Visual Studio/Installer/vswhere.exe"))
        .find(|candidate| {
            host.metadata(candidate, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
        });
    let Some(locator) = locator else {
        return BString::default();
    };
    let range = format!("[{major}.0,{}.0)", major + 1);
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: path::from_native(&locator),
            args: vec![
                "-latest".into(),
                "-products".into(),
                "*".into(),
                "-version".into(),
                range.into(),
                "-property".into(),
                "installationPath".into(),
                "-utf8".into(),
            ],
        }],
        cwd: host.initial_cwd(),
        env: host.env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Null,
        timeout: Some(Duration::from_secs(5)),
        tee: false,
        default_error_mode: false,
    };
    let Ok(output) = host.run_bounded(&request, 64 * 1024) else {
        return BString::default();
    };
    if output.results != [ProcessResult::Exited(0)] {
        return BString::default();
    }
    output
        .stdout
        .split(|&byte| byte == b'\n')
        .find_map(|line| {
            let trimmed = line.trim_ascii();
            let dir = trimmed.strip_prefix(b"\xef\xbb\xbf").unwrap_or(trimmed);
            (!dir.is_empty()).then(|| normalize_vs_dir(dir))
        })
        .unwrap_or_default()
}

fn normalize_vs_dir(dir: &[u8]) -> BString {
    let mut normalized = rcmake_base::path_lex::cmake_style_windows(dir);
    while normalized.len() > 3 && normalized.ends_with(b"/") {
        normalized.pop();
    }
    normalized
}

#[cfg(test)]
mod unit_tests {
    use rcmake_base::BString;
    use rcmake_host::{RegistryRoot, RegistryView};

    use super::{normalize_vs_dir, parse_registry_key, registry_views};

    #[test]
    fn parses_windows_registry_root_keys_at_separator_boundary() {
        assert_eq!(
            parse_registry_key(b"HKCU/Software/CMake"),
            Ok((RegistryRoot::CurrentUser, b"Software/CMake".as_slice()))
        );
        assert_eq!(
            parse_registry_key(br"HKEY_LOCAL_MACHINE\SOFTWARE\Kitware"),
            Ok((RegistryRoot::LocalMachine, br"SOFTWARE\Kitware".as_slice()))
        );
        assert_eq!(
            parse_registry_key(b"WRONG_ROOT/SUBKEY"),
            Err(BString::from("WRONG_ROOT: invalid root key."))
        );
        assert_eq!(
            parse_registry_key(b"HKLM-SUBKEY"),
            Err(BString::from("HKLM-SUBKEY: invalid root key."))
        );
    }

    #[test]
    fn resolves_registry_views_and_vs_dir_paths() {
        assert_eq!(
            registry_views(Some(b"64_32"), RegistryView::Bits64, None),
            vec![RegistryView::Bits64, RegistryView::Bits32]
        );
        assert_eq!(
            registry_views(Some(b"32_64"), RegistryView::Bits64, None),
            vec![RegistryView::Bits32, RegistryView::Bits64]
        );
        // Omitted VIEW defaults to BOTH; CMAKE_SIZEOF_VOID_P selects order.
        assert_eq!(
            registry_views(None, RegistryView::Bits64, None),
            vec![RegistryView::Bits64, RegistryView::Bits32]
        );
        assert_eq!(
            registry_views(None, RegistryView::Bits32, None),
            vec![RegistryView::Bits32]
        );
        assert_eq!(
            registry_views(
                Some(b"BOTH"),
                RegistryView::Bits64,
                Some(RegistryView::Bits32)
            ),
            vec![RegistryView::Bits32, RegistryView::Bits64]
        );
        assert_eq!(
            registry_views(
                Some(b"TARGET"),
                RegistryView::Bits64,
                Some(RegistryView::Bits32)
            ),
            vec![RegistryView::Bits32]
        );
        assert_eq!(
            normalize_vs_dir(br"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\"),
            "C:/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools"
        );
    }

    #[test]
    fn queries_windows_registry_multi_sz_value_names_and_subkeys_across_views()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::path::Path;
        use std::sync::Arc;

        use rcmake_eval::{Interp, InterpConfig, RunMode};
        use rcmake_host::PureHost;

        let host = Arc::new(PureHost::default());
        host.set_registry_host_view(RegistryView::Bits64)?;
        let key = b"Software/Classes/CLSID/CMake-Tests/chsi-registry";
        host.seed_registry_value(
            RegistryRoot::CurrentUser,
            key,
            None,
            RegistryView::Bits64,
            b"default 64bit",
        )?;
        host.seed_registry_value(
            RegistryRoot::CurrentUser,
            key,
            Some(b"BYTE_SIZE"),
            RegistryView::Bits64,
            b"64bit",
        )?;
        host.seed_registry_value(
            RegistryRoot::CurrentUser,
            key,
            Some(b"VALUE_SZ"),
            RegistryView::Bits64,
            b"data with space",
        )?;
        host.seed_registry_multi_sz(
            RegistryRoot::CurrentUser,
            key,
            Some(b"VALUE_MULTI_SZ"),
            RegistryView::Bits64,
            b"data1\0data2\0",
        )?;
        host.seed_registry_multi_sz(
            RegistryRoot::CurrentUser,
            key,
            Some(b"VALUE2_MULTI_SZ"),
            RegistryView::Bits64,
            b"data1|data2\0",
        )?;
        host.seed_registry_key(
            RegistryRoot::CurrentUser,
            b"Software/Classes/CLSID/CMake-Tests/chsi-registry/subkey1",
            RegistryView::Bits64,
        )?;
        host.seed_registry_key(
            RegistryRoot::CurrentUser,
            b"Software/Classes/CLSID/CMake-Tests/chsi-registry/subkey2",
            RegistryView::Bits64,
        )?;

        host.seed_registry_value(
            RegistryRoot::CurrentUser,
            key,
            None,
            RegistryView::Bits32,
            b"default 32bit",
        )?;
        host.seed_registry_value(
            RegistryRoot::CurrentUser,
            key,
            Some(b"BYTE_SIZE"),
            RegistryView::Bits32,
            b"32bit",
        )?;
        host.seed_registry_value(
            RegistryRoot::CurrentUser,
            key,
            Some(b"VALUE2_SZ"),
            RegistryView::Bits32,
            b"data2 with space",
        )?;
        host.seed_registry_key(
            RegistryRoot::CurrentUser,
            b"Software/Classes/CLSID/CMake-Tests/chsi-registry/subkey1",
            RegistryView::Bits32,
        )?;
        host.seed_registry_key(
            RegistryRoot::CurrentUser,
            b"Software/Classes/CLSID/CMake-Tests/chsi-registry/subkey3",
            RegistryView::Bits32,
        )?;

        let script = Path::new("/work/reg.cmake");
        host.seed_file(
            script,
            br#"
set(KEY "HKCU/Software/Classes/CLSID/CMake-Tests/chsi-registry")
cmake_host_system_information(RESULT multi1 QUERY WINDOWS_REGISTRY "${KEY}" VALUE VALUE_MULTI_SZ ERROR_VARIABLE e1)
cmake_host_system_information(RESULT multi2 QUERY WINDOWS_REGISTRY "${KEY}" VALUE VALUE2_MULTI_SZ SEPARATOR "|" ERROR_VARIABLE e2)
cmake_host_system_information(RESULT names_both QUERY WINDOWS_REGISTRY "${KEY}" VALUE_NAMES ERROR_VARIABLE e3)
cmake_host_system_information(RESULT names_64 QUERY WINDOWS_REGISTRY "${KEY}" VALUE_NAMES VIEW 64 ERROR_VARIABLE e4)
cmake_host_system_information(RESULT names_32 QUERY WINDOWS_REGISTRY "${KEY}" VALUE_NAMES VIEW 32 ERROR_VARIABLE e5)
cmake_host_system_information(RESULT subkeys_both QUERY WINDOWS_REGISTRY "${KEY}" SUBKEYS ERROR_VARIABLE e6)
cmake_host_system_information(RESULT subkeys_64 QUERY WINDOWS_REGISTRY "${KEY}" SUBKEYS VIEW 64 ERROR_VARIABLE e7)
cmake_host_system_information(RESULT subkeys_32 QUERY WINDOWS_REGISTRY "${KEY}" SUBKEYS VIEW 32 ERROR_VARIABLE e8)
set(CMAKE_SIZEOF_VOID_P 4)
cmake_host_system_information(RESULT target32_subkeys QUERY WINDOWS_REGISTRY "${KEY}" SUBKEYS VIEW TARGET ERROR_VARIABLE e9)
cmake_host_system_information(RESULT both32_default QUERY WINDOWS_REGISTRY "${KEY}" ERROR_VARIABLE e10)
"#,
        )?;
        let mut interp = Interp::new(
            InterpConfig {
                host: host.clone(),
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
        );
        assert_eq!(interp.run_script(script).exit_code, 0);
        for err in ["e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8", "e9", "e10"] {
            assert_eq!(interp.cx().var_or_empty(err.as_bytes()), "");
        }
        assert_eq!(interp.cx().var_or_empty(b"multi1"), "data1;data2");
        assert_eq!(interp.cx().var_or_empty(b"multi2"), "data1;data2");
        assert_eq!(
            interp.cx().var_or_empty(b"names_both"),
            "(default);BYTE_SIZE;VALUE2_MULTI_SZ;VALUE2_SZ;VALUE_MULTI_SZ;VALUE_SZ"
        );
        assert_eq!(
            interp.cx().var_or_empty(b"names_64"),
            "(default);BYTE_SIZE;VALUE2_MULTI_SZ;VALUE_MULTI_SZ;VALUE_SZ"
        );
        assert_eq!(
            interp.cx().var_or_empty(b"names_32"),
            "(default);BYTE_SIZE;VALUE2_SZ"
        );
        assert_eq!(
            interp.cx().var_or_empty(b"subkeys_both"),
            "subkey1;subkey2;subkey3"
        );
        assert_eq!(interp.cx().var_or_empty(b"subkeys_64"), "subkey1;subkey2");
        assert_eq!(interp.cx().var_or_empty(b"subkeys_32"), "subkey1;subkey3");
        assert_eq!(
            interp.cx().var_or_empty(b"target32_subkeys"),
            "subkey1;subkey3"
        );
        assert_eq!(interp.cx().var_or_empty(b"both32_default"), "default 32bit");
        Ok(())
    }

    #[test]
    fn windows_registry_case_insensitive_default_32bit_views_and_error_preservation()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::path::Path;
        use std::sync::Arc;

        use rcmake_eval::{Interp, InterpConfig, RunMode};
        use rcmake_host::PureHost;

        // Case-insensitive VALUE "(DEFAULT)" and 32-bit host view filtering.
        assert_eq!(
            super::registry_views(Some(b"32_64"), RegistryView::Bits32, None),
            vec![RegistryView::Bits32]
        );
        assert_eq!(
            super::registry_views(Some(b"64_32"), RegistryView::Bits32, None),
            vec![RegistryView::Bits32]
        );
        assert_eq!(
            super::registry_views(
                Some(b"BOTH"),
                RegistryView::Bits32,
                Some(RegistryView::Bits32)
            ),
            vec![RegistryView::Bits32]
        );

        let host = Arc::new(PureHost::default());
        host.set_registry_host_view(RegistryView::Bits32)?;
        let key = b"Software/CMakeTest";
        host.seed_registry_value(
            RegistryRoot::CurrentUser,
            key,
            None,
            RegistryView::Bits32,
            b"unnamed-default",
        )?;
        let script = Path::new("/work/reg32.cmake");
        host.seed_file(
            script,
            br#"
set(KEY "HKCU/Software/CMakeTest")
cmake_host_system_information(RESULT def_upper QUERY WINDOWS_REGISTRY "${KEY}" VALUE "(DEFAULT)" ERROR_VARIABLE e_upper)
cmake_host_system_information(RESULT def_mixed QUERY WINDOWS_REGISTRY "${KEY}" VALUE "(Default)" ERROR_VARIABLE e_mixed)
cmake_host_system_information(RESULT only64 QUERY WINDOWS_REGISTRY "${KEY}" VIEW 64 ERROR_VARIABLE e_64)
cmake_host_system_information(RESULT only64_names QUERY WINDOWS_REGISTRY "${KEY}" VALUE_NAMES VIEW 64 ERROR_VARIABLE e_64_names)
set(CMAKE_SIZEOF_VOID_P 8)
cmake_host_system_information(RESULT fallback_to_32 QUERY WINDOWS_REGISTRY "${KEY}" VALUE "(DEFAULT)" VIEW BOTH ERROR_VARIABLE e_both)
"#,
        )?;
        let mut interp = Interp::new(
            InterpConfig {
                host: host.clone(),
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
        );
        assert_eq!(interp.run_script(script).exit_code, 0);
        assert_eq!(interp.cx().var_or_empty(b"def_upper"), "unnamed-default");
        assert_eq!(interp.cx().var_or_empty(b"e_upper"), "");
        assert_eq!(interp.cx().var_or_empty(b"def_mixed"), "unnamed-default");
        assert_eq!(interp.cx().var_or_empty(b"e_mixed"), "");
        assert_eq!(interp.cx().var_or_empty(b"only64"), "");
        assert_eq!(
            interp.cx().var_or_empty(b"e_64"),
            "No 64bit registry on Windows32."
        );
        assert_eq!(interp.cx().var_or_empty(b"only64_names"), "");
        assert_eq!(
            interp.cx().var_or_empty(b"e_64_names"),
            "No 64bit registry on Windows32."
        );
        // First view (64) errors on 32-bit host, second view (32) succeeds -> value found, error cleared.
        assert_eq!(
            interp.cx().var_or_empty(b"fallback_to_32"),
            "unnamed-default"
        );
        assert_eq!(interp.cx().var_or_empty(b"e_both"), "");
        Ok(())
    }
}
