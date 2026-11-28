use std::io;

use rcmake_base::{BString, ByteSlice as _};
use winreg::{RegKey, enums::*};

use crate::{Host, HostError, HostResult, RegistryRoot, RegistryView, decode_multi_sz_utf16le};

pub(crate) fn host_view(host: &dyn Host) -> RegistryView {
    if cfg!(target_pointer_width = "64")
        || host.env_get(b"PROCESSOR_ARCHITEW6432").is_some_and(|arch| {
            [b"AMD64".as_slice(), b"ARM64", b"IA64"]
                .iter()
                .any(|name| arch.eq_ignore_ascii_case(name))
        })
    {
        RegistryView::Bits64
    } else {
        RegistryView::Bits32
    }
}

fn open_key(
    host: &dyn Host,
    root: RegistryRoot,
    subkey: &[u8],
    view: RegistryView,
) -> HostResult<RegKey> {
    let flags = KEY_READ
        | match view {
            RegistryView::Bits32 => KEY_WOW64_32KEY,
            RegistryView::Bits64 if host_view(host) == RegistryView::Bits64 => KEY_WOW64_64KEY,
            RegistryView::Bits64 => {
                return Err(HostError::unsupported("No 64bit registry on Windows32."));
            }
        };
    let root = match root {
        RegistryRoot::CurrentUser => HKEY_CURRENT_USER,
        RegistryRoot::LocalMachine => HKEY_LOCAL_MACHINE,
        RegistryRoot::ClassesRoot => HKEY_CLASSES_ROOT,
        RegistryRoot::CurrentConfig => HKEY_CURRENT_CONFIG,
        RegistryRoot::Users => HKEY_USERS,
    };
    let subkey = std::str::from_utf8(subkey)
        .map_err(|_| HostError::from_io(io::Error::from(io::ErrorKind::InvalidInput), None))?
        .replace('/', "\\");
    RegKey::predef(root)
        .open_subkey_with_flags(subkey, flags)
        .map_err(|err| HostError::from_io(err, None))
}

pub(crate) fn value(
    host: &dyn Host,
    root: RegistryRoot,
    subkey: &[u8],
    name: Option<&[u8]>,
    view: RegistryView,
) -> HostResult<Option<BString>> {
    match value_with_separator(host, root, subkey, name, view, None) {
        Ok(val) => Ok(val),
        Err(err)
            if matches!(
                err.kind,
                io::ErrorKind::NotFound | io::ErrorKind::Unsupported
            ) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn value_with_separator(
    host: &dyn Host,
    root: RegistryRoot,
    subkey: &[u8],
    name: Option<&[u8]>,
    view: RegistryView,
    separator: Option<&[u8]>,
) -> HostResult<Option<BString>> {
    let key = open_key(host, root, subkey, view)?;
    let name = std::str::from_utf8(name.unwrap_or_default())
        .map_err(|_| HostError::from_io(io::Error::from(io::ErrorKind::InvalidInput), None))?;
    let raw = key
        .get_raw_value(name)
        .map_err(|err| HostError::from_io(err, None))?;
    use winreg::types::FromRegValue as _;
    match raw.vtype {
        REG_SZ => String::from_reg_value(&raw)
            .map(|text| Some(text.into()))
            .map_err(|err| HostError::from_io(err, None)),
        REG_EXPAND_SZ => {
            let text = String::from_reg_value(&raw).map_err(|err| HostError::from_io(err, None))?;
            Ok(Some(expand(host, text.as_bytes())))
        }
        REG_MULTI_SZ => Ok(Some(decode_multi_sz_utf16le(&raw.bytes, separator))),
        REG_DWORD => u32::from_reg_value(&raw)
            .map(|number| Some(number.to_string().into()))
            .map_err(|err| HostError::from_io(err, None)),
        REG_QWORD => u64::from_reg_value(&raw)
            .map(|number| Some(number.to_string().into()))
            .map_err(|err| HostError::from_io(err, None)),
        other => Err(HostError::unsupported(format!(
            "{}: unsupported type.",
            other as u32
        ))),
    }
}

pub(crate) fn value_names(
    host: &dyn Host,
    root: RegistryRoot,
    subkey: &[u8],
    view: RegistryView,
) -> HostResult<Option<Vec<BString>>> {
    let key = open_key(host, root, subkey, view)?;
    let mut names = Vec::new();
    for entry in key.enum_values() {
        let (name, _) = entry.map_err(|err| HostError::from_io(err, None))?;
        if name.is_empty() {
            names.push(BString::from("(default)"));
        } else {
            names.push(BString::from(name));
        }
    }
    Ok(Some(names))
}

pub(crate) fn subkeys(
    host: &dyn Host,
    root: RegistryRoot,
    subkey: &[u8],
    view: RegistryView,
) -> HostResult<Option<Vec<BString>>> {
    let key = open_key(host, root, subkey, view)?;
    let mut keys = Vec::new();
    for entry in key.enum_keys() {
        let name = entry.map_err(|err| HostError::from_io(err, None))?;
        keys.push(BString::from(name));
    }
    Ok(Some(keys))
}

fn expand(host: &dyn Host, mut text: &[u8]) -> BString {
    let mut output = BString::default();
    while let Some((before, tail)) = text.split_once_str(b"%") {
        output.extend_from_slice(before);
        let Some((name, rest)) = tail.split_once_str(b"%") else {
            text = text.get(before.len()..).unwrap_or_default();
            break;
        };
        if let Some(value) = host.env_get(name) {
            output.extend_from_slice(&value);
        } else {
            output.push(b'%');
            output.extend_from_slice(name);
            output.push(b'%');
        }
        text = rest;
    }
    output.extend_from_slice(text);
    output
}
