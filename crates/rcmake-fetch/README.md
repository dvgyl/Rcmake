# FetchContent cache

The `rcmake-fetch` crate provides a shared archive store, Git mirrors, and
project lockfiles. Enable the `native` feature for these operations. The
default features provide the lock mode and Git command classifiers for
portable callers.

## Configure integration

Pass the full ordered URL declaration to
`LockSession::fetch_archive_urls_with`. The transport closure receives the
original URL and can use the declaration's headers, netrc settings, and
other transfer options. Frozen mode validates the declaration and checks
the content pin before it calls the transport. Commit the lock session
after a successful configure. Only update mode writes the lockfile.
`ArchiveResult::offline_seeded` reports whether the cache confirmed the pin.
In update and frozen modes, keep queries and fragments out of locked URLs.
Pass credentials through the transport. The lockfile removes user information
from the portable source identity.

Use the Git classifiers at the ExternalProject template boundary. The
caller must account for submodules, custom download commands, and source
directory overrides before it offers an offline or frozen guarantee.
The caller must reject unsupported network operations in these modes.
The configure dispatcher and CLI flag registration are outside this crate.

## Cache maintenance

`FetchPool` limits each optional archive to 32 MiB and active archive
payloads to 64 MiB. The default request budget is 15 seconds. Pool shutdown
waits up to one second. A host operation that continues after the shutdown
budget keeps its request timeout.

Use `GitMirror::collect` to collect the Git mirror cache. Set
`RCMAKE_FETCH_GIT_MAX_SIZE` to set the mirror cap in bytes. The default is
2 GiB. Collection reports Git mirror bytes and evictions separately from
the archive store. It preserves pin references and skips busy mirrors.
The retained mirrors can exceed the cap.

For an immutable offline snapshot, use `GitMirror::read_only_cache`.
The requested commit must exist in the snapshot. A checkout needs a
writable destination. Set `RCMAKE_FETCH_OFFLINE=1` and
`RCMAKE_FETCH_GIT_READ_ONLY=1` to select the same mode through the
environment. Use an immutable snapshot or a cache that no process can
change during the fetch.
