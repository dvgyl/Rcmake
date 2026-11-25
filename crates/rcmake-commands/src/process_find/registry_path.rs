use rcmake_base::{BString, ByteSlice as _};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, Cx};
use rcmake_host::{RegistryRoot, RegistryView};

use RegistryView::{Bits32, Bits64};

pub(super) fn valid_view(view: &[u8]) -> bool {
    matches!(
        view,
        b"32" | b"64" | b"32_64" | b"64_32" | b"HOST" | b"TARGET" | b"BOTH"
    )
}

pub(super) fn expand(
    cx: &Cx<'_>,
    input: &[u8],
    requested: Option<&[u8]>,
    program: bool,
) -> Result<Vec<BString>, CmdError> {
    if reference(input).is_none() {
        return Ok(vec![input.into()]);
    }
    let Some(host) = cx.host().registry_host_view() else {
        return Ok(Vec::new());
    };
    let target = match cx.var_or_empty(b"CMAKE_SIZEOF_VOID_P").as_slice() {
        b"4" => Some(Bits32),
        b"8" => Some(Bits64),
        _ => None,
    };
    let both = |first| match (first, host) {
        (Bits64, Bits64) => &[Bits64, Bits32][..],
        (Bits32, Bits64) => &[Bits32, Bits64],
        (_, Bits32) => &[Bits32],
    };
    let views = match requested {
        Some(b"32") => &[Bits32][..],
        Some(b"64") => &[Bits64],
        Some(b"32_64") => both(Bits32),
        Some(b"64_32") => both(Bits64),
        Some(b"BOTH") => both(target.unwrap_or(host)),
        Some(b"HOST") => std::slice::from_ref(&host),
        Some(b"TARGET") => std::slice::from_ref(target.as_ref().unwrap_or(&host)),
        _ if cx.policy(PolicyId::new(134)) == PolicyStatus::New => {
            if program {
                both(target.unwrap_or(host))
            } else {
                std::slice::from_ref(target.as_ref().unwrap_or(&host))
            }
        }
        _ if program => both(target.unwrap_or(Bits32)),
        _ if target == Some(Bits64) => &[Bits64],
        _ => &[Bits32],
    };
    views
        .iter()
        .map(|&view| substitute(cx, input, view))
        .collect()
}

fn substitute(cx: &Cx<'_>, input: &[u8], view: RegistryView) -> Result<BString, CmdError> {
    let mut path = input.to_vec();
    for _ in 0..cx.limits().max_recursion.min(128) {
        let Some((start, end, root, key, name)) = reference(&path) else {
            return Ok(path.into());
        };
        let value = cx
            .host()
            .registry_value(root, key, name, view)
            .map_err(|error| CmdError::status(error.to_string()))?
            .unwrap_or_else(|| "/REGISTRY-NOTFOUND".into());
        if (path.len() - (end - start)).saturating_add(value.len()) > cx.limits().max_value_bytes {
            break;
        }
        path.splice(start..end, value.iter().copied());
    }
    Err(CmdError::status(
        "Registry path expansion exceeded its limit.",
    ))
}

type Reference<'a> = (usize, usize, RegistryRoot, &'a [u8], Option<&'a [u8]>);

fn reference(path: &[u8]) -> Option<Reference<'_>> {
    const ROOTS: &[(&[u8], &[u8], RegistryRoot)] = &[
        (b"HKCU", b"HKEY_CURRENT_USER", RegistryRoot::CurrentUser),
        (b"HKLM", b"HKEY_LOCAL_MACHINE", RegistryRoot::LocalMachine),
        (b"HKCR", b"HKEY_CLASSES_ROOT", RegistryRoot::ClassesRoot),
        (b"HKCC", b"HKEY_CURRENT_CONFIG", RegistryRoot::CurrentConfig),
        (b"HKU", b"HKEY_USERS", RegistryRoot::Users),
    ];
    let mut offset = 0;
    for segment in path.split_inclusive(|&byte| byte == b']') {
        let Some(segment_body) = segment.strip_suffix(b"]") else {
            break;
        };
        let braces = segment_body.rfind(b"}");
        for (start, _) in segment_body
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'[')
        {
            let body_start = start + 1;
            let body = segment_body.get(body_start..)?;
            let (separator, body) = if body.starts_with(b"{")
                && let Some(close) = braces.filter(|&close| close > body_start + 1)
            {
                (
                    segment_body.get(body_start + 1..close)?,
                    segment_body.get(close + 1..)?,
                )
            } else {
                (b";".as_slice(), body)
            };
            for &(short, long, root) in ROOTS {
                let Some(key) = body.strip_prefix(long).or_else(|| body.strip_prefix(short)) else {
                    continue;
                };
                let key = key
                    .strip_prefix(b"/")
                    .or_else(|| key.strip_prefix(b"\\"))
                    .unwrap_or(key);
                let (key, name) = key.split_once_str(separator).unwrap_or((key, b""));
                let name =
                    (!name.is_empty() && !name.eq_ignore_ascii_case(b"(default)")).then_some(name);
                return Some((offset + start, offset + segment.len(), root, key, name));
            }
        }
        offset += segment.len();
    }

    None
}
