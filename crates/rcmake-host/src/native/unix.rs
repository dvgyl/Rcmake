use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use rcmake_base::BString;
use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, UTIME_OMIT};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use rustix::io::Errno;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use super::rename_noreplace_with_link;
use super::{file_kind, io_at};
use crate::error::HostResult;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::error::{HostError, from_errno};
use crate::{FileTime, Follow, Metadata, SystemInfo};

pub(super) use super::cwd::logical_cwd;
pub(super) use std::os::unix::fs::symlink;

pub(super) fn metadata(p: &Path, follow: Follow) -> HostResult<Metadata> {
    let m = io_at(
        match follow {
            Follow::Yes => fs::metadata(p),
            Follow::No => fs::symlink_metadata(p),
        },
        p,
    )?;
    Ok(Metadata {
        kind: file_kind(m.file_type()),
        len: m.len(),
        mode: m.mode() & 0o7777,
        mtime: FileTime {
            secs: m.mtime(),
            nanos: u32::try_from(m.mtime_nsec()).unwrap_or_default(),
        },
        atime: FileTime {
            secs: m.atime(),
            nanos: u32::try_from(m.atime_nsec()).unwrap_or_default(),
        },
        dev: m.dev(),
        ino: m.ino(),
        nlink: m.nlink(),
    })
}

pub(super) fn open_input(path: &Path) -> io::Result<File> {
    File::open(path)
}

pub(super) fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

pub(super) fn remove_dir_all(path: &Path) -> io::Result<()> {
    fs::remove_dir_all(path)
}

pub(super) fn same_file(a: &File, b: &File) -> io::Result<bool> {
    let a = a.metadata()?;
    let b = b.metadata()?;
    Ok((a.dev(), a.ino()) == (b.dev(), b.ino()))
}

pub(super) fn preserve_mtime(dst: &File, metadata: &fs::Metadata) -> io::Result<()> {
    rustix::fs::futimens(
        dst,
        &Timestamps {
            last_access: timestamp(None),
            last_modification: Timespec {
                tv_sec: metadata.mtime(),
                tv_nsec: metadata.mtime_nsec(),
            },
        },
    )
    .map_err(Into::into)
}

pub(super) fn hard_link(src: &Path, dst: &Path) -> io::Result<()> {
    rustix::fs::linkat(CWD, src, CWD, dst, AtFlags::empty()).map_err(Into::into)
}

pub(super) fn set_permissions(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
}

pub(super) fn set_times(
    p: &Path,
    atime: Option<FileTime>,
    mtime: Option<FileTime>,
) -> io::Result<()> {
    rustix::fs::utimensat(
        CWD,
        p,
        &Timestamps {
            last_access: timestamp(atime),
            last_modification: timestamp(mtime),
        },
        AtFlags::empty(),
    )
    .map_err(Into::into)
}

pub(super) fn system() -> SystemInfo {
    let u = rustix::system::uname();
    SystemInfo {
        sysname: BString::from(u.sysname().to_bytes()),
        release: BString::from(u.release().to_bytes()),
        version: BString::from(u.version().to_bytes()),
        machine: BString::from(u.machine().to_bytes()),
        hostname: BString::from(u.nodename().to_bytes()),
        cpu_count: 0,
        pid: 0,
    }
}

fn timestamp(time: Option<FileTime>) -> Timespec {
    match time {
        Some(time) => Timespec {
            tv_sec: time.secs,
            tv_nsec: time.nanos.into(),
        },
        None => Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn rename_noreplace(from: &Path, to: &Path) -> HostResult<()> {
    use rustix::fs::{RenameFlags, renameat_with};
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(errno)
            if errno == Errno::NOSYS || errno == Errno::NOTSUP || errno == Errno::OPNOTSUPP =>
        {
            Err(HostError::unsupported(
                "atomic no-replace rename is not supported on this filesystem",
            ))
        }
        Err(errno) if errno == Errno::EXIST => Err(from_errno(errno, Some(to))),
        Err(errno) => Err(from_errno(errno, Some(from))),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn rename_noreplace(from: &Path, to: &Path) -> HostResult<()> {
    rename_noreplace_with_link(from, to)
}
