//! Tests for SDK listings that fresh directory stamps validate.

use std::path::Path;
use std::sync::Arc;

use rcmake_host::{Host, HostError, HostResult, PureHost, WriteMode};

use super::TraceMemo;

fn io(error: std::io::Error) -> HostError {
    HostError::from_io(error, None)
}

/// Make a small SDK with a file, a subdirectory, and links to both.
fn sdk(host: &dyn Host, root: &Path) -> HostResult<std::path::PathBuf> {
    let include = root.join("usr/include");
    host.create_dir_all(&include.join("sub"))?;
    host.create_dir_all(&include.join("other"))?;
    host.write(&include.join("a.h"), b"a", WriteMode::CreateNew)?;
    host.write(&include.join("sub/b.h"), b"b", WriteMode::CreateNew)?;
    host.symlink(Path::new("sub"), &include.join("dir"))?;
    host.symlink(Path::new("a.h"), &include.join("alias.h"))?;
    Ok(include)
}

#[test]
fn verification_memos_keep_no_answers() -> HostResult<()> {
    let base: Arc<dyn Host> = Arc::new(PureHost::new("/work".into(), "/work/cmake".into()));
    let include = sdk(base.as_ref(), Path::new("/work/sdk"))?;
    let file = include.join("a.h");
    let memo = TraceMemo::verifying(Arc::clone(&base));
    let first = memo.sdk_digest(base.as_ref(), &include)?;
    memo.file_digest(base.as_ref(), &file)?;
    memo.tool_digest(base.as_ref(), &file)?;
    memo.prefetch_sdk_digest(base.as_ref(), &include);
    let lock = || io(std::io::Error::other("lock"));
    assert!(memo.sdk.lock().map_err(|_| lock())?.is_empty());
    assert!(memo.files.lock().map_err(|_| lock())?.is_empty());
    assert!(memo.tools.lock().map_err(|_| lock())?.is_empty());
    // A change through the base host is visible without a generation change.
    base.write(&include.join("new.h"), b"n", WriteMode::CreateNew)?;
    assert_ne!(memo.sdk_digest(base.as_ref(), &include)?, first);
    assert_eq!(memo.sdk_graph_hits(), 0);
    Ok(())
}

#[cfg(unix)]
mod native {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rcmake_host::{Follow, Host, HostResult, MemoHost, Metadata, NativeHost, WriteMode};

    use super::super::{Digest, TraceMemo, sdk_tree_listing_digest};
    use super::{io, sdk};

    /// A native host that reports every object as owned by the superuser
    /// without group or other write permission, as a system SDK is.
    struct SystemOwned {
        base: NativeHost,
    }

    fn system_owned(mut metadata: Metadata) -> Metadata {
        metadata.owner = Some(0);
        metadata.mode &= !0o022;
        metadata
    }

    impl Host for SystemOwned {
        fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
            self.base.metadata(p, follow).map(system_owned)
        }

        fn metadata_fresh(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
            self.base.metadata_fresh(p, follow).map(system_owned)
        }

        fn directory_times_track_names(&self) -> bool {
            true
        }

        rcmake_host::forward_host!(@ base, read);
        rcmake_host::forward_host!(@ base, read_with_origin);
        rcmake_host::forward_host!(@ base, read_chunks);
        rcmake_host::forward_host!(@method base, write(p: &::std::path::Path, data: &[u8], mode: rcmake_host::WriteMode) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, read_dir(p: &::std::path::Path) -> rcmake_host::HostResult<Vec<rcmake_host::DirEntry>>);
        rcmake_host::forward_host!(@method base, create_dir_all(p: &::std::path::Path) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, create_private_dir(p: &::std::path::Path) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, remove_file(p: &::std::path::Path) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, remove_dir(p: &::std::path::Path) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, remove_dir_all(p: &::std::path::Path) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, rename(from: &::std::path::Path, to: &::std::path::Path) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, rename_noreplace(from: &::std::path::Path, to: &::std::path::Path) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, lock_file(path: &::std::path::Path,
        timeout: Option<::core::time::Duration>,
        ) -> rcmake_host::HostResult<Box<dyn rcmake_host::FileLockGuard>>);
        rcmake_host::forward_host!(@method base, copy_file(from: &::std::path::Path, to: &::std::path::Path, opts: rcmake_host::CopyOptions) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, symlink(target: &::std::path::Path, link: &::std::path::Path) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, hard_link(src: &::std::path::Path, dst: &::std::path::Path) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@ base, read_link);
        rcmake_host::forward_host!(@method base, canonicalize(p: &::std::path::Path) -> rcmake_host::HostResult<::std::path::PathBuf>);
        rcmake_host::forward_host!(@method base, canonicalize_fresh(p: &::std::path::Path) -> rcmake_host::HostResult<::std::path::PathBuf>);
        rcmake_host::forward_host!(@method base, set_permissions(p: &::std::path::Path, mode: u32) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, set_times(p: &::std::path::Path,
        atime: Option<rcmake_host::FileTime>,
        mtime: Option<rcmake_host::FileTime>,
        ) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@method base, set_symlink_times(path: &::std::path::Path,
        atime: Option<rcmake_host::FileTime>,
        mtime: Option<rcmake_host::FileTime>,
        ) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(@ base, open_read);
        rcmake_host::forward_host!(@ base, scratch_file);
        rcmake_host::forward_host!(@ base, scratch_file_in);
        rcmake_host::forward_host!(@method base, write_from(path: &::std::path::Path,
        mode: rcmake_host::WriteMode,
        input: &mut dyn ::std::io::Read,
        len: u64,
        ) -> rcmake_host::HostResult<()>);
        rcmake_host::forward_host!(base, env, process, services);
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        native: Arc<dyn Host>,
        host: Arc<MemoHost<dyn Host>>,
        memo: TraceMemo,
        root: PathBuf,
    }

    impl Fixture {
        fn new(system: bool) -> HostResult<Self> {
            let temp = tempfile::tempdir().map_err(io)?;
            let native: Arc<dyn Host> = if system {
                Arc::new(SystemOwned {
                    base: NativeHost::new()?,
                })
            } else {
                Arc::new(NativeHost::new()?)
            };
            let root = native.canonicalize(temp.path())?;
            let host = Arc::new(MemoHost::from_shared(Arc::clone(&native)));
            let memo = TraceMemo::with_generation(Arc::clone(&host));
            Ok(Self {
                _temp: temp,
                native,
                host,
                memo,
                root,
            })
        }

        fn digest(&self, sdk: &Path) -> HostResult<Digest> {
            let digest = self.memo.sdk_digest(self.host.as_ref(), sdk)?;
            assert_eq!(digest, sdk_tree_listing_digest(self.native.as_ref(), sdk)?);
            Ok(digest)
        }

        /// Stand for a process call, which gives every path a new generation.
        fn run_process(&self) {
            self.host.clear();
        }
    }

    /// Wait until the stamps of directories made now are usable.
    fn age() {
        std::thread::sleep(std::time::Duration::from_millis(2_100));
    }

    #[test]
    fn stamped_sdk_listings_survive_process_calls_and_detect_every_change() -> HostResult<()> {
        let fixture = Fixture::new(true)?;
        let native = fixture.native.as_ref();
        let names = [
            "same", "add", "remove", "rename", "link", "external", "return",
        ];
        let mut sdks = Vec::new();
        for name in names {
            sdks.push(sdk(native, &fixture.root.join(name))?);
        }
        // `external` links to a directory outside the SDK through a switchable link.
        let outside = fixture.root.join("outside");
        for choice in ["A", "B"] {
            native.create_dir_all(&outside.join(choice).join("inc"))?;
        }
        native.symlink(Path::new("A"), &outside.join("choice"))?;
        native.symlink(&outside.join("choice/inc"), &sdks[5].join("ext"))?;
        // `return` resolves outside the SDK and back: `inside -> ../../../out/dir`,
        // `t -> inside/../back`, and `out/back -> <sdk>/sub`.
        let back = fixture.root.join("out");
        native.create_dir_all(&back.join("dir"))?;
        native.symlink(Path::new("../../../out/dir"), &sdks[6].join("inside"))?;
        native.symlink(Path::new("inside/../back"), &sdks[6].join("t"))?;
        native.symlink(&sdks[6].join("sub"), &back.join("back"))?;
        age();

        let first: Vec<_> = sdks
            .iter()
            .map(|sdk| fixture.digest(sdk))
            .collect::<HostResult<_>>()?;

        // Fast path 2: the stamps match after a process call.
        fixture.run_process();
        let hits = fixture.memo.sdk_graph_hits();
        assert_eq!(fixture.digest(&sdks[0])?, first[0]);
        assert_eq!(fixture.memo.sdk_graph_hits(), hits + 1);
        // Fast path 1: the stamp check refreshed the generation.
        assert_eq!(fixture.digest(&sdks[0])?, first[0]);
        assert_eq!(fixture.memo.sdk_graph_hits(), hits + 2);

        // Change each other SDK outside the configure host, as a compiler can.
        native.write(&sdks[1].join("sub/new.h"), b"n", WriteMode::CreateNew)?;
        native.remove_file(&sdks[2].join("a.h"))?;
        native.rename(&sdks[3].join("sub/b.h"), &sdks[3].join("sub/c.h"))?;
        native.remove_file(&sdks[4].join("dir"))?;
        native.symlink(Path::new("other"), &sdks[4].join("dir"))?;
        native.remove_file(&outside.join("choice"))?;
        native.symlink(Path::new("B"), &outside.join("choice"))?;
        native.remove_file(&back.join("back"))?;
        native.symlink(&sdks[6].join("other"), &back.join("back"))?;
        fixture.run_process();
        let hits = fixture.memo.sdk_graph_hits();
        for (index, sdk) in sdks.iter().enumerate().skip(1) {
            assert_ne!(fixture.digest(sdk)?, first[index], "{}", names[index]);
        }
        assert_eq!(fixture.memo.sdk_graph_hits(), hits);
        Ok(())
    }

    #[test]
    fn sdk_listings_that_this_user_can_change_are_walked_again() -> HostResult<()> {
        let fixture = Fixture::new(false)?;
        let include = sdk(fixture.native.as_ref(), &fixture.root.join("user"))?;
        age();
        let first = fixture.digest(&include)?;
        fixture.run_process();
        let hits = fixture.memo.sdk_graph_hits();
        assert_eq!(fixture.digest(&include)?, first);
        assert_eq!(fixture.memo.sdk_graph_hits(), hits);
        // A file of this user is hashed again even when its metadata is restored.
        let file = include.join("a.h");
        let before = fixture.native.metadata(&file, Follow::Yes)?;
        let digest = fixture.memo.file_digest(fixture.host.as_ref(), &file)?;
        fixture.native.write(&file, b"z", WriteMode::Truncate)?;
        fixture
            .native
            .set_times(&file, Some(before.atime), Some(before.mtime))?;
        assert_ne!(
            fixture.memo.file_digest(fixture.host.as_ref(), &file)?,
            digest
        );
        Ok(())
    }

    #[test]
    fn fresh_sdk_directories_are_walked_again_after_a_process_call() -> HostResult<()> {
        let fixture = Fixture::new(true)?;
        let include = sdk(fixture.native.as_ref(), &fixture.root.join("fresh"))?;
        let first = fixture.digest(&include)?;
        fixture.run_process();
        let hits = fixture.memo.sdk_graph_hits();
        // A change within the same second can keep the stamp. Walk again.
        assert_eq!(fixture.digest(&include)?, first);
        assert_eq!(fixture.memo.sdk_graph_hits(), hits);
        Ok(())
    }

    #[test]
    fn prefetched_sdk_listings_are_checked_before_use() -> HostResult<()> {
        let fixture = Fixture::new(true)?;
        let native = fixture.native.as_ref();
        let same = sdk(native, &fixture.root.join("same"))?;
        let changed = sdk(native, &fixture.root.join("changed"))?;
        age();
        fixture.memo.prefetch_sdk_digest(native, &same);
        fixture.memo.prefetch_sdk_digest(native, &changed);
        native.write(&changed.join("late.h"), b"l", WriteMode::CreateNew)?;
        let hits = fixture.memo.sdk_graph_hits();
        fixture.digest(&same)?;
        assert_eq!(fixture.memo.sdk_graph_hits(), hits + 1);
        fixture.digest(&changed)?;
        assert_eq!(fixture.memo.sdk_graph_hits(), hits + 1);
        Ok(())
    }
}
