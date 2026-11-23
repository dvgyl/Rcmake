//! `compile_commands.json` (`CMAKE_EXPORT_COMPILE_COMMANDS`).

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{Host, path::from_native};
use rcmake_json::{Value, write_styled};
use rcmake_resolve::{BuildGraph, BuildStep};

use crate::{GenError, write_if_changed};

/// Render `compile_commands.json` for one build graph.
pub fn render(graph: &BuildGraph, build_dir: &[u8]) -> Result<BString, GenError> {
    let mut out = BString::from("[");
    let mut separator = b"\n".as_slice();
    for step in &graph.steps {
        let BuildStep::Compile {
            object,
            source,
            invocation,
            ..
        } = step
        else {
            continue;
        };
        out.extend_from_slice(separator);
        separator = b",\n";
        let directory = invocation
            .working_dir
            .as_deref()
            .map_or(build_dir, |v| v.as_ref());
        // Preserve the compile database's field order and use the JSON writer for values.
        out.extend_from_slice(b"{\n  \"directory\": ");
        json_string(&mut out, directory);
        out.extend_from_slice(b",\n  \"command\": ");
        json_string(&mut out, &invocation.shell);
        out.extend_from_slice(b",\n  \"file\": ");
        json_string(&mut out, source);
        out.extend_from_slice(b",\n  \"output\": ");
        json_string(&mut out, object);
        out.extend_from_slice(b"\n}");
    }
    if separator == b",\n" {
        out.push(b'\n');
    }
    out.push(b']');
    Ok(out)
}

/// Write the compile database to an existing build directory.
pub fn write_compile_commands(
    graph: &BuildGraph,
    build_dir: &Path,
    host: &dyn Host,
) -> Result<(), GenError> {
    let content = render(graph, &from_native(build_dir))?;
    write_if_changed(host, &build_dir.join("compile_commands.json"), &content)
}

fn json_string(out: &mut BString, value: &[u8]) {
    out.push(b'"');
    for chunk in value.utf8_chunks() {
        for part in chunk
            .valid()
            .as_bytes()
            .chunk_by(|a, b| a.is_ascii() == b.is_ascii())
        {
            if part.is_ascii() {
                json_escaped_content(out, part);
            } else {
                // CMake's compile database stores UTF-8 directly.
                out.extend_from_slice(part);
            }
        }
        if !chunk.invalid().is_empty() {
            json_escaped_content(out, chunk.invalid());
        }
    }
    out.push(b'"');
}

fn json_escaped_content(out: &mut BString, value: &[u8]) {
    let encoded = write_styled(&Value::string(value), "  ");
    if let Some(content) = encoded
        .strip_prefix(b"\"")
        .and_then(|s| s.strip_suffix(b"\""))
    {
        out.extend_from_slice(content);
    }
}
