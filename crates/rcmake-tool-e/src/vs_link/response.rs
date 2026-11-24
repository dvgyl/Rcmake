//! Expand NMake response files and retain CMake-owned response file arguments.

use rcmake_base::BString;
use rcmake_host::{CommandSpec, ReadRange};

use super::{ToolCtx, path};

pub(super) struct Expanded {
    pub(super) options: Vec<BString>,
    pub(super) tool: CommandSpec,
}

pub(super) fn expand(args: &[BString], cx: &ToolCtx<'_>) -> Result<Expanded, String> {
    let mut budget = 16 * 1024 * 1024;
    let mut expanded = Vec::new();
    for arg in args {
        if let Some(file) = arg.strip_prefix(b"@").filter(|_| !retained(arg)) {
            expanded.extend(read(cx, file, &mut budget)?);
        } else {
            expanded.push(arg.clone());
        }
    }

    let mut linker = expanded.into_iter().peekable();
    let mut options = Vec::new();
    while linker.peek().is_some_and(|arg| arg.starts_with(b"-")) {
        let Some(arg) = linker.next() else { break };
        if arg == b"--" {
            break;
        }
        let manifests = arg == b"--manifests";
        options.push(arg);
        if manifests {
            while let Some(arg) = linker.next_if(|arg| !arg.starts_with(b"-")) {
                options.push(arg);
            }
        }
    }
    let Some(program) = linker.next().filter(|program| !program.is_empty()) else {
        return Err("The link wrapper requires a linker command.".into());
    };
    Ok(Expanded {
        options,
        tool: CommandSpec {
            program,
            args: linker.collect(),
        },
    })
}

fn retained(arg: &[u8]) -> bool {
    arg.starts_with(b"@CMakeFiles") || arg.starts_with(b"@.o/") || arg.starts_with(b"@.o\\")
}

fn read(cx: &ToolCtx<'_>, file: &[u8], budget: &mut u64) -> Result<Vec<BString>, String> {
    let contents = cx
        .host
        .read(&path(cx, file), ReadRange::All { limit: *budget })
        .map_err(|err| {
            format!(
                "Cannot read linker response file '{}'. {err}",
                BString::from(file)
            )
        })?;
    *budget = budget.saturating_sub(contents.len() as u64);
    let words = if let Some(utf16) = contents.strip_prefix(b"\xff\xfe") {
        let (units, remainder) = utf16.as_chunks::<2>();
        if !remainder.is_empty() {
            return Err("The linker response file contains incomplete UTF-16.".into());
        }
        let units = units.iter().copied().map(u16::from_le_bytes);
        let text = char::decode_utf16(units)
            .collect::<Result<String, _>>()
            .map_err(|err| err.to_string())?;
        words(text.as_bytes())
    } else {
        words(contents.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&contents))
    }?;
    if words
        .iter()
        .any(|word| word.starts_with(b"@") && !retained(word))
    {
        return Err("The linker response file contains a nested response file.".into());
    }
    Ok(words)
}

fn words(input: &[u8]) -> Result<Vec<BString>, String> {
    const MAX_ARGS: usize = 262_144;
    let mut result = Vec::new();
    let mut current = BString::default();
    let mut started = false;
    let mut quoted = false;
    let mut slash_count = 0;
    let mut bytes = input.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'\\' {
            slash_count += 1;
            started = true;
            continue;
        }
        if byte == b'"' {
            current.extend(std::iter::repeat_n(b'\\', slash_count / 2));
            if slash_count % 2 != 0 || (quoted && bytes.next_if_eq(&b'"').is_some()) {
                current.push(b'"');
            } else {
                quoted = !quoted;
            }
            slash_count = 0;
            started = true;
            continue;
        }
        current.extend(std::iter::repeat_n(b'\\', slash_count));
        slash_count = 0;
        if byte.is_ascii_whitespace() && !quoted {
            if started {
                if result.len() >= MAX_ARGS {
                    return Err("The linker response file contains too many arguments.".into());
                }
                result.push(std::mem::take(&mut current));
            }
            started = false;
        } else {
            current.push(byte);
            started = true;
        }
    }
    if started {
        if result.len() >= MAX_ARGS {
            return Err("The linker response file contains too many arguments.".into());
        }
        current.extend(std::iter::repeat_n(b'\\', slash_count));
        result.push(current);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_windows_arguments_with_quotes_and_backslashes() {
        let args = super::words(
            br#"/OUT:"target dir/main.exe" "" "/DKEY=\"with space\"" "C:\object dir\leaf\\" "my'file.obj""#,
        ).unwrap();
        assert_eq!(
            args,
            [
                "/OUT:target dir/main.exe",
                "",
                "/DKEY=\"with space\"",
                "C:\\object dir\\leaf\\",
                "my'file.obj",
            ]
        );
    }
}
