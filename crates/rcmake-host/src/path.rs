//! Byte-preserving conversions between CMake byte paths and native [`Path`]/[`PathBuf`].

use std::path::{Path, PathBuf};

use rcmake_base::BString;

/// Convert CMake byte path to a native [`PathBuf`] without requiring UTF-8 on Unix.
#[must_use]
pub fn to_native(p: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(p))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(p).into_owned())
    }
}

/// Convert a native [`Path`] to a CMake [`BString`] without losing non-UTF-8 bytes on Unix.
#[must_use]
pub fn from_native(p: &Path) -> BString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        BString::from(p.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        BString::from(p.to_string_lossy().as_bytes())
    }
}

/// Convert a native path to the forward-slash spelling used by CMake.
#[must_use]
pub fn from_native_cmake(p: &Path) -> BString {
    rcmake_base::path_lex::cmake_style(&from_native(p))
}

pub(crate) fn directory_removal_path(
    p: &Path,
    require_absolute: fn(&Path) -> crate::HostResult<()>,
    windows_separators: bool,
) -> crate::HostResult<PathBuf> {
    use crate::error::SyntheticError;

    require_absolute(p)?;
    let bytes = from_native(p);
    let bytes = bytes.as_slice();
    let separator = |byte: &u8| *byte == b'/' || (windows_separators && *byte == b'\\');
    if p.parent().is_none() {
        return Err(SyntheticError::ResourceBusy.at(Some(p)));
    }
    let end = bytes
        .iter()
        .rposition(|byte| !separator(byte))
        .map(|index| index + 1)
        .ok_or_else(|| SyntheticError::ResourceBusy.at(Some(p)))?;
    let trimmed = bytes.get(..end).unwrap_or_default();
    if matches!(trimmed.rsplit(separator).next(), Some(b"." | b"..")) {
        return Err(SyntheticError::InvalidInput.at(Some(p)));
    }
    Ok(to_native(trimmed))
}

/// Find an executable in an absolute directory from the host's PATH.
#[must_use]
pub fn executable_on_absolute_path(host: &dyn crate::Host, name: &str) -> Option<PathBuf> {
    let mut components = Path::new(name).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) {
        return None;
    }
    #[cfg(any(unix, windows))]
    const EXE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;
    #[cfg(not(any(unix, windows)))]
    const EXE_SUFFIX: &str = "";

    let name = format!("{name}{EXE_SUFFIX}");
    let value = host.env_get(b"PATH")?;
    #[cfg(any(unix, windows))]
    let native_value = to_native(&value);
    #[cfg(any(unix, windows))]
    let directories = std::env::split_paths(&native_value);
    #[cfg(not(any(unix, windows)))]
    let directories = value.split(|&byte| byte == b':').map(to_native);

    directories
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(&name))
        .find(|file| {
            host.metadata(file, crate::Follow::Yes).is_ok_and(|meta| {
                meta.kind == crate::FileKind::File && (cfg!(windows) || meta.mode & 0o111 != 0)
            })
        })
}

/// Hash a regular file with bounded reads.
pub fn hash_file(
    host: &dyn crate::Host,
    file: &Path,
    algorithm: rcmake_base::hash::HashAlgo,
) -> crate::HostResult<String> {
    let metadata = host.metadata(file, crate::Follow::Yes)?;
    if metadata.kind != crate::FileKind::File {
        return Err(crate::HostError::unsupported(
            "The path is not a regular file.",
        ));
    }
    let mut hash = algorithm.hasher();
    host.read_chunks(file, &mut |chunk| hash.update(chunk))?;
    Ok(hash.finish_hex())
}

/// Recover the on-disk spelling (case) of an absolute path without dereferencing symlinks.
#[must_use]
pub fn path_on_disk(host: &dyn crate::Host, path: &Path) -> PathBuf {
    path_on_disk_with(path, |dir| host.read_dir(dir))
}

/// Split a backslash-form (`\\server\share\...`) or slash-form (`//server/share/...`)
/// UNC prefix into `(root_prefix, trailing_bytes, component_separator)`.
fn split_unc_root(raw: &[u8]) -> Option<(BString, &[u8], u8)> {
    let (prefix, tail, sep_out) = if let Some(tail) = raw.strip_prefix(b"\\\\") {
        (b"\\\\".as_slice(), tail, b'\\')
    } else {
        let tail = raw.strip_prefix(b"//")?;
        (b"//".as_slice(), tail, b'/')
    };
    let sep_pos = tail.iter().position(|&b| b == b'/' || b == b'\\')?;
    let server = tail.get(..sep_pos)?;
    if server.is_empty() {
        return None;
    }
    let after_server = tail.get(sep_pos + 1..)?;
    let (share, rest) = match after_server.iter().position(|&b| b == b'/' || b == b'\\') {
        Some(pos) => (
            after_server.get(..pos)?,
            after_server.get(pos + 1..).unwrap_or_default(),
        ),
        None => (after_server, b"".as_slice()),
    };
    if share.is_empty() {
        return None;
    }
    let mut root = Vec::with_capacity(prefix.len() + server.len() + 1 + share.len());
    root.extend_from_slice(prefix);
    root.extend_from_slice(server);
    root.push(sep_out);
    root.extend_from_slice(share);
    Some((BString::from(root), rest, sep_out))
}

/// Recover the on-disk spelling (case) of `path` using `read_dir` for directory lookups.
///
/// Handles POSIX root paths (`/a/b`), Windows drive-letter paths (`c:/a/b` or `c:\a\b`),
/// and both backslash-form (`\\server\share\...`) and slash-form (`//server/share/...`) UNC
/// paths. Uppercases ASCII Windows drive letters (`c:` -> `C:`) and matches each normal
/// component against directory entries (exact match first, then ASCII case-insensitive match,
/// falling back to Unicode `to_lowercase()` folding for non-ASCII filenames).
#[must_use]
pub fn path_on_disk_with<F>(path: &Path, mut read_dir: F) -> PathBuf
where
    F: FnMut(&Path) -> crate::HostResult<Vec<crate::DirEntry>>,
{
    let raw = from_native(path);
    let raw = raw.as_slice();
    let (mut current, rest, sep_out) = if let Some(unc) = split_unc_root(raw) {
        unc
    } else {
        match raw {
            [drive, b':', b'/' | b'\\', tail @ ..] if drive.is_ascii_alphabetic() => {
                let root = [drive.to_ascii_uppercase(), b':', b'/'];
                (BString::from(root.as_slice()), tail, b'/')
            }
            [b'/', tail @ ..] => (BString::from("/"), tail, b'/'),
            _ => return path.to_path_buf(),
        }
    };
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in rest
        .split(|&byte| byte == b'/' || byte == b'\\')
        .filter(|part| !part.is_empty())
    {
        match part {
            b"." => {}
            b".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    for part in parts {
        let dir_path = to_native(&current);
        let matched = read_dir(&dir_path).ok().and_then(|entries| {
            let mut folded = None;
            let mut part_lower: Option<Option<String>> = None;
            for entry in entries {
                if entry.name.as_slice() == part {
                    return Some(entry.name);
                }
                if folded.is_none() {
                    if entry.name.eq_ignore_ascii_case(part) {
                        folded = Some(entry.name);
                    } else if (!part.is_ascii() || !entry.name.is_ascii())
                        && let Some(lower) = part_lower
                            .get_or_insert_with(|| {
                                std::str::from_utf8(part).ok().map(str::to_lowercase)
                            })
                            .as_deref()
                        && std::str::from_utf8(entry.name.as_slice())
                            .is_ok_and(|name| name.to_lowercase() == lower)
                    {
                        folded = Some(entry.name);
                    }
                }
            }
            folded
        });
        if !current.ends_with(&[sep_out]) {
            current.push(sep_out);
        }
        match matched {
            Some(name) => current.extend_from_slice(&name),
            None => current.extend_from_slice(part),
        }
    }
    to_native(&current)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{from_native, path_on_disk_with};
    use crate::{DirEntry, FileKind};

    #[test]
    fn path_on_disk_preserves_backslash_and_slash_unc_prefixes() {
        let backslash = path_on_disk_with(
            Path::new(r"\\server\share\sub\..\SubDir\script.cmake"),
            |dir| {
                let s = from_native(dir);
                let name = match s.as_slice() {
                    br"\\server\share" => "SubDir",
                    br"\\server\share\SubDir" => "Script.cmake",
                    _ => return Ok(Vec::new()),
                };
                Ok(vec![DirEntry {
                    name: name.into(),
                    kind: FileKind::File,
                }])
            },
        );
        assert_eq!(
            backslash,
            PathBuf::from(r"\\server\share\SubDir\Script.cmake")
        );

        let slash = path_on_disk_with(
            Path::new("//server/share/a/../subdir/script.cmake"),
            |dir| {
                let s = from_native(dir);
                let name = match s.as_slice() {
                    b"//server/share" => "SubDir",
                    b"//server/share/SubDir" => "Script.cmake",
                    _ => return Ok(Vec::new()),
                };
                Ok(vec![DirEntry {
                    name: name.into(),
                    kind: FileKind::File,
                }])
            },
        );
        assert_eq!(slash, PathBuf::from("//server/share/SubDir/Script.cmake"));

        // Bare UNC roots without trailing components are preserved verbatim.
        assert_eq!(
            path_on_disk_with(Path::new(r"\\server\share"), |_| Ok(Vec::new())),
            PathBuf::from(r"\\server\share")
        );
        assert_eq!(
            path_on_disk_with(Path::new("//server/share"), |_| Ok(Vec::new())),
            PathBuf::from("//server/share")
        );
        // `..` cannot pop above the UNC share root.
        assert_eq!(
            path_on_disk_with(Path::new("//server/share/../../leaf"), |_| Ok(Vec::new())),
            PathBuf::from("//server/share/leaf")
        );
        // Three leading slashes are POSIX root, not UNC.
        assert_eq!(
            path_on_disk_with(Path::new("///foo/bar"), |_| Ok(Vec::new())),
            PathBuf::from("/foo/bar")
        );
    }

    #[test]
    fn path_on_disk_matches_unicode_case_folded_components() {
        let resolved = path_on_disk_with(Path::new("c:/work/étage/étage.cmake"), |dir| {
            let s = from_native(dir);
            let name = match s.as_slice() {
                b"C:/" => "Work",
                b"C:/Work" => "Étage",
                b"C:/Work/\xc3\x89tage" => "Étage.cmake",
                _ => return Ok(Vec::new()),
            };
            Ok(vec![DirEntry {
                name: name.into(),
                kind: FileKind::File,
            }])
        });
        assert_eq!(resolved, PathBuf::from("C:/Work/Étage/Étage.cmake"));

        let upper = path_on_disk_with(Path::new("/work/ÉTAGE.CMAKE"), |dir| {
            let s = from_native(dir);
            let name = match s.as_slice() {
                b"/" => "work",
                b"/work" => "Étage.cmake",
                _ => return Ok(Vec::new()),
            };
            Ok(vec![DirEntry {
                name: name.into(),
                kind: FileKind::File,
            }])
        });
        assert_eq!(upper, PathBuf::from("/work/Étage.cmake"));
    }
}
