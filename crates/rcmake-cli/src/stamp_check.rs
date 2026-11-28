//! `--check-stamp-file`, `--check-stamp-list`, and `--vs-solution-file` regeneration checks.
//!
//! Matches CMake 4.5.1 (`cmakeCheckStampFile` / `cmakeCheckStampList` in `Source/cmake.cxx`):
//! Visual Studio `ZERO_CHECK` and per-directory stamp rules invoke `cmake` with
//! `--check-stamp-file <stamp>` or `--check-stamp-list <list>` (optionally paired with
//! `--vs-solution-file <sln>`). When every dependency listed in `<stamp>.depend` exists with
//! `mtime <= depend.mtime`, `<stamp>` is atomically restored and `cmake` exits `0` immediately
//! without re-running configure/generate. `--vs-solution-file` is not treated as an up-to-date
//! prerequisite before checking stamps (CMake uses it only to stop a VS IDE build if configure
//! fails).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::Limits;
use rcmake_host::{FileKind, Follow, Host, ReadRange, Stream, WriteMode, path};

use crate::resolve_cli_path;

static STAMP_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Return `true` when stamp checking was requested and either the stamp list or stamp file
/// is up to date, allowing `cmake` to exit `0` without re-running configure and generate.
///
/// Matches `cmake.cxx:3164-3173`: checks `--check-stamp-list` first, and if that is absent
/// or stale, still checks `--check-stamp-file` when provided.
pub(crate) fn is_up_to_date(
    host: &dyn Host,
    stamp_list: Option<&Path>,
    stamp_file: Option<&Path>,
) -> bool {
    if let Some(list) = stamp_list
        && check_stamp_list(host, list)
    {
        return true;
    }
    if let Some(stamp) = stamp_file
        && check_stamp_file(host, stamp)
    {
        return true;
    }
    false
}

/// Check every stamp file listed in `stamp_list` (one path per line).
pub(crate) fn check_stamp_list(host: &dyn Host, stamp_list: &Path) -> bool {
    if !host
        .metadata(stamp_list, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        host.write_out(
            Stream::Stdout,
            b"CMake is re-running because generate.stamp.list is missing.\n",
        );
        return false;
    }
    let Ok(bytes) = host.read(
        stamp_list,
        ReadRange::All {
            limit: Limits::default().max_value_bytes as u64,
        },
    ) else {
        host.write_out(
            Stream::Stdout,
            b"CMake is re-running because generate.stamp.list could not be read.\n",
        );
        return false;
    };
    for raw_line in bytes.split(|&byte| byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if line.is_empty() {
            continue;
        }
        let stamp_path = resolve_cli_path(host, line);
        if !check_stamp_file(host, &stamp_path) {
            return false;
        }
    }
    true
}

/// Check a single `<stamp>` file's `<stamp>.depend` list against `<stamp>.depend`'s mtime
/// and atomically restore `<stamp>` when all listed dependencies are up to date.
///
/// Matches `cmakeCheckStampFile` in CMake 4.5.1 (`Source/cmake.cxx:208-272`).
pub(crate) fn check_stamp_file(host: &dyn Host, stamp: &Path) -> bool {
    let stamp_display = path::from_native_cmake(stamp);
    let depend_path = depend_path_for(stamp);
    let depend_display = path::from_native_cmake(&depend_path);
    let Ok(depend_meta) = host.metadata(&depend_path, Follow::Yes) else {
        host.write_out(
            Stream::Stdout,
            format!("CMake is re-running because {stamp_display} dependency file is missing.\n")
                .as_bytes(),
        );
        return false;
    };
    if depend_meta.kind != FileKind::File {
        host.write_out(
            Stream::Stdout,
            format!("CMake is re-running because {stamp_display} dependency file is missing.\n")
                .as_bytes(),
        );
        return false;
    }
    let Ok(bytes) = host.read(
        &depend_path,
        ReadRange::All {
            limit: Limits::default().max_value_bytes as u64,
        },
    ) else {
        host.write_out(
            Stream::Stdout,
            format!("CMake is re-running because {stamp_display} dependency file is missing.\n")
                .as_bytes(),
        );
        return false;
    };

    for raw_line in bytes.split(|&byte| byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        let dep_path = resolve_cli_path(host, line);
        let compare_result = match host.metadata(&dep_path, Follow::Yes) {
            // `cmFileTimeCache::Compare` sets `result = 0` and returns `false` when a file
            // cannot be stat'd (missing dependency).
            Err(_) => Some(0),
            // `stampDepends < dep` => `result = -1` (out of date).
            Ok(dep_meta) if depend_meta.mtime < dep_meta.mtime => Some(-1),
            Ok(_) => None,
        };
        if let Some(result) = compare_result {
            let dep_display = path::from_native_cmake(&dep_path);
            host.write_out(
                Stream::Stdout,
                format!(
                    "CMake is re-running because {stamp_display} is out-of-date.\n  the file '{dep_display}'\n  is newer than '{depend_display}'\n  result='{result}'\n"
                )
                .as_bytes(),
            );
            return false;
        }
    }

    // All listed dependencies are up to date; restore `<stamp>` atomically (matching CMake
    // 4.5.1, where VS IDE "Rebuild" removes `<stamp>` while leaving `<stamp>.depend`).
    restore_stamp_atomically(host, stamp, &stamp_display)
}

fn restore_stamp_atomically(host: &dyn Host, stamp: &Path, stamp_display: &[u8]) -> bool {
    const STAMP_CONTENT: &[u8] = b"# CMake generation timestamp file for this directory.\n";
    let seq = STAMP_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = host.monotonic().as_nanos();
    let temp_path = temp_path_for(stamp, nanos ^ u128::from(seq));
    let result = host
        .write(&temp_path, STAMP_CONTENT, WriteMode::Truncate)
        .and_then(|()| host.rename(&temp_path, stamp));
    match result {
        Ok(()) => true,
        Err(err) => {
            let _ = host.remove_file(&temp_path);
            let display = String::from_utf8_lossy(stamp_display);
            host.write_out(
                Stream::Stderr,
                format!("CMake Error: Cannot restore timestamp \"{display}\": {err}\n").as_bytes(),
            );
            false
        }
    }
}

fn depend_path_for(stamp: &Path) -> PathBuf {
    let mut raw = path::from_native(stamp);
    raw.extend_from_slice(b".depend");
    path::to_native(&raw)
}

fn temp_path_for(stamp: &Path, nonce: u128) -> PathBuf {
    let mut raw = path::from_native(stamp);
    raw.extend_from_slice(format!(".tmp{nonce:x}").as_bytes());
    path::to_native(&raw)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use rcmake_base::BString;
    use rcmake_host::{FileKind, FileTime, Follow, Host, PureHost, Stream};

    use super::{check_stamp_file, check_stamp_list, is_up_to_date};
    use crate::run;

    fn invoke(host: &Arc<PureHost>, args: &[&str]) -> i32 {
        let argv: Vec<_> = args.iter().copied().map(BString::from).collect();
        run(&argv, host.clone())
    }

    #[test]
    fn check_stamp_file_compares_against_depend_recreates_missing_stamp_and_reports_result_codes() {
        let host = PureHost::default();
        let dep = Path::new("/work/src/CMakeLists.txt");
        let stamp = Path::new("/work/build/CMakeFiles/generate.stamp");
        let depend = Path::new("/work/build/CMakeFiles/generate.stamp.depend");

        host.seed_file(dep, b"project(Demo)\n").unwrap();
        host.set_times(
            dep,
            None,
            Some(FileTime {
                secs: 100,
                nanos: 0,
            }),
        )
        .unwrap();
        host.seed_file(
            depend,
            b"# CMake generation dependency list for this directory.\r\n\r\n/work/src/CMakeLists.txt\r\n",
        )
        .unwrap();
        host.set_times(
            depend,
            None,
            Some(FileTime {
                secs: 200,
                nanos: 0,
            }),
        )
        .unwrap();

        // Missing stamp file with fresh `.depend` succeeds AND atomically restores `<stamp>`.
        assert!(host.metadata(stamp, Follow::Yes).is_err());
        assert!(check_stamp_file(&host, stamp));
        assert_eq!(
            host.metadata(stamp, Follow::Yes).unwrap().kind,
            FileKind::File
        );

        // An older `<stamp>` mtime does NOT force regeneration when `<stamp>.depend` is fresh.
        host.set_times(stamp, None, Some(FileTime { secs: 50, nanos: 0 }))
            .unwrap();
        assert!(check_stamp_file(&host, stamp));

        // Updating a listed dependency newer than `.depend` fails with `result='-1'`.
        host.set_times(
            dep,
            None,
            Some(FileTime {
                secs: 300,
                nanos: 0,
            }),
        )
        .unwrap();
        assert!(!check_stamp_file(&host, stamp));
        let out = String::from_utf8_lossy(&host.stream_bytes(Stream::Stdout)).into_owned();
        assert!(out.contains("is out-of-date."), "{out}");
        assert!(out.contains("result='-1'"), "{out}");

        // Removing a listed dependency fails with `result='0'` (matching cmFileTimeCache::Compare).
        host.remove_file(dep).unwrap();
        let host2 = PureHost::default();
        host2
            .seed_file(depend, b"/work/src/missing_dep.cmake\n")
            .unwrap();
        assert!(!check_stamp_file(&host2, stamp));
        let out2 = String::from_utf8_lossy(&host2.stream_bytes(Stream::Stdout)).into_owned();
        assert!(out2.contains("is out-of-date."), "{out2}");
        assert!(out2.contains("result='0'"), "{out2}");
    }

    #[test]
    fn check_stamp_list_falls_through_to_stamp_file_and_ignores_missing_vs_solution_file() {
        let host = PureHost::default();
        let dep1 = Path::new("/work/src/CMakeLists.txt");
        let dep2 = Path::new("/work/src/sub/CMakeLists.txt");
        let stamp1 = Path::new("/work/build/CMakeFiles/generate.stamp");
        let depend1 = Path::new("/work/build/CMakeFiles/generate.stamp.depend");
        let stamp2 = Path::new("/work/build/sub/CMakeFiles/generate.stamp");
        let depend2 = Path::new("/work/build/sub/CMakeFiles/generate.stamp.depend");
        let list = Path::new("/work/build/CMakeFiles/generate.stamp.list");

        for file in [dep1, dep2] {
            host.seed_file(file, b"").unwrap();
            host.set_times(
                file,
                None,
                Some(FileTime {
                    secs: 100,
                    nanos: 0,
                }),
            )
            .unwrap();
        }
        host.seed_file(depend1, b"# comment\n/work/src/CMakeLists.txt\n")
            .unwrap();
        host.seed_file(depend2, b"/work/src/sub/CMakeLists.txt\n")
            .unwrap();
        for file in [depend1, depend2] {
            host.set_times(
                file,
                None,
                Some(FileTime {
                    secs: 200,
                    nanos: 0,
                }),
            )
            .unwrap();
        }
        host.seed_file(
            list,
            b"/work/build/CMakeFiles/generate.stamp\n/work/build/sub/CMakeFiles/generate.stamp\n",
        )
        .unwrap();

        assert!(check_stamp_list(&host, list));
        assert!(is_up_to_date(&host, Some(list), None));

        // Making the second stamp's dependency stale invalidates the list...
        host.set_times(
            dep2,
            None,
            Some(FileTime {
                secs: 250,
                nanos: 0,
            }),
        )
        .unwrap();
        assert!(!check_stamp_list(&host, list));
        assert!(!is_up_to_date(&host, Some(list), None));

        // ...but when BOTH `--check-stamp-list` and `--check-stamp-file` are supplied and
        // the stamp list is stale while `--check-stamp-file` is fresh, `is_up_to_date` falls
        // through to check `--check-stamp-file` (matching `cmake.cxx:3164-3173`).
        assert!(is_up_to_date(&host, Some(list), Some(stamp1)));
        assert!(!is_up_to_date(&host, Some(list), Some(stamp2)));
    }

    #[test]
    fn cli_check_stamp_short_circuits_fresh_and_regenerates_stale() {
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work/build"),
            PathBuf::from("/bin/cmake"),
        ));
        let dep = Path::new("/work/src/CMakeLists.txt");
        let stamp = Path::new("/work/build/CMakeFiles/generate.stamp");
        let depend = Path::new("/work/build/CMakeFiles/generate.stamp.depend");
        let list = Path::new("/work/build/CMakeFiles/generate.stamp.list");

        // Deliberately omit cmake root so any unintended configure attempt fails.
        host.seed_file(dep, b"project(Demo NONE)\n").unwrap();
        host.sleep(Duration::from_secs(5));
        host.seed_file(depend, b"# deps\n/work/src/CMakeLists.txt\n")
            .unwrap();
        host.seed_file(list, b"/work/build/CMakeFiles/generate.stamp\n")
            .unwrap();

        // Fresh --check-stamp-file exits 0 and recreates missing `<stamp>` without configure.
        assert_eq!(
            invoke(
                &host,
                &[
                    "cmake",
                    "-S/work/src",
                    "-B/work/build",
                    "--check-stamp-file",
                    "CMakeFiles/generate.stamp",
                ],
            ),
            0
        );
        assert!(host.metadata(stamp, Follow::Yes).is_ok());

        // Fresh --check-stamp-list with a non-existent --vs-solution-file still exits 0
        // because CMake does not treat --vs-solution-file as a stamp prerequisite.
        assert_eq!(
            invoke(
                &host,
                &[
                    "cmake",
                    "-S/work/src",
                    "-B/work/build",
                    "--check-stamp-list",
                    "CMakeFiles/generate.stamp.list",
                    "--vs-solution-file",
                    "/work/build/Demo.sln",
                ],
            ),
            0
        );

        // Making the listed dependency newer than `.depend` forces regeneration (which fails
        // here because no cmake root is seeded).
        host.sleep(Duration::from_secs(5));
        host.seed_file(dep, b"project(DemoUpdated NONE)\n").unwrap();
        assert_ne!(
            invoke(
                &host,
                &[
                    "cmake",
                    "-S/work/src",
                    "-B/work/build",
                    "--check-stamp-list",
                    "CMakeFiles/generate.stamp.list",
                    "--vs-solution-file",
                    "/work/build/Demo.sln",
                ],
            ),
            0
        );
    }

    #[test]
    fn check_stamp_regeneration_preserves_warning_as_error_cache_flags() {
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work/build"),
            PathBuf::from("/bin/cmake"),
        ));
        for (name, contents) in [
            ("CMakeDetermineSystem.cmake", ""),
            ("CMakeSystemSpecificInitialize.cmake", ""),
            ("CMakeSystemSpecificInformation.cmake", ""),
            (
                "CMakeNinjaFindMake.cmake",
                "set(CMAKE_MAKE_PROGRAM /bin/ninja CACHE FILEPATH \"\")\n",
            ),
        ] {
            host.seed_file(
                &Path::new("/share/cmake-4.5/Modules").join(name),
                contents.as_bytes(),
            )
            .unwrap();
        }
        host.seed_file(Path::new("/bin/ninja"), b"").unwrap();
        host.set_permissions(Path::new("/bin/ninja"), 0o755)
            .unwrap();
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 3.20)\nproject(Demo NONE)\n",
        )
        .unwrap();
        // Pre-seed cache with compile & link no-warning-as-error flags as set during initial configure.
        host.seed_file(
            Path::new("/work/build/CMakeCache.txt"),
            b"CMAKE_HOME_DIRECTORY:INTERNAL=/work/src\n\
CMAKE_GENERATOR:INTERNAL=Ninja\n\
CMAKE_MAKE_PROGRAM:FILEPATH=/bin/ninja\n\
_RCMAKE_NO_WARNING_AS_ERROR:INTERNAL=ON\n\
_RCMAKE_IGNORE_LINK_WARNING_AS_ERROR:INTERNAL=ON\n",
        )
        .unwrap();
        // Missing `.depend` triggers regeneration via --check-stamp-file without wiping warning flags.
        let code = invoke(
            &host,
            &[
                "cmake",
                "-S/work/src",
                "-B/work/build",
                "--check-stamp-file",
                "CMakeFiles/generate.stamp",
            ],
        );
        assert_eq!(
            code,
            0,
            "stderr={}",
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
        );
        let cache = String::from_utf8_lossy(
            &host
                .read(
                    Path::new("/work/build/CMakeCache.txt"),
                    rcmake_host::ReadRange::All { limit: 1024 * 1024 },
                )
                .unwrap(),
        )
        .into_owned();
        assert!(
            cache.contains("_RCMAKE_NO_WARNING_AS_ERROR:INTERNAL=ON"),
            "{cache}"
        );
        assert!(
            cache.contains("_RCMAKE_IGNORE_LINK_WARNING_AS_ERROR:INTERNAL=ON"),
            "{cache}"
        );
    }
}
