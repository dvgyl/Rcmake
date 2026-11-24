//! Embed application manifests in MSVC executable and DLL outputs.

mod response;

#[cfg(test)]
mod tests;

use std::io::ErrorKind;
use std::time::Duration;

use rcmake_base::{BString, ByteSlice};
use rcmake_host::{
    CommandSpec, FileKind, Follow, Input, Output, ProcessResult, RunRequest, Stream, WriteMode,
    path::{from_native, to_native},
};

use super::{ToolCtx, error};

pub(super) fn run(command: &[u8], args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let id = if command == b"vs_link_dll" { '2' } else { '1' };
    match Link::parse(id, args, cx).and_then(|link| link.run(cx)) {
        Ok(status) => status,
        Err(message) => error(cx, &[message.as_bytes(), b"\n"]),
    }
}

struct Link {
    id: char,
    intdir: BString,
    rc: BString,
    mt: BString,
    manifests: Vec<BString>,
    tool: CommandSpec,
    target: BString,
    incremental: bool,
    linker_embeds: bool,
    linker_generates: bool,
}

impl Link {
    fn parse(id: char, args: &[BString], cx: &ToolCtx<'_>) -> Result<Self, String> {
        let response::Expanded {
            options,
            tool,
            inspected,
        } = response::expand(args, cx)?;
        let mut link = Self {
            id,
            intdir: BString::default(),
            rc: "rc".into(),
            mt: "mt".into(),
            manifests: Vec::new(),
            tool,
            target: BString::default(),
            incremental: false,
            linker_embeds: true,
            linker_generates: true,
        };
        let mut manifests = false;
        for arg in &options {
            if let Some(dir) = arg.strip_prefix(b"--intdir=") {
                link.intdir = dir.into();
            } else if let Some(rc) = arg.strip_prefix(b"--rc=") {
                if !rc.is_empty() {
                    link.rc = rc.into();
                }
            } else if let Some(mt) = arg.strip_prefix(b"--mt=") {
                if !mt.is_empty() {
                    link.mt = mt.into();
                }
            } else if let Some(version) = arg.strip_prefix(b"--msvc-ver=") {
                if let Ok(version) = String::from_utf8_lossy(version).parse::<u32>() {
                    link.linker_embeds = version > 1600;
                }
            } else if arg == b"--manifests" {
                manifests = true;
                continue;
            } else if manifests && !arg.starts_with(b"-") {
                link.manifests.push(arg.clone());
                continue;
            } else {
                return Err(format!("Unknown link wrapper argument '{arg}'."));
            }
            manifests = false;
        }
        if link.intdir.is_empty() {
            return Err("The link wrapper requires --intdir=<directory>.".into());
        }

        let mut args = inspected.iter();
        while let Some(arg) = args.next() {
            let Some(option) = arg.strip_prefix(b"/").or_else(|| arg.strip_prefix(b"-")) else {
                continue;
            };
            let upper = option.to_ascii_uppercase();
            match upper.as_slice() {
                b"INCREMENTAL" | b"INCREMENTAL:YES" => link.incremental = true,
                b"INCREMENTAL:NO" => link.incremental = false,
                b"MANIFEST:NO" => link.linker_generates = false,
                b"OUT:" | b"FE" => {
                    link.target = args.next().cloned().unwrap_or_default();
                }
                _ => {
                    // Keep /Fe distinct from linker options that begin with /FE.
                    if upper.starts_with(b"OUT:") {
                        link.target = option.get(4..).unwrap_or_default().into();
                    } else if let Some(target) = option.strip_prefix(b"Fe") {
                        link.target = target.strip_prefix(b":").unwrap_or(target).into();
                    }
                }
            }
        }
        if link.target.is_empty() {
            return Err("The linker command requires an output file.".into());
        }
        Ok(link)
    }

    fn run(mut self, cx: &ToolCtx<'_>) -> Result<i32, String> {
        let embed = self.linker_generates || !self.manifests.is_empty();
        if !embed {
            let fallback = self.linker_embeds.then(|| rc_directory(&self.rc)).flatten();
            return Ok(run_tool(cx, "LINK", &self.tool, fallback, false));
        }
        let merged = self.intermediate("embed.manifest");
        let generated = self.intermediate("intermediate.manifest");
        if self.incremental || !self.linker_embeds {
            cx.host
                .create_dir_all(&path(cx, &self.intdir))
                .map_err(|error| error.to_string())?;
            if self.linker_generates {
                self.tool.args.push("/MANIFEST".into());
                self.tool.args.push(concat(b"/MANIFESTFILE:", &generated));
            }
        }

        if self.incremental {
            return self.link_incremental(cx, &merged, &generated);
        }
        if self.linker_embeds {
            self.tool
                .args
                .push(format!("/MANIFEST:EMBED,ID={}", self.id).into());
            self.tool.args.extend(
                self.manifests
                    .iter()
                    .map(|file| concat(b"/MANIFESTINPUT:", file)),
            );
            return Ok(run_tool(
                cx,
                "LINK",
                &self.tool,
                rc_directory(&self.rc),
                false,
            ));
        }

        let link_status = run_tool(cx, "LINK", &self.tool, None, false);
        if link_status != 0 {
            return Ok(link_status);
        }
        let mut output = concat(b"/outputresource:", &self.target);
        output.extend_from_slice(format!(";#{}", self.id).as_bytes());
        self.merge(cx, Some(&generated), output, false)
    }

    fn link_incremental(
        &mut self,
        cx: &ToolCtx<'_>,
        merged: &[u8],
        generated: &[u8],
    ) -> Result<i32, String> {
        let script = self.intermediate("manifest.rc");
        let resource = self.intermediate("manifest.res");
        let manifest_path = path(cx, merged);
        let manifest = from_native(&manifest_path).replace("\\", "/");
        if manifest
            .iter()
            .any(|byte| matches!(byte, b'"' | b'\r' | b'\n'))
        {
            return Err("The manifest path contains a character that RC cannot use.".into());
        }
        let contents = format!(
            "#pragma code_page(65001)\n{} 24 \"{}\"\n",
            self.id,
            String::from_utf8_lossy(&manifest)
        );
        cx.host
            .write(&path(cx, &script), contents.as_bytes(), WriteMode::Truncate)
            .map_err(|error| error.to_string())?;
        let mt_out = concat(b"/out:", merged);
        if !file_exists(cx, merged)? {
            if self.manifests.is_empty() {
                cx.host
                    .write(&manifest_path, b"", WriteMode::CreateNew)
                    .map_err(|error| error.to_string())?;
            } else {
                let status = self.merge(cx, None, mt_out.clone(), false)?;
                if status != 0 {
                    return Ok(status);
                }
            }
        }

        let compile = CommandSpec {
            program: self.rc.clone(),
            args: vec!["/fo".into(), resource.clone(), script],
        };
        self.tool.args.push(resource);
        for (label, tool) in [("RC Pass 1", &compile), ("LINK Pass 1", &self.tool)] {
            let status = run_tool(cx, label, tool, None, false);
            if status != 0 {
                return Ok(status);
            }
        }

        let status = self.merge(cx, Some(generated), mt_out, true)?;
        if !mt_updated(status) {
            return Ok(status);
        }
        let status = run_tool(cx, "RC Pass 2", &compile, None, false);
        if status != 0 {
            return Ok(status);
        }
        Ok(run_tool(cx, "FINAL LINK", &self.tool, None, false))
    }

    fn merge(
        &self,
        cx: &ToolCtx<'_>,
        generated: Option<&[u8]>,
        output: BString,
        notify: bool,
    ) -> Result<i32, String> {
        let mut args = vec!["/nologo".into()];
        if let Some(generated) = generated
            && self.linker_generates
            && file_exists(cx, generated)?
        {
            args.extend(["/manifest".into(), generated.into()]);
        }
        for file in &self.manifests {
            args.extend(["/manifest".into(), file.clone()]);
        }
        args.push(output);
        if notify {
            args.push("/notify_update".into());
        }
        let mt = CommandSpec {
            program: self.mt.clone(),
            args,
        };
        Ok(run_tool(cx, "MT", &mt, None, notify))
    }

    fn intermediate(&self, file: &str) -> BString {
        let mut name = self.intdir.clone();
        if !name.ends_with(b"/") && !name.ends_with(b"\\") {
            name.push(b'/');
        }
        name.extend_from_slice(file.as_bytes());
        name
    }
}

fn concat(prefix: &[u8], value: &[u8]) -> BString {
    let mut text = BString::from(prefix);
    text.extend_from_slice(value);
    text
}

fn file_exists(cx: &ToolCtx<'_>, file: &[u8]) -> Result<bool, String> {
    match cx.host.metadata(&path(cx, file), Follow::Yes) {
        Ok(info) => Ok(info.kind == FileKind::File),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn path(cx: &ToolCtx<'_>, value: &[u8]) -> std::path::PathBuf {
    let value = value.replace(b"\\", b"/");
    if value.starts_with(b"//")
        || matches!(value.as_slice(), [drive, b':', b'/', ..] if drive.is_ascii_alphabetic())
    {
        to_native(&value)
    } else {
        cx.cwd.join(to_native(&value))
    }
}

fn rc_directory(rc: &[u8]) -> Option<&[u8]> {
    let separator = |byte| matches!(byte, b'/' | b'\\');
    let full = rc.starts_with(b"/")
        || rc.starts_with(b"\\\\")
        || matches!(rc, [drive, b':', delimiter, ..]
            if drive.is_ascii_alphabetic() && separator(*delimiter));
    if full {
        let last = rc.iter().rposition(|&byte| separator(byte))?;
        let end = if last == 0 || (last == 2 && rc.get(1) == Some(&b':')) {
            last + 1
        } else {
            last
        };
        return rc.get(..end);
    }
    None
}

fn mt_updated(status: i32) -> bool {
    matches!(status, 0x4102_0001 | 0xbb)
}

fn run_tool(
    cx: &ToolCtx<'_>,
    stage: &str,
    tool: &CommandSpec,
    fallback: Option<&[u8]>,
    accept_update: bool,
) -> i32 {
    let mut env = cx.host.env_all();
    if let Some(dir) = fallback {
        let native_windows = cx.host.system().sysname.eq_ignore_ascii_case(b"Windows");
        let separator = if native_windows || dir.get(1) == Some(&b':') || dir.starts_with(b"\\\\") {
            b';'
        } else {
            b':'
        };
        let dir = if native_windows {
            dir.replace(b"/", b"\\")
        } else {
            dir.to_vec()
        };
        if let Some((_, value)) = env
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(b"PATH"))
        {
            if !value.is_empty() {
                value.push(separator);
            }
            value.extend_from_slice(&dir);
        } else {
            env.push(("PATH".into(), dir.into()));
        }
    }
    if cx.host.env_get(b"VERBOSE").is_some() {
        cx.host.write_out(
            Stream::Stdout,
            format!("{stage}: {}\n", tool.program).as_bytes(),
        );
    }

    let request = RunRequest {
        pipeline: vec![tool.clone()],
        cwd: cx.cwd.to_path_buf(),
        env,
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(Duration::from_secs(120)),
        tee: false,
    };
    match cx.host.run(&request, &mut |_, _| {}) {
        Ok(result) => {
            cx.host.write_out(Stream::Stdout, &result.stdout);
            cx.host.write_out(Stream::Stderr, &result.stderr);
            let status = match result.results.first() {
                Some(ProcessResult::Exited(code)) => *code,
                Some(ProcessResult::Signaled(signal)) => 128_i32.saturating_add(*signal),
                Some(ProcessResult::TimedOut) => 124,
                Some(ProcessResult::SpawnFailed(error)) => {
                    cx.host
                        .write_out(Stream::Stderr, format!("{error}\n").as_bytes());
                    127
                }
                None => 1,
            };
            if status != 0 && !(accept_update && mt_updated(status)) {
                cx.host.write_out(
                    Stream::Stderr,
                    format!("{stage} failed with exit code {status}.\n").as_bytes(),
                );
            }
            status
        }
        Err(err) => error(cx, &[format!("{stage} failed. {err}\n").as_bytes()]),
    }
}
