use std::path::{Component, Path, PathBuf};

use rcmake_host::path::{from_native_cmake, to_native};

pub(crate) fn path(uri: &str) -> Option<PathBuf> {
    let file = uri.strip_prefix("file://")?;
    let file = file
        .strip_prefix("localhost/")
        .and_then(|_| file.get(9..))
        .unwrap_or(file);
    if !file.starts_with('/') || file.contains(['?', '#']) {
        return None;
    }
    let mut bytes = file.bytes();
    let mut decoded = Vec::with_capacity(file.len());
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let hi = char::from(bytes.next()?).to_digit(16)?;
            let lo = char::from(bytes.next()?).to_digit(16)?;
            ((hi << 4) | lo) as u8
        } else {
            byte
        };
        if byte == 0 {
            return None;
        }
        decoded.push(byte);
    }
    #[cfg(windows)]
    let decoded = match decoded.as_slice() {
        [b'/', drive, b':', b'/', ..] if drive.is_ascii_alphabetic() => decoded.get(1..)?,
        _ => &decoded,
    };
    let path = to_native(decoded.as_ref());
    path.is_absolute().then_some(path)
}

pub(crate) fn file(path: &Path) -> String {
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir if normalized.parent().is_some() => {
                normalized.pop();
            }
            Component::ParentDir => {}
            _ => normalized.push(part),
        }
    }
    let bytes = from_native_cmake(&normalized);
    let mut uri = "file://".to_owned();
    if !bytes.starts_with(b"/") {
        uri.push('/');
    }
    for &byte in bytes.as_slice() {
        if byte.is_ascii_alphanumeric() || b"/-._~:".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            uri.push('%');
            if let (Some(hi), Some(lo)) = (
                HEX.get(usize::from(byte >> 4)),
                HEX.get(usize::from(byte & 15)),
            ) {
                uri.push(char::from(*hi));
                uri.push(char::from(*lo));
            }
        }
    }
    uri
}
