use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::windows::fs::{FileTypeExt, OpenOptionsExt, symlink_dir, symlink_file};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rcmake_base::BString;
use winapi_util::{
    file::information,
    sysinfo::{ComputerNameKind, get_computer_name},
};
use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};

use super::{file_kind, io_at, rename_noreplace_with_link};
use crate::error::{HostError, SyntheticError};
use crate::{FileTime, Follow, HostResult, Metadata, SystemInfo};

const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

fn open_attributes(path: &Path, access: u32, follow: Follow) -> io::Result<File> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if follow == Follow::No {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    OpenOptions::new()
        .access_mode(access)
        .custom_flags(flags)
        .open(path)
}

pub(super) fn open_input(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

pub(super) fn logical_cwd(physical: &Path, pwd: Option<&Path>) -> PathBuf {
    let same_directory = |candidate: &Path| -> io::Result<bool> {
        let a = open_attributes(physical, FILE_READ_ATTRIBUTES, Follow::Yes)?;
        let b = open_attributes(candidate, FILE_READ_ATTRIBUTES, Follow::Yes)?;
        same_file(&a, &b)
    };
    pwd.filter(|path| path.is_absolute() && same_directory(path).unwrap_or(false))
        .unwrap_or(physical)
        .to_path_buf()
}

pub(super) fn same_file(a: &File, b: &File) -> io::Result<bool> {
    let a = information(a)?;
    let b = information(b)?;
    Ok((a.volume_serial_number(), a.file_index()) == (b.volume_serial_number(), b.file_index()))
}

pub(super) fn metadata(path: &Path, follow: Follow) -> HostResult<Metadata> {
    let file = io_at(open_attributes(path, FILE_READ_ATTRIBUTES, follow), path)?;
    let meta = io_at(file.metadata(), path)?;
    let info = io_at(information(&file), path)?;
    let kind = file_kind(meta.file_type());
    let mut mode = if kind == crate::FileKind::Dir {
        0o777
    } else {
        0o666
    };
    if meta.permissions().readonly() {
        mode &= !0o222;
    }
    Ok(Metadata {
        kind,
        len: meta.len(),
        mode,
        mtime: io_at(meta.modified().and_then(from_system_time), path)?,
        atime: io_at(meta.accessed().and_then(from_system_time), path)?,
        dev: info.volume_serial_number(),
        ino: info.file_index(),
        nlink: info.number_of_links(),
    })
}

fn from_system_time(value: SystemTime) -> io::Result<FileTime> {
    let nanos = match value.duration_since(UNIX_EPOCH) {
        Ok(delta) => delta.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    };
    let secs = i64::try_from(nanos.div_euclid(1_000_000_000))
        .map_err(|_| io::Error::from(ErrorKind::InvalidData))?;
    Ok(FileTime {
        secs,
        nanos: nanos.rem_euclid(1_000_000_000) as u32,
    })
}

fn to_system_time(value: FileTime) -> io::Result<SystemTime> {
    let nanos = i128::from(value.secs) * 1_000_000_000 + i128::from(value.nanos);
    let magnitude = nanos.unsigned_abs();
    let delta = Duration::new(
        (magnitude / 1_000_000_000) as u64,
        (magnitude % 1_000_000_000) as u32,
    );
    let time = if nanos < 0 {
        UNIX_EPOCH.checked_sub(delta)
    } else {
        UNIX_EPOCH.checked_add(delta)
    };
    time.ok_or_else(|| io::Error::from(ErrorKind::InvalidInput))
}

pub(super) fn preserve_mtime(dst: &File, metadata: &fs::Metadata) -> io::Result<()> {
    dst.set_times(FileTimes::new().set_modified(metadata.modified()?))
}

/// Set each supplied timestamp using the requested link behavior.
pub(super) fn set_times(
    path: &Path,
    atime: Option<FileTime>,
    mtime: Option<FileTime>,
    follow: Follow,
) -> io::Result<()> {
    let mut times = FileTimes::new();
    if let Some(atime) = atime {
        times = times.set_accessed(to_system_time(atime)?);
    }
    if let Some(mtime) = mtime {
        times = times.set_modified(to_system_time(mtime)?);
    }
    open_attributes(path, FILE_WRITE_ATTRIBUTES, follow)?.set_times(times)
}

pub(super) fn set_permissions(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o222 == 0);
    fs::set_permissions(path, permissions)
}

pub(super) fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    let resolved = link
        .parent()
        .map_or_else(|| target.to_path_buf(), |parent| parent.join(target));
    if resolved.is_dir() {
        symlink_dir(target, link)
    } else {
        symlink_file(target, link)
    }
}

pub(super) fn normalize_path(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\")
        && matches!(
            rest.as_bytes(),
            [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic()
        )
    {
        return PathBuf::from(rest);
    }
    path
}

pub(super) fn remove_file(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink_dir() {
        fs::remove_dir(path)
    } else {
        match fs::remove_file(path) {
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                if let Ok(meta) = fs::symlink_metadata(path) {
                    let mut permissions = meta.permissions();
                    if permissions.readonly() {
                        #[allow(clippy::permissions_set_readonly_false)]
                        permissions.set_readonly(false);
                        let _ = fs::set_permissions(path, permissions);
                        return fs::remove_file(path);
                    }
                }
                Err(err)
            }
            result => result,
        }
    }
}

pub(super) fn remove_dir_all(path: &Path) -> io::Result<()> {
    let kind = fs::symlink_metadata(path)?.file_type();
    if kind.is_symlink_dir() {
        fs::remove_dir(path)
    } else if kind.is_symlink_file() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    }
}

pub(super) use std::fs::hard_link;

pub(super) fn rename_noreplace(from: &Path, to: &Path) -> HostResult<()> {
    let source = io_at(fs::symlink_metadata(from), from)?;
    if !source.file_type().is_dir() {
        match rename_noreplace_with_link(from, to) {
            Err(err) if err.kind == ErrorKind::Unsupported => {}
            result => return result,
        }
    }
    match fs::symlink_metadata(to) {
        Ok(_) => return Err(SyntheticError::AlreadyExists.at(Some(to))),
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(HostError::from_io(err, Some(to.to_path_buf()))),
    }
    io_at(fs::rename(from, to), to)
}

pub(super) fn system() -> SystemInfo {
    let arch = std::env::consts::ARCH;
    let machine = std::env::var("PROCESSOR_ARCHITECTURE")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            match arch {
                "x86_64" => "AMD64",
                "aarch64" => "ARM64",
                "arm" => "ARM",
                _ => arch,
            }
            .to_owned()
        });
    let (release, version) = os_version().unwrap_or_default();
    SystemInfo {
        sysname: BString::from("Windows"),
        release: BString::from(release),
        version: BString::from(version),
        machine: BString::from(machine),
        hostname: crate::path::from_native(Path::new(
            &get_computer_name(ComputerNameKind::DnsHostname).unwrap_or_default(),
        )),
        cpu_count: 0,
        total_memory: 0,
        pid: 0,
    }
}

fn os_version() -> io::Result<(String, String)> {
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")?;
    let release = match (
        key.get_value::<u32, _>("CurrentMajorVersionNumber"),
        key.get_value::<u32, _>("CurrentMinorVersionNumber"),
    ) {
        (Ok(major), Ok(minor)) => format!("{major}.{minor}"),
        _ => key.get_value("CurrentVersion")?,
    };
    Ok((release, key.get_value("CurrentBuildNumber")?))
}
