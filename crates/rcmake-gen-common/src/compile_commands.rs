//! `compile_commands.json` (`CMAKE_EXPORT_COMPILE_COMMANDS`).

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::Path,
};

use rcmake_base::{BString, ByteSlice, path_lex::Lexical, shell::quote};
use rcmake_host::{Host, path::from_native};
use rcmake_json::{Value, write_styled};
use rcmake_model::{GeneratorKind, TargetId};
use rcmake_resolve::{BuildGraph, BuildStep, GenerationResult};

use crate::{GenError, write_if_changed};

/// Render `compile_commands.json` for one build graph.
pub fn render(graph: &BuildGraph, build_dir: &[u8]) -> Result<BString, GenError> {
    render_with_directories(
        std::iter::once(graph),
        build_dir,
        &HashMap::new(),
        false,
        None,
    )
}

/// Render `compile_commands.json` with the selected generator's path conventions.
///
/// The Ninja generators write an absolute `output` path and end the file with a
/// newline. Makefiles write the object path relative to the build root.
pub fn render_for_generator(
    result: &GenerationResult,
    graph: &BuildGraph,
    build_dir: &[u8],
    generator: GeneratorKind,
) -> Result<BString, GenError> {
    render_selected(result, graph, build_dir, generator, None)
}

/// Render the compile database with commands from the selected targets.
/// With no selection, include every compile step.
pub fn render_selected(
    result: &GenerationResult,
    graph: &BuildGraph,
    build_dir: &[u8],
    generator: GeneratorKind,
    selection: Option<&HashSet<TargetId>>,
) -> Result<BString, GenError> {
    let graphs = if generator == GeneratorKind::NinjaMultiConfig {
        result.graphs.as_slice()
    } else {
        std::slice::from_ref(graph)
    };
    let objects = selection.map(|selection| {
        graphs
            .iter()
            .flat_map(owned_compile_objects)
            .filter_map(|(id, object)| selection.contains(&id).then_some(object))
            .collect()
    });
    if generator != GeneratorKind::UnixMakefiles {
        return render_with_directories(graphs, build_dir, &HashMap::new(), true, objects.as_ref());
    }

    let target_dirs: HashMap<_, _> = result
        .targets
        .iter()
        .filter(|target| graph.config.matches(&target.config.0))
        .map(|target| (target.id, target.metadata.binary_dir.as_slice()))
        .collect();
    let directories = owned_compile_objects(graph)
        .filter_map(|(id, object)| Some((object, *target_dirs.get(&id)?)))
        .collect();
    render_with_directories(
        std::iter::once(graph),
        build_dir,
        &directories,
        false,
        objects.as_ref(),
    )
}

fn owned_compile_objects(graph: &BuildGraph) -> impl Iterator<Item = (TargetId, &[u8])> {
    graph
        .target_ids
        .iter()
        .zip(&graph.targets)
        .flat_map(move |(&id, target)| {
            target.steps.iter().filter_map(move |&index| {
                let BuildStep::Compile { object, .. } = graph.steps.get(index)? else {
                    return None;
                };
                Some((id, object.as_slice()))
            })
        })
}

fn render_with_directories<'a>(
    graphs: impl IntoIterator<Item = &'a BuildGraph>,
    build_dir: &[u8],
    directories: &HashMap<&[u8], &[u8]>,
    ninja_style: bool,
    objects: Option<&HashSet<&[u8]>>,
) -> Result<BString, GenError> {
    let mut out = BString::from(Vec::with_capacity(64 * 1024));
    out.push(b'[');
    let mut separator = b"\n".as_slice();
    for step in graphs.into_iter().flat_map(|graph| &graph.steps) {
        let BuildStep::Compile {
            object,
            source,
            invocation,
            ..
        } = step
        else {
            continue;
        };
        if objects.is_some_and(|objects| !objects.contains(object.as_slice())) {
            continue;
        }
        let shell = compiler_shell(&invocation.shell);
        out.extend_from_slice(separator);
        separator = b",\n";
        let (directory, command) = if let Some(&directory) = directories.get(object.as_slice()) {
            // Makefiles uses the target directory for the command and the build root for `output`.
            let absolute_object = Lexical::new(build_dir).append(object);
            let local_object = Lexical::new(&absolute_object).relative(Lexical::new(directory));
            let paths = local_object
                .filter(|path| path.as_slice() != object.as_slice())
                .map(|path| (quote(object), quote(&path)));
            (
                directory,
                compile_command(
                    shell,
                    paths
                        .as_ref()
                        .map(|(from, to)| (from.as_slice(), to.as_slice())),
                ),
            )
        } else {
            (
                invocation
                    .working_dir
                    .as_deref()
                    .map_or(build_dir, |v| v.as_ref()),
                compile_command(shell, None),
            )
        };
        // Preserve the compile database's field order and use the JSON writer for values.
        out.extend_from_slice(b"{\n  \"directory\": ");
        json_string(&mut out, directory);
        out.extend_from_slice(b",\n  \"command\": ");
        json_string(&mut out, &command);
        out.extend_from_slice(b",\n  \"file\": ");
        json_string(&mut out, source);
        out.extend_from_slice(b",\n  \"output\": ");
        if ninja_style && !Lexical::new(object).absolute() {
            json_string(&mut out, &Lexical::new(build_dir).append(object));
        } else {
            json_string(&mut out, object);
        }
        out.extend_from_slice(b"\n}");
    }
    if separator == b",\n" {
        out.push(b'\n');
    }
    out.push(b']');
    if ninja_style {
        out.push(b'\n');
    }
    Ok(out)
}

fn compiler_shell(shell: &[u8]) -> &[u8] {
    if shell.find(b"__run_co_compile").is_none() {
        return shell;
    }
    let mut rest = shell;
    let mut previous = b"".as_slice();
    let mut helper = false;
    while !rest.is_empty() {
        rest = rest.trim_ascii_start();
        let (word, tail) = rest.split_at(shell_word_len(rest));
        if word == b"__run_co_compile" && previous == b"-E" {
            helper = true;
        } else if helper && word == b"--" {
            return tail.trim_ascii_start();
        }
        previous = word;
        rest = tail;
    }
    shell
}

fn compile_command<'a>(shell: &'a [u8], object_paths: Option<(&[u8], &[u8])>) -> Cow<'a, [u8]> {
    if object_paths.is_none() && shell.find(b"-M").is_none() {
        return Cow::Borrowed(shell);
    }
    let mut command = Vec::with_capacity(shell.len());
    let mut rest = shell;
    let mut skip_value = false;
    let mut removed_depfile_flags = false;
    while !rest.is_empty() {
        let (word, following) = rest
            .split_at_checked(shell_word_len(rest))
            .unwrap_or((rest, b""));
        if matches!(word, b"-MD" | b"-MMD" | b"-MT" | b"-MF") || skip_value {
            skip_value = !skip_value && matches!(word, b"-MT" | b"-MF");
            removed_depfile_flags = true;
            let spaces = following
                .iter()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            rest = following.get(spaces..).unwrap_or_default();
        } else {
            let word = object_paths
                .filter(|(original, _)| word == *original)
                .map_or(word, |(_, local)| local);
            command.extend_from_slice(word);
            rest = following;
        }
    }
    if removed_depfile_flags {
        Cow::Owned(command.as_slice().trim_ascii_end().to_vec())
    } else {
        Cow::Owned(command)
    }
}

// Keep the original shell quoting, including quoted paths that contain spaces.
fn shell_word_len(input: &[u8]) -> usize {
    let separator = |byte: u8| byte.is_ascii_whitespace() || b";|&()<>".contains(&byte);
    if input.first().copied().is_some_and(separator) {
        return 1;
    }
    let mut quote = None;
    let mut bytes = input.iter().copied().enumerate();
    while let Some((index, byte)) = bytes.next() {
        match (quote, byte) {
            (Some(b'\''), b'\'') | (Some(b'"'), b'"') => quote = None,
            (Some(b'\''), _) => {}
            (_, b'\\') => {
                bytes.next();
            }
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, byte) if separator(byte) => return index,
            _ => {}
        }
    }
    input.len()
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
        // CMake's compile database stores valid UTF-8 directly and escapes JSON control,
        // quote, and backslash bytes.
        json_escaped_valid(out, chunk.valid().as_bytes());
        if !chunk.invalid().is_empty() {
            json_escaped_content(out, chunk.invalid());
        }
    }
    out.push(b'"');
}

fn json_escaped_valid(out: &mut BString, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut start = 0;
    for (i, &byte) in bytes.iter().enumerate() {
        let escape: Option<&[u8]> = match byte {
            b'"' => Some(b"\\\""),
            b'\\' => Some(b"\\\\"),
            0x08 => Some(b"\\b"),
            0x0c => Some(b"\\f"),
            b'\n' => Some(b"\\n"),
            b'\r' => Some(b"\\r"),
            b'\t' => Some(b"\\t"),
            0x00..=0x1f => {
                if start < i {
                    out.extend_from_slice(bytes.get(start..i).unwrap_or_default());
                }
                let hi = HEX.get((byte >> 4) as usize).copied().unwrap_or(b'0');
                let lo = HEX.get((byte & 0x0f) as usize).copied().unwrap_or(b'0');
                out.extend_from_slice(&[b'\\', b'u', b'0', b'0', hi, lo]);
                start = i + 1;
                continue;
            }
            _ => None,
        };
        if let Some(esc) = escape {
            if start < i {
                out.extend_from_slice(bytes.get(start..i).unwrap_or_default());
            }
            out.extend_from_slice(esc);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.extend_from_slice(bytes.get(start..).unwrap_or_default());
    }
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

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;
    use rcmake_model::{BT, DirId, ProjectModel, Target, TargetKind, TargetOrigin};
    use rcmake_resolve::Invocation;

    use super::*;

    fn subdirectory_project(generator: GeneratorKind, binary_dir: &str) -> GenerationResult {
        let mut model = ProjectModel::default();
        model.generator = Some(generator);
        model.set_root_dirs("/src".into(), "/out".into());
        let root = model.dir_mut(DirId::ROOT);
        root.vars.insert("CMAKE_C_COMPILER".into(), "cc".into());
        let child = model
            .add_directory(
                DirId::ROOT,
                "/src/sub".into(),
                binary_dir.into(),
                Default::default(),
            )
            .unwrap();
        let mut target = Target::new(
            "sublib".into(),
            TargetKind::StaticLibrary,
            TargetOrigin::Normal,
            child,
            Default::default(),
            Default::default(),
        );
        target
            .sources
            .push(BT::new("a.c".into(), Default::default()));
        model.add_target(target).unwrap();
        let host = PureHost::default();
        host.seed_file(Path::new("/src/sub/a.c"), b"").unwrap();
        rcmake_resolve::resolve(&model, &host, &[]).unwrap()
    }

    #[test]
    fn ninja_uses_the_build_root_for_subdirectory_commands() {
        let generator = GeneratorKind::Ninja;
        let result = subdirectory_project(generator, "/out/sub");
        let graph = &result.graphs[0];
        let output = render_for_generator(&result, graph, b"/out", generator).unwrap();
        assert_eq!(
            output.to_string(),
            r#"[
{
  "directory": "/out",
  "command": "cc   -o sub/CMakeFiles/sublib.dir/a.c.o -c /src/sub/a.c",
  "file": "/src/sub/a.c",
  "output": "/out/sub/CMakeFiles/sublib.dir/a.c.o"
}
]
"#
        );
    }

    #[test]
    fn makefiles_uses_the_target_directory_for_subdirectory_commands() {
        let generator = GeneratorKind::UnixMakefiles;
        for (directory, object) in [
            ("/out/sub", "sub/CMakeFiles/sublib.dir/a.c.o"),
            ("/out/sub build", "sub build/CMakeFiles/sublib.dir/a.c.o"),
            ("/other build", "/other build/CMakeFiles/sublib.dir/a.c.o"),
        ] {
            let result = subdirectory_project(generator, directory);
            let output = render_for_generator(&result, &result.graphs[0], b"/out", generator)
                .unwrap()
                .to_string();
            assert_eq!(
                output,
                format!(
                    r#"[
{{
  "directory": "{directory}",
  "command": "cc   -o CMakeFiles/sublib.dir/a.c.o -c /src/sub/a.c",
  "file": "/src/sub/a.c",
  "output": "{object}"
}}
]"#
                )
            );
        }
    }

    #[test]
    fn omits_dependency_flags_from_the_database_command() {
        for flag in ["-MD", "-MMD"] {
            let graph = BuildGraph {
                steps: vec![BuildStep::Compile { byproducts: Vec::new(), compile_input: None,
                    object: "obj file.o".into(),
                    source: "source file.c".into(),
                    depfile: Some("obj file.o.d".into()),
                    invocation: Invocation {
                        shell: format!(
                            "cc '-DVARIABLE=-MD -MF' {flag} -MT 'obj file.o' -MF \"obj file.o.d\" -o 'obj file.o' -c 'source file.c'"
                        ).into(),
                        ..Invocation::default()
                    }, modmap: None, dyndep: None, }],
                ..BuildGraph::default()
            };
            let output = render(&graph, b"/work").unwrap().to_string();
            assert!(output.contains(
                r#""command": "cc '-DVARIABLE=-MD -MF' -o 'obj file.o' -c 'source file.c'""#
            ));
        }
    }

    #[test]
    fn json_string_matches_json_writer_and_preserves_utf8() {
        let mut all_ascii = Vec::with_capacity(128);
        for byte in 0..=127_u8 {
            all_ascii.push(byte);
        }
        let mut actual = BString::default();
        json_string(&mut actual, &all_ascii);
        assert_eq!(
            actual,
            write_styled(&Value::string(all_ascii.as_slice()), "  ")
        );

        let mut utf8 = BString::default();
        json_string(&mut utf8, "a\"b\\c\n\t\x01café".as_bytes());
        assert_eq!(utf8, r#""a\"b\\c\n\t\u0001café""#.as_bytes());

        let mut invalid = BString::default();
        json_string(&mut invalid, &[b'a', 0xff, b'b']);
        let expected = format!(
            "\"a{}b\"",
            write_styled(&Value::string([0xff_u8]), "  ")
                .to_string()
                .trim_matches('"')
        );
        assert_eq!(invalid.to_string(), expected);
    }
}
