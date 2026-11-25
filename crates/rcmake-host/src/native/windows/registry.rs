use rcmake_base::{BString, ByteSlice as _};
use winreg::{RegKey, enums::*};

use crate::{Host, RegistryRoot, RegistryView};

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

pub(crate) fn value(
    host: &dyn Host,
    root: RegistryRoot,
    subkey: &[u8],
    name: Option<&[u8]>,
    view: RegistryView,
) -> Option<BString> {
    let flags = KEY_READ
        | match view {
            RegistryView::Bits32 => KEY_WOW64_32KEY,
            RegistryView::Bits64 if host_view(host) == RegistryView::Bits64 => KEY_WOW64_64KEY,
            RegistryView::Bits64 => return None,
        };
    let root = match root {
        RegistryRoot::CurrentUser => HKEY_CURRENT_USER,
        RegistryRoot::LocalMachine => HKEY_LOCAL_MACHINE,
        RegistryRoot::ClassesRoot => HKEY_CLASSES_ROOT,
        RegistryRoot::CurrentConfig => HKEY_CURRENT_CONFIG,
        RegistryRoot::Users => HKEY_USERS,
    };
    let subkey = std::str::from_utf8(subkey).ok()?.replace('/', "\\");
    let name = std::str::from_utf8(name.unwrap_or_default()).ok()?;
    let key = RegKey::predef(root)
        .open_subkey_with_flags(subkey, flags)
        .ok()?;
    let raw = key.get_raw_value(name).ok()?;
    use winreg::types::FromRegValue as _;
    match raw.vtype {
        REG_SZ => String::from_reg_value(&raw).ok().map(Into::into),
        REG_EXPAND_SZ => {
            let text = String::from_reg_value(&raw).ok()?;
            Some(expand(host, text.as_bytes()))
        }
        REG_DWORD => u32::from_reg_value(&raw)
            .ok()
            .map(|number| number.to_string().into()),
        REG_QWORD => u64::from_reg_value(&raw)
            .ok()
            .map(|number| number.to_string().into()),
        _ => None,
    }
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
