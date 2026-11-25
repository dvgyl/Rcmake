// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{path::PathBuf, sync::Arc};

use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{Host, ProcessResult, PureHost, RunOutput, Stream};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn native(path: &str) -> PathBuf {
    PathBuf::from(if cfg!(windows) {
        format!("C:{path}")
    } else {
        path.to_owned()
    })
}

fn setup() -> Result<(Arc<PureHost>, Interp), Box<dyn std::error::Error>> {
    setup_with_limit(Default::default())
}

fn setup_with_limit(
    limits: rcmake_base::limits::Limits,
) -> Result<(Arc<PureHost>, Interp), Box<dyn std::error::Error>> {
    let host = Arc::new(PureHost::new(native("/work"), native("/bin/cmake")));
    let interp = Interp::new(
        InterpConfig {
            host: host.clone(),
            mode: RunMode::Script,
            cmake_root: native("/share/cmake"),
            cmake_command: native("/bin/cmake"),
            cwd: native("/work"),
            limits,
            diag: Default::default(),
            defines: Vec::new(),
            services: Default::default(),
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    host.set_command_handler(|_, request, _| {
        if let [command] = request.pipeline.as_slice() {
            match command.program.as_slice() {
                b"ldconfig-test" => {
                    assert_eq!(command.args, ["--fixture", "-v", "-N", "-X"]);
                    return Ok(output(b"/system2: (from /etc/ld.so.conf:1)\n/system1:\n"));
                }
                b"bsd-ldconfig" => {
                    assert_eq!(command.args, ["-r"]);
                    return Ok(output(
                        b"/var/run/ld-elf.so.hints:\n  search directories: /bsd1:/bsd2\n",
                    ));
                }
                b"objdump-test" => {
                    assert_eq!(
                        command.args.first().map(|arg| arg.as_slice()),
                        Some(&b"--fixture"[..])
                    );
                    assert_eq!(
                        command.args.get(1).map(|arg| arg.as_slice()),
                        Some(&b"-p"[..])
                    );
                    let file = command
                        .args
                        .last()
                        .map(|arg| arg.as_slice())
                        .unwrap_or_default();
                    if file.ends_with(b"/-main") {
                        return Ok(output(b"  NEEDED  libfake.so\n  RUNPATH  /work\n"));
                    }
                    return Ok(output(b"  SONAME ignored.so\n"));
                }
                _ => {}
            }
        }
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(1)],
            ..RunOutput::default()
        })
    })?;
    Ok((host, interp))
}

fn output(stdout: &[u8]) -> RunOutput {
    RunOutput {
        results: vec![ProcessResult::Exited(0)],
        stdout: stdout.to_vec(),
        ..RunOutput::default()
    }
}

fn run(
    host: &PureHost,
    interp: &mut Interp,
    commands: &str,
) -> Result<i32, Box<dyn std::error::Error>> {
    let input = native("/work/test.cmake");
    let code = format!(
        "cmake_minimum_required(VERSION 4.5)\nset(CMAKE_HOST_SYSTEM_NAME Synthetic)\nset(CMAKE_GET_RUNTIME_DEPENDENCIES_PLATFORM linux+elf)\nset(CMAKE_LDCONFIG_COMMAND ldconfig-test;--fixture)\n{commands}"
    );
    host.seed_file(&input, code.as_bytes())?;
    Ok(interp.run_script(&input).exit_code)
}

fn ok(host: &PureHost, interp: &mut Interp, commands: &str) -> TestResult {
    assert_eq!(
        run(host, interp, commands)?,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    Ok(())
}

fn value(interp: &mut Interp, name: &str, expected: &str) {
    assert_eq!(interp.cx().var_or_empty(name.as_bytes()), expected);
}

fn elf(machine: u16, tags: &[(u64, &str)]) -> Vec<u8> {
    const HEADER: usize = 64;
    const SEGMENT: usize = 56;
    const BASE: u64 = 0x400000;
    let mut bytes = vec![0; HEADER];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[52..54].copy_from_slice(&(HEADER as u16).to_le_bytes());
    if tags.is_empty() {
        return bytes;
    }
    bytes[32..40].copy_from_slice(&(HEADER as u64).to_le_bytes());
    bytes[54..56].copy_from_slice(&(SEGMENT as u16).to_le_bytes());
    bytes[56..58].copy_from_slice(&2u16.to_le_bytes());

    let mut strings = vec![0];
    let mut dynamic = Vec::new();
    let entries = tags
        .iter()
        .map(|&(tag, text)| {
            let offset = strings.len() as u64;
            strings.extend(text.bytes());
            strings.push(0);
            (tag, offset)
        })
        .collect::<Vec<_>>();
    let dynamic_offset = (HEADER + 2 * SEGMENT) as u64;
    let dynamic_size = ((entries.len() + 3) * 16) as u64;
    let strings_offset = dynamic_offset + dynamic_size;
    for (tag, value) in entries.into_iter().chain([
        (5, BASE + strings_offset),
        (10, strings.len() as u64),
        (0, 0),
    ]) {
        dynamic.extend(tag.to_le_bytes());
        dynamic.extend(value.to_le_bytes());
    }
    for (kind, offset, len) in [
        (1u32, 0, strings_offset + strings.len() as u64),
        (2, dynamic_offset, dynamic_size),
    ] {
        bytes.extend(kind.to_le_bytes());
        bytes.extend(4u32.to_le_bytes());
        for field in [offset, BASE + offset, 0, len, len, 1] {
            bytes.extend(field.to_le_bytes());
        }
    }
    bytes.extend(dynamic);
    bytes.extend(strings);
    bytes
}

fn add(host: &PureHost, path: &str, tags: &[(u64, &str)]) -> rcmake_host::HostResult<()> {
    host.seed_file(&native(path), &elf(62, tags))
}

#[test]
fn origin_soname_transitive_and_runpath_precedence() -> TestResult {
    let (host, mut interp) = setup()?;
    add(
        &host,
        "/work/bin/main",
        &[(1, "libparent.so"), (15, "/wrong"), (29, "$ORIGIN/../lib")],
    )?;
    add(&host, "/wrong/libparent.so", &[(1, "missing.so")])?;
    add(
        &host,
        "/work/lib/libparent.so",
        &[(1, "libleaf.so"), (14, "other.so"), (29, "${ORIGIN}/leaf")],
    )?;
    add(&host, "/work/lib/leaf/libleaf.so", &[])?;
    ok(
        &host,
        &mut interp,
        "file(GET_RUNTIME_DEPENDENCIES EXECUTABLES /work/bin/main RESOLVED_DEPENDENCIES_VAR result RPATH_PREFIX rp)",
    )?;
    value(
        &mut interp,
        "result",
        "/work/bin/../lib/leaf/libleaf.so;/work/bin/../lib/libparent.so",
    );
    value(&mut interp, "rp_/work/bin/../lib/libparent.so", "");
    assert!(
        interp
            .cx()
            .var(b"rp_/work/bin/../lib/libparent.so")
            .is_some()
    );
    Ok(())
}

#[test]
fn shared_dependencies_are_walked_once_with_the_first_root_search() -> TestResult {
    let (host, mut interp) = setup()?;
    for (path, tag, search) in [
        ("/work/one", 15, "/one:/common"),
        ("/work/two", 15, "/two:/common"),
        ("/work/three", 29, "/one:/common"),
    ] {
        add(&host, path, &[(1, "parent.so"), (tag, search)])?;
    }
    add(&host, "/common/parent.so", &[(1, "leaf.so")])?;
    add(&host, "/one/leaf.so", &[])?;
    add(&host, "/two/leaf.so", &[])?;
    ok(
        &host,
        &mut interp,
        "file(GET_RUNTIME_DEPENDENCIES EXECUTABLES /work/one /work/two /work/three RESOLVED_DEPENDENCIES_VAR result CONFLICTING_DEPENDENCIES_PREFIX clash UNRESOLVED_DEPENDENCIES_VAR missing)",
    )?;
    value(&mut interp, "result", "/one/leaf.so;/common/parent.so");
    value(&mut interp, "clash_FILENAMES", "");
    value(&mut interp, "missing", "");
    Ok(())
}

#[test]
fn breadth_first_search_and_file_identity() -> TestResult {
    let (host, mut interp) = setup()?;
    add(
        &host,
        "/work/root",
        &[(1, "first.so"), (1, "second.so"), (29, "/work")],
    )?;
    add(&host, "/work/first.so", &[(1, "middle.so"), (29, "/work")])?;
    add(&host, "/work/middle.so", &[(1, "same.so"), (29, "/deep")])?;
    add(
        &host,
        "/work/second.so",
        &[(1, "same.so"), (29, "/shallow")],
    )?;
    add(&host, "/deep/same.so", &[(1, "wrong.so")])?;
    add(&host, "/shallow/same.so", &[(1, "first.so"), (29, "/work")])?;
    add(&host, "/work/alias-root", &[(1, "same.so"), (29, "/alias")])?;
    host.create_dir_all(&native("/alias"))?;
    host.hard_link(&native("/shallow/same.so"), &native("/alias/same.so"))?;
    ok(
        &host,
        &mut interp,
        "file(GET_RUNTIME_DEPENDENCIES EXECUTABLES /work/root /work/alias-root RESOLVED_DEPENDENCIES_VAR result CONFLICTING_DEPENDENCIES_PREFIX clash)",
    )?;
    value(
        &mut interp,
        "result",
        "/work/first.so;/work/middle.so;/alias/same.so;/work/second.so",
    );
    value(&mut interp, "clash_FILENAMES", "");
    Ok(())
}

#[test]
fn system_order_machine_check_explicit_directories_and_environment() -> TestResult {
    let (host, mut interp) = setup()?;
    add(
        &host,
        "/work/main",
        &[
            (1, "order.so"),
            (1, "arch.so"),
            (1, "fallback.so"),
            (1, "environment.so"),
        ],
    )?;
    host.env_set(b"LD_LIBRARY_PATH", Some(b"/env"));
    host.seed_file(&native("/system2/arch.so"), &elf(183, &[]))?;
    for path in [
        "/system2/order.so",
        "/system1/order.so",
        "/system1/arch.so",
        "/explicit/order.so",
        "/explicit/fallback.so",
        "/env/environment.so",
    ] {
        add(&host, path, &[])?;
    }
    ok(
        &host,
        &mut interp,
        "file(GET_RUNTIME_DEPENDENCIES EXECUTABLES /work/main DIRECTORIES /explicit RESOLVED_DEPENDENCIES_VAR result UNRESOLVED_DEPENDENCIES_VAR missing)",
    )?;
    value(
        &mut interp,
        "result",
        "/system1/arch.so;/explicit/fallback.so;/system2/order.so",
    );
    value(&mut interp, "missing", "environment.so");
    assert_eq!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .matches("found in search directory")
            .count(),
        1
    );
    assert_ne!(
        run(
            &host,
            &mut interp,
            "file(GET_RUNTIME_DEPENDENCIES LIBRARIES /work/main /system2/arch.so UNRESOLVED_DEPENDENCIES_VAR missing)"
        )?,
        0
    );
    Ok(())
}

#[test]
fn pre_and_post_filters_prune_branches_and_strict_exclusion_wins() -> TestResult {
    let (host, mut interp) = setup()?;
    add(
        &host,
        "/work/main",
        &[
            (1, "slash/invalid"),
            (1, "pruned.so"),
            (1, "kept.so"),
            (1, "strict.so"),
            (29, "/work"),
        ],
    )?;
    add(&host, "/work/pruned.so", &[(1, "pruned-child.so")])?;
    add(&host, "/work/kept.so", &[])?;
    add(&host, "/work/strict.so", &[(1, "strict-child.so")])?;
    ok(
        &host,
        &mut interp,
        r#"file(GET_RUNTIME_DEPENDENCIES EXECUTABLES /work/main
      PRE_INCLUDE_REGEXES "[.]so$" PRE_EXCLUDE_REGEXES ".*"
      POST_INCLUDE_REGEXES "strict" POST_INCLUDE_FILES /work/kept.so
      POST_EXCLUDE_REGEXES ".*" POST_EXCLUDE_FILES_STRICT /work/strict.so
      RESOLVED_DEPENDENCIES_VAR result)"#,
    )?;
    value(&mut interp, "result", "/work/kept.so");
    assert_ne!(
        run(
            &host,
            &mut interp,
            "file(GET_RUNTIME_DEPENDENCIES EXECUTABLES /work/main)"
        )?,
        0
    );
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("slash/invalid")
    );
    Ok(())
}

#[test]
fn static_invalid_truncated_and_unresolved_inputs() -> TestResult {
    let mut big_endian = vec![0; 52];
    big_endian[..7].copy_from_slice(b"\x7fELF\x01\x02\x01");
    big_endian[16..18].copy_from_slice(&2u16.to_be_bytes());
    big_endian[18..20].copy_from_slice(&20u16.to_be_bytes());
    big_endian[20..24].copy_from_slice(&1u32.to_be_bytes());
    big_endian[40..42].copy_from_slice(&52u16.to_be_bytes());
    let mut truncated = elf(62, &[(1, "missing.so")]);
    truncated.truncate(185);
    for (bytes, error) in [
        (big_endian, None),
        (b"not an elf".to_vec(), Some("Could not read ELF")),
        (truncated, Some("Could not read ELF")),
        (
            elf(62, &[(1, "missing.so")]),
            Some("Could not resolve runtime dependencies"),
        ),
    ] {
        let (host, mut interp) = setup()?;
        host.seed_file(&native("/work/main"), &bytes)?;
        let code = run(
            &host,
            &mut interp,
            "file(GET_RUNTIME_DEPENDENCIES EXECUTABLES /work/main)",
        )?;
        if let Some(error) = error {
            assert_ne!(code, 0);
            let stderr = host.stream_bytes(Stream::Stderr).to_string();
            assert!(stderr.contains(error), "{stderr}");
        } else {
            assert_eq!(code, 0);
        }
    }
    Ok(())
}

#[test]
fn explicit_objdump_command_and_posix_path_list() -> TestResult {
    let (host, mut interp) = setup()?;
    add(&host, "/work/-main", &[])?;
    add(&host, "/work/libfake.so", &[])?;
    ok(
        &host,
        &mut interp,
        "set(CMAKE_GET_RUNTIME_DEPENDENCIES_COMMAND objdump-test;--fixture)\nfile(GET_RUNTIME_DEPENDENCIES EXECUTABLES -main RESOLVED_DEPENDENCIES_VAR result)",
    )?;
    value(&mut interp, "result", "/work/libfake.so");
    assert_eq!(
        super::expand_paths(&["a".into(), "".into(), "b".into(), "".into()], b"/origin"),
        ["a", ":b", ""]
    );
    assert_eq!(
        super::expand_origin(b"$ORIGINALLY/${ORIGIN}/$ORIGIN", b"/origin"),
        "$ORIGINALLY/${ORIGIN}//origin"
    );
    Ok(())
}

#[test]
fn freebsd_search_and_depth_limit() -> TestResult {
    let limits = rcmake_base::limits::Limits {
        max_recursion: 1,
        ..Default::default()
    };
    let (host, mut interp) = setup_with_limit(limits)?;
    add(&host, "/work/root", &[(1, "parent.so")])?;
    add(&host, "/bsd1/parent.so", &[(1, "leaf.so")])?;
    add(&host, "/bsd2/leaf.so", &[])?;
    let code = run(
        &host,
        &mut interp,
        "set(CMAKE_GET_RUNTIME_DEPENDENCIES_PLATFORM freebsd+elf)\nset(CMAKE_LDCONFIG_TOOL ldconfig)\nset(CMAKE_LDCONFIG_COMMAND bsd-ldconfig)\nfile(GET_RUNTIME_DEPENDENCIES EXECUTABLES /work/root)",
    )?;
    assert_ne!(code, 0);
    let stderr = host.stream_bytes(Stream::Stderr);
    assert!(stderr.to_string().contains("depth limit"), "{stderr}");
    Ok(())
}

#[test]
fn native_backslashes_do_not_split_elf_dependency_names() -> TestResult {
    let (host, mut interp) = setup()?;
    add(
        &host,
        "/work/main",
        &[(1, r"nested\leaf.so"), (29, "/work")],
    )?;
    add(&host, "/work/nested/leaf.so", &[])?;
    if !cfg!(windows) {
        add(&host, r"/work/nested\leaf.so", &[])?;
    }
    ok(
        &host,
        &mut interp,
        "file(GET_RUNTIME_DEPENDENCIES EXECUTABLES /work/main RESOLVED_DEPENDENCIES_VAR result UNRESOLVED_DEPENDENCIES_VAR missing)",
    )?;
    if cfg!(windows) {
        value(&mut interp, "result", "");
        value(&mut interp, "missing", r"nested\leaf.so");
    } else {
        value(&mut interp, "result", r"/work/nested\leaf.so");
        value(&mut interp, "missing", "");
    }
    Ok(())
}

#[test]
fn ldconfig_tool_selects_the_type() -> TestResult {
    let (host, mut interp) = setup()?;
    let code = run(
        &host,
        &mut interp,
        "set(CMAKE_LDCONFIG_TOOL custom-loader)\nfile(GET_RUNTIME_DEPENDENCIES)",
    )?;
    assert_ne!(code, 0);
    let stderr = host.stream_bytes(Stream::Stderr);
    assert!(
        stderr
            .to_string()
            .contains("Invalid value for CMAKE_LDCONFIG_TOOL: custom-loader"),
        "{stderr}"
    );
    Ok(())
}
