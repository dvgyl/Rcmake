//! Build Linux AppImage packages via `appimagetool` (and optional `patchelf`).

use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::path::{executable_on_absolute_path, from_native, from_native_cmake, to_native};
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, ReadRange, RunRequest,
    Stream, WriteMode,
};

use crate::config::{CpackConfig, absolute};
use crate::source::{SymlinkPolicy, validate_staged_tree};
use crate::stage::clear_output_file;
use crate::{CpackError, PackageContext, PackageGenerator, error, log, safe_name};

pub(super) struct AppImage;

impl PackageGenerator for AppImage {
    fn name(&self) -> &'static str {
        "AppImage"
    }

    fn extension(&self) -> &'static str {
        ".AppImage"
    }

    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        let host = cx.host;
        let cfg = CpackConfig {
            vars: cx.vars.clone(),
        };
        let canon_staging = cx.expected_staging_root()?;
        validate_staged_tree(
            host,
            cx.staging_dir,
            &canon_staging,
            &[],
            SymlinkPolicy::PreserveContained,
            false,
        )?;

        let app_dir = cx.staging_dir;
        let app_dir_cmake = from_native_cmake(app_dir);
        log(host, format!("AppDir: \"{app_dir_cmake}\""));

        let tool_spec = cfg
            .get(b"CPACK_APPIMAGE_TOOL_EXECUTABLE")
            .filter(|v| !v.is_empty())
            .unwrap_or(b"appimagetool");
        let tool_path = resolve_tool(host, "appimagetool", tool_spec)?;

        let files = collect_files(host, app_dir)?;

        let desktop_rel = find_desktop_file(&cfg, &files)?;
        let desktop_full = app_dir.join(&desktop_rel);
        log(
            host,
            format!(
                "Found Desktop file: \"{}\"",
                from_native_cmake(&desktop_full)
            ),
        );

        let desktop_name = desktop_rel
            .file_name()
            .ok_or_else(|| error("Desktop file has no filename"))?;
        let desktop_dest = app_dir.join(desktop_name);
        log(
            host,
            format!(
                "Desktop file destination: \"{}\"",
                from_native_cmake(&desktop_dest)
            ),
        );
        if desktop_full != desktop_dest {
            link_into_appdir_root(host, &desktop_rel, &desktop_dest)?;
        }

        let desktop_bytes = host.read(&desktop_full, ReadRange::All { limit: 1024 * 1024 })?;
        let (desktop_exec, desktop_icon) = parse_desktop_entry(&desktop_bytes);

        if let Some(icon_rel) = find_icon_file(&cfg, desktop_icon.as_deref(), &files) {
            let icon_full = app_dir.join(&icon_rel);
            log(
                host,
                format!("Icon file: \"{}\"", from_native_cmake(&icon_full)),
            );
            if let Some(icon_name) = icon_rel.file_name() {
                let icon_dest = app_dir.join(icon_name);
                log(
                    host,
                    format!(
                        "Icon link destination: \"{}\"",
                        from_native_cmake(&icon_dest)
                    ),
                );
                if icon_full != icon_dest {
                    link_into_appdir_root(host, &icon_rel, &icon_dest)?;
                }
                let dir_icon = app_dir.join(".DirIcon");
                if host.metadata(&dir_icon, Follow::No).is_err() {
                    let _ = host.symlink(Path::new(icon_name), &dir_icon);
                }
            }
        }

        let app_run = app_dir.join("AppRun");
        if host.metadata(&app_run, Follow::No).is_err() {
            let exec_rel = find_exec_target(desktop_exec.as_deref(), &files)?;
            let exec_str = from_native_cmake(&exec_rel);
            if exec_str.as_slice().contains(&0) {
                return Err(error("AppImage executable path contains a NUL byte"));
            }
            let exec_display = String::from_utf8_lossy(exec_str.as_slice());
            log(
                host,
                format!(
                    "No AppRun found, generating a default one that will run: \"{exec_display}\""
                ),
            );
            let quoted_exec = posix_shell_quote(&exec_display);
            let script = format!(
                "#!/bin/sh\nHERE=\"$(dirname \"$(readlink -f \"${{0}}\")\")\"\nexec \"${{HERE}}/\"{quoted_exec} \"$@\"\n"
            );
            host.write(&app_run, script.as_bytes(), WriteMode::Truncate)?;
            host.set_permissions(&app_run, 0o755)?;
        }

        patch_elf_rpaths(host, &cfg, app_dir, &files)?;

        let output_name = safe_name(&[cx.file_name, b".AppImage"].concat())?;
        let output_rel = format!("../{}", from_native_cmake(&output_name));
        let output_path = cx.output_dir.join(&output_name);

        let mut args: Vec<BString> = vec![app_dir_cmake, output_rel.into()];
        if let Some(val) = cfg
            .get(b"CPACK_APPIMAGE_UPDATE_INFORMATION")
            .filter(|v| !v.is_empty())
        {
            args.push("--updateinformation".into());
            args.push(val.into());
        }
        if cfg.on(b"CPACK_APPIMAGE_GUESS_UPDATE_INFORMATION") {
            args.push("--guess".into());
        }
        if let Some(val) = cfg
            .get(b"CPACK_APPIMAGE_COMPRESSOR")
            .filter(|v| !v.is_empty())
        {
            args.push("--comp".into());
            args.push(val.into());
        }
        for opt in cfg.list(b"CPACK_APPIMAGE_MKSQUASHFS_OPTIONS") {
            if !opt.is_empty() {
                args.push("--mksquashfs-opt".into());
                args.push(opt);
            }
        }
        if cfg.on(b"CPACK_APPIMAGE_NO_APPSTREAM") {
            args.push("--no-appstream".into());
        }
        if let Some(val) = cfg
            .get(b"CPACK_APPIMAGE_EXCLUDE_FILE")
            .filter(|v| !v.is_empty())
        {
            args.push("--exclude-file".into());
            args.push(val.into());
        }
        if let Some(val) = cfg
            .get(b"CPACK_APPIMAGE_RUNTIME_FILE")
            .filter(|v| !v.is_empty())
        {
            args.push("--runtime-file".into());
            args.push(val.into());
        }
        if cfg.on(b"CPACK_APPIMAGE_SIGN") {
            args.push("--sign".into());
        }
        if let Some(val) = cfg
            .get(b"CPACK_APPIMAGE_SIGN_KEY")
            .filter(|v| !v.is_empty())
        {
            args.push("--sign-key".into());
            args.push(val.into());
        }

        let tool_cmake = from_native_cmake(&tool_path);
        let mut cmd_display = format!("\"{tool_cmake}\"");
        for arg in &args {
            cmd_display.push_str(&format!(" \"{arg}\""));
        }
        log(host, format!("Running AppImageTool: {cmd_display}"));

        let result = host.run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: from_native(&tool_path),
                    args,
                }],
                cwd: app_dir.to_owned(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: None,
                tee: false,
                default_error_mode: false,
            },
            &mut |_, _| {},
        )?;
        if !result.stdout.is_empty() {
            host.write_out(Stream::Stdout, &result.stdout);
        }
        if !result.stderr.is_empty() {
            host.write_out(Stream::Stderr, &result.stderr);
        }
        match result.results.as_slice() {
            [ProcessResult::Exited(0)] => {}
            [ProcessResult::SpawnFailed(err)] => {
                return Err(error(format!("Cannot start {tool_cmake}: {err}")));
            }
            results => {
                return Err(error(format!("appimagetool failed ({results:?})")));
            }
        }
        Ok(vec![output_path])
    }
}

fn resolve_tool(host: &dyn Host, label: &str, spec: &[u8]) -> Result<PathBuf, CpackError> {
    let raw = to_native(spec);
    if raw.is_absolute() || raw.components().count() > 1 {
        let resolved = absolute(&host.initial_cwd(), spec);
        if host
            .metadata(&resolved, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            return Ok(resolved);
        }
        return Err(error(format!(
            "Could not find {label} executable: {}",
            BString::from(spec)
        )));
    }
    let name = std::str::from_utf8(spec)
        .map_err(|_| error(format!("Invalid UTF-8 in {label} executable path")))?;
    executable_on_absolute_path(host, name)
        .ok_or_else(|| error(format!("Could not find {name} executable in PATH")))
}

fn collect_files(host: &dyn Host, root: &Path) -> Result<Vec<PathBuf>, CpackError> {
    let mut out = Vec::new();
    walk_dir(host, root, Path::new(""), &mut out)?;
    Ok(out)
}

fn walk_dir(
    host: &dyn Host,
    root: &Path,
    rel: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), CpackError> {
    let dir = if rel.as_os_str().is_empty() {
        root.to_owned()
    } else {
        root.join(rel)
    };
    let mut entries = host.read_dir(&dir)?;
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        if entry.name.starts_with(b"._") {
            continue;
        }
        let child_rel = rel.join(to_native(&entry.name));
        let child_full = root.join(&child_rel);
        let meta = host.metadata(&child_full, Follow::No)?;
        match meta.kind {
            FileKind::Dir => walk_dir(host, root, &child_rel, out)?,
            FileKind::File | FileKind::Symlink => out.push(child_rel),
            FileKind::Other => {}
        }
    }
    Ok(())
}

fn find_desktop_file(cfg: &CpackConfig, files: &[PathBuf]) -> Result<PathBuf, CpackError> {
    if let Some(configured) = cfg
        .get(b"CPACK_APPIMAGE_DESKTOP_FILE")
        .filter(|v| !v.is_empty())
    {
        let target = to_native(configured);
        if let Some(found) = files.iter().find(|path| {
            *path == &target
                || path
                    .file_name()
                    .is_some_and(|name| target.file_name().is_some_and(|want| name == want))
        }) {
            return Ok(found.clone());
        }
        return Err(error(format!(
            "Could not find specified desktop file: {}",
            BString::from(configured)
        )));
    }
    files
        .iter()
        .find(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("desktop"))
        })
        .cloned()
        .ok_or_else(|| error("No .desktop file found in AppDir"))
}

fn parse_desktop_entry(bytes: &[u8]) -> (Option<String>, Option<String>) {
    let text = String::from_utf8_lossy(bytes);
    let mut in_entry = false;
    let mut exec = None;
    let mut icon = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_entry = trimmed == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if key == "Exec" && exec.is_none() {
                let first = value
                    .split_whitespace()
                    .next()
                    .map(|tok| tok.trim_matches('"').to_owned());
                if first.as_deref().is_some_and(|s| !s.is_empty()) {
                    exec = first;
                }
            } else if key == "Icon" && icon.is_none() && !value.is_empty() {
                icon = Some(value.to_owned());
            }
        }
    }
    (exec, icon)
}

fn find_icon_file(
    cfg: &CpackConfig,
    desktop_icon: Option<&str>,
    files: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(configured) = cfg.get(b"CPACK_PACKAGE_ICON").filter(|v| !v.is_empty()) {
        let want = to_native(configured);
        let want_name = want.file_name();
        if let Some(found) = files
            .iter()
            .find(|path| *path == &want || (want_name.is_some() && path.file_name() == want_name))
        {
            return Some(found.clone());
        }
    }
    let icon = desktop_icon?;
    files
        .iter()
        .find(|path| {
            let stem_match = path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == icon);
            let name_match = path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == icon);
            stem_match || name_match
        })
        .cloned()
}

fn find_exec_target(desktop_exec: Option<&str>, files: &[PathBuf]) -> Result<PathBuf, CpackError> {
    let Some(exec) = desktop_exec else {
        return Err(error(
            "Desktop file does not specify an Exec entry and no AppRun was provided",
        ));
    };
    let candidate = PathBuf::from(exec);
    if files.iter().any(|path| path == &candidate) {
        return Ok(candidate);
    }
    let want_name = candidate.file_name();
    for preferred in [
        PathBuf::from("bin").join(exec),
        PathBuf::from("usr/bin").join(exec),
    ] {
        if files.iter().any(|path| path == &preferred) {
            return Ok(preferred);
        }
    }
    if let Some(found) = files
        .iter()
        .find(|path| want_name.is_some() && path.file_name() == want_name)
    {
        return Ok(found.clone());
    }
    Ok(candidate)
}

fn link_into_appdir_root(
    host: &dyn Host,
    rel_target: &Path,
    dest: &Path,
) -> Result<(), CpackError> {
    clear_output_file(host, dest)?;
    host.symlink(rel_target, dest)?;
    Ok(())
}

fn posix_shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn resolve_optional_patchelf(
    host: &dyn Host,
    cfg: &CpackConfig,
) -> Result<Option<PathBuf>, CpackError> {
    if let Some(configured) = cfg
        .get(b"CPACK_APPIMAGE_PATCHELF_EXECUTABLE")
        .filter(|v| !v.is_empty())
    {
        return resolve_tool(host, "patchelf", configured).map(Some);
    }
    Ok(executable_on_absolute_path(host, "patchelf"))
}

fn is_elf_file(host: &dyn Host, path: &Path) -> bool {
    if !host
        .metadata(path, Follow::No)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return false;
    }
    host.read(
        path,
        ReadRange::Slice {
            offset: 0,
            len: Some(4),
        },
    )
    .is_ok_and(|magic| magic == b"\x7fELF")
}

fn is_shared_object(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".so") || name.contains(".so.")
}

fn relative_origin_path(from_dir: &Path, to_dir: &Path) -> String {
    let from_parts: Vec<_> = from_dir.components().collect();
    let to_parts: Vec<_> = to_dir.components().collect();
    let common = from_parts
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut rel = String::from("$ORIGIN");
    for _ in common..from_parts.len() {
        rel.push_str("/..");
    }
    for comp in to_parts.iter().skip(common) {
        rel.push('/');
        rel.push_str(&comp.as_os_str().to_string_lossy());
    }
    rel
}

fn patch_elf_rpaths(
    host: &dyn Host,
    cfg: &CpackConfig,
    app_dir: &Path,
    files: &[PathBuf],
) -> Result<(), CpackError> {
    let mut lib_dirs: Vec<PathBuf> = Vec::new();
    for rel in files {
        if is_shared_object(rel)
            && let Some(parent) = rel.parent()
            && !lib_dirs.iter().any(|dir| dir == parent)
        {
            lib_dirs.push(parent.to_path_buf());
        }
    }
    if lib_dirs.is_empty() {
        return Ok(());
    }
    let Some(patchelf) = resolve_optional_patchelf(host, cfg)? else {
        return Ok(());
    };
    let patchelf_program = from_native(&patchelf);
    for rel in files {
        let full = app_dir.join(rel);
        if !is_elf_file(host, &full) {
            continue;
        }
        let from_dir = rel.parent().unwrap_or_else(|| Path::new(""));
        let rpath = lib_dirs
            .iter()
            .map(|dir| relative_origin_path(from_dir, dir))
            .collect::<Vec<_>>()
            .join(":");
        let result = host.run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: patchelf_program.clone(),
                    args: vec!["--set-rpath".into(), rpath.into(), from_native_cmake(&full)],
                }],
                cwd: app_dir.to_owned(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: None,
                tee: false,
                default_error_mode: false,
            },
            &mut |_, _| {},
        )?;
        match result.results.as_slice() {
            [ProcessResult::Exited(0)] => {}
            [ProcessResult::SpawnFailed(err)] => {
                return Err(error(format!(
                    "Cannot start patchelf ({}): {err}",
                    patchelf.display()
                )));
            }
            results => {
                return Err(error(format!(
                    "patchelf failed on {} ({results:?}): {}",
                    from_native_cmake(&full),
                    String::from_utf8_lossy(&result.stderr).trim()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_base::BString;
    use rcmake_host::path::from_native_cmake;
    use rcmake_host::{Follow, Host, ProcessResult, ReadRange, RunOutput, Stream, WriteMode};

    use crate::generators::create_unrestricted;
    use crate::generators::tests::package_host;

    #[test]
    fn appimage_prepares_appdir_and_invokes_appimagetool() {
        let host = package_host();
        let root = host.initial_cwd();
        let payload = root.join("payload");
        host.create_dir_all(&payload.join("bin")).unwrap();
        host.create_dir_all(&payload.join("share/applications"))
            .unwrap();
        host.create_dir_all(&payload.join("share/icons/hicolor/64x64/apps"))
            .unwrap();
        host.write(&payload.join("bin/app"), b"elf", WriteMode::Truncate)
            .unwrap();
        host.set_permissions(&payload.join("bin/app"), 0o755)
            .unwrap();
        host.write(
            &payload.join("share/applications/com.example.app.desktop"),
            b"[Desktop Entry]\nName=App\nExec=app %u\nIcon=ApplicationIcon\nType=Application\n",
            WriteMode::Truncate,
        )
        .unwrap();
        host.write(
            &payload.join("share/icons/hicolor/64x64/apps/ApplicationIcon.png"),
            b"png",
            WriteMode::Truncate,
        )
        .unwrap();

        let tool = root.join("bin/appimagetool");
        host.create_dir_all(&root.join("bin")).unwrap();
        host.write(&tool, b"#!/bin/sh\n", WriteMode::Truncate)
            .unwrap();
        host.set_permissions(&tool, 0o755).unwrap();
        host.env_set(
            b"PATH",
            Some(from_native_cmake(&root.join("bin")).as_slice()),
        );

        let host_cmd = Arc::clone(&host);
        host.set_command_handler(move |_, req, _| {
            let cmd = req.pipeline.first().unwrap();
            assert_eq!(cmd.args.len(), 4);
            assert_eq!(cmd.args[1], "../GeneratorTest-1.2.3-Linux.AppImage");
            assert_eq!(cmd.args[2], "--runtime-file");
            assert_eq!(cmd.args[3], "/runtime.bin");
            let apprun = host_cmd
                .read(&req.cwd.join("AppRun"), ReadRange::All { limit: 4096 })
                .unwrap();
            let apprun_text = String::from_utf8_lossy(&apprun);
            assert!(apprun_text.contains("bin/app"), "{apprun_text}");
            let out = req
                .cwd
                .parent()
                .unwrap()
                .join("GeneratorTest-1.2.3-Linux.AppImage");
            host_cmd
                .write(&out, b"appimage-bytes", WriteMode::Truncate)
                .unwrap();
            Ok(RunOutput {
                stdout: b"Parallel mksquashfs: Using 2 processors\n".to_vec(),
                stderr: b"appimagetool\nSuccess\n".to_vec(),
                results: vec![ProcessResult::Exited(0)],
            })
        })
        .unwrap();

        let config = root.join("CPackConfig.cmake");
        let payload_str = from_native_cmake(&payload);
        host.write(
            &config,
            format!(
                r#"set(CPACK_GENERATOR AppImage)
set(CPACK_PACKAGE_NAME GeneratorTest)
set(CPACK_PACKAGE_VERSION "1.2.3")
set(CPACK_PACKAGE_FILE_NAME "GeneratorTest-1.2.3-Linux")
set(CPACK_TOPLEVEL_TAG "Linux")
set(CPACK_PACKAGE_ICON "ApplicationIcon.png")
set(CPACK_APPIMAGE_RUNTIME_FILE "/runtime.bin")
set(CPACK_INSTALLED_DIRECTORIES "{payload_str};/")
"#
            )
            .as_bytes(),
            WriteMode::Truncate,
        )
        .unwrap();

        let argv = [
            BString::from("cpack"),
            BString::from("--config"),
            from_native_cmake(&config),
        ];
        assert_eq!(
            crate::main_with_factory(&argv, host.clone(), create_unrestricted),
            0
        );
        let stdout = String::from_utf8_lossy(&host.stream_bytes(Stream::Stdout)).into_owned();
        let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
        assert!(stdout.contains("CPack: AppDir:"), "{stdout}");
        assert!(stdout.contains("CPack: Found Desktop file:"), "{stdout}");
        assert!(
            stdout.contains("CPack: Desktop file destination:"),
            "{stdout}"
        );
        assert!(stdout.contains("CPack: Icon file:"), "{stdout}");
        assert!(stdout.contains("CPack: Icon link destination:"), "{stdout}");
        assert!(
            stdout.contains(
                "CPack: No AppRun found, generating a default one that will run: \"bin/app\""
            ),
            "{stdout}"
        );
        assert!(stdout.contains("CPack: Running AppImageTool:"), "{stdout}");
        assert!(stderr.contains("Success"), "{stderr}");
        assert!(
            host.metadata(
                &root.join("GeneratorTest-1.2.3-Linux.AppImage"),
                Follow::Yes
            )
            .is_ok()
        );
    }

    #[test]
    fn appimage_shell_quotes_apprun_and_runs_patchelf_for_shared_libs() {
        let host = package_host();
        let root = host.initial_cwd();
        let payload = root.join("payload");
        host.create_dir_all(&payload.join("bin")).unwrap();
        host.create_dir_all(&payload.join("lib")).unwrap();
        host.create_dir_all(&payload.join("share/applications"))
            .unwrap();
        let weird_bin = "bin/app'$(id)";
        host.write(
            &payload.join(weird_bin),
            b"\x7fELFbinary",
            WriteMode::Truncate,
        )
        .unwrap();
        host.set_permissions(&payload.join(weird_bin), 0o755)
            .unwrap();
        host.write(
            &payload.join("lib/libdemo.so.1"),
            b"\x7fELFshared",
            WriteMode::Truncate,
        )
        .unwrap();
        host.write(
            &payload.join("share/applications/weird.desktop"),
            b"[Desktop Entry]\nName=App\nExec=\"app'$(id)\"\nType=Application\n",
            WriteMode::Truncate,
        )
        .unwrap();

        let bin_dir = root.join("tools");
        host.create_dir_all(&bin_dir).unwrap();
        for tool in ["appimagetool", "patchelf"] {
            let path = bin_dir.join(tool);
            host.write(&path, b"#!/bin/sh\n", WriteMode::Truncate)
                .unwrap();
            host.set_permissions(&path, 0o755).unwrap();
        }
        host.env_set(b"PATH", Some(from_native_cmake(&bin_dir).as_slice()));

        let patchelf_calls = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let patchelf_calls_cmd = Arc::clone(&patchelf_calls);
        let host_cmd = Arc::clone(&host);
        host.set_command_handler(move |_, req, _| {
            let cmd = req.pipeline.first().unwrap();
            let prog = String::from_utf8_lossy(cmd.program.as_slice());
            if prog.ends_with("patchelf") {
                patchelf_calls_cmd.lock().unwrap().push(
                    cmd.args
                        .iter()
                        .map(|a| String::from_utf8_lossy(a.as_slice()).into_owned())
                        .collect(),
                );
                return Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    ..RunOutput::default()
                });
            }
            let apprun = host_cmd
                .read(&req.cwd.join("AppRun"), ReadRange::All { limit: 4096 })
                .unwrap();
            let apprun_text = String::from_utf8_lossy(&apprun);
            assert!(
                apprun_text.contains("exec \"${HERE}/\"'bin/app'\\''$(id)' \"$@\""),
                "{apprun_text}"
            );
            let out = req.cwd.parent().unwrap().join("Weird-1.0-Linux.AppImage");
            host_cmd.write(&out, b"img", WriteMode::Truncate).unwrap();
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })
        .unwrap();

        let config = root.join("CPackConfig.cmake");
        let payload_str = from_native_cmake(&payload);
        host.write(
            &config,
            format!(
                r#"set(CPACK_GENERATOR AppImage)
set(CPACK_PACKAGE_NAME Weird)
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "Weird-1.0-Linux")
set(CPACK_TOPLEVEL_TAG "Linux")
set(CPACK_INSTALLED_DIRECTORIES "{payload_str};/")
"#
            )
            .as_bytes(),
            WriteMode::Truncate,
        )
        .unwrap();
        let argv = [
            BString::from("cpack"),
            BString::from("--config"),
            from_native_cmake(&config),
        ];
        assert_eq!(
            crate::main_with_factory(&argv, host.clone(), create_unrestricted),
            0
        );
        let calls = patchelf_calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(
            calls.iter().any(|args| args[0] == "--set-rpath"
                && args[1] == "$ORIGIN/../lib"
                && args[2].ends_with(weird_bin)),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|args| args[0] == "--set-rpath"
                && args[1] == "$ORIGIN"
                && args[2].ends_with("lib/libdemo.so.1")),
            "{calls:?}"
        );
    }
}
