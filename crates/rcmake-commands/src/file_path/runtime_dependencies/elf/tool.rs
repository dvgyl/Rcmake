// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{sync::Arc, time::Duration};

use rcmake_base::{BString, binfmt::ElfDeps, list};
use rcmake_eval::Cx;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Input, Output, ProcessResult, RunRequest,
    path::{executable_on_absolute_path, from_native, from_native_cmake, to_native},
};

use super::{Result, error, header, invalid, native};

pub(super) struct Tools {
    pub(super) objdump: Option<CommandSpec>,
    directories: Option<Arc<[BString]>>,
    freebsd: bool,
}

impl Tools {
    pub(super) fn new(cx: &Cx<'_>, platform: &[u8]) -> Result<Self> {
        let objdump = configured(cx, b"CMAKE_GET_RUNTIME_DEPENDENCIES_COMMAND")?;
        let freebsd = platform == b"freebsd+elf";
        let directories = if let Some(command) = configured(cx, b"CMAKE_LDCONFIG_COMMAND")? {
            Some(ldconfig(cx, command, freebsd)?)
        } else if cx.var_or_empty(b"CMAKE_LDCONFIG_TOOL") == "ldconfig"
            || matches!(
                (cx.host().system().sysname.as_slice(), freebsd),
                (b"Linux", false) | (b"FreeBSD", true)
            )
        {
            default_ldconfig(cx).and_then(|command| ldconfig(cx, command, freebsd).ok())
        } else {
            None
        };
        Ok(Self {
            objdump,
            directories,
            freebsd,
        })
    }

    pub(super) fn system_dirs(&self, machine: u16) -> Arc<[BString]> {
        if let Some(directories) = &self.directories {
            return Arc::clone(directories);
        }
        // Rcmake fallback for cross-host scans and hosts without ldconfig.
        let mut directories = vec!["/lib".into(), "/usr/lib".into()];
        if !self.freebsd {
            match machine {
                62 | 183 | 21 | 22 | 243 | 258 => {
                    directories.extend([BString::from("/lib64"), BString::from("/usr/lib64")]);
                }
                _ => {}
            }
        }
        directories.into()
    }

    pub(super) fn read(&self, cx: &Cx<'_>, path: &[u8]) -> Result<ElfDeps> {
        let mut command = self.objdump.clone().ok_or_else(|| invalid(path))?;
        let file = native(cx, path);
        let bytes = cx
            .host()
            .read(
                &file,
                rcmake_host::ReadRange::Slice {
                    offset: 0,
                    len: Some(20),
                },
            )
            .map_err(|_| invalid(path))?;
        let (_, format) = header(&bytes).ok_or_else(|| invalid(path))?;
        let operand = if path.starts_with(b"-") {
            from_native_cmake(&file)
        } else {
            path.into()
        };
        command.args.extend(["-p".into(), operand]);
        let output = run(cx, command)
            .ok_or_else(|| error([b"Failed to run objdump on:\n  ".as_slice(), path].concat()))?;
        let mut image = ElfDeps {
            format,
            needed: Vec::new(),
            rpaths: Vec::new(),
            runpaths: Vec::new(),
            soname: None,
        };
        for line in output.split(|&byte| byte == b'\n') {
            let line = line.trim_ascii_start();
            let mut parts = line.splitn(2, u8::is_ascii_whitespace);
            let Some(tag) = parts.next() else { continue };
            let value = parts.next().unwrap_or_default().trim_ascii();
            match tag {
                b"NEEDED" => image.needed.push(value.into()),
                b"RPATH" => image
                    .rpaths
                    .extend(value.split(|&byte| byte == b':').map(BString::from)),
                b"RUNPATH" => image
                    .runpaths
                    .extend(value.split(|&byte| byte == b':').map(BString::from)),
                b"SONAME" => image.soname = Some(value.into()),
                _ => {}
            }
        }
        Ok(image)
    }
}

fn configured(cx: &Cx<'_>, key: &[u8]) -> Result<Option<CommandSpec>> {
    let value = cx.var_or_empty(key);
    if value.is_empty() {
        return Ok(None);
    }
    let mut words = list::split(&value, list::Empty::Drop).into_iter();
    let program = words
        .next()
        .ok_or_else(|| error([key, b" is empty"].concat()))?;
    Ok(Some(CommandSpec {
        program,
        args: words.collect(),
    }))
}

fn default_ldconfig(cx: &Cx<'_>) -> Option<CommandSpec> {
    let host = cx.host();
    let path = executable_on_absolute_path(host, "ldconfig").or_else(|| {
        [
            b"/sbin/ldconfig".as_slice(),
            b"/usr/sbin/ldconfig",
            b"/usr/local/sbin/ldconfig",
        ]
        .into_iter()
        .map(to_native)
        .find(|path| {
            host.metadata(path, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
        })
    })?;
    Some(CommandSpec {
        program: from_native(&path),
        args: Vec::new(),
    })
}

fn ldconfig(cx: &Cx<'_>, mut command: CommandSpec, freebsd: bool) -> Result<Arc<[BString]>> {
    let flags: &[&str] = if freebsd {
        &["-r"]
    } else {
        &["-v", "-N", "-X"]
    };
    command
        .args
        .extend(flags.iter().map(|flag| BString::from(*flag)));
    let output = run(cx, command).ok_or_else(|| error("Failed to run ldconfig"))?;
    let mut dirs = Vec::new();
    for line in output.split(|&byte| byte == b'\n') {
        if freebsd {
            if let Some(paths) = line.trim_ascii_start().strip_prefix(b"search directories:") {
                dirs.extend(
                    paths
                        .trim_ascii()
                        .split(|&byte| byte == b':')
                        .filter(|path| path.starts_with(b"/"))
                        .map(BString::from),
                );
            }
        } else if line.starts_with(b"/")
            && let Some(end) = line
                .windows(2)
                .position(|part| part == b": " || part == b":\t")
                .or_else(|| line.trim_ascii_end().strip_suffix(b":").map(<[u8]>::len))
            && let Some(path) = line.get(..end)
        {
            dirs.push(path.into());
        }
    }
    Ok(dirs.into())
}

fn run(cx: &Cx<'_>, command: CommandSpec) -> Option<Vec<u8>> {
    let host = cx.host();
    let mut env = host.env_all();
    env.retain(|(key, _)| !key.eq_ignore_ascii_case(b"LC_ALL"));
    env.push(("LC_ALL".into(), "C".into()));
    let output = host
        .run_bounded(
            &RunRequest {
                pipeline: vec![command],
                cwd: host.initial_cwd(),
                env,
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Null,
                timeout: Some(Duration::from_secs(30)),
                tee: false,
            },
            cx.limits().max_value_bytes.min(4 * 1024 * 1024),
        )
        .ok()?;
    matches!(output.results.as_slice(), [ProcessResult::Exited(0)]).then_some(output.stdout)
}
