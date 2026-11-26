//! Run Windows build commands and prepare their side output paths.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::{BString, ByteSlice, shell::quote};
use rcmake_host::{CommandSpec, Host, HostError, WriteMode, path};
use rcmake_resolve::Invocation;

use crate::create_dir_cached;

mod batch;

pub(super) fn absolute(base: &Path, value: &[u8]) -> PathBuf {
    // PureHost accepts drive and UNC paths on all platforms.
    let value = value.replace(b"\\", b"/");
    if value.starts_with(b"//")
        || matches!(value.as_slice(), [drive, b':', b'/', ..] if drive.is_ascii_alphabetic())
    {
        path::to_native(&value)
    } else {
        base.join(path::to_native(&value))
    }
}

pub(super) fn prepare_outputs(
    host: &dyn Host,
    cwd: &Path,
    args: &[BString],
    created_dirs: &mut HashSet<PathBuf>,
) -> Result<(), HostError> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(option) = arg.strip_prefix(b"/").or_else(|| arg.strip_prefix(b"-")) else {
            continue;
        };
        let upper = option.to_ascii_uppercase();
        let value = if upper.starts_with(b"PDB:") {
            option.get(4..).unwrap_or_default()
        } else if upper.starts_with(b"IMPLIB:") {
            option.get(7..).unwrap_or_default()
        } else if let Some(value) = option.strip_prefix(b"Fd") {
            if value.is_empty() {
                args.next().map_or(b"".as_slice(), |arg| arg.as_slice())
            } else {
                value
            }
        } else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let path = absolute(cwd, value);
        if value.ends_with(b"/") || value.ends_with(b"\\") {
            create_dir_cached(host, &path, created_dirs)?;
        } else if let Some(parent) = path.parent() {
            create_dir_cached(host, parent, created_dirs)?;
        }
    }
    Ok(())
}

pub(super) fn shell_command(
    host: &dyn Host,
    cwd: &Path,
    base_env: &[(BString, BString)],
    command: BatchRule<'_>,
    created_dirs: &mut HashSet<PathBuf>,
) -> Result<(CommandSpec, PathBuf), HostError> {
    // cmd.exe uses its own parser for /C. Put the script in a file so that the
    // native process adapter only needs to pass simple arguments.
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let pid = host.system().pid;
    let mut contents = Vec::from(b"@echo off\r\n".as_slice());
    match command {
        BatchRule::Native(rule) => {
            if rule.contains(&0) {
                return Err(HostError::unsupported(
                    "The Windows build rule contains a null byte.",
                ));
            }
            contents.extend_from_slice(rule);
        }
        BatchRule::Lowered(rule) => {
            contents.extend(batch::render(rule, |args| {
                prepare_outputs(host, cwd, args, created_dirs)
            })?);
        }
    }
    contents.extend_from_slice(b"\r\n");
    let program = base_env
        .iter()
        .find_map(|(name, value)| {
            (name.eq_ignore_ascii_case(b"COMSPEC") && !value.is_empty()).then(|| value.clone())
        })
        .unwrap_or_else(|| "cmd.exe".into());
    for _ in 0..64 {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!("rcmake-build-{pid}-{sequence}.cmd");
        let file = cwd.join(&name);
        match host.write(&file, &contents, WriteMode::CreateNew) {
            Ok(()) => {
                let args = vec![
                    "/D".into(),
                    "/V:OFF".into(),
                    "/C".into(),
                    format!(".\\{name}").into(),
                ];
                return Ok((CommandSpec { program, args }, file));
            }
            Err(error) if error.kind == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(HostError::unsupported(
        "Cannot reserve a file for the Windows build command.",
    ))
}

pub(super) enum BatchRule<'a> {
    Native(&'a [u8]),
    Lowered(&'a [u8]),
}

pub(super) fn batch_rule(invocation: &Invocation) -> Option<(&[BString], BatchRule<'_>)> {
    match invocation.argv.as_slice() {
        [] => Some((&[], BatchRule::Native(&invocation.shell))),
        [launcher @ .., program, option, command]
            if program == b"sh"
                && option == b"-c"
                && (invocation.shell == *command
                    || (!launcher.is_empty()
                        && (invocation.shell.ends_with(command)
                            || invocation.shell.ends_with(&quote(command))))) =>
        {
            Some((launcher, BatchRule::Lowered(command)))
        }
        _ => None,
    }
}
