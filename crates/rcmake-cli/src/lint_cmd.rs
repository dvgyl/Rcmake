//! Native `rcmake lint` subcommand.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Host, ReadRange, Stream, discover_cmake_root};
use rcmake_language_tools::config::ConfigCache;
use rcmake_language_tools::discovery::{discover_files, infer_project_root};
use rcmake_language_tools::lint::{
    Finding, HostWorkspaceReader, LintLimits, Severity, lint_bytes, render_terminal,
    sort_and_dedup_findings, to_sarif_document,
};

const USAGE: &str = "\
Usage: rcmake lint [OPTIONS] [PATHS...]

Run static lint rules on CMakeLists.txt and *.cmake files.

Options:
  --sarif       Emit SARIF 2.1.0 JSON to standard output
  -h, --help    Show this help message
";

#[derive(Default)]
struct LintCliArgs {
    sarif: bool,
    help: bool,
    operands: Vec<PathBuf>,
}

fn parse_args(args: &[BString]) -> Result<LintCliArgs, String> {
    let mut parsed = LintCliArgs::default();
    let mut positional_only = false;
    for arg in args {
        let bytes = arg.as_slice();
        if !positional_only && bytes == b"--" {
            positional_only = true;
            continue;
        }
        if !positional_only && bytes.starts_with(b"-") {
            match bytes {
                b"--sarif" => parsed.sarif = true,
                b"-h" | b"--help" => parsed.help = true,
                _ => {
                    return Err(format!(
                        "rcmake lint: unknown option '{}'. Run 'rcmake lint --help' for usage.\n",
                        String::from_utf8_lossy(bytes)
                    ));
                }
            }
            continue;
        }
        parsed.operands.push(rcmake_host::path::to_native(bytes));
    }
    Ok(parsed)
}

/// Execute `rcmake lint`.
pub(crate) fn run(_argv0: Option<&BString>, args: &[BString], host: Arc<dyn Host>) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(msg) => {
            host.write_out(Stream::Stderr, msg.as_bytes());
            return 2;
        }
    };
    if parsed.help {
        host.write_out(Stream::Stdout, USAGE.as_bytes());
        return 0;
    }

    let cwd = host.initial_cwd();
    let files = match discover_files(host.as_ref(), &cwd, &parsed.operands) {
        Ok(files) => files,
        Err(err) => {
            let msg = format!("rcmake lint: {}: {}\n", err.path.display(), err.message);
            host.write_out(Stream::Stderr, msg.as_bytes());
            return 2;
        }
    };

    let bundled_modules = discover_cmake_root(host.as_ref(), None)
        .1
        .map(|root| root.join("Modules"));
    let limits = LintLimits::default();

    let mut cfg_cache = ConfigCache::default();
    let mut all_findings: Vec<Finding> = Vec::new();
    let mut any_tool_error = false;

    // Precompute readable roots so project-local static includes work across all operands.
    let mut roots_set = BTreeSet::new();
    roots_set.insert(infer_project_root(host.as_ref(), &cwd, &cwd));
    let mut file_roots: Vec<PathBuf> = Vec::with_capacity(files.len());
    for path in &files {
        let dir = path.parent().unwrap_or(&cwd);
        let root = infer_project_root(host.as_ref(), dir, &cwd);
        roots_set.insert(root.clone());
        file_roots.push(root);
    }

    let reader = HostWorkspaceReader::new(
        host.as_ref(),
        roots_set.into_iter().collect(),
        bundled_modules,
    );

    for (path, root) in files.iter().zip(&file_roots) {
        let dir = path.parent().unwrap_or(&cwd);
        let resolved_cfg = match cfg_cache.resolve(host.as_ref(), dir, Some(root)) {
            Ok(cfg) => cfg,
            Err(err) => {
                any_tool_error = true;
                let msg = format!("rcmake lint: configuration error: {err}\n");
                host.write_out(Stream::Stderr, msg.as_bytes());
                continue;
            }
        };

        let bytes = match host.read(
            path,
            ReadRange::All {
                limit: limits.max_input_bytes as u64,
            },
        ) {
            Ok(bytes) => bytes,
            Err(err) => {
                any_tool_error = true;
                let msg = format!(
                    "rcmake lint: {}: failed to read file: {err}\n",
                    path.display()
                );
                host.write_out(Stream::Stderr, msg.as_bytes());
                continue;
            }
        };

        match lint_bytes(
            path,
            &bytes,
            &resolved_cfg.config.lint,
            &limits,
            &reader,
            Some(root),
        ) {
            Ok(result) => {
                all_findings.extend(result.findings);
            }
            Err(err) => {
                any_tool_error = true;
                let msg = format!("rcmake lint: {}: {err}\n", path.display());
                host.write_out(Stream::Stderr, msg.as_bytes());
            }
        }
    }

    sort_and_dedup_findings(&mut all_findings);

    if parsed.sarif {
        let doc = to_sarif_document(&all_findings);
        let mut json = rcmake_json::write_styled(&doc, "  ");
        json.push(b'\n');
        host.write_out(Stream::Stdout, json.as_slice());
    } else {
        let rendered = render_terminal(&all_findings, Some(&cwd));
        if !rendered.is_empty() {
            host.write_out(Stream::Stdout, rendered.as_bytes());
        }
    }

    if any_tool_error {
        2
    } else if all_findings.iter().any(|f| f.severity == Severity::Error) {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rcmake_host::PureHost;

    use super::*;

    fn bargs(items: &[&str]) -> Vec<BString> {
        items.iter().map(|s| BString::from(*s)).collect()
    }

    fn stdout_str(host: &PureHost) -> String {
        String::from_utf8_lossy(host.stream_bytes(Stream::Stdout).as_slice()).into_owned()
    }

    fn stderr_str(host: &PureHost) -> String {
        String::from_utf8_lossy(host.stream_bytes(Stream::Stderr).as_slice()).into_owned()
    }

    #[test]
    fn lint_terminal_and_sarif_emit_identical_sorted_findings_and_honor_exit_codes() {
        let work = Path::new("/work");
        let host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        host.seed_file(
            &work.join("CMakeLists.txt"),
            b"project(Demo)\nset(CMAKE_BUILD_TYPE Release)\ninclude_directories(inc)\n",
        )
        .unwrap();
        host.seed_file(
            &work.join(".rcmake.toml"),
            b"[lint]\ndisable = [\"directory-options\"]\n[lint.severity]\nunguarded-build-type = \"error\"\n",
        )
        .unwrap();

        let code = run(None, &[], Arc::clone(&host) as Arc<dyn Host>);
        assert_eq!(code, 1);
        let terminal = stdout_str(&host);
        assert!(terminal.contains("error[unguarded-build-type]"));
        assert!(!terminal.contains("directory-options"));

        let sarif_host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        sarif_host
            .seed_file(
                &work.join("CMakeLists.txt"),
                b"project(Demo)\nset(CMAKE_BUILD_TYPE Release)\ninclude_directories(inc)\n",
            )
            .unwrap();
        sarif_host
            .seed_file(
                &work.join(".rcmake.toml"),
                b"[lint]\ndisable = [\"directory-options\"]\n[lint.severity]\nunguarded-build-type = \"error\"\n",
            )
            .unwrap();
        let code = run(
            None,
            &bargs(&["--sarif"]),
            Arc::clone(&sarif_host) as Arc<dyn Host>,
        );
        assert_eq!(code, 1);
        let sarif = stdout_str(&sarif_host);
        let parsed =
            rcmake_json::parse(sarif.as_bytes(), &rcmake_json::ReadOptions::default()).unwrap();
        let compact = rcmake_json::write_compact(&parsed).to_string();
        assert!(compact.contains(r#""version":"2.1.0""#), "{compact}");
        assert!(
            compact.contains(r#""ruleId":"unguarded-build-type""#),
            "{compact}"
        );
        assert!(compact.contains(r#""level":"error""#), "{compact}");
        assert!(
            !compact.contains(r#""ruleId":"directory-options""#),
            "{compact}"
        );
    }

    #[test]
    fn lint_sarif_emits_valid_document_when_clean_and_exits_0() {
        let work = Path::new("/work");
        let host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        host.seed_file(
            &work.join("CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 3.28)\nproject(Clean)\n",
        )
        .unwrap();
        let code = run(
            None,
            &bargs(&["--sarif"]),
            Arc::clone(&host) as Arc<dyn Host>,
        );
        assert_eq!(code, 0);
        let sarif = stdout_str(&host);
        let parsed =
            rcmake_json::parse(sarif.as_bytes(), &rcmake_json::ReadOptions::default()).unwrap();
        let compact = rcmake_json::write_compact(&parsed).to_string();
        assert!(compact.contains(r#""version":"2.1.0""#), "{compact}");
        assert!(compact.contains(r#""results":[]"#), "{compact}");
    }

    #[test]
    fn lint_exits_2_on_invalid_config() {
        let work = Path::new("/work");
        let host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        host.seed_file(&work.join("CMakeLists.txt"), b"project(Clean)\n")
            .unwrap();
        host.seed_file(
            &work.join(".rcmake.toml"),
            b"[lint]\ndisable = [\"nope\"]\n",
        )
        .unwrap();
        let code = run(None, &[], Arc::clone(&host) as Arc<dyn Host>);
        assert_eq!(code, 2);
        assert!(stderr_str(&host).contains("Unknown lint rule"));
    }
}
