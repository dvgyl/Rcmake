//! Deterministic `PureHost` unit tests for `rcmake-verify` covering parity, semantic
//! divergences, File API containment (`P1-09`), Ninja dry-run guards (`P1-13`), version
//! validation, and scratch isolation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{Host, HostError, ProcessResult, PureHost, RunOutput, RunRequest, Stream, path};
use serde_json::{Value, json};

use super::run;

#[derive(Clone)]
struct FakeRunSpec {
    exit_code: i32,
    timed_out: bool,
    generator: String,
    defines_a: Vec<String>,
    defines_b: Vec<String>,
    includes: Vec<(String, bool)>,
    cache_type: String,
    write_fileapi: bool,
    symlink_escape_reply: bool,
    write_compdb: bool,
    compdb_extra_flag: Option<String>,
    make_program: String,
    ninja_dyndep_missing: bool,
    ninja_commands: Vec<String>,
    ninja_regen_only: bool,
    two_targets_same_source: bool,
}

impl Default for FakeRunSpec {
    fn default() -> Self {
        Self {
            exit_code: 0,
            timed_out: false,
            generator: "Ninja".to_owned(),
            defines_a: vec!["FOO=1".to_owned()],
            defines_b: vec!["BAR=2".to_owned()],
            includes: vec![
                ("<SRC>/inc1".to_owned(), false),
                ("<SRC>/inc2".to_owned(), true),
            ],
            cache_type: "STRING".to_owned(),
            write_fileapi: true,
            symlink_escape_reply: false,
            write_compdb: true,
            compdb_extra_flag: None,
            make_program: "/usr/bin/ninja".to_owned(),
            ninja_dyndep_missing: false,
            ninja_commands: vec![
                "/usr/bin/cc -DFOO=1 -c <SRC>/main.c -o CMakeFiles/app.dir/main.c.o".to_owned(),
                "/usr/bin/cc CMakeFiles/app.dir/main.c.o -o app".to_owned(),
            ],
            ninja_regen_only: false,
            two_targets_same_source: false,
        }
    }
}

type RecordedRuns = Arc<Mutex<Vec<(String, String, String, bool)>>>;

struct FakeEnv {
    host: Arc<PureHost>,
    oracle_ver: String,
    oracle_is_rcmake: bool,
    ninja_valid: bool,
    oracle_spec: FakeRunSpec,
    rcmake_spec: FakeRunSpec,
    recorded_staged_paths: RecordedRuns,
}

impl FakeEnv {
    fn new(oracle_spec: FakeRunSpec, rcmake_spec: FakeRunSpec) -> Self {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/bin/rcmake"), b"#!/bin/sh\n")
            .expect("seed rcmake");
        host.seed_file(Path::new("/opt/cmake/bin/cmake"), b"#!/bin/sh\n")
            .expect("seed oracle");
        host.seed_file(Path::new("/usr/bin/ninja"), b"#!/bin/sh\n")
            .expect("seed ninja");
        host.seed_file(
            Path::new("/proj/CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 4.5)\nproject(Demo C)\n",
        )
        .expect("seed cmakelists");
        host.seed_file(Path::new("/proj/main.c"), b"int main(void){return 0;}\n")
            .expect("seed main.c");
        host.seed_file(Path::new("/proj/build/keep.txt"), b"original-build\n")
            .expect("seed original build");
        host.seed_file(Path::new("/outside/secret.json"), b"{}")
            .expect("seed secret");
        Self {
            host,
            oracle_ver: "4.5.1".to_owned(),
            oracle_is_rcmake: false,
            ninja_valid: true,
            oracle_spec,
            rcmake_spec,
            recorded_staged_paths: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn install_runner(&self) {
        let oracle_ver = self.oracle_ver.clone();
        let oracle_is_rcmake = self.oracle_is_rcmake;
        let ninja_valid = self.ninja_valid;
        let oracle_spec = self.oracle_spec.clone();
        let rcmake_spec = self.rcmake_spec.clone();
        let recorded = Arc::clone(&self.recorded_staged_paths);

        self.host
            .set_command_handler(move |host_ref, req: &RunRequest, _| {
                let Some(cmd) = req.pipeline.first() else {
                    return Err(HostError::unsupported("empty pipeline"));
                };
                let prog = String::from_utf8_lossy(&cmd.program).into_owned();
                let args: Vec<String> = cmd
                    .args
                    .iter()
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect();

                if args.as_slice() == ["--version"] {
                    if prog.ends_with("/rcmake") {
                        return Ok(exited_out(0, b"cmake version 4.5.1 (rcmake 0.1.0)\n"));
                    }
                    if prog.ends_with("/cmake") {
                        let banner = if oracle_is_rcmake {
                            format!("cmake version {oracle_ver} (rcmake 0.1.0)\n")
                        } else {
                            format!(
                                "cmake version {oracle_ver}\n\nCMake suite maintained by Kitware.\n"
                            )
                        };
                        return Ok(exited_out(0, banner.as_bytes()));
                    }
                    if prog.contains("ninja") || prog.contains("wrapper") {
                        return if ninja_valid {
                            Ok(exited_out(0, b"1.12.1\n"))
                        } else {
                            Ok(exited_out(0, b"custom-wrapper v1\n"))
                        };
                    }
                }

                if args.as_slice() == ["-t", "list"] {
                    return if ninja_valid {
                        Ok(exited_out(
                            0,
                            b"ninja subtools:\n  commands  list all commands\n",
                        ))
                    } else {
                        Ok(exited_out(1, b"unknown flag -t\n"))
                    };
                }

                if args.iter().any(|a| a == "-n") {
                    let is_rcmake = req.env.iter().any(|(k, v)| {
                        k.as_slice() == b"RCMAKE_ORACLE_MODE" && v.as_slice() == b"1"
                    });
                    let spec = if is_rcmake {
                        &rcmake_spec
                    } else {
                        &oracle_spec
                    };
                    let src = req.cwd.parent().map(|p| p.join("src")).unwrap_or_default();
                    let src_s = src.display().to_string();
                    let mut stdout = String::new();
                    if spec.ninja_regen_only {
                        stdout.push_str(
                            "[1/1] /opt/cmake/bin/cmake --regenerate-during-build -S/s -B/b\n",
                        );
                    } else {
                        let total = spec.ninja_commands.len();
                        for (idx, line) in spec.ninja_commands.iter().enumerate() {
                            let rendered = line.replace("<SRC>", &src_s);
                            stdout.push_str(&format!("[{}/{total}] {rendered}\n", idx + 1));
                        }
                    }
                    return Ok(exited_out(0, stdout.as_bytes()));
                }

                // Configure invocation
                let is_rcmake = prog.ends_with("/rcmake");
                let spec = if is_rcmake {
                    &rcmake_spec
                } else {
                    &oracle_spec
                };
                let src = find_arg(&args, "-S").unwrap_or_default();
                let bin = find_arg(&args, "-B").unwrap_or_default();
                let oracle_mode = req
                    .env
                    .iter()
                    .any(|(k, v)| k.as_slice() == b"RCMAKE_ORACLE_MODE" && v.as_slice() == b"1");
                recorded.lock().expect("lock").push((
                    prog.clone(),
                    src.clone(),
                    bin.clone(),
                    oracle_mode,
                ));

                host_ref.sleep(Duration::from_millis(if is_rcmake { 5 } else { 15 }));

                if spec.timed_out {
                    return Ok(RunOutput {
                        results: vec![ProcessResult::TimedOut],
                        stdout: Vec::new(),
                        stderr: b"timed out\n".to_vec(),
                    });
                }
                if spec.exit_code != 0 {
                    return Ok(RunOutput {
                        results: vec![ProcessResult::Exited(spec.exit_code)],
                        stdout: Vec::new(),
                        stderr: b"CMake Error: configure failed\n".to_vec(),
                    });
                }

                let bin_path = PathBuf::from(&bin);
                let root = if is_rcmake {
                    "/share/rcmake-4.5"
                } else {
                    "/opt/cmake/share/cmake-4.5"
                };
                materialize_run(host_ref, &src, &bin_path, &prog, root, spec);
                Ok(exited_out(0, b"-- Configuring done\n-- Generating done\n"))
            })
            .expect("handler");
    }

    fn run_verify(&self, json_out: bool, extra_req: Value) -> (i32, String, String) {
        self.install_runner();
        let mut req = json!({
            "schema_version": 1,
            "rcmake_exe": "/bin/rcmake",
            "oracle_exe": "/opt/cmake/bin/cmake",
            "source_dir": "/proj",
            "excluded_build_dir": "/proj/build",
            "cwd_rel_to_source": "",
            "args": [
                { "kind": "literal", "value": "-G" },
                { "kind": "literal", "value": self.oracle_spec.generator }
            ],
            "json": json_out,
            "request_compile_commands": true,
            "compile_commands_user_disabled": false,
            "generator_hint": self.oracle_spec.generator,
            "external_inputs": []
        });
        if let (Some(base), Some(extra)) = (req.as_object_mut(), extra_req.as_object()) {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        self.host
            .seed_stdin(req.to_string().as_bytes())
            .expect("stdin");
        let code = run(self.host.as_ref(), &[BString::from("--request-stdin")]);
        let stdout = String::from_utf8_lossy(&self.host.stream_bytes(Stream::Stdout)).into_owned();
        let stderr = String::from_utf8_lossy(&self.host.stream_bytes(Stream::Stderr)).into_owned();
        (code, stdout, stderr)
    }
}

fn exited_out(code: i32, stdout: &[u8]) -> RunOutput {
    RunOutput {
        results: vec![ProcessResult::Exited(code)],
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
    }
}

fn find_arg(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w.first().is_some_and(|a| a == flag))
        .and_then(|w| w.get(1).cloned())
}

fn materialize_run(
    host: &PureHost,
    src: &str,
    bin: &Path,
    exe: &str,
    cmake_root: &str,
    spec: &FakeRunSpec,
) {
    let bin_str = path::from_native_cmake(bin);
    let bin_s = String::from_utf8_lossy(&bin_str).into_owned();

    let cache = format!(
        "// Comment that differs on {exe}\n\
         CMAKE_COMMAND:INTERNAL={exe}\n\
         CMAKE_ROOT:INTERNAL={cmake_root}\n\
         CMAKE_GENERATOR:INTERNAL={}\n\
         CMAKE_MAKE_PROGRAM:FILEPATH={}\n\
         CMAKE_C_COMPILER:FILEPATH=/usr/bin/cc\n\
         CMAKE_C_COMPILER_ID:STRING=Clang\n\
         CMAKE_EDIT_COMMAND:FILEPATH={}\n\
         MY_OPTION:{}=hello\n",
        spec.generator,
        spec.make_program,
        if exe.ends_with("/rcmake") {
            "/bin/ccmake"
        } else {
            "CMAKE_EDIT_COMMAND-NOTFOUND"
        },
        spec.cache_type,
    );
    host.seed_file(&bin.join("CMakeCache.txt"), cache.as_bytes())
        .expect("seed cache");

    if spec.write_fileapi {
        let reply = bin.join(".cmake/api/v1/reply");
        let index = json!({
            "cmake": {
                "generator": { "name": spec.generator },
                "paths": { "cmake": exe, "root": cmake_root },
                "version": { "string": "4.5.1" }
            },
            "objects": [
                { "kind": "codemodel", "version": { "major": 2, "minor": 12 }, "jsonFile": "codemodel-v2.json" },
                { "kind": "toolchains", "version": { "major": 1, "minor": 0 }, "jsonFile": "toolchains-v1.json" }
            ]
        });
        host.seed_file(
            &reply.join("index-2026-01-01T00-00-00-0000.json"),
            index.to_string().as_bytes(),
        )
        .expect("seed index");
        let toolchains = json!({
            "toolchains": [{
                "language": "C",
                "compiler": { "id": "Clang", "path": "/usr/bin/cc" }
            }]
        });
        host.seed_file(
            &reply.join("toolchains-v1.json"),
            toolchains.to_string().as_bytes(),
        )
        .expect("seed toolchains");

        let mut target_list = vec![json!({
            "id": "app::@1234",
            "name": "app",
            "directoryIndex": 0,
            "projectIndex": 0,
            "jsonFile": "target-app.json"
        })];
        let mut target_indexes = vec![0];
        if spec.two_targets_same_source {
            target_list.push(json!({
                "id": "app2::@5678",
                "name": "app2",
                "directoryIndex": 0,
                "projectIndex": 0,
                "jsonFile": "target-app2.json"
            }));
            target_indexes.push(1);
        }
        let codemodel = json!({
            "paths": { "source": src, "build": bin_s },
            "configurations": [{
                "name": "",
                "directories": [{ "source": ".", "build": ".", "projectIndex": 0 }],
                "projects": [{ "name": "Demo", "directoryIndexes": [0], "targetIndexes": target_indexes }],
                "targets": target_list
            }]
        });
        host.seed_file(
            &reply.join("codemodel-v2.json"),
            codemodel.to_string().as_bytes(),
        )
        .expect("seed codemodel");

        let includes_json: Vec<Value> = spec
            .includes
            .iter()
            .map(|(p, sys)| json!({ "path": p.replace("<SRC>", src), "isSystem": sys }))
            .collect();
        let make_target = |name: &str, defs: &[String]| {
            let defs_json: Vec<Value> = defs.iter().map(|d| json!({ "define": d })).collect();
            json!({
                "name": name,
                "type": "EXECUTABLE",
                "nameOnDisk": name,
                "artifacts": [{ "path": name }],
                "compileGroups": [{
                    "language": "C",
                    "defines": defs_json,
                    "includes": includes_json,
                    "compileCommandFragments": [{ "fragment": "-O2" }]
                }],
                "sources": [{ "path": "main.c", "compileGroupIndex": 0 }],
                "link": {
                    "language": "C",
                    "commandFragments": [{ "role": "flags", "fragment": "-lm" }]
                }
            })
        };

        if spec.symlink_escape_reply {
            host.symlink(
                Path::new("/outside/secret.json"),
                &reply.join("target-app.json"),
            )
            .expect("seed escaping symlink");
        } else {
            host.seed_file(
                &reply.join("target-app.json"),
                make_target("app", &spec.defines_a).to_string().as_bytes(),
            )
            .expect("seed target-app");
        }
        if spec.two_targets_same_source {
            host.seed_file(
                &reply.join("target-app2.json"),
                make_target("app2", &spec.defines_b).to_string().as_bytes(),
            )
            .expect("seed target-app2");
        }
    }

    if spec.write_compdb {
        let mut rows = Vec::new();
        let extra = spec
            .compdb_extra_flag
            .as_deref()
            .map(|f| format!(" {f}"))
            .unwrap_or_default();
        let defs_a: String = spec
            .defines_a
            .iter()
            .map(|d| format!("-D{d}"))
            .collect::<Vec<_>>()
            .join(" ");
        rows.push(json!({
            "directory": bin_s,
            "file": format!("{src}/main.c"),
            "output": "CMakeFiles/app.dir/main.c.o",
            "command": format!("/usr/bin/cc {defs_a}{extra} -c {src}/main.c -o CMakeFiles/app.dir/main.c.o")
        }));
        if spec.two_targets_same_source {
            let defs_b: String = spec
                .defines_b
                .iter()
                .map(|d| format!("-D{d}"))
                .collect::<Vec<_>>()
                .join(" ");
            rows.push(json!({
                "directory": bin_s,
                "file": format!("{src}/main.c"),
                "output": "CMakeFiles/app2.dir/main.c.o",
                "command": format!("/usr/bin/cc {defs_b} -c {src}/main.c -o CMakeFiles/app2.dir/main.c.o")
            }));
        }
        host.seed_file(
            &bin.join("compile_commands.json"),
            Value::Array(rows).to_string().as_bytes(),
        )
        .expect("seed compdb");
    }

    if spec.generator == "Ninja" {
        let mut ninja = String::from(
            "rule CC\n  command = /usr/bin/cc $in -o $out\n\
             build CMakeFiles/app.dir/main.c.o: CC main.c\n\
             build app: CC CMakeFiles/app.dir/main.c.o\n",
        );
        if spec.ninja_dyndep_missing {
            ninja.push_str("  dyndep = CMakeFiles/app.dir/CXX.dd\n");
        }
        host.seed_file(&bin.join("build.ninja"), ninja.as_bytes())
            .expect("seed build.ninja");
    }
}

#[test]
fn parity_passes_with_identical_staged_paths_and_leaves_caller_trees_intact() {
    let env = FakeEnv::new(FakeRunSpec::default(), FakeRunSpec::default());
    let (code, stdout, stderr) = env.run_verify(true, json!({}));
    assert_eq!(code, 0, "stdout={stdout}\nstderr={stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(doc["verdict"], "parity");
    assert_eq!(doc["exit_code"], 0);
    assert_eq!(doc["timings"]["observation"], "single_paired_observation");
    assert_eq!(doc["timings"]["run_order"], json!(["oracle", "rcmake"]));
    assert!(doc["timings"]["speedup"].as_f64().is_some());
    assert_eq!(doc["sections"]["codemodel"]["status"], "equal");
    assert_eq!(doc["sections"]["cache"]["status"], "equal");
    assert_eq!(doc["sections"]["compile_commands"]["status"], "equal");
    assert_eq!(doc["sections"]["ninja_dry_run"]["status"], "equal");

    // Verify BOTH engines ran against identical staged absolute paths and rcmake used compat mode.
    let runs = env.recorded_staged_paths.lock().expect("lock");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].1, runs[1].1, "staged source path must be identical");
    assert_eq!(runs[0].2, runs[1].2, "staged build path must be identical");
    assert!(!runs[0].3, "oracle should not receive RCMAKE_ORACLE_MODE=1");
    assert!(runs[1].3, "rcmake must run under RCMAKE_ORACLE_MODE=1");

    // Original source and build trees are untouched.
    assert_eq!(
        env.host
            .read(
                Path::new("/proj/build/keep.txt"),
                rcmake_host::ReadRange::All { limit: 1024 }
            )
            .expect("original build preserved"),
        b"original-build\n"
    );
    assert!(
        env.host
            .metadata(
                Path::new("/proj/build/CMakeCache.txt"),
                rcmake_host::Follow::No
            )
            .is_err(),
        "caller build dir must not be written"
    );
}

#[test]
fn detects_single_define_change_and_include_order_reversal() {
    let r_spec = FakeRunSpec {
        defines_a: vec!["FOO=2".to_owned()],
        ..Default::default()
    };
    let env = FakeEnv::new(FakeRunSpec::default(), r_spec);
    let (code, stdout, _) = env.run_verify(true, json!({}));
    assert_eq!(code, 1);
    let doc: Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(doc["verdict"], "divergent");
    assert_eq!(doc["sections"]["codemodel"]["status"], "different");

    // Include-order reversal alone must diverge.
    let mut r_inc = FakeRunSpec::default();
    r_inc.includes.reverse();
    let env2 = FakeEnv::new(FakeRunSpec::default(), r_inc);
    let (code2, stdout2, _) = env2.run_verify(true, json!({}));
    assert_eq!(code2, 1);
    let doc2: Value = serde_json::from_str(&stdout2).expect("json");
    assert_eq!(doc2["sections"]["codemodel"]["status"], "different");
}

#[test]
fn detects_two_targets_sharing_source_and_unequal_cache_type() {
    let mut o_spec = FakeRunSpec {
        two_targets_same_source: true,
        ..FakeRunSpec::default()
    };
    let mut r_spec = o_spec.clone();
    // Matching two targets -> exit 0.
    let env_ok = FakeEnv::new(o_spec.clone(), r_spec.clone());
    assert_eq!(env_ok.run_verify(true, json!({})).0, 0);

    // Differing second target on same source -> exit 1.
    r_spec.defines_b = vec!["BAR=99".to_owned()];
    let env_diff = FakeEnv::new(o_spec.clone(), r_spec);
    let (code, stdout, _) = env_diff.run_verify(true, json!({}));
    assert_eq!(code, 1);
    let doc: Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(doc["sections"]["codemodel"]["status"], "different");
    assert_eq!(doc["sections"]["compile_commands"]["status"], "different");

    // Unequal cache entry type with same value -> exit 1.
    o_spec.two_targets_same_source = false;
    let r_type = FakeRunSpec {
        cache_type: "FILEPATH".to_owned(),
        ..Default::default()
    };
    let env_type = FakeEnv::new(o_spec, r_type);
    let (code_t, stdout_t, _) = env_type.run_verify(true, json!({}));
    assert_eq!(code_t, 1);
    let doc_t: Value = serde_json::from_str(&stdout_t).expect("json");
    assert_eq!(doc_t["sections"]["cache"]["status"], "different");
}

#[test]
fn handles_missing_fileapi_symlink_escape_matched_failure_and_timeout() {
    // One-sided missing File API reply -> Divergent (exit 1).
    let r_missing = FakeRunSpec {
        write_fileapi: false,
        ..Default::default()
    };
    let env1 = FakeEnv::new(FakeRunSpec::default(), r_missing.clone());
    let (c1, out1, _) = env1.run_verify(true, json!({}));
    assert_eq!(c1, 1);
    let d1: Value = serde_json::from_str(&out1).expect("json");
    assert_eq!(d1["sections"]["codemodel"]["status"], "different");

    // Both missing File API reply -> IncompleteEvidence (exit 2).
    let env2 = FakeEnv::new(r_missing.clone(), r_missing);
    let (c2, out2, _) = env2.run_verify(true, json!({}));
    assert_eq!(c2, 2);
    let d2: Value = serde_json::from_str(&out2).expect("json");
    assert_eq!(d2["verdict"], "incomplete_evidence");
    assert_eq!(d2["sections"]["codemodel"]["status"], "unavailable");

    // Symlink escaping reply directory rejected by ArtifactReader (P1-09) -> exit 2.
    let r_sym = FakeRunSpec {
        symlink_escape_reply: true,
        ..Default::default()
    };
    let env_sym = FakeEnv::new(FakeRunSpec::default(), r_sym);
    let (cs, outs, _) = env_sym.run_verify(true, json!({}));
    assert_eq!(cs, 2);
    let ds: Value = serde_json::from_str(&outs).expect("json");
    assert_eq!(ds["sections"]["codemodel"]["status"], "unavailable");

    // Matched non-zero configure failure -> exit 2 (no parity claim).
    let fail = FakeRunSpec {
        exit_code: 1,
        ..Default::default()
    };
    let env_fail = FakeEnv::new(fail.clone(), fail);
    let (cf, outf, _) = env_fail.run_verify(true, json!({}));
    assert_eq!(cf, 2);
    let df: Value = serde_json::from_str(&outf).expect("json");
    assert_eq!(df["verdict"], "matched_failure");
    assert!(df["timings"]["speedup"].is_null());

    // Timed-out configure -> exit 2.
    let to = FakeRunSpec {
        timed_out: true,
        ..Default::default()
    };
    let env_to = FakeEnv::new(FakeRunSpec::default(), to);
    let (ct, outt, _) = env_to.run_verify(true, json!({}));
    assert_eq!(ct, 2);
    let dt: Value = serde_json::from_str(&outt).expect("json");
    assert_eq!(dt["verdict"], "incomplete_evidence");
}

#[test]
fn ninja_dry_run_guards_wrapper_dyndep_regen_and_non_ninja_generator() {
    // Non-Ninja generator -> inapplicable (exit 0).
    let make_spec = FakeRunSpec {
        generator: "Unix Makefiles".to_owned(),
        ..Default::default()
    };
    let env_make = FakeEnv::new(make_spec.clone(), make_spec);
    let (cm, outm, _) = env_make.run_verify(true, json!({}));
    assert_eq!(cm, 0, "{outm}");
    let dm: Value = serde_json::from_str(&outm).expect("json");
    assert_eq!(dm["sections"]["ninja_dry_run"]["status"], "inapplicable");

    // Non-Ninja wrapper make program -> unavailable (exit 2, P1-13).
    let mut env_wrap = FakeEnv::new(FakeRunSpec::default(), FakeRunSpec::default());
    env_wrap.ninja_valid = false;
    let (cw, outw, _) = env_wrap.run_verify(true, json!({}));
    assert_eq!(cw, 2);
    let dw: Value = serde_json::from_str(&outw).expect("json");
    assert_eq!(dw["sections"]["ninja_dry_run"]["status"], "unavailable");

    // Unresolved dyndep in build.ninja -> unavailable (exit 2, P1-13).
    let dd = FakeRunSpec {
        ninja_dyndep_missing: true,
        ..Default::default()
    };
    let env_dd = FakeEnv::new(dd.clone(), dd);
    let (cdd, outdd, _) = env_dd.run_verify(true, json!({}));
    assert_eq!(cdd, 2);
    let ddd: Value = serde_json::from_str(&outdd).expect("json");
    assert_eq!(ddd["sections"]["ninja_dry_run"]["status"], "unavailable");

    // Regeneration-only Ninja output -> unavailable (exit 2, P1-13).
    let rg = FakeRunSpec {
        ninja_regen_only: true,
        ..Default::default()
    };
    let env_rg = FakeEnv::new(rg.clone(), rg);
    let (crg, outrg, _) = env_rg.run_verify(true, json!({}));
    assert_eq!(crg, 2);
    let drg: Value = serde_json::from_str(&outrg).expect("json");
    assert_eq!(drg["sections"]["ninja_dry_run"]["status"], "unavailable");
}

#[test]
fn rejects_wrong_oracle_version_and_recursive_oracle_and_respects_user_disabled_compdb() {
    // Wrong oracle version -> exit 2.
    let mut env_old = FakeEnv::new(FakeRunSpec::default(), FakeRunSpec::default());
    env_old.oracle_ver = "3.31.0".to_owned();
    let (co, outo, _) = env_old.run_verify(true, json!({}));
    assert_eq!(co, 2);
    let doco: Value = serde_json::from_str(&outo).expect("json");
    assert_eq!(doco["verdict"], "invalid_input");
    assert_eq!(doco["engines"]["oracle"]["version"], "3.31.0");

    // Recursive oracle (banner contains rcmake) -> exit 2.
    let mut env_rec = FakeEnv::new(FakeRunSpec::default(), FakeRunSpec::default());
    env_rec.oracle_is_rcmake = true;
    let (cr, outr, _) = env_rec.run_verify(true, json!({}));
    assert_eq!(cr, 2);
    let docr: Value = serde_json::from_str(&outr).expect("json");
    assert_eq!(docr["verdict"], "invalid_input");

    // User disabled compile_commands -> skipped (exit 0).
    let no_cc = FakeRunSpec {
        write_compdb: false,
        ..Default::default()
    };
    let env_skip = FakeEnv::new(no_cc.clone(), no_cc);
    let (cs, outs, _) = env_skip.run_verify(
        true,
        json!({ "request_compile_commands": false, "compile_commands_user_disabled": true }),
    );
    assert_eq!(cs, 0, "{outs}");
    let docs: Value = serde_json::from_str(&outs).expect("json");
    assert_eq!(docs["sections"]["compile_commands"]["status"], "skipped");
}
