use std::io::ErrorKind;

use super::*;
use crate::PureHost;

const ALL: ReadRange = ReadRange::All { limit: 16 * 1024 };

fn policy(case_sensitive: bool) -> OverlayPathPolicy {
    OverlayPathPolicy {
        style: OverlayPathStyle::Virtual,
        case_sensitive,
    }
}

fn assert_error<T>(result: HostResult<T>, kind: ErrorKind) {
    assert_eq!(result.err().map(|err| err.kind), Some(kind));
}

#[test]
#[cfg(not(target_family = "wasm"))]
fn conforms_to_shared_host_checks() {
    let host = OverlayHost::new(PureHost::default());
    crate::conformance::run_all(&host, Path::new("/work/conformance"));
}

#[test]
fn sensitive_names_remain_distinct() -> HostResult<()> {
    let base = PureHost::default();
    base.symlink(Path::new("target"), Path::new("/work/link"))?;
    let host = OverlayHost::with_path_policy(Arc::new(base), policy(true));
    assert_error(
        host.remove_file(Path::new("/work/LINK")),
        ErrorKind::NotFound,
    );
    host.write(Path::new("/work/LINK"), b"upper", WriteMode::CreateNew)?;
    assert_eq!(
        host.read_link(Path::new("/work/link"))?,
        Path::new("target")
    );
    assert_eq!(host.read(Path::new("/work/LINK"), ALL)?, b"upper");
    assert_eq!(host.read_dir(Path::new("/work"))?.len(), 2);
    Ok(())
}

#[test]
fn insensitive_keys_cover_base_symlinks_and_moved_entries() -> HostResult<()> {
    let base = PureHost::default();
    let old = Path::new("/work/Folder");
    base.seed_file(&old.join("Keep"), b"base")?;
    base.symlink(Path::new("missing"), &old.join("link"))?;
    let host = OverlayHost::with_path_policy(Arc::new(base), policy(false));
    host.remove_file(Path::new("/work/folder/LINK"))?;
    assert_error(host.read_link(&old.join("link")), ErrorKind::NotFound);
    assert_eq!(host.deleted_paths(), [old.join("link")]);

    host.write(Path::new("/work/FOLDER/New"), b"new", WriteMode::CreateNew)?;
    assert_error(
        host.write(
            Path::new("/work/folder/NEW"),
            b"other",
            WriteMode::CreateNew,
        ),
        ErrorKind::AlreadyExists,
    );
    let moved = Path::new("/work/Moved");
    host.rename_noreplace(Path::new("/work/folder"), moved)?;
    assert_error(host.read_dir(old), ErrorKind::NotFound);
    assert_error(host.read_link(&moved.join("LINK")), ErrorKind::NotFound);
    assert_eq!(
        host.canonicalize(Path::new("/work/MOVED/nEW"))?,
        moved.join("New")
    );
    let names: Vec<_> = host
        .read_dir(moved)?
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["Keep", "New"].map(BString::from));
    host.rename(moved, Path::new("/work/moved"))?;
    assert_eq!(
        host.canonicalize(&moved.join("new"))?,
        Path::new("/work/moved/New")
    );
    Ok(())
}

#[test]
fn normalizes_drive_and_unc_aliases() -> HostResult<()> {
    let base = PureHost::default();
    base.create_dir_all(Path::new("C:/work"))?;
    base.create_dir_all(Path::new("//server/share/work"))?;
    let host = OverlayHost::with_path_policy(
        Arc::new(base),
        OverlayPathPolicy {
            style: OverlayPathStyle::Windows,
            case_sensitive: false,
        },
    );
    host.write(Path::new(r"c:\work\Name"), b"drive", WriteMode::CreateNew)?;
    assert_eq!(host.read(Path::new(r"\\?\C:\WORK\NAME"), ALL)?, b"drive");
    host.write(
        Path::new("//server/share/work/Name"),
        b"unc",
        WriteMode::CreateNew,
    )?;
    assert_eq!(
        host.read(Path::new(r"\\?\UNC\SERVER\SHARE\WORK\NAME"), ALL)?,
        b"unc"
    );
    assert_error(host.read_dir(Path::new("/work")), ErrorKind::InvalidInput);
    Ok(())
}

#[test]
fn rejects_dot_and_symlink_rename_endpoints() -> HostResult<()> {
    let host = OverlayHost::new(PureHost::default());
    let dir = Path::new("/work/dir");
    let moved = Path::new("/work/moved");
    host.create_dir_all(&dir.join("child"))?;
    host.symlink(dir, Path::new("/work/link"))?;
    for bad in ["/work/dir/.", "/work/dir/child/../"] {
        assert_error(host.rename(Path::new(bad), moved), ErrorKind::InvalidInput);
        assert_error(
            host.rename_noreplace(dir, Path::new(bad)),
            ErrorKind::InvalidInput,
        );
    }
    assert_error(
        host.rename(Path::new("/work/link/"), moved),
        ErrorKind::NotADirectory,
    );
    assert_error(
        host.rename(dir, Path::new("/work/link/")),
        ErrorKind::NotADirectory,
    );
    host.rename(Path::new("/work/dir/child/../child/"), moved)?;
    assert_eq!(host.metadata(moved, Follow::No)?.kind, FileKind::Dir);
    Ok(())
}

#[test]
fn charges_copies_and_appends_and_credits_the_last_link() -> HostResult<()> {
    let host = OverlayHost::with_limit(Arc::new(PureHost::default()), policy(true), 12 * 1024);
    let first = Path::new("/work/first");
    let second = Path::new("/work/second");
    let third = Path::new("/work/third");
    let alias = Path::new("/work/alias");
    let data = vec![5; 4 * 1024];
    host.write(first, &data, WriteMode::CreateNew)?;
    host.copy_file(first, second, CopyOptions::default())?;
    assert_error(
        host.copy_file(first, third, CopyOptions::default()),
        ErrorKind::FileTooLarge,
    );
    assert_eq!(host.audit().overlay_files, 2);
    assert_error(
        host.write(first, &data, WriteMode::Append),
        ErrorKind::FileTooLarge,
    );
    assert_eq!(host.read(first, ALL)?, data);
    host.hard_link(first, alias)?;
    assert!(host.written_paths().contains(&first.to_path_buf()));
    assert!(host.written_paths().contains(&alias.to_path_buf()));
    assert_eq!(host.audit().overlay_files, 2);
    host.remove_file(first)?;
    assert_error(
        host.copy_file(second, third, CopyOptions::default()),
        ErrorKind::FileTooLarge,
    );
    host.remove_file(alias)?;
    assert_eq!(host.audit().deleted_paths, 0);
    host.write(second, &data, WriteMode::Append)?;
    assert_eq!(
        host.metadata(second, Follow::No)?.len,
        (2 * data.len()) as u64
    );
    host.write(second, b"small", WriteMode::Truncate)?;
    host.write(third, &data, WriteMode::CreateNew)?;
    Ok(())
}

#[test]
fn updates_file_and_parent_times_and_preserves_explicit_times() -> HostResult<()> {
    let host = OverlayHost::new(PureHost::default());
    let work = Path::new("/work");
    let src = work.join("source");
    let dst = work.join("destination");
    let initial = host.metadata(work, Follow::Yes)?.mtime;
    host.write(&src, b"a", WriteMode::CreateNew)?;
    let parent = host.metadata(work, Follow::Yes)?.mtime;
    assert!(parent > initial);
    let first = host.metadata(&src, Follow::Yes)?.mtime;
    host.write(&src, b"b", WriteMode::Truncate)?;
    assert!(host.metadata(&src, Follow::Yes)?.mtime > first);
    assert_eq!(host.metadata(work, Follow::Yes)?.mtime, parent);
    let explicit = FileTime { secs: 9, nanos: 17 };
    host.set_times(&src, None, Some(explicit))?;
    host.copy_file(
        &src,
        &dst,
        CopyOptions {
            preserve_mtime: true,
            ..CopyOptions::default()
        },
    )?;
    assert_eq!(host.metadata(&dst, Follow::Yes)?.mtime, explicit);
    let parent_after_copy = host.metadata(work, Follow::Yes)?.mtime;
    assert!(parent_after_copy > parent);
    host.remove_file(&src)?;
    assert!(host.metadata(work, Follow::Yes)?.mtime > parent_after_copy);
    Ok(())
}

#[test]
fn stores_link_times_in_the_overlay() -> HostResult<()> {
    let base = Arc::new(PureHost::default());
    let link = Path::new("/work/link");
    base.symlink(Path::new("missing"), link)?;
    let original = base.metadata(link, Follow::No)?;
    let host = OverlayHost::from_shared(Arc::clone(&base));

    let mtime = FileTime { secs: 11, nanos: 5 };
    host.set_symlink_times(link, None, Some(mtime))?;
    let updated = host.metadata(link, Follow::No)?;
    assert_eq!(updated.mtime, mtime);
    assert_eq!(updated.atime, original.atime);
    assert_eq!(base.metadata(link, Follow::No)?, original);
    Ok(())
}

#[test]
fn streaming_uses_one_version_for_overlay_and_base_files() -> HostResult<()> {
    let original = vec![5; FILE_CHUNK_SIZE + 16];
    for base_backed in [false, true] {
        let base = PureHost::default();
        let p = Path::new("/work/file");
        if base_backed {
            base.seed_file(p, &original)?;
        }
        let host = OverlayHost::new(base);
        if !base_backed {
            host.write(p, &original, WriteMode::CreateNew)?;
        }
        let mut chunks = 0;
        let mut replacement = Ok(());
        let mut data = Vec::new();
        host.read_chunks(p, &mut |chunk| {
            if chunks == 0 {
                replacement = host.write(p, b"replacement", WriteMode::Truncate);
            }
            chunks += 1;
            data.extend_from_slice(chunk);
        })?;
        replacement?;
        assert_eq!(chunks, 2);
        assert_eq!(data, original);
        assert_eq!(host.read(p, ALL)?, b"replacement");
    }
    Ok(())
}

#[test]
fn pinned_versions_remain_in_the_budget_until_the_reader_returns() -> HostResult<()> {
    let host = OverlayHost::with_limit(Arc::new(PureHost::default()), policy(true), 10 * 1024);
    let p = Path::new("/work/file");
    host.write(p, &vec![5; 4 * 1024], WriteMode::CreateNew)?;
    let replacement = vec![6; 7 * 1024];

    host.read_chunks(p, &mut |_| {
        assert_error(
            host.write(p, &replacement, WriteMode::Truncate),
            ErrorKind::FileTooLarge,
        );
    })?;
    host.write(p, &replacement, WriteMode::Truncate)?;
    assert_eq!(host.read(p, ALL)?, replacement);
    Ok(())
}

#[test]
fn reports_the_base_name_of_an_overlay_hard_link() -> HostResult<()> {
    let base = PureHost::default();
    let original = Path::new("/work/original");
    let alias = Path::new("/work/alias");
    base.seed_file(original, b"base")?;
    let host = OverlayHost::new(base);

    host.hard_link(original, alias)?;
    host.write(alias, b"changed", WriteMode::Truncate)?;
    assert_eq!(
        host.written_paths(),
        [alias.to_path_buf(), original.to_path_buf()]
    );
    assert_eq!(host.read(original, ALL)?, b"changed");
    assert_eq!(host.audit().overlay_files, 1);
    Ok(())
}

#[test]
fn charges_symlink_targets_and_reclaims_overlay_only_names() -> HostResult<()> {
    let host = OverlayHost::with_limit(Arc::new(PureHost::default()), policy(false), 6 * 1024);
    let first = Path::new("/work/first");
    let second = Path::new("/work/second");
    let target = PathBuf::from("x".repeat(4 * 1024));
    host.symlink(&target, first)?;
    assert_error(host.symlink(&target, second), ErrorKind::FileTooLarge);
    host.remove_file(Path::new("/work/FIRST"))?;
    assert_eq!(host.audit().deleted_paths, 0);
    host.symlink(&target, second)?;
    Ok(())
}
