//! Native `rcmake fmt` subcommand.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host, ReadRange, Stream};
use rcmake_language_tools::config::ConfigCache;
use rcmake_language_tools::discovery::{
    DiscoveryLimits, FileStamp, ProjectRootCache, discover_files_with_config, infer_project_root,
    resolve_operand, write_atomic_checked,
};
use rcmake_language_tools::fmt::{
    DEFAULT_MAX_DIFF_BYTES, FormatLimits, FormatStyle, format_bytes, unified_diff,
};

const MAX_SHARED_READ_BYTES: usize = 256 * 1024 * 1024;
const MAX_WORKERS: usize = 2;

const USAGE: &str = "\
Usage: rcmake fmt [OPTIONS] [PATHS...]

Losslessly format CMakeLists.txt and *.cmake files.

Options:
  --check                  Report files that would change without writing (exit 1 if changes needed)
  --diff                   Print a unified diff without writing (exit 1 if changes needed)
  --stdin                  Read CMake input from standard input and write formatted bytes to stdout
  --stdin-filepath <path>  Virtual path for --stdin (used to locate .rcmake.toml and label diffs)
  -h, --help               Show this help message

Configuration:
  Nearest ancestor .rcmake.toml (up to project root) configures [fmt] settings
  (indent_width, line_width, command_case, end_of_line) and top-level exclude = [...].

Exit codes:
  0  All files already formatted (or formatted cleanly in place)
  1  One or more files need formatting (--check or --diff)
  2  Invalid CLI usage, parse/configuration error, or I/O failure
";

#[derive(Default)]
struct FmtCliArgs {
    check: bool,
    diff: bool,
    stdin: bool,
    stdin_filepath: Option<PathBuf>,
    help: bool,
    operands: Vec<PathBuf>,
}

fn parse_args(args: &[BString]) -> Result<FmtCliArgs, String> {
    let mut parsed = FmtCliArgs::default();
    let mut positional_only = false;
    let mut index = 0usize;
    while let Some(arg) = args.get(index) {
        index += 1;
        let bytes = arg.as_slice();
        if !positional_only && bytes == b"--" {
            positional_only = true;
            continue;
        }
        if !positional_only && bytes.starts_with(b"-") {
            if let Some(val) = bytes.strip_prefix(b"--stdin-filepath=") {
                if val.is_empty() {
                    return Err("rcmake fmt: --stdin-filepath requires a non-empty path.\n".into());
                }
                parsed.stdin_filepath = Some(rcmake_host::path::to_native(val));
                continue;
            }
            match bytes {
                b"--check" => parsed.check = true,
                b"--diff" => parsed.diff = true,
                b"--stdin" => parsed.stdin = true,
                b"--stdin-filepath" => {
                    let Some(next) = args.get(index) else {
                        return Err(
                            "rcmake fmt: --stdin-filepath requires a path argument.\n".into()
                        );
                    };
                    if next.is_empty() {
                        return Err(
                            "rcmake fmt: --stdin-filepath requires a non-empty path.\n".into()
                        );
                    }
                    parsed.stdin_filepath = Some(rcmake_host::path::to_native(next.as_slice()));
                    index += 1;
                }
                b"-h" | b"--help" => parsed.help = true,
                _ => {
                    return Err(format!(
                        "rcmake fmt: unknown option '{}'. Run 'rcmake fmt --help' for usage.\n",
                        String::from_utf8_lossy(bytes)
                    ));
                }
            }
            continue;
        }
        parsed.operands.push(rcmake_host::path::to_native(bytes));
    }
    if parsed.stdin && !parsed.operands.is_empty() {
        return Err(
            "rcmake fmt: --stdin cannot be combined with file or directory paths.\n".into(),
        );
    }
    if parsed.stdin_filepath.is_some() && !parsed.stdin {
        return Err("rcmake fmt: --stdin-filepath requires --stdin.\n".into());
    }
    Ok(parsed)
}

/// Execute `rcmake fmt`.
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
    let limits = FormatLimits::default();

    if parsed.stdin {
        return run_stdin(host.as_ref(), &cwd, &parsed, &limits);
    }

    let mut cfg_cache = ConfigCache::default();
    let mut root_cache = ProjectRootCache::default();
    let files = match discover_files_with_config(
        host.as_ref(),
        &cwd,
        &parsed.operands,
        &DiscoveryLimits::default(),
        &mut cfg_cache,
        &mut root_cache,
    ) {
        Ok(files) => files,
        Err(err) => {
            let msg = format!("rcmake fmt: {}: {}\n", err.path.display(), err.message);
            host.write_out(Stream::Stderr, msg.as_bytes());
            return 2;
        }
    };

    if files.is_empty() {
        return 0;
    }

    // Resolve and cache `.rcmake.toml` per parent directory up front.
    let mut tasks: Vec<(PathBuf, Result<FormatStyle, String>)> = Vec::with_capacity(files.len());
    for path in files {
        let dir = path.parent().unwrap_or(&cwd);
        let root = root_cache.infer(host.as_ref(), dir, &cwd);
        let style = cfg_cache
            .resolve(host.as_ref(), dir, Some(&root))
            .map(|resolved| FormatStyle::from_config(&resolved.config.fmt))
            .map_err(|err| format!("rcmake fmt: configuration error: {err}"));
        tasks.push((path, style));
    }

    let shared_bytes = AtomicUsize::new(0);
    let results = process_files(host.as_ref(), &tasks, &limits, &shared_bytes, &parsed);

    let mut any_changed = false;
    let mut any_error = false;
    let mut diff_budget = DEFAULT_MAX_DIFF_BYTES;
    let mut diff_truncated = false;

    for outcome in results {
        let display = display_rel(&cwd, &outcome.path);
        match outcome.status {
            FileOutcome::Unchanged => {}
            FileOutcome::Changed {
                original,
                formatted,
            } => {
                any_changed = true;
                if parsed.diff {
                    if diff_budget > 0 {
                        let rendered = unified_diff(&display, &original, &formatted, diff_budget);
                        if rendered.len() >= diff_budget {
                            diff_truncated = true;
                        }
                        diff_budget = diff_budget.saturating_sub(rendered.len());
                        host.write_out(Stream::Stdout, &rendered);
                    } else {
                        diff_truncated = true;
                    }
                } else if parsed.check {
                    let line = format!("{display}\n");
                    host.write_out(Stream::Stdout, line.as_bytes());
                }
            }
            FileOutcome::Written => {
                any_changed = true;
            }
            FileOutcome::Error(err) => {
                any_error = true;
                let line = format!("{err}\n");
                host.write_out(Stream::Stderr, line.as_bytes());
            }
        }
    }

    if diff_truncated {
        host.write_out(
            Stream::Stderr,
            b"rcmake fmt: diff output truncated after reaching the 4 MiB diff budget.\n",
        );
    }

    if any_error {
        2
    } else if (parsed.check || parsed.diff) && any_changed {
        1
    } else {
        0
    }
}

fn run_stdin(host: &dyn Host, cwd: &Path, parsed: &FmtCliArgs, limits: &FormatLimits) -> i32 {
    let (config_dir, display_label) = match &parsed.stdin_filepath {
        Some(fp) => {
            let resolved = resolve_operand(cwd, fp);
            let dir = resolved
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| cwd.to_path_buf());
            let label = fp.display().to_string();
            (dir, label)
        }
        None => (cwd.to_path_buf(), "<stdin>".to_owned()),
    };
    let root = infer_project_root(host, &config_dir, cwd);
    let style = match rcmake_language_tools::config::resolve_config(host, &config_dir, Some(&root))
    {
        Ok(resolved) => FormatStyle::from_config(&resolved.config.fmt),
        Err(err) => {
            let msg = format!("rcmake fmt: configuration error: {err}\n");
            host.write_out(Stream::Stderr, msg.as_bytes());
            return 2;
        }
    };

    let read_limit = (limits.max_input_bytes as u64).saturating_add(1);
    let input = match host.read_stdin(read_limit) {
        Ok(bytes) => bytes,
        Err(err) => {
            let msg = format!("rcmake fmt: failed to read stdin: {err}\n");
            host.write_out(Stream::Stderr, msg.as_bytes());
            return 2;
        }
    };

    let formatted = match format_bytes(&input, &style, limits) {
        Ok(res) => res,
        Err(err) => {
            let msg = format!("rcmake fmt: {display_label}: {err}\n");
            host.write_out(Stream::Stderr, msg.as_bytes());
            return 2;
        }
    };

    if parsed.diff {
        if formatted.changed() {
            let diff = unified_diff(
                &display_label,
                &input,
                &formatted.output,
                DEFAULT_MAX_DIFF_BYTES,
            );
            host.write_out(Stream::Stdout, &diff);
            if diff.len() >= DEFAULT_MAX_DIFF_BYTES {
                host.write_out(
                    Stream::Stderr,
                    b"rcmake fmt: diff output truncated after reaching the 4 MiB diff budget.\n",
                );
            }
            return 1;
        }
        return 0;
    }
    if parsed.check {
        return i32::from(formatted.changed());
    }
    host.write_out(Stream::Stdout, &formatted.output);
    0
}

enum FileOutcome {
    Unchanged,
    Changed {
        original: Vec<u8>,
        formatted: Vec<u8>,
    },
    Written,
    Error(String),
}

struct FileResult {
    path: PathBuf,
    status: FileOutcome,
}

fn process_files(
    host: &dyn Host,
    tasks: &[(PathBuf, Result<FormatStyle, String>)],
    limits: &FormatLimits,
    shared_bytes: &AtomicUsize,
    parsed: &FmtCliArgs,
) -> Vec<FileResult> {
    let write_files = !parsed.check && !parsed.diff;
    let worker_count = if tasks.len() >= 8 { MAX_WORKERS } else { 1 };
    if worker_count <= 1 {
        return tasks
            .iter()
            .map(|(path, style)| {
                process_one(
                    host,
                    path,
                    style,
                    limits,
                    shared_bytes,
                    write_files,
                    parsed.diff,
                )
            })
            .collect();
    }

    let next_index = AtomicUsize::new(0);
    let mut slots: Vec<Option<FileResult>> = (0..tasks.len()).map(|_| None).collect();
    let per_worker = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            handles.push(scope.spawn(|| {
                let mut local = Vec::new();
                loop {
                    let idx = next_index.fetch_add(1, Ordering::Relaxed);
                    let Some((path, style)) = tasks.get(idx) else {
                        break;
                    };
                    let res = process_one(
                        host,
                        path,
                        style,
                        limits,
                        shared_bytes,
                        write_files,
                        parsed.diff,
                    );
                    local.push((idx, res));
                }
                local
            }));
        }
        let mut combined = Vec::new();
        for handle in handles {
            if let Ok(part) = handle.join() {
                combined.extend(part);
            }
        }
        combined
    });

    for (idx, res) in per_worker {
        if let Some(slot) = slots.get_mut(idx) {
            *slot = Some(res);
        }
    }
    slots.into_iter().flatten().collect()
}

fn process_one(
    host: &dyn Host,
    path: &Path,
    style: &Result<FormatStyle, String>,
    limits: &FormatLimits,
    shared_bytes: &AtomicUsize,
    write_file: bool,
    keep_bytes: bool,
) -> FileResult {
    let style = match style {
        Ok(style) => style,
        Err(err) => {
            return FileResult {
                path: path.to_owned(),
                status: FileOutcome::Error(err.clone()),
            };
        }
    };

    // Check link identity without following symlinks first. In-place writes refuse symbolic link
    // operands so links are never silently replaced with regular files; read-only modes
    // (`--check` and `--diff`) follow explicit symlink operands to inspect their targets.
    let link_meta = match host.metadata_fresh(path, Follow::No) {
        Ok(meta) => meta,
        Err(err) => {
            return FileResult {
                path: path.to_owned(),
                status: FileOutcome::Error(format!(
                    "rcmake fmt: {}: cannot stat file: {err}",
                    path.display()
                )),
            };
        }
    };
    let meta = match link_meta.kind {
        FileKind::Symlink if write_file => {
            return FileResult {
                path: path.to_owned(),
                status: FileOutcome::Error(format!(
                    "rcmake fmt: {}: refusing to overwrite symbolic link in place; format the target file directly.",
                    path.display()
                )),
            };
        }
        FileKind::Symlink => match host.metadata_fresh(path, Follow::Yes) {
            Ok(target_meta) if target_meta.kind == FileKind::File => target_meta,
            Ok(_) => {
                return FileResult {
                    path: path.to_owned(),
                    status: FileOutcome::Error(format!(
                        "rcmake fmt: {}: symbolic link target is not a regular file.",
                        path.display()
                    )),
                };
            }
            Err(err) => {
                return FileResult {
                    path: path.to_owned(),
                    status: FileOutcome::Error(format!(
                        "rcmake fmt: {}: cannot stat symbolic link target: {err}",
                        path.display()
                    )),
                };
            }
        },
        FileKind::File => link_meta,
        _ => {
            return FileResult {
                path: path.to_owned(),
                status: FileOutcome::Error(format!(
                    "rcmake fmt: {}: path is not a regular file.",
                    path.display()
                )),
            };
        }
    };
    if meta.len > limits.max_input_bytes as u64 {
        return FileResult {
            path: path.to_owned(),
            status: FileOutcome::Error(format!(
                "rcmake fmt: {}: file exceeds the {} byte limit.",
                path.display(),
                limits.max_input_bytes
            )),
        };
    }

    let stamp = FileStamp::from_metadata(&meta);
    let read_size = usize::try_from(meta.len).unwrap_or(usize::MAX);
    let prior_total = shared_bytes.fetch_add(read_size, Ordering::Relaxed);
    if prior_total.saturating_add(read_size) > MAX_SHARED_READ_BYTES {
        return FileResult {
            path: path.to_owned(),
            status: FileOutcome::Error(format!(
                "rcmake fmt: {}: shared formatting byte budget ({MAX_SHARED_READ_BYTES} bytes) exceeded.",
                path.display()
            )),
        };
    }

    let original = match host.read(
        path,
        ReadRange::All {
            limit: limits.max_input_bytes as u64,
        },
    ) {
        Ok(bytes) => bytes,
        Err(err) => {
            return FileResult {
                path: path.to_owned(),
                status: FileOutcome::Error(format!(
                    "rcmake fmt: {}: failed to read file: {err}",
                    path.display()
                )),
            };
        }
    };

    if write_file {
        let after_read = match host.metadata_fresh(path, Follow::No) {
            Ok(meta) => meta,
            Err(err) => {
                return FileResult {
                    path: path.to_owned(),
                    status: FileOutcome::Error(format!(
                        "rcmake fmt: {}: cannot re-stat file after read: {err}",
                        path.display()
                    )),
                };
            }
        };
        if after_read.kind == FileKind::Symlink {
            return FileResult {
                path: path.to_owned(),
                status: FileOutcome::Error(format!(
                    "rcmake fmt: {}: refusing to overwrite symbolic link in place; format the target file directly.",
                    path.display()
                )),
            };
        }
        if after_read.kind != FileKind::File
            || !stamp.matches_identity(&FileStamp::from_metadata(&after_read))
        {
            return FileResult {
                path: path.to_owned(),
                status: FileOutcome::Error(format!(
                    "rcmake fmt: Refusing to overwrite {}: file changed on disk during read.",
                    path.display()
                )),
            };
        }
    }

    let result = match format_bytes(&original, style, limits) {
        Ok(res) => res,
        Err(err) => {
            return FileResult {
                path: path.to_owned(),
                status: FileOutcome::Error(format!("rcmake fmt: {}: {err}", path.display())),
            };
        }
    };

    if !result.changed() {
        return FileResult {
            path: path.to_owned(),
            status: FileOutcome::Unchanged,
        };
    }

    if write_file {
        match write_atomic_checked(host, path, &stamp, &result.output) {
            Ok(()) => FileResult {
                path: path.to_owned(),
                status: FileOutcome::Written,
            },
            Err(err) => FileResult {
                path: path.to_owned(),
                status: FileOutcome::Error(format!("rcmake fmt: {err}")),
            },
        }
    } else if keep_bytes {
        FileResult {
            path: path.to_owned(),
            status: FileOutcome::Changed {
                original,
                formatted: result.output,
            },
        }
    } else {
        FileResult {
            path: path.to_owned(),
            status: FileOutcome::Changed {
                original: Vec::new(),
                formatted: Vec::new(),
            },
        }
    }
}

fn display_rel(cwd: &Path, path: &Path) -> String {
    let shown = path.strip_prefix(cwd).unwrap_or(path);
    shown.display().to_string()
}

#[cfg(test)]
mod tests {
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
    fn fmt_check_diff_write_and_stdin_exit_codes() {
        let work = Path::new("/work");
        let host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        let file = work.join("CMakeLists.txt");
        host.seed_file(&file, b"if(ON)\nset(A   1)\nendif()\n")
            .unwrap();
        host.seed_file(&work.join(".rcmake.toml"), b"[fmt]\nindent_width = 4\n")
            .unwrap();

        // --check reports changed path and exits 1 without modifying disk.
        let code = run(
            None,
            &bargs(&["--check"]),
            Arc::clone(&host) as Arc<dyn Host>,
        );
        assert_eq!(code, 1);
        assert!(stdout_str(&host).contains("CMakeLists.txt"));
        assert_eq!(
            host.read(&file, ReadRange::All { limit: 1024 }).unwrap(),
            b"if(ON)\nset(A   1)\nendif()\n"
        );

        // --diff emits unified diff and exits 1 without modifying disk.
        let host2 = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        host2
            .seed_file(&file, b"if(ON)\nset(A   1)\nendif()\n")
            .unwrap();
        host2
            .seed_file(&work.join(".rcmake.toml"), b"[fmt]\nindent_width = 4\n")
            .unwrap();
        let code = run(
            None,
            &bargs(&["--check", "--diff"]),
            Arc::clone(&host2) as Arc<dyn Host>,
        );
        assert_eq!(code, 1);
        assert!(stdout_str(&host2).contains("+    set(A 1)"));

        // Normal mode formats in-place and exits 0; subsequent --check exits 0.
        let code = run(None, &[], Arc::clone(&host2) as Arc<dyn Host>);
        assert_eq!(code, 0);
        assert_eq!(
            host2.read(&file, ReadRange::All { limit: 1024 }).unwrap(),
            b"if(ON)\n    set(A 1)\nendif()\n"
        );
        assert_eq!(
            run(
                None,
                &bargs(&["--check"]),
                Arc::clone(&host2) as Arc<dyn Host>
            ),
            0
        );

        // --stdin + path is rejected with exit 2.
        assert_eq!(
            run(
                None,
                &bargs(&["--stdin", "CMakeLists.txt"]),
                Arc::clone(&host2) as Arc<dyn Host>
            ),
            2
        );

        // --stdin formats from stdin to stdout using cwd's .rcmake.toml.
        let stdin_host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        stdin_host
            .seed_file(
                &work.join(".rcmake.toml"),
                b"[fmt]\ncommand_case = \"lower\"\n",
            )
            .unwrap();
        stdin_host
            .seed_stdin(b"IF(ON)\nSET(X 1)\nENDIF()\n")
            .unwrap();
        let code = run(
            None,
            &bargs(&["--stdin"]),
            Arc::clone(&stdin_host) as Arc<dyn Host>,
        );
        assert_eq!(code, 0);
        assert_eq!(stdout_str(&stdin_host), "if(ON)\n  set(X 1)\nendif()\n");
    }

    #[test]
    fn fmt_rejects_parse_error_with_exit_2_without_writing() {
        let work = Path::new("/work");
        let host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        let file = work.join("CMakeLists.txt");
        host.seed_file(&file, b"if(ON)\nmessage(unclosed\n")
            .unwrap();

        let code = run(None, &[], Arc::clone(&host) as Arc<dyn Host>);
        assert_eq!(code, 2);
        assert!(!stderr_str(&host).is_empty());
        assert_eq!(
            host.read(&file, ReadRange::All { limit: 1024 }).unwrap(),
            b"if(ON)\nmessage(unclosed\n"
        );
    }

    #[test]
    fn fmt_dispatches_via_native_persona_and_preserves_directory_operand() {
        let work = Path::new("/work");
        // Native `rcmake fmt --help` dispatches to fmt_cmd.
        let native_host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        let code = crate::run(
            &bargs(&["rcmake", "fmt", "--help"]),
            Arc::clone(&native_host) as Arc<dyn Host>,
        );
        assert_eq!(code, 0);
        assert!(stdout_str(&native_host).contains("Usage: rcmake fmt"));

        // Compatibility `cmake fmt --help` does NOT invoke native fmt_cmd.
        let compat_host = Arc::new(PureHost::new(work.into(), work.join("bin/cmake")));
        let _ = crate::run(
            &bargs(&["cmake", "fmt", "--help"]),
            Arc::clone(&compat_host) as Arc<dyn Host>,
        );
        assert!(!stdout_str(&compat_host).contains("Usage: rcmake fmt"));
    }

    #[test]
    fn fmt_skips_recursive_symlinks_and_refuses_inplace_overwrite_of_direct_symlink() {
        let work = Path::new("/work");
        let host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        let real_target = Path::new("/outside/target.cmake");
        let link = work.join("linked.cmake");
        let unformatted = b"if(ON)\nset(A   1)\nendif()\n";
        host.seed_file(real_target, unformatted).unwrap();
        host.seed_file(&work.join("CMakeLists.txt"), b"project(Clean)\n")
            .unwrap();
        host.symlink(real_target, &link).unwrap();

        // Recursive discovery skips file symlinks; neither link nor target is modified.
        let code = run(None, &[], Arc::clone(&host) as Arc<dyn Host>);
        assert_eq!(code, 0);
        assert_eq!(
            host.metadata_fresh(&link, Follow::No).unwrap().kind,
            FileKind::Symlink
        );
        assert_eq!(
            host.read(real_target, ReadRange::All { limit: 1024 })
                .unwrap(),
            unformatted
        );

        // Direct symlink operand works cleanly in read-only --check and --diff modes.
        let check_host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        check_host.seed_file(real_target, unformatted).unwrap();
        check_host.symlink(real_target, &link).unwrap();
        let check_code = run(
            None,
            &bargs(&["--check", "linked.cmake"]),
            Arc::clone(&check_host) as Arc<dyn Host>,
        );
        assert_eq!(check_code, 1);
        assert!(stdout_str(&check_host).contains("linked.cmake"));

        let diff_host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        diff_host.seed_file(real_target, unformatted).unwrap();
        diff_host.symlink(real_target, &link).unwrap();
        let diff_code = run(
            None,
            &bargs(&["--diff", "linked.cmake"]),
            Arc::clone(&diff_host) as Arc<dyn Host>,
        );
        assert_eq!(diff_code, 1);
        assert!(stdout_str(&diff_host).contains("+  set(A 1)"));

        // Direct symlink operand in in-place write mode is refused with exit 2 and preserves link.
        let write_host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        write_host.seed_file(real_target, unformatted).unwrap();
        write_host.symlink(real_target, &link).unwrap();
        let write_code = run(
            None,
            &bargs(&["linked.cmake"]),
            Arc::clone(&write_host) as Arc<dyn Host>,
        );
        assert_eq!(write_code, 2);
        assert!(
            stderr_str(&write_host).contains("symbolic link"),
            "stderr={}",
            stderr_str(&write_host)
        );
        assert_eq!(
            write_host.metadata_fresh(&link, Follow::No).unwrap().kind,
            FileKind::Symlink
        );
        assert_eq!(
            write_host
                .read(real_target, ReadRange::All { limit: 1024 })
                .unwrap(),
            unformatted
        );
    }

    #[test]
    fn fmt_stdin_filepath_resolves_config_and_labels_diff_and_exclude_skips_files() {
        let work = Path::new("/work");
        let host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        host.seed_file(&work.join("sub/.rcmake.toml"), b"[fmt]\nindent_width = 4\n")
            .unwrap();
        host.seed_stdin(b"if(ON)\nset(X 1)\nendif()\n").unwrap();
        let code = run(
            None,
            &bargs(&[
                "--stdin",
                "--stdin-filepath",
                "sub/CMakeLists.txt",
                "--diff",
            ]),
            Arc::clone(&host) as Arc<dyn Host>,
        );
        assert_eq!(code, 1);
        let out = stdout_str(&host);
        assert!(out.contains("sub/CMakeLists.txt"), "{out}");
        assert!(out.contains("+    set(X 1)"), "{out}");

        // --stdin-filepath without --stdin fails with exit 2.
        let bad_host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        assert_eq!(
            run(
                None,
                &bargs(&["--stdin-filepath=sub/CMakeLists.txt"]),
                Arc::clone(&bad_host) as Arc<dyn Host>,
            ),
            2
        );

        // .rcmake.toml exclude skips matching files during directory formatting.
        let excl_host = Arc::new(PureHost::new(work.into(), work.join("bin/rcmake")));
        excl_host
            .seed_file(&work.join(".rcmake.toml"), b"exclude = [\"vendor/**\"]\n")
            .unwrap();
        excl_host
            .seed_file(&work.join("CMakeLists.txt"), b"project(Clean)\n")
            .unwrap();
        excl_host
            .seed_file(
                &work.join("vendor/third.cmake"),
                b"if(ON)\nset(UNFORMATTED   1)\nendif()\n",
            )
            .unwrap();
        let excl_code = run(
            None,
            &bargs(&["--check"]),
            Arc::clone(&excl_host) as Arc<dyn Host>,
        );
        assert_eq!(excl_code, 0, "stdout={}", stdout_str(&excl_host));
    }
}
