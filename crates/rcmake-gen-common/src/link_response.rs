//! Response files for compiler drivers, linkers, and archivers.

use std::path::Path;

use rcmake_base::{BString, hash::HashAlgo, shell::quote};
use rcmake_host::{Host, path};
use rcmake_resolve::{Invocation, ResponseQuoting, ResponseRole, ResponseSpan};

use crate::{GenError, write_if_changed};

/// Maximum POSIX shell argument length, with room for wrappers and termination.
pub const POSIX_BUDGET: usize = 120 * 1024;

/// A generated link input file with a build-relative path.
pub struct File {
    /// Path from the build directory.
    pub path: BString,
    /// Arguments encoded for the tool that reads the file.
    pub contents: BString,
}

impl File {
    /// Write the argument file if its contents changed.
    pub fn write(&self, host: &dyn Host, build: &Path) -> Result<(), GenError> {
        let mut bytes = self.contents.clone();
        bytes.push(b'\n');
        let file = build.join(path::to_native(&self.path));
        if let Some(parent) = file.parent() {
            host.create_dir_all(parent)
                .map_err(|error| GenError::Write {
                    path: path::from_native(parent),
                    message: error.to_string().into(),
                })?;
        }
        write_if_changed(host, &file, &bytes)
    }
}

/// Rewrite selected, possibly adjacent expansions in one invocation.
pub fn rewrite(
    invocation: &Invocation,
    (output, config, index): (&[u8], &[u8], usize),
    build: &Path,
    select: impl Fn(ResponseRole, usize, Option<bool>) -> bool,
) -> Option<(Invocation, Vec<File>)> {
    let settings = invocation.link_response.as_ref()?;
    let flag = settings.flag.trim_ascii_end();
    if flag.is_empty() {
        return None;
    }
    let mut spans: Vec<ResponseSpan> = Vec::new();
    for span in &settings.spans {
        let setting = match span.role {
            ResponseRole::Objects => settings.objects,
            ResponseRole::Libraries => settings.libraries,
        };
        if !select(span.role, span.shell.len(), setting) {
            continue;
        }
        if let Some(previous) = spans
            .last_mut()
            .filter(|previous| previous.argv.end == span.argv.start)
        {
            previous.argv.end = span.argv.end;
            previous.shell.end = span.shell.end;
        } else {
            spans.push(span.clone());
        }
    }
    if spans.is_empty() {
        return None;
    }
    let mut revised = invocation.clone();
    let mut files = Vec::with_capacity(spans.len());
    for (ordinal, span) in spans.into_iter().enumerate().rev() {
        let contents = encode(invocation.argv.get(span.argv.clone())?, settings.quoting);
        let id = HashAlgo::Sha256.hex_digest(&[output, b"\0", config].concat());
        let path: BString = format!("CMakeFiles/rcmake-link-{id}-{index}-{ordinal}.rsp").into();
        let reference_path = if invocation
            .working_dir
            .as_ref()
            .is_some_and(|dir| path::to_native(dir) != build)
        {
            path::from_native(&build.join(path::to_native(&path)))
        } else {
            path.clone()
        };
        let references = if flag.len() == settings.flag.len() {
            vec![[flag, reference_path.as_slice()].concat().into()]
        } else {
            vec![flag.into(), reference_path]
        };
        let shell = references
            .iter()
            .map(|argument| match settings.quoting {
                ResponseQuoting::Gnu => quote(argument),
                ResponseQuoting::Msvc => {
                    let mut out = Vec::new();
                    windows_argument(argument, &mut out, false);
                    out.into()
                }
            })
            .collect::<Vec<_>>();
        revised.argv.splice(span.argv, references);
        revised.shell.splice(span.shell, shell.join(&b' '));
        files.push(File { path, contents });
    }
    revised.link_response = None;
    files.reverse();
    Some((revised, files))
}

fn encode(arguments: &[BString], quoting: ResponseQuoting) -> BString {
    let mut out = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index != 0 {
            out.push(b' ');
        }
        match quoting {
            ResponseQuoting::Gnu => {
                out.push(b'"');
                for &byte in argument.as_slice() {
                    if matches!(byte, b'"' | b'\\') {
                        out.push(b'\\');
                    }
                    out.push(byte);
                }
                out.push(b'"');
            }
            ResponseQuoting::Msvc => windows_argument(argument, &mut out, false),
        }
    }
    out.into()
}

/// Append one argument using Microsoft command-line quoting.
pub fn windows_argument(word: &[u8], out: &mut Vec<u8>, force_quote: bool) {
    if !force_quote && !word.is_empty() && !word.iter().any(|b| matches!(b, b' ' | b'\t' | b'"')) {
        out.extend_from_slice(word);
        return;
    }
    out.push(b'"');
    let mut slashes = 0;
    for &byte in word {
        if byte == b'\\' {
            slashes += 1;
        } else {
            let count = if byte == b'"' {
                2 * slashes + 1
            } else {
                slashes
            };
            out.extend(std::iter::repeat_n(b'\\', count));
            slashes = 0;
            out.push(byte);
        }
    }
    out.extend(std::iter::repeat_n(b'\\', 2 * slashes));
    out.push(b'"');
}
