use super::{new_interp, run, run_configure, run_files};
use crate::RunMode;
use rcmake_diag::{PolicyId, PolicyStatus};

#[test]
fn policies_write_through_weak_call_scopes_and_capture_at_definition() {
    let o = run(concat!(
        "function(f)\n  cmake_policy(SET CMP0140 NEW)\nendfunction()\n",
        "f()\n",
        "cmake_policy(GET CMP0140 x)\n",
        "message(\"after f: [${x}]\")\n",
        "cmake_policy(SET CMP0124 NEW)\n",
        "function(g)\n  cmake_policy(GET CMP0124 y)\n  message(\"in g: [${y}]\")\nendfunction()\n",
        "cmake_policy(SET CMP0124 OLD)\n",
        "g()\n",
        "cmake_policy(PUSH)\n",
        "cmake_policy(SET CMP0124 NEW)\n",
        "cmake_policy(POP)\n",
        "cmake_policy(GET CMP0124 z)\n",
        "message(\"after pop: [${z}]\")\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "after f: [NEW]\nin g: [NEW]\nafter pop: [OLD]\n");
}

#[test]
fn removed_policies_are_always_new() {
    let (mut interp, _) = new_interp(&[], RunMode::Script);
    assert_eq!(interp.cx().policy(PolicyId::new(54)), PolicyStatus::New);
    assert_eq!(interp.cx().policy(PolicyId::new(219)), PolicyStatus::Warn);
}

#[test]
fn policy_pop_without_push_and_unbalanced_push() {
    let o = run("cmake_policy(POP)\n");
    assert_eq!(o.exit, 1);
    assert!(o.stderr.contains("cmake_policy POP without matching PUSH"));

    let o = run_files(
        "include(inc.cmake)\nmessage(after)\n",
        &[("/work/inc.cmake", "cmake_policy(PUSH)\n")],
    );
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr.contains("cmake_policy PUSH without matching POP"),
        "{}",
        o.stderr
    );
}

#[test]
fn diagnostic_scopes_are_independent() {
    let o = run(concat!(
        "block(SCOPE_FOR DIAGNOSTICS)\n",
        "  cmake_diagnostic(SET CMD_AUTHOR IGNORE)\n",
        "  message(AUTHOR_WARNING hidden)\n",
        "endblock()\n",
        "message(AUTHOR_WARNING shown)\n",
        "cmake_diagnostic(PUSH)\n",
        "cmake_diagnostic(SET CMD_AUTHOR IGNORE)\n",
        "cmake_diagnostic(POP)\n",
        "message(AUTHOR_WARNING shown2)\n",
        "cmake_diagnostic(POP)\n",
    ));
    assert_eq!(o.exit, 1);
    assert!(!o.stderr.contains("hidden"));
    assert!(o.stderr.contains("shown\n"));
    assert!(o.stderr.contains("shown2\n"));
    assert!(
        o.stderr
            .contains("cmake_diagnostic POP without matching PUSH")
    );
}

#[test]
fn include_sets_list_file_variables_and_policy_scope() {
    let o = run_files(
        concat!(
            "include(sub/inc.cmake RESULT_VARIABLE r)\n",
            "message(\"r=${r} clf=${CMAKE_CURRENT_LIST_FILE} plf=[${CMAKE_PARENT_LIST_FILE}]\")\n",
            "cmake_policy(GET CMP0124 p)\n",
            "message(\"p=[${p}] from_inc=${from_inc}\")\n",
            "include(sub/inc.cmake NO_POLICY_SCOPE)\n",
            "cmake_policy(GET CMP0124 p)\n",
            "message(\"p=[${p}]\")\n",
        ),
        &[(
            "/work/sub/inc.cmake",
            concat!(
                "message(\"in: ${CMAKE_CURRENT_LIST_FILE} ${CMAKE_CURRENT_LIST_DIR} ${CMAKE_PARENT_LIST_FILE}\")\n",
                "cmake_policy(SET CMP0124 NEW)\n",
                "set(from_inc yes)\n",
                "return()\n",
                "message(\"not reached\")\n",
            ),
        )],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        concat!(
            "in: /work/sub/inc.cmake /work/sub /work/main.cmake\n",
            "r=/work/sub/inc.cmake clf=/work/main.cmake plf=[]\n",
            "p=[] from_inc=yes\n",
            "in: /work/sub/inc.cmake /work/sub /work/main.cmake\n",
            "p=[NEW]\n",
        )
    );
}

#[test]
fn include_resolves_modules_and_reports_missing_files() {
    let o = run_files(
        concat!(
            "set(CMAKE_MODULE_PATH /work/cmake)\n",
            "include(Mine)\n",
            "include(Std)\n",
            "include(Nope OPTIONAL RESULT_VARIABLE r)\n",
            "message(\"r=${r}\")\n",
            "include(Nope)\n",
        ),
        &[
            ("/work/cmake/Mine.cmake", "message(mine)\n"),
            (
                "/opt/cmake/share/cmake-4.5/Modules/Std.cmake",
                "message(std)\n",
            ),
        ],
    );
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "mine\nstd\nr=NOTFOUND\n",
            "CMake Error at main.cmake:6 (include):\n",
            "  include could not find requested file:\n\n",
            "    Nope\n\n\n",
        )
    );
}

#[test]
fn include_parse_error_shows_call_stack() {
    let o = run_files(
        "include(bad.cmake)\n",
        &[("/work/bad.cmake", "message(a)\nendif()\n")],
    );
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at bad.cmake:2 (endif):\n",
            "  Flow control statements are not properly nested.\n",
            "Call Stack (most recent call first):\n",
            "  main.cmake:1 (include)\n\n\n",
        )
    );
}

#[test]
fn include_guard_skips_second_inclusion() {
    let o = run_files(
        "include(g.cmake)\ninclude(g.cmake)\ninclude(gg.cmake)\ninclude(gg.cmake)\n",
        &[
            ("/work/g.cmake", "include_guard()\nmessage(once)\n"),
            ("/work/gg.cmake", "include_guard(GLOBAL)\nmessage(global)\n"),
        ],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "once\nglobal\n");
    let o = run("include_guard(BAD)\n");
    assert!(
        o.stderr
            .contains("include_guard given an invalid scope: BAD")
    );
}

#[test]
fn include_file_api_reports_missing_and_optional() {
    let o =
        run("test_include_file(/work/none.cmake OPTIONAL)\ntest_include_file(/work/none.cmake)\n");
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("test_include_file could not find requested file:\n\n    /work/none.cmake\n")
    );
}

#[test]
fn cmp0198_controls_parent_list_file_in_directory_listfile() {
    let o = run_configure(
        concat!(
            "message(\"[${CMAKE_PARENT_LIST_FILE}]\")\n",
            "include(inc.cmake)\n",
            "message(\"[${CMAKE_PARENT_LIST_FILE}]\")\n",
            "cmake_policy(SET CMP0198 NEW)\n",
            "message(\"[${CMAKE_PARENT_LIST_FILE}]\")\n",
            "include(inc.cmake)\n",
            "message(\"[${CMAKE_PARENT_LIST_FILE}]\")\n",
        ),
        &[(
            "/work/src/inc.cmake",
            "message(\"inc [${CMAKE_PARENT_LIST_FILE}]\")\n",
        )],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        concat!(
            "[/work/src/CMakeLists.txt]\n",
            "inc [/work/src/CMakeLists.txt]\n",
            "[/work/src/CMakeLists.txt]\n",
            "[]\n",
            "inc [/work/src/CMakeLists.txt]\n",
            "[]\n",
        )
    );
}

#[test]
fn cmp0198_does_not_apply_in_script_mode() {
    let o = run("message(\"[${CMAKE_PARENT_LIST_FILE}]\")\n");
    assert_eq!(o.stderr, "[]\n");
}

#[test]
fn include_bundled_module_prefers_bundled_over_module_path() {
    let o = run_files(
        concat!(
            "set(CMAKE_MODULE_PATH /work/cmake)\n",
            "include(Helper)\n",
            "include(Bundled)\n",
        ),
        &[
            ("/work/cmake/Helper.cmake", "message(user-helper)\n"),
            (
                "/opt/cmake/share/cmake-4.5/Modules/Bundled.cmake",
                "include(Helper)\n",
            ),
            (
                "/opt/cmake/share/cmake-4.5/Modules/Helper.cmake",
                "message(bundled-helper)\n",
            ),
        ],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "user-helper\nbundled-helper\n");
}

#[cfg(any(target_os = "macos", windows))]
mod case_mismatch_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rcmake_diag::{DiagCategory, MessageKind};
    use rcmake_host::{
        CopyOptions, DirEntry, FileTime, Follow, Host, HostResult, Metadata, PureHost, ReadRange,
        Stream, WriteMode,
    };

    use super::super::{ROOT, TEST_FRONTEND, register_test};
    use crate::{Interp, InterpConfig, RunMode};

    /// PureHost adapter simulating case-insensitive lookups while preserving exact on-disk
    /// directory entry names from `read_dir`.
    struct CaseInsensitiveHost {
        base: Arc<PureHost>,
        equivalences: Vec<(rcmake_base::BString, rcmake_base::BString)>,
    }

    impl CaseInsensitiveHost {
        fn resolve(&self, path: &Path) -> PathBuf {
            if self.base.metadata(path, Follow::No).is_ok() {
                return path.to_path_buf();
            }
            if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
                && let Ok(entries) = self.base.read_dir(parent)
            {
                let requested = name.as_encoded_bytes();
                let req_lower = std::str::from_utf8(requested).ok().map(str::to_lowercase);
                if let Some(entry) = entries.into_iter().find(|entry| {
                    if self
                        .equivalences
                        .iter()
                        .any(|(req, disk)| req.as_slice() == requested && disk == &entry.name)
                    {
                        return true;
                    }
                    if entry.name.eq_ignore_ascii_case(requested) {
                        return true;
                    }
                    req_lower.as_ref().is_some_and(|lower| {
                        std::str::from_utf8(entry.name.as_slice())
                            .ok()
                            .is_some_and(|text| text.to_lowercase() == *lower)
                    })
                }) {
                    return parent.join(rcmake_host::path::to_native(&entry.name));
                }
            }
            path.to_path_buf()
        }
    }

    impl Host for CaseInsensitiveHost {
        rcmake_host::forward_host!(base, env, process, services);

        fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
            self.base.metadata(&self.resolve(p), follow)
        }

        fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
            self.base.read(&self.resolve(p), range)
        }

        rcmake_host::forward_host!(@method base, write(p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()>);
        rcmake_host::forward_host!(@method base, read_dir(p: &Path) -> HostResult<Vec<DirEntry>>);
        rcmake_host::forward_host!(@method base, create_dir_all(p: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method base, remove_file(p: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method base, remove_dir(p: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method base, remove_dir_all(p: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method base, rename(from: &Path, to: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method base, copy_file(from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()>);
        rcmake_host::forward_host!(@method base, symlink(target: &Path, link: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method base, hard_link(src: &Path, dst: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method base, read_link(p: &Path) -> HostResult<PathBuf>);
        rcmake_host::forward_host!(@method base, canonicalize(p: &Path) -> HostResult<PathBuf>);
        rcmake_host::forward_host!(@method base, set_permissions(p: &Path, mode: u32) -> HostResult<()>);
        rcmake_host::forward_host!(@method base, set_times(p: &Path, atime: Option<FileTime>, mtime: Option<FileTime>) -> HostResult<()>);
    }

    #[test]
    fn include_warns_on_module_case_mismatch() {
        let base = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/usr/local/bin/cmake"),
        ));
        for (path, text) in [
            (
                "/work/main.cmake",
                concat!(
                    "set(CMAKE_MODULE_PATH /work/mods)\n",
                    "include(ExactMod)\n",
                    "include(ExactMod)\n",
                    "include(casemismatch)\n",
                    "include(stdmod)\n",
                ),
            ),
            ("/work/mods/ExactMod.cmake", "message(exact)\n"),
            ("/work/mods/CaseMismatch.cmake", "message(matched)\n"),
            (
                "/opt/cmake/share/cmake-4.5/Modules/StdMod.cmake",
                "message(std)\n",
            ),
        ] {
            base.seed_file(&PathBuf::from(path), text.as_bytes())
                .expect("seed file");
        }
        let host = Arc::new(CaseInsensitiveHost {
            base: base.clone(),
            equivalences: Vec::new(),
        });
        let config =
            InterpConfig::new(host, RunMode::Script, ROOT, "/usr/local/bin/cmake", "/work");
        let mut interp = Interp::new(config, register_test);
        interp.frontend = TEST_FRONTEND;
        let outcome = interp.run_script(&PathBuf::from("main.cmake"));
        let stderr = String::from_utf8_lossy(&base.stream_bytes(Stream::Stderr)).into_owned();
        assert_eq!(outcome.exit_code, 0, "{stderr}");
        assert!(stderr.contains("exact\nexact\n"), "{stderr}");
        assert!(stderr.contains("matched\n"), "{stderr}");
        assert!(stderr.contains("std\n"), "{stderr}");
        assert!(!stderr.contains("ExactMod.cmake"), "{stderr}");

        let expected_user = concat!(
            "The module name\n",
            "  casemismatch.cmake\n",
            "does not match the case of the module file name on disk\n",
            "  /work/mods/CaseMismatch.cmake\n",
            "This may fail on case-sensitive file systems.  Use the module name\n",
            "  CaseMismatch.cmake\n",
            "instead.",
        );
        let expected_std = concat!(
            "The module name\n",
            "  stdmod.cmake\n",
            "does not match the case of the module file name on disk\n",
            "  /opt/cmake/share/cmake-4.5/Modules/StdMod.cmake\n",
            "This may fail on case-sensitive file systems.  Use the module name\n",
            "  StdMod.cmake\n",
            "instead.",
        );
        let warnings: Vec<_> = interp
            .diag_engine()
            .diagnostics()
            .iter()
            .filter(|d| {
                d.kind == MessageKind::AuthorWarning && d.category == Some(DiagCategory::Author)
            })
            .map(|d| d.body.to_string())
            .collect();
        assert_eq!(
            warnings,
            vec![expected_user.to_owned(), expected_std.to_owned()]
        );
    }

    #[test]
    fn include_warns_after_case_rename_for_unicode_and_preserves_symlink_name() {
        fn register_with_rename(reg: &mut crate::Registry) {
            register_test(reg);
            reg.add(
                "test_rename",
                |cx, args| {
                    let from = rcmake_host::path::to_native(&args[0].value);
                    let to = rcmake_host::path::to_native(&args[1].value);
                    cx.host().rename(&from, &to).expect("rename");
                    Ok(crate::Flow::Normal)
                },
                crate::CmdFlags { scriptable: true },
            );
        }

        let base = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/usr/local/bin/cmake"),
        ));
        let nfd_disk = "E\u{0301}nfd.cmake";
        for (path, text) in [
            (
                "/work/main.cmake".to_owned(),
                concat!(
                    "set(CMAKE_MODULE_PATH /work/mods)\n",
                    "include(Exact)\n",
                    "test_rename(/work/mods/Exact.cmake /work/mods/EXACT.cmake)\n",
                    "include(Exact)\n",
                    "include(éclair)\n",
                    "include(énfd)\n",
                    "include(LinkMod)\n",
                    "include(linkmod)\n",
                    "include(beta)\n",
                ),
            ),
            ("/work/mods/Exact.cmake".to_owned(), "message(exact)\n"),
            ("/work/mods/Éclair.cmake".to_owned(), "message(eclair)\n"),
            (format!("/work/mods/{nfd_disk}"), "message(nfd)\n"),
            ("/work/other/RealTarget.cmake".to_owned(), "message(link)\n"),
            ("/work/mods/Alpha.cmake".to_owned(), "message(hardlink)\n"),
        ] {
            base.seed_file(&PathBuf::from(path), text.as_bytes())
                .expect("seed file");
        }
        base.symlink(
            Path::new("/work/other/RealTarget.cmake"),
            Path::new("/work/mods/LinkMod.cmake"),
        )
        .expect("symlink");
        base.hard_link(
            Path::new("/work/mods/Alpha.cmake"),
            Path::new("/work/mods/Beta.cmake"),
        )
        .expect("hard link");

        let host = Arc::new(CaseInsensitiveHost {
            base: base.clone(),
            equivalences: vec![("énfd.cmake".into(), nfd_disk.into())],
        });
        let config =
            InterpConfig::new(host, RunMode::Script, ROOT, "/usr/local/bin/cmake", "/work");
        let mut interp = Interp::new(config, register_with_rename);
        interp.frontend = TEST_FRONTEND;
        let outcome = interp.run_script(&PathBuf::from("main.cmake"));
        let stderr = String::from_utf8_lossy(&base.stream_bytes(Stream::Stderr)).into_owned();
        assert_eq!(outcome.exit_code, 0, "{stderr}");

        let expected = |requested: &str, actual: &str| {
            format!(
                concat!(
                    "The module name\n",
                    "  {req}\n",
                    "does not match the case of the module file name on disk\n",
                    "  /work/mods/{act}\n",
                    "This may fail on case-sensitive file systems.  Use the module name\n",
                    "  {act}\n",
                    "instead.",
                ),
                req = requested,
                act = actual,
            )
        };
        let warnings: Vec<_> = interp
            .diag_engine()
            .diagnostics()
            .iter()
            .filter(|d| {
                d.kind == MessageKind::AuthorWarning && d.category == Some(DiagCategory::Author)
            })
            .map(|d| d.body.to_string())
            .collect();
        assert_eq!(
            warnings,
            vec![
                expected("Exact.cmake", "EXACT.cmake"),
                expected("éclair.cmake", "Éclair.cmake"),
                expected("énfd.cmake", nfd_disk),
                expected("linkmod.cmake", "LinkMod.cmake"),
                expected("beta.cmake", "Beta.cmake"),
            ]
        );
    }
}
