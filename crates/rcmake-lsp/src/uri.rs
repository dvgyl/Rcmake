use std::path::{Component, Path, PathBuf};

use rcmake_host::path::{from_native_cmake, to_native};

pub(crate) struct Root {
    pub(crate) path: PathBuf,
    drive: Option<Drive>,
}

#[derive(Clone, Copy)]
struct Drive {
    letter: u8,
    colon: &'static str,
}

impl Root {
    pub(crate) fn new(uri: &str) -> Option<Self> {
        let file = uri.strip_prefix("file://")?;
        let drive = match local_path(file).and_then(|path| path.strip_prefix('/')) {
            Some(tail) => match tail.as_bytes() {
                [letter, b':', b'/', ..] => Some(Drive {
                    letter: *letter,
                    colon: ":",
                }),
                [letter, b'%', b'3', b'A', b'/', ..] => Some(Drive {
                    letter: *letter,
                    colon: "%3A",
                }),
                [letter, b'%', b'3', b'a', b'/', ..] => Some(Drive {
                    letter: *letter,
                    colon: "%3a",
                }),
                _ => None,
            },
            None => None,
        };
        let root = path(uri)?;
        Some(Self {
            path: path(&self::file(&root))?,
            drive,
        })
    }
}

fn local_path(file: &str) -> Option<&str> {
    if file.starts_with('/') {
        return Some(file);
    }
    let (authority, _) = file.split_once('/')?;
    authority.eq_ignore_ascii_case("localhost").then_some(())?;
    file.get(authority.len()..)
}

pub(crate) fn path(uri: &str) -> Option<PathBuf> {
    let file = uri.strip_prefix("file://")?;
    if file.contains(['?', '#']) {
        return None;
    }
    #[cfg(windows)]
    let mut decoded = local_path(file).map_or_else(|| decode_unc(file), decode)?;
    #[cfg(not(windows))]
    let mut decoded = decode(local_path(file)?)?;
    if cfg!(windows) {
        if matches!(decoded.as_slice(), [b'/', drive, b':', b'/', ..] if drive.is_ascii_alphabetic())
        {
            decoded.remove(0);
        }
        for byte in &mut decoded {
            if *byte == b'/' {
                *byte = b'\\';
            }
        }
    }
    let path = to_native(&decoded);
    path.is_absolute().then_some(path)
}

#[cfg(windows)]
fn decode_unc(file: &str) -> Option<Vec<u8>> {
    let (authority, share) = file.split_once('/')?;
    let authority = decode(authority)?;
    let share = decode(share)?;
    if authority.is_empty()
        || authority.iter().any(|byte| b"/\\:@".contains(byte))
        || share.is_empty()
        || matches!(share.first(), Some(b'/' | b'\\'))
    {
        return None;
    }
    let mut bytes = b"//".to_vec();
    bytes.extend(authority);
    bytes.push(b'/');
    bytes.extend(share);
    Some(bytes)
}

fn decode(text: &str) -> Option<Vec<u8>> {
    let mut bytes = text.bytes();
    let mut decoded = Vec::with_capacity(text.len());
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
    Some(decoded)
}

pub(crate) fn file(path: &Path) -> String {
    encode_file(path, &[])
}

pub(crate) fn client_file(path: &Path, roots: &[Root]) -> String {
    encode_file(path, roots)
}

fn encode_file(path: &Path, roots: &[Root]) -> String {
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
    let mut remainder = bytes.as_slice();
    if cfg!(windows) {
        match remainder {
            [b'/', b'/', tail @ ..] => remainder = tail,
            [letter, b':', tail @ ..] => {
                let default = Drive {
                    letter: letter.to_ascii_uppercase(),
                    colon: ":",
                };
                let style = roots
                    .iter()
                    .filter_map(|root| root.drive.map(|drive| (root, drive)))
                    .filter(|(_, drive)| drive.letter.eq_ignore_ascii_case(letter))
                    .max_by_key(|(root, _)| {
                        if normalized.starts_with(&root.path) {
                            root.path.components().count()
                        } else {
                            0
                        }
                    })
                    .map_or(default, |(_, drive)| drive);
                uri.push('/');
                uri.push(char::from(style.letter));
                uri.push_str(style.colon);
                remainder = tail;
            }
            _ if !remainder.starts_with(b"/") => uri.push('/'),
            _ => {}
        }
    } else if !remainder.starts_with(b"/") {
        uri.push('/');
    }
    for &byte in remainder {
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn windows_uris_use_client_drive_style_and_keep_unc_authority() -> Result<(), &'static str> {
        let roots = [Root::new("file:///c%3A/work").ok_or("Cannot read the workspace URI.")?];
        let target = path("file:///C:/work/Sub%20Dir/CMakeLists.txt")
            .ok_or("Cannot read the definition URI.")?;
        assert_eq!(
            client_file(&target, &roots),
            "file:///c%3A/work/Sub%20Dir/CMakeLists.txt",
        );

        let unc = "file://server/share/work/CMakeLists.txt";
        let target = path(unc).ok_or("Cannot read the UNC URI.")?;
        assert_eq!(client_file(&target, &roots), unc);
        Ok(())
    }
}
