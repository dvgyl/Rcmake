//! Evaluate and write `file(GENERATE)` requests.

use std::borrow::Cow;
use std::collections::{BTreeMap, btree_map::Entry};
use std::path::{Path, PathBuf};

use rcmake_base::{BString, Limits, path_lex::Lexical};
use rcmake_host::{
    Follow, Host, ReadRange,
    path::{from_native, to_native},
};

use super::{output_error, write_output};
use crate::{GenError, write_error};

/// The source of a `file(GENERATE)` request.
#[derive(Clone, Copy, Debug)]
pub enum FileGenerateInput<'a> {
    /// Evaluate this content as a generator expression.
    Content(&'a [u8]),
    /// Read this file, then evaluate its content as a generator expression.
    File(&'a [u8]),
}

/// Select the permissions of a generated file.
#[derive(Clone, Copy, Debug, Default)]
pub enum FileGeneratePermissions {
    /// Copy the input file mode, or use 0644 for inline content.
    #[default]
    Default,
    /// Use 0644.
    NoSource,
    /// Use the supplied POSIX permission bits.
    Explicit(u32),
}

/// Select the line endings of a generated file.
#[derive(Clone, Copy, Debug, Default)]
pub enum FileGenerateNewline {
    /// Use the host platform's line ending.
    #[default]
    Native,
    /// Use LF.
    Lf,
    /// Use CRLF.
    CrLf,
}

/// One validated `file(GENERATE)` request.
#[derive(Clone, Copy, Debug)]
pub struct FileGenerateRequest<'a> {
    /// Base directory for relative input paths, selected by policy CMP0070.
    pub input_base: &'a Path,
    /// Base directory for relative output paths, selected by policy CMP0070.
    pub output_base: &'a Path,
    /// Output path expression.
    pub output: &'a [u8],
    /// Input file or inline content.
    pub input: FileGenerateInput<'a>,
    /// Optional condition expression. The result must be `0` or `1`.
    pub condition: Option<&'a [u8]>,
    /// Optional target name for generator expression evaluation.
    pub target: Option<&'a [u8]>,
    /// Output file permissions.
    pub permissions: FileGeneratePermissions,
    /// Output line endings.
    pub newline: FileGenerateNewline,
}

/// Configuration and language for one pass of `file(GENERATE)`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileGenerateContext<'a> {
    /// Configuration name. Use an empty name when there is no configuration.
    pub configuration: &'a [u8],
    /// Enabled language, if any.
    pub language: Option<&'a [u8]>,
}

/// Evaluate and write a batch of `file(GENERATE)` requests.
///
/// Pass one context for each configuration and enabled language. Use one context
/// with no language when the project enables no languages. The callback evaluates
/// an expression in the given request and context with the project's generator
/// expression engine. The request supplies the directory and target for lookup.
///
/// Pass all requests together so the writer can check for duplicate outputs. A
/// single request can reuse a file across contexts if the generated content is
/// identical. Return paths in sorted order. Keep timestamps for unchanged files.
pub fn write_generated_files(
    requests: &[FileGenerateRequest<'_>],
    contexts: &[FileGenerateContext<'_>],
    mut evaluate: impl FnMut(
        &[u8],
        &FileGenerateRequest<'_>,
        FileGenerateContext<'_>,
    ) -> Result<BString, GenError>,
    host: &dyn Host,
) -> Result<Vec<PathBuf>, GenError> {
    let mut outputs = BTreeMap::new();
    for (request_index, request) in requests.iter().enumerate() {
        let (input, mode) = generate_input(request, host)?;
        for &context in contexts {
            let name = evaluate(request.output, request, context)?;
            if let Some(condition) = request.condition {
                let result = evaluate(condition, request, context)?;
                match result.as_slice() {
                    b"0" => continue,
                    b"1" => {}
                    _ => {
                        return Err(output_error(
                            &name,
                            format!(
                                "file(GENERATE) condition must be 0 or 1, but evaluated to \"{result}\""
                            ),
                        ));
                    }
                }
            }
            if name.is_empty() {
                return Err(output_error(&name, "file(GENERATE) output path is empty"));
            }

            let path = generate_path(request.output_base, &name);
            let content = evaluate(&input, request, context)?;
            match outputs.entry(path) {
                Entry::Vacant(entry) => {
                    entry.insert((request_index, content, mode, request.newline));
                }
                Entry::Occupied(entry) => {
                    let (previous_request, previous_content, _, _) = entry.get();
                    if *previous_request != request_index || *previous_content != content {
                        let message = if *previous_request == request_index {
                            "file(GENERATE) produced different content for the same output path"
                        } else {
                            "more than one file(GENERATE) request uses the same output path"
                        };
                        return Err(output_error(&from_native(entry.key()), message));
                    }
                }
            }
        }
    }

    let mut paths = Vec::with_capacity(outputs.len());
    for (path, (_, content, mode, newline)) in outputs {
        let crlf = match newline {
            FileGenerateNewline::Native => cfg!(windows),
            FileGenerateNewline::Lf => false,
            FileGenerateNewline::CrLf => true,
        };
        let content = convert_newlines(&content, crlf);
        write_output(host, &path, &content)?;
        if !host
            .metadata(&path, Follow::Yes)
            .is_ok_and(|metadata| metadata.mode & 0o7777 == mode)
        {
            host.set_permissions(&path, mode)
                .map_err(|error| write_error(&path, error))?;
        }
        paths.push(path);
    }
    Ok(paths)
}

fn generate_path(base: &Path, name: &[u8]) -> PathBuf {
    to_native(&Lexical::new(&from_native(&base.join(to_native(name)))).normalize())
}

fn generate_input<'a>(
    request: &FileGenerateRequest<'a>,
    host: &dyn Host,
) -> Result<(Cow<'a, [u8]>, u32), GenError> {
    let (input, path) = match request.input {
        FileGenerateInput::Content(content) => (Cow::Borrowed(content), None),
        FileGenerateInput::File(name) => {
            let path = generate_path(request.input_base, name);
            let bytes = host
                .read(
                    &path,
                    ReadRange::All {
                        limit: Limits::default().max_file_read,
                    },
                )
                .map_err(|error| write_error(&path, error))?;
            let mut bytes = convert_newlines(&bytes, false);
            if !bytes.is_empty() && !bytes.ends_with(b"\n") {
                bytes.push(b'\n');
            }
            (Cow::Owned(bytes), Some(path))
        }
    };
    let mode = match (request.permissions, path) {
        (FileGeneratePermissions::Default, Some(path)) => {
            host.metadata(&path, Follow::Yes)
                .map_err(|error| write_error(&path, error))?
                .mode
                & 0o7777
        }
        (FileGeneratePermissions::Explicit(mode), _) => mode & 0o7777,
        _ => 0o644,
    };
    Ok((input, mode))
}

fn convert_newlines(bytes: &[u8], crlf: bool) -> Vec<u8> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut bytes = bytes.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'\r' && bytes.peek() == Some(&b'\n') {
            continue;
        }
        if crlf && byte == b'\n' {
            result.push(b'\r');
        }
        result.push(byte);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::{PureHost, WriteMode};

    fn generate_request() -> FileGenerateRequest<'static> {
        FileGenerateRequest {
            input_base: Path::new("/work/src"),
            output_base: Path::new("/work/build"),
            output: b"out-$<CONFIG>.txt",
            input: FileGenerateInput::File(b"in.txt"),
            condition: Some(b"$<CONFIG:Release>"),
            target: Some(b"example"),
            permissions: FileGeneratePermissions::Default,
            newline: FileGenerateNewline::CrLf,
        }
    }

    fn evaluate_test_expression(
        value: &[u8],
        request: &FileGenerateRequest<'_>,
        context: FileGenerateContext<'_>,
    ) -> Result<BString, GenError> {
        assert_eq!(request.target, Some(b"example".as_slice()));
        let config = String::from_utf8_lossy(context.configuration);
        Ok(String::from_utf8_lossy(value)
            .replace(
                "$<CONFIG:Release>",
                if config == "Release" { "1" } else { "0" },
            )
            .replace("$<CONFIG>", &config)
            .into())
    }

    #[test]
    fn generates_selected_configuration_with_input_mode_and_line_endings() {
        let host = PureHost::default();
        let request = generate_request();
        host.create_dir_all(request.input_base).unwrap();
        let input = request.input_base.join("in.txt");
        host.write(&input, b"config=$<CONFIG>\r\nlast", WriteMode::Truncate)
            .unwrap();
        host.set_permissions(&input, 0o751).unwrap();
        let contexts = [
            FileGenerateContext {
                configuration: b"Debug",
                language: Some(b"C"),
            },
            FileGenerateContext {
                configuration: b"Release",
                language: Some(b"C"),
            },
            FileGenerateContext {
                configuration: b"Release",
                language: Some(b"CXX"),
            },
        ];
        let output = request.output_base.join("out-Release.txt");
        assert_eq!(
            write_generated_files(&[request], &contexts, evaluate_test_expression, &host).unwrap(),
            std::slice::from_ref(&output)
        );
        assert_eq!(
            host.read(&output, ReadRange::All { limit: 1024 }).unwrap(),
            b"config=Release\r\nlast\r\n"
        );
        assert_eq!(
            host.metadata(&output, Follow::Yes).unwrap().mode & 0o777,
            0o751
        );
        assert!(
            host.metadata(&request.output_base.join("out-Debug.txt"), Follow::Yes)
                .is_err()
        );
    }

    #[test]
    fn rejects_conflicting_generated_outputs() {
        let host = PureHost::default();
        let mut request = FileGenerateRequest {
            output: b"same.txt",
            input: FileGenerateInput::Content(b"$<CONFIG>"),
            condition: None,
            ..generate_request()
        };
        let contexts = [
            FileGenerateContext::default(),
            FileGenerateContext {
                configuration: b"Debug",
                language: None,
            },
        ];
        let error = write_generated_files(&[request], &contexts, evaluate_test_expression, &host)
            .unwrap_err();
        assert!(error.to_string().contains("different content"));
        request.input = FileGenerateInput::Content(b"same");
        let error = write_generated_files(
            &[request, request],
            &contexts,
            evaluate_test_expression,
            &host,
        )
        .unwrap_err();
        assert!(error.to_string().contains("more than one"));
        assert!(
            host.metadata(&request.output_base.join("same.txt"), Follow::Yes)
                .is_err()
        );
    }
}
