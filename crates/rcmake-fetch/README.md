# FetchContent cache

The `rcmake-fetch` crate implements a shared archive store, Git mirrors, and project lockfiles for FetchContent. Enable the `native` feature to use those operations. With only the default features enabled, the crate stays portable and exposes the lock mode types and Git command classifiers.

## Configure integration

Pass the full ordered URL declaration to `LockSession::fetch_archive_urls_with`. The transport closure receives each original URL so it can apply headers, netrc settings, and other transfer options from the declaration. In frozen mode, the session validates the declaration and checks the content pin before it calls the transport. Commit the lock session after a successful configure. Only update mode writes the lockfile. After a fetch, `ArchiveResult::offline_seeded` tells you whether the local cache confirmed the pin.

In update and frozen modes, keep query strings and fragments out of locked URLs. Pass credentials through the transport instead of putting them in the URL, because the lockfile strips user information when it builds the portable source identity.

Run the Git classifiers at the ExternalProject template boundary. Before a caller offers an offline or frozen guarantee, it must handle submodules, custom download commands, and source directory overrides, and it must reject unsupported network operations in those modes. Configure dispatch and CLI flag registration live outside this crate.

## Cache maintenance

`FetchPool` caps each optional archive at 32 MiB and active archive payloads at 64 MiB in total. Each request has a default budget of 15 seconds. Pool shutdown waits up to 1 second, and any host operation that continues after shutdown keeps its request timeout.

Call `GitMirror::collect` to prune the Git mirror cache. The mirror cap defaults to 2 GiB, and you can override it in bytes with `RCMAKE_FETCH_GIT_MAX_SIZE`. Collection reports Git mirror bytes and evictions separately from the archive store. Because collection preserves pinned references and skips mirrors that are in use, the retained mirrors can exceed the cap.

For an immutable offline snapshot, open the cache with `GitMirror::read_only_cache`. The requested commit must already exist in the snapshot, while the checkout target still needs a writable destination. You can select the same mode from the environment by setting `RCMAKE_FETCH_OFFLINE=1` and `RCMAKE_FETCH_GIT_READ_ONLY=1`. Make sure the snapshot is immutable or that no other process modifies the cache during the fetch.
