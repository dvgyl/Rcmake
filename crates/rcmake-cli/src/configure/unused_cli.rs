use std::collections::BTreeSet;

use rcmake_base::{BString, suggest::levenshtein};
use rcmake_diag::{
    DiagAction, DiagCategory, DiagEngine, DiagOptions, Diagnostic, Effect, Location, MessageKind,
    RenderOptions,
};
use rcmake_eval::{CacheDefine, Interp};
use rcmake_host::{Host, Stream};
use rcmake_json::Value;
use rcmake_model::{Cache, CacheType};

const MAX_HINT_NAME_BYTES: usize = 128;

/// Well-known documented CMake variables commonly supplied via `-D`.
const KNOWN_CMAKE_VARS: &[&[u8]] = &[
    b"BUILD_SHARED_LIBS",
    b"BUILD_TESTING",
    b"CMAKE_ARCHIVE_OUTPUT_DIRECTORY",
    b"CMAKE_BUILD_TYPE",
    b"CMAKE_BUILD_WITH_INSTALL_RPATH",
    b"CMAKE_COLOR_DIAGNOSTICS",
    b"CMAKE_COLOR_MAKEFILE",
    b"CMAKE_COMPILE_WARNING_AS_ERROR",
    b"CMAKE_CONFIGURATION_TYPES",
    b"CMAKE_CUDA_ARCHITECTURES",
    b"CMAKE_CUDA_COMPILER",
    b"CMAKE_CUDA_STANDARD",
    b"CMAKE_CXX_COMPILER",
    b"CMAKE_CXX_COMPILER_LAUNCHER",
    b"CMAKE_CXX_EXTENSIONS",
    b"CMAKE_CXX_FLAGS",
    b"CMAKE_CXX_FLAGS_DEBUG",
    b"CMAKE_CXX_FLAGS_MINSIZEREL",
    b"CMAKE_CXX_FLAGS_RELEASE",
    b"CMAKE_CXX_FLAGS_RELWITHDEBINFO",
    b"CMAKE_CXX_SCAN_FOR_MODULES",
    b"CMAKE_CXX_STANDARD",
    b"CMAKE_CXX_STANDARD_REQUIRED",
    b"CMAKE_C_COMPILER",
    b"CMAKE_C_COMPILER_LAUNCHER",
    b"CMAKE_C_EXTENSIONS",
    b"CMAKE_C_FLAGS",
    b"CMAKE_C_FLAGS_DEBUG",
    b"CMAKE_C_FLAGS_MINSIZEREL",
    b"CMAKE_C_FLAGS_RELEASE",
    b"CMAKE_C_FLAGS_RELWITHDEBINFO",
    b"CMAKE_C_STANDARD",
    b"CMAKE_C_STANDARD_REQUIRED",
    b"CMAKE_DEBUG_POSTFIX",
    b"CMAKE_EXE_LINKER_FLAGS",
    b"CMAKE_EXPORT_COMPILE_COMMANDS",
    b"CMAKE_EXPORT_SARIF",
    b"CMAKE_FIND_DEBUG_MODE",
    b"CMAKE_FIND_PACKAGE_PREFER_CONFIG",
    b"CMAKE_FIND_ROOT_PATH",
    b"CMAKE_Fortran_COMPILER",
    b"CMAKE_INSTALL_BINDIR",
    b"CMAKE_INSTALL_DATADIR",
    b"CMAKE_INSTALL_DOCDIR",
    b"CMAKE_INSTALL_INCLUDEDIR",
    b"CMAKE_INSTALL_LIBDIR",
    b"CMAKE_INSTALL_MANDIR",
    b"CMAKE_INSTALL_NAME_DIR",
    b"CMAKE_INSTALL_PREFIX",
    b"CMAKE_INSTALL_RPATH",
    b"CMAKE_INSTALL_RPATH_USE_LINK_PATH",
    b"CMAKE_INSTALL_SYSCONFDIR",
    b"CMAKE_INTERPROCEDURAL_OPTIMIZATION",
    b"CMAKE_LIBRARY_OUTPUT_DIRECTORY",
    b"CMAKE_LINKER_TYPE",
    b"CMAKE_LINK_WARNING_AS_ERROR",
    b"CMAKE_MAKE_PROGRAM",
    b"CMAKE_MESSAGE_LOG_LEVEL",
    b"CMAKE_MODULE_LINKER_FLAGS",
    b"CMAKE_MODULE_PATH",
    b"CMAKE_MSVC_RUNTIME_LIBRARY",
    b"CMAKE_OSX_ARCHITECTURES",
    b"CMAKE_OSX_DEPLOYMENT_TARGET",
    b"CMAKE_OSX_SYSROOT",
    b"CMAKE_POLICY_VERSION_MINIMUM",
    b"CMAKE_POSITION_INDEPENDENT_CODE",
    b"CMAKE_PREFIX_PATH",
    b"CMAKE_PROJECT_TOP_LEVEL_INCLUDES",
    b"CMAKE_RULE_MESSAGES",
    b"CMAKE_RUNTIME_OUTPUT_DIRECTORY",
    b"CMAKE_SHARED_LINKER_FLAGS",
    b"CMAKE_SKIP_BUILD_RPATH",
    b"CMAKE_SKIP_INSTALL_RULES",
    b"CMAKE_SKIP_RPATH",
    b"CMAKE_STAGING_PREFIX",
    b"CMAKE_SYSROOT",
    b"CMAKE_SYSTEM_NAME",
    b"CMAKE_SYSTEM_PROCESSOR",
    b"CMAKE_TOOLCHAIN_FILE",
    b"CMAKE_UNITY_BUILD",
    b"CMAKE_UNITY_BUILD_BATCH_SIZE",
    b"CMAKE_VERBOSE_MAKEFILE",
    b"CMAKE_VERIFY_INTERFACE_HEADER_SETS",
    b"CPACK_GENERATOR",
    b"CPACK_OUTPUT_FILE_PREFIX",
    b"CPACK_PACKAGE_DIRECTORY",
    b"FETCHCONTENT_BASE_DIR",
    b"FETCHCONTENT_FULLY_DISCONNECTED",
    b"FETCHCONTENT_QUIET",
    b"FETCHCONTENT_UPDATES_DISCONNECTED",
];

pub(super) fn apply_define(cache: &mut Cache, def: CacheDefine) -> bool {
    let previous = cache.get(&def.name);
    let ty = def.ty.or_else(|| previous.map(|entry| entry.ty));
    let normalized = |value: &[u8]| {
        if matches!(ty, Some(CacheType::Path | CacheType::Filepath)) {
            value
                .iter()
                .map(|&byte| if byte == b'\\' { b'/' } else { byte })
                .collect()
        } else {
            BString::from(value)
        }
    };
    let changed = previous.is_none_or(|entry| normalized(&entry.value) != normalized(&def.value));
    crate::cache::apply(cache, def);
    changed
}

pub(super) fn report(
    host: &dyn Host,
    interp: &mut Interp,
    results: &mut Vec<Value>,
    allow_hints: bool,
) -> bool {
    let category = DiagCategory::UnusedCli;
    let state = *interp.cx().diag_state();
    let action = state.get(category);
    if action == DiagAction::Ignore {
        return false;
    }
    let unread: Vec<_> = interp.unread_variables().map(BString::from).collect();
    let mut cx = interp.cx();
    let names: Vec<BString> = unread
        .into_iter()
        .filter(|name| cx.cache().get(name).is_some())
        .collect();
    if names.is_empty() {
        return false;
    }
    let is_error = action >= DiagAction::SendError;
    let message = b"Manually-specified variables were not used by the project:";
    let mut body = BString::from(message.as_slice());
    let mut display = BString::from(if is_error {
        "CMake Error (unused-cli):\n  "
    } else {
        "CMake Warning (unused-cli):\n  "
    });
    display.extend_from_slice(message);
    display.extend_from_slice(b"\n\n");
    for name in &names {
        body.extend_from_slice(b"\n  ");
        body.extend_from_slice(name);
        display.extend_from_slice(b"    ");
        display.extend_from_slice(name);
        display.push(b'\n');
    }
    display.extend_from_slice(b"\n\n");
    host.write_out(Stream::Stderr, &display);

    if allow_hints {
        let hints = format_typo_hints(&names, cx.cache());
        if !hints.is_empty() {
            host.write_out(Stream::Stderr, hints.as_bytes());
        }
    }

    let mut engine = DiagEngine::new(DiagOptions {
        initial_state: state,
        sarif: true,
    });
    let diagnostic = Diagnostic {
        annotations: None,
        kind: if is_error {
            MessageKind::Error
        } else {
            MessageKind::Warning
        },
        effect: Effect::None,
        category: Some(category),
        policy: None,
        location: Location::None,
        body,
    };
    engine.issue(&diagnostic, &state, &RenderOptions::default());
    results.extend(crate::sarif::results_from_engine(&engine));
    is_error
}

fn format_typo_hints(unused: &[BString], cache: &Cache) -> String {
    let unused_set: BTreeSet<&[u8]> = unused.iter().map(|s| s.as_slice()).collect();
    let mut candidates = BTreeSet::new();
    for (name, entry) in cache.iter() {
        if !matches!(entry.ty, CacheType::Internal | CacheType::Static)
            && !unused_set.contains(name.as_slice())
        {
            candidates.insert(name.clone());
        }
    }
    for &known in KNOWN_CMAKE_VARS {
        if !unused_set.contains(known) {
            candidates.insert(BString::from(known));
        }
    }
    let mut out = String::new();
    for name in unused {
        if let Some(best) = unique_closest_match(name.as_slice(), &candidates) {
            out.push_str(&format!("hint: {name} is unused. Did you mean {best}?\n"));
        }
    }
    out
}

fn max_distance(len: usize) -> usize {
    match len {
        0..=2 => 0,
        3 => 1,
        _ => 2,
    }
}

fn unique_closest_match<'a>(
    input: &[u8],
    candidates: &'a BTreeSet<BString>,
) -> Option<&'a BString> {
    if input.len() > MAX_HINT_NAME_BYTES {
        return None;
    }
    let limit = max_distance(input.len());
    if limit == 0 {
        return None;
    }
    let mut best: Option<(&'a BString, usize)> = None;
    let mut tied = false;
    for candidate in candidates {
        let bytes = candidate.as_slice();
        if bytes.is_empty()
            || bytes.len() > MAX_HINT_NAME_BYTES
            || bytes == input
            || input.len().abs_diff(bytes.len()) > limit
        {
            continue;
        }
        let dist = levenshtein(input, bytes);
        if dist == 0 || dist > limit {
            continue;
        }
        match best {
            Some((_, best_dist)) if dist < best_dist => {
                best = Some((candidate, dist));
                tied = false;
            }
            Some((_, best_dist)) if dist == best_dist => {
                tied = true;
            }
            None => {
                best = Some((candidate, dist));
                tied = false;
            }
            _ => {}
        }
    }
    if tied {
        None
    } else {
        best.map(|(candidate, _)| candidate)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rcmake_model::CacheEntry;

    use super::*;

    fn entry(ty: CacheType, value: &str) -> CacheEntry {
        CacheEntry {
            value: value.into(),
            ty,
            help: BString::default(),
            advanced: false,
            props: rcmake_model::PropertyMap::default(),
        }
    }

    #[test]
    fn unique_closest_match_suggests_known_cmake_and_project_vars() {
        let mut cache = Cache::default();
        cache.insert("ENABLE_WIDGETS".into(), entry(CacheType::Bool, "ON"));
        cache.insert("SECRET_INTERNAL".into(), entry(CacheType::Internal, "1"));

        let hints = format_typo_hints(
            &[
                "CMAKE_BUILD_TPYE".into(),
                "BUILD_TESTNG".into(),
                "ENABLE_WIDGTS".into(),
                "SECRET_INTERNL".into(),
                "COMPLETELY_UNRELATED".into(),
            ],
            &cache,
        );
        assert_eq!(
            hints,
            "hint: CMAKE_BUILD_TPYE is unused. Did you mean CMAKE_BUILD_TYPE?\n\
             hint: BUILD_TESTNG is unused. Did you mean BUILD_TESTING?\n\
             hint: ENABLE_WIDGTS is unused. Did you mean ENABLE_WIDGETS?\n"
        );
    }

    #[test]
    fn ties_and_short_names_emit_no_hints() {
        let mut cache = Cache::default();
        cache.insert("MY_OPT_A".into(), entry(CacheType::Bool, "ON"));
        cache.insert("MY_OPT_B".into(), entry(CacheType::Bool, "ON"));
        cache.insert("AB".into(), entry(CacheType::String, "1"));

        let hints = format_typo_hints(&["MY_OPT_C".into(), "AC".into(), "X".into()], &cache);
        assert!(hints.is_empty(), "{hints}");
    }

    #[test]
    fn unused_cli_variables_never_suggest_each_other() {
        let mut cache = Cache::default();
        cache.insert("FOO_BAR".into(), entry(CacheType::Uninitialized, "1"));
        cache.insert("FOO_BAZ".into(), entry(CacheType::Uninitialized, "1"));

        let hints = format_typo_hints(&["FOO_BAR".into(), "FOO_BAZ".into()], &cache);
        assert!(hints.is_empty(), "{hints}");
    }

    #[test]
    fn native_emits_typo_hint_while_compat_and_oracle_stay_byte_identical() {
        let project = b"cmake_minimum_required(VERSION 3.20)\nproject(HintParity NONE)\noption(ENABLE_LOGGING \"Enable logging\" OFF)\n";

        let run_case = |argv0: &str, oracle: bool, no_hints: bool| -> (i32, BString, BString) {
            let host = crate::driver_tests::project_host();
            host.seed_file(Path::new("/work/CMakeLists.txt"), project)
                .unwrap();
            if oracle {
                host.env_set(b"RCMAKE_ORACLE_MODE", Some(b"1"));
            }
            if no_hints {
                host.env_set(b"RCMAKE_NO_HINTS", Some(b"1"));
            }
            let argv = [
                argv0,
                "-S",
                ".",
                "-B",
                "out",
                "-G",
                "Ninja",
                "-DCMAKE_BUILD_TPYE=Release",
                "-DBUILD_TESTNG=OFF",
                "-DENABLE_LOGGNG=ON",
            ]
            .map(BString::from);
            let code = crate::run(&argv, host.clone());
            (
                code,
                host.stream_bytes(Stream::Stdout),
                host.stream_bytes(Stream::Stderr),
            )
        };

        let (compat_code, _, compat_err) = run_case("cmake", false, false);
        let (oracle_code, _, oracle_err) = run_case("rcmake", true, false);
        let (no_hints_code, _, no_hints_err) = run_case("rcmake", false, true);
        let (native_code, _, native_err) = run_case("rcmake", false, false);

        assert_eq!(compat_code, 0, "{compat_err}");
        assert_eq!(oracle_code, 0, "{oracle_err}");
        assert_eq!(no_hints_code, 0, "{no_hints_err}");
        assert_eq!(native_code, 0, "{native_err}");

        // Compat, oracle mode, and RCMAKE_NO_HINTS=1 stay 100% byte-identical with zero hints.
        assert_eq!(compat_err, oracle_err);
        assert_eq!(compat_err, no_hints_err);
        let compat_text = compat_err.to_string();
        assert!(
            compat_text.contains("Manually-specified variables were not used by the project:"),
            "{compat_text}"
        );
        assert!(!compat_text.contains("hint:"), "{compat_text}");

        // Native keeps the exact CMake warning prefix and appends the three typo hints.
        assert!(
            native_err.starts_with(compat_err.as_slice()),
            "native stderr must preserve exact warning prefix:\ncompat:\n{compat_err}\nnative:\n{native_err}"
        );
        let suffix = String::from_utf8_lossy(&native_err[compat_err.len()..]);
        assert!(
            suffix.contains("hint: CMAKE_BUILD_TPYE is unused. Did you mean CMAKE_BUILD_TYPE?\n"),
            "{suffix}"
        );
        assert!(
            suffix.contains("hint: BUILD_TESTNG is unused. Did you mean BUILD_TESTING?\n"),
            "{suffix}"
        );
        assert!(
            suffix.contains("hint: ENABLE_LOGGNG is unused. Did you mean ENABLE_LOGGING?\n"),
            "{suffix}"
        );
    }

    #[test]
    fn compat_debug_find_native_list_cache_and_no_warn_unused_cli_emit_no_hints() {
        let project = b"cmake_minimum_required(VERSION 3.20)\nproject(HintGate NONE)\noption(ENABLE_LOGGING \"Enable logging\" OFF)\n";

        // 1. Compat + --debug-find must never emit hints.
        let compat_host = crate::driver_tests::project_host();
        compat_host
            .seed_file(Path::new("/work/CMakeLists.txt"), project)
            .unwrap();
        let compat_args = [
            "cmake",
            "-S",
            ".",
            "-B",
            "out",
            "-G",
            "Ninja",
            "--debug-find",
            "-DCMAKE_BUILD_TPYE=Release",
        ]
        .map(BString::from);
        assert_eq!(crate::run(&compat_args, compat_host.clone()), 0);
        let compat_err = compat_host.stream_bytes(Stream::Stderr).to_string();
        assert!(
            compat_err.contains("Manually-specified variables were not used by the project:"),
            "{compat_err}"
        );
        assert!(!compat_err.contains("hint:"), "{compat_err}");

        // 2. Native + -L must never emit hints.
        let list_host = crate::driver_tests::project_host();
        list_host
            .seed_file(Path::new("/work/CMakeLists.txt"), project)
            .unwrap();
        let list_args = [
            "rcmake",
            "-S",
            ".",
            "-B",
            "out",
            "-G",
            "Ninja",
            "-L",
            "-DCMAKE_BUILD_TPYE=Release",
        ]
        .map(BString::from);
        assert_eq!(crate::run(&list_args, list_host.clone()), 0);
        let list_err = list_host.stream_bytes(Stream::Stderr).to_string();
        assert!(
            list_err.contains("Manually-specified variables were not used by the project:"),
            "{list_err}"
        );
        assert!(!list_err.contains("hint:"), "{list_err}");

        // 3. Native + --no-warn-unused-cli emits neither warning nor hints.
        let suppressed_host = crate::driver_tests::project_host();
        suppressed_host
            .seed_file(Path::new("/work/CMakeLists.txt"), project)
            .unwrap();
        let suppressed_args = [
            "rcmake",
            "-S",
            ".",
            "-B",
            "out",
            "-G",
            "Ninja",
            "--no-warn-unused-cli",
            "-DCMAKE_BUILD_TPYE=Release",
        ]
        .map(BString::from);
        assert_eq!(crate::run(&suppressed_args, suppressed_host.clone()), 0);
        let suppressed_err = suppressed_host.stream_bytes(Stream::Stderr).to_string();
        assert!(
            !suppressed_err.contains("Manually-specified variables were not used"),
            "{suppressed_err}"
        );
        assert!(!suppressed_err.contains("hint:"), "{suppressed_err}");
    }
}
