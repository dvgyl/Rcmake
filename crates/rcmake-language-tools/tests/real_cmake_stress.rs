//! Read-only real-world stress test of `rcmake fmt` and `rcmake lint` against
//! CMake 4.5.1's own `CMakeLists.txt` files and vendored `Modules/*.cmake`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

use rcmake_host::{FileKind, Follow, Host, NativeHost, ReadRange};
use rcmake_language_tools::{
    CommandCase, EndOfLine, FormatLimits, FormatStyle, HostWorkspaceReader, LintConfig, LintLimits,
    format_bytes, lint_bytes, to_sarif_document,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn collect_cmake_files(host: &dyn Host, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(mut entries) = host.read_dir(dir) else {
        return;
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        if entry.name.starts_with(b"._") {
            continue;
        }
        let path = dir.join(rcmake_host::path::to_native(&entry.name));
        match entry.kind {
            FileKind::Dir => collect_cmake_files(host, &path, out),
            FileKind::File
                if entry.name.as_slice() == b"CMakeLists.txt"
                    || entry.name.as_slice().ends_with(b".cmake") =>
            {
                out.push(path);
            }
            _ => {}
        }
    }
}

#[test]
fn stress_format_idempotency_and_lint_on_real_cmake_4_5_1_corpus() {
    let host = NativeHost::new().expect("native host");
    let root = repo_root();
    let modules_dir = root.join("modules/cmake-4.5/Modules");

    let mut targets = Vec::new();
    collect_cmake_files(&host, &modules_dir, &mut targets);
    assert!(
        targets.len() >= 400,
        "expected vendored CMake 4.5.1 Modules/*.cmake, found {}",
        targets.len()
    );

    // Also include CMake 4.5.1's own large CMakeLists.txt files if the oracle checkout is present.
    let oracle_root = root.join("env/oracle/cmake-4.5.1");
    for rel in [
        "CMakeLists.txt",
        "Source/CMakeLists.txt",
        "Tests/CMakeLists.txt",
    ] {
        let candidate = oracle_root.join(rel);
        if host
            .metadata(&candidate, Follow::Yes)
            .is_ok_and(|m| m.kind == FileKind::File)
        {
            targets.push(candidate);
        }
    }

    let styles = [
        FormatStyle::default(),
        FormatStyle {
            command_case: CommandCase::Lower,
            end_of_line: EndOfLine::Lf,
            ..FormatStyle::default()
        },
    ];
    let fmt_limits = FormatLimits::default();
    let lint_limits = LintLimits::default();
    let reader = HostWorkspaceReader::new(
        &host,
        vec![modules_dir.clone(), oracle_root.clone()],
        Some(modules_dir),
    );

    let mut formatted_count = 0usize;
    let mut skipped_templates = 0usize;
    let mut total_bytes = 0usize;
    let mut total_findings = 0usize;

    for path in &targets {
        let bytes = host
            .read(
                path,
                ReadRange::All {
                    limit: fmt_limits.max_input_bytes as u64,
                },
            )
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        total_bytes += bytes.len();

        // Some files under Modules/ (such as FindCUDA/run_nvcc.cmake) are configure_file
        // templates containing `@VAR@` placeholders and are not valid standalone listfiles.
        let mut sources = rcmake_diag::SourceMap::default();
        let sid = sources
            .add(
                "".into(),
                rcmake_diag::source::SourceKind::File,
                std::sync::Arc::from(b"".as_slice()),
            )
            .unwrap();
        if rcmake_syntax::parse_list_file(
            &bytes,
            sid,
            rcmake_diag::source::SourceKind::File,
            &rcmake_base::Limits::default(),
        )
        .is_err()
        {
            for style in &styles {
                assert!(
                    matches!(
                        format_bytes(&bytes, style, &fmt_limits),
                        Err(rcmake_language_tools::FormatError::Parse(_))
                    ),
                    "template file {} should fail with Parse error",
                    path.display()
                );
            }
            skipped_templates += 1;
            continue;
        }

        for style in &styles {
            let pass1 = format_bytes(&bytes, style, &fmt_limits).unwrap_or_else(|e| {
                panic!(
                    "pass 1 format/verify failed on {} ({style:?}): {e}",
                    path.display()
                )
            });
            let pass2 = format_bytes(&pass1.output, style, &fmt_limits).unwrap_or_else(|e| {
                panic!(
                    "pass 2 format/verify failed on {} ({style:?}): {e}",
                    path.display()
                )
            });
            assert!(
                pass2.edits.is_empty(),
                "non-idempotent format on {} ({style:?}): {} edits on pass 2",
                path.display(),
                pass2.edits.len()
            );
            assert_eq!(pass1.output, pass2.output);
        }
        formatted_count += 1;

        let lint_res = lint_bytes(
            path,
            &bytes,
            &LintConfig::default(),
            &lint_limits,
            &reader,
            Some(&root),
        )
        .unwrap_or_else(|e| panic!("lint failed on {}: {e}", path.display()));
        total_findings += lint_res.findings.len();
    }

    // Verify SARIF serialization on a real CMakeLists.txt (even if empty).
    let sample_path = targets.last().unwrap();
    let sample_bytes = host
        .read(
            sample_path,
            ReadRange::All {
                limit: fmt_limits.max_input_bytes as u64,
            },
        )
        .unwrap();
    let sample_lint = lint_bytes(
        sample_path,
        &sample_bytes,
        &LintConfig::default(),
        &lint_limits,
        &reader,
        Some(&root),
    )
    .unwrap();
    let sarif = to_sarif_document(&sample_lint.findings);
    let sarif_json = rcmake_json::write_compact(&sarif).to_string();
    assert!(sarif_json.contains(r#""version":"2.1.0""#));
    assert!(sarif_json.contains(r#""columnKind":"unicodeCodePoints""#));

    assert!(formatted_count >= 500, "formatted_count={formatted_count}");
    assert!(
        skipped_templates < 20,
        "skipped_templates={skipped_templates}"
    );
    assert!(total_bytes > 1_000_000);
    assert!(total_findings > 0);
}
