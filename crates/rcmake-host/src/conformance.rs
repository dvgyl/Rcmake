//! Shared file system, environment, and clock checks for both host implementations.

use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use rcmake_base::BString;
use rustix::io::Errno;

use crate::{
    CopyOptions, DirEntry, FileKind, FileTime, Follow, Host, HostError, HostResult, ReadRange,
    Stream, WriteMode,
};

const ALL: ReadRange = ReadRange::All { limit: 64 };

/// Run the shared checks in an empty directory at an absolute path.
///
/// # Panics
///
/// Panics if the host fails a check or if the directory cannot be created.
pub fn run_all(host: &dyn Host, root: &Path) {
    let result = check_fs(host, root);
    assert!(result.is_ok(), "{result:?}");
    check_environment_and_clock(host);
}

fn check_fs(host: &dyn Host, root: &Path) -> HostResult<()> {
    host.create_dir_all(root)?;
    let root = host.canonicalize(root)?;
    check_files(host, &root)?;
    check_links(host, &root)?;
    check_directories(host, &root)?;
    check_rename_noreplace(host, &root)?;
    check_file_locks(host, &root)?;
    check_relative_paths(host, &root);
    Ok(())
}

fn check_rename_noreplace(host: &dyn Host, root: &Path) -> HostResult<()> {
    let from = root.join("noreplace-from");
    let to = root.join("noreplace-to");
    host.write(&from, b"new", WriteMode::Truncate)?;
    host.write(&to, b"old", WriteMode::Truncate)?;
    match host.rename_noreplace(&from, &to) {
        // The filesystem cannot do it. The files must not change.
        Err(err) if err.kind == ErrorKind::Unsupported => {}
        result => {
            assert_kind(result, ErrorKind::AlreadyExists);
            host.remove_file(&to)?;
            host.rename_noreplace(&from, &to)?;
            assert!(host.metadata(&from, Follow::No).is_err());
        }
    }
    let expected: &[u8] = if host.metadata(&from, Follow::No).is_ok() {
        b"old"
    } else {
        b"new"
    };
    assert_eq!(host.read(&to, ALL)?, expected);
    assert_kind(
        host.rename_noreplace(Path::new("relative"), &to),
        ErrorKind::InvalidInput,
    );
    Ok(())
}

fn check_file_locks(host: &dyn Host, root: &Path) -> HostResult<()> {
    let path = root.join("lock-file");
    let guard = host.lock_file(&path, Some(Duration::ZERO))?;
    assert_eq!(guard.path().file_name(), path.file_name());
    assert_eq!(host.read(&path, ALL)?, b"");
    assert_kind(
        host.lock_file(&path, Some(Duration::ZERO)),
        ErrorKind::TimedOut,
    );
    assert_kind(
        host.lock_file(&path, Some(Duration::from_millis(30))),
        ErrorKind::TimedOut,
    );
    drop(guard);
    let again = host.lock_file(&path, None)?;
    drop(again);
    assert_kind(
        host.lock_file(Path::new("relative.lock"), None),
        ErrorKind::InvalidInput,
    );
    Ok(())
}

fn check_files(host: &dyn Host, root: &Path) -> HostResult<()> {
    let file = root.join("file");
    host.write(&file, b"old", WriteMode::Truncate)?;
    host.write(&file, b"abc", WriteMode::Truncate)?;
    host.write(&file, b"def", WriteMode::Append)?;
    assert_eq!(host.read(&file, ALL)?, b"abcdef");
    assert_kind(
        host.write(&file, b"lost", WriteMode::CreateNew),
        ErrorKind::AlreadyExists,
    );
    assert_eq!(host.read(&file, ReadRange::All { limit: 6 })?, b"abcdef");
    assert_eq!(
        host.read(&file, ReadRange::All { limit: 5 })
            .err()
            .and_then(|err| err.errno),
        Some(Errno::FBIG.raw_os_error())
    );
    for (offset, len, expected) in [
        (2, Some(2), b"cd".as_slice()),
        (4, None, b"ef".as_slice()),
        (4, Some(0), b"".as_slice()),
        (u64::MAX, None, b"".as_slice()),
    ] {
        assert_eq!(
            host.read(&file, ReadRange::Slice { offset, len })?,
            expected
        );
    }
    let empty = root.join("empty");
    host.write(&empty, b"", WriteMode::CreateNew)?;
    assert!(host.read(&empty, ReadRange::All { limit: 0 })?.is_empty());
    host.remove_file(&empty)?;

    host.set_permissions(&file, 0o710)?;
    let atime = FileTime {
        secs: 1_600_000_000,
        nanos: 123_456_789,
    };
    let mtime = FileTime {
        secs: 1_600_000_001,
        nanos: 987_654_321,
    };
    host.set_times(&file, Some(atime), None)?;
    host.set_times(&file, None, Some(mtime))?;
    let stat = host.metadata(&file, Follow::Yes)?;
    assert_eq!((stat.kind, stat.len, stat.mode), (FileKind::File, 6, 0o710));
    assert_eq!((stat.atime, stat.mtime), (atime, mtime));
    assert_kind(
        host.set_times(
            &file,
            Some(FileTime {
                nanos: 1_000_000_000,
                ..atime
            }),
            None,
        ),
        ErrorKind::InvalidInput,
    );
    assert_eq!(host.metadata(&file, Follow::Yes)?, stat);

    let copy = root.join("copy");
    for preserve_permissions in [false, true] {
        for preserve_mtime in [false, true] {
            host.write(&copy, b"", WriteMode::CreateNew)?;
            host.set_permissions(&copy, 0o600)?;
            host.copy_file(
                &file,
                &copy,
                CopyOptions {
                    preserve_permissions,
                    preserve_mtime,
                },
            )?;
            let copied = host.metadata(&copy, Follow::Yes)?;
            assert_eq!(
                copied.mode,
                if preserve_permissions {
                    stat.mode
                } else {
                    0o600
                }
            );
            assert_eq!(copied.mtime == stat.mtime, preserve_mtime);
            assert_eq!(host.read(&copy, ALL)?, b"abcdef");
            assert_ne!((copied.dev, copied.ino), (stat.dev, stat.ino));
            host.remove_file(&copy)?;
        }
    }
    assert_kind(
        host.copy_file(&file, &file, CopyOptions::default()),
        ErrorKind::InvalidInput,
    );
    assert_eq!(host.read(&file, ALL)?, b"abcdef");
    host.remove_file(&file)?;
    assert_message(host.read(&file, ALL), "No such file or directory");
    Ok(())
}

fn check_links(host: &dyn Host, root: &Path) -> HostResult<()> {
    let file = root.join("original");
    let hard = root.join("hard");
    let sym = root.join("symbolic");
    host.symlink(Path::new("original"), &sym)?;
    assert_message(
        host.metadata(&sym, Follow::Yes),
        "No such file or directory",
    );
    assert_message(host.write(&sym, b"no", WriteMode::CreateNew), "File exists");
    host.write(&sym, b"abc", WriteMode::Truncate)?;
    host.hard_link(&file, &hard)?;

    let stat = host.metadata(&file, Follow::Yes)?;
    assert_eq!(stat.nlink, 2);
    assert_eq!(host.metadata(&hard, Follow::No)?, stat);
    assert_eq!(host.metadata(&sym, Follow::Yes)?, stat);
    let lstat = host.metadata(&sym, Follow::No)?;
    assert_eq!(lstat.kind, FileKind::Symlink);
    assert_ne!(lstat.ino, stat.ino);
    assert_eq!(host.read_link(&sym)?, Path::new("original"));
    assert_eq!(host.canonicalize(&sym)?, file);
    host.write(&hard, b"d", WriteMode::Append)?;
    assert_eq!(host.read(&sym, ALL)?, b"abcd");
    host.remove_file(&hard)?;
    assert_eq!(host.metadata(&file, Follow::No)?.nlink, 1);
    host.remove_file(&sym)?;

    let cycle_a = root.join("cycle-a");
    let cycle_b = root.join("cycle-b");
    host.symlink(&cycle_b, &cycle_a)?;
    host.symlink(Path::new("cycle-a"), &cycle_b)?;
    assert_message(
        host.canonicalize(&cycle_a),
        "Too many levels of symbolic links",
    );
    assert_eq!(host.metadata(&cycle_a, Follow::No)?.kind, FileKind::Symlink);
    host.remove_file(&cycle_a)?;
    host.remove_file(&cycle_b)?;
    host.remove_file(&file)?;
    Ok(())
}

fn check_directories(host: &dyn Host, root: &Path) -> HostResult<()> {
    let tree = root.join("tree");
    let child = tree.join("child");
    let outside = root.join("outside");
    host.create_dir_all(&child)?;
    host.create_dir_all(&child)?;
    host.create_dir_all(&outside)?;
    host.write(&outside.join("keep"), b"keep", WriteMode::CreateNew)?;
    host.write(&tree.join("file"), b"data", WriteMode::CreateNew)?;
    host.symlink(&outside, &tree.join("link"))?;
    host.symlink(&outside, &root.join("dir-link"))?;

    let mut entries = host.read_dir(&tree)?;
    entries.sort_by(|lhs, rhs| lhs.name.cmp(&rhs.name));
    assert_eq!(
        entries,
        [
            DirEntry {
                name: BString::from("child"),
                kind: FileKind::Dir
            },
            DirEntry {
                name: BString::from("file"),
                kind: FileKind::File
            },
            DirEntry {
                name: BString::from("link"),
                kind: FileKind::Symlink
            },
        ]
    );
    assert_eq!(
        host.read_dir(&root.join("dir-link"))?,
        host.read_dir(&outside)?
    );
    assert_eq!(
        host.canonicalize(&tree.join("link/../tree/./child"))?,
        child
    );
    assert_message(
        host.canonicalize(&tree.join("file/../child")),
        "Not a directory",
    );
    assert_message(host.read(&tree, ALL), "Is a directory");
    assert_message(host.remove_dir(&tree), "Directory not empty");
    assert_message(host.remove_dir(&tree.join("link")), "Not a directory");
    host.remove_file(&root.join("dir-link"))?;
    host.symlink(&outside, &root.join("dir-link"))?;
    host.remove_dir_all(&root.join("dir-link/"))?;
    assert_message(
        host.metadata(&root.join("dir-link"), Follow::No),
        "No such file or directory",
    );

    let renamed = root.join("moved");
    host.rename(&tree, &renamed)?;
    assert_message(host.read_dir(&tree), "No such file or directory");
    assert_eq!(host.read(&renamed.join("file"), ALL)?, b"data");
    assert_eq!(host.canonicalize(&renamed.join("link"))?, outside);
    host.remove_dir(&renamed.join("child"))?;
    host.remove_dir_all(&renamed)?;
    assert_message(
        host.metadata(&renamed, Follow::No),
        "No such file or directory",
    );
    assert_eq!(host.read(&outside.join("keep"), ALL)?, b"keep");
    host.remove_dir_all(&outside)?;
    Ok(())
}

fn check_relative_paths(host: &dyn Host, root: &Path) {
    let p = Path::new("relative");
    let error = HostError::not_absolute(p.to_path_buf());
    let dst = root.join("unexpected");
    for result in [
        host.metadata(p, Follow::Yes).map(|_| ()),
        host.read(p, ALL).map(|_| ()),
        host.write(p, b"", WriteMode::CreateNew),
        host.read_dir(p).map(|_| ()),
        host.create_dir_all(p),
        host.remove_file(p),
        host.remove_dir(p),
        host.remove_dir_all(p),
        host.rename(p, &dst),
        host.rename(&dst, p),
        host.copy_file(p, &dst, CopyOptions::default()),
        host.copy_file(&dst, p, CopyOptions::default()),
        host.symlink(root, p),
        host.hard_link(p, &dst),
        host.hard_link(&dst, p),
        host.read_link(p).map(|_| ()),
        host.canonicalize(p).map(|_| ()),
        host.set_permissions(p, 0o700),
        host.set_times(p, None, None),
    ] {
        assert_eq!(result, Err(error.clone()));
    }
}

fn check_environment_and_clock(host: &dyn Host) {
    let key = b"RCMAKE_HOST_CONFORMANCE";
    let original = host.env_get(key);
    host.env_set(key, Some(b"hello"));
    assert_eq!(host.env_get(key), Some(BString::from("hello")));
    let env = host.env_all();
    assert!(
        env.windows(2)
            .all(|pair| matches!(pair, [a, b] if a.0 < b.0))
    );
    assert!(env.contains(&(BString::from(key.as_slice()), BString::from("hello"))));
    host.env_set(key, Some(b"changed"));
    assert_eq!(host.env_get(key), Some(BString::from("changed")));
    host.env_set(key, None);
    assert!(host.env_get(key).is_none());
    host.env_set(key, original.as_ref().map(|value| value.as_slice()));

    let wall = host.now();
    assert!(wall.nanos < 1_000_000_000);
    let start = host.monotonic();
    let interval = Duration::from_millis(2);
    host.sleep(interval);
    assert!(host.monotonic().saturating_sub(start) >= interval);
    assert!(host.initial_cwd().is_absolute());
    assert!(host.current_exe().is_ok_and(|path| path.is_absolute()));
    assert!(host.system().cpu_count > 0);
    host.write_out(Stream::Stdout, b"");
}

fn assert_kind<T>(result: HostResult<T>, kind: ErrorKind) {
    assert_eq!(result.err().map(|err| err.kind), Some(kind));
}

fn assert_message<T>(result: HostResult<T>, message: &str) {
    assert_eq!(
        result.err().as_ref().map(|err| err.os_message.as_str()),
        Some(message)
    );
}
