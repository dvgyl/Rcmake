//! Regenerate Ninja manifests when an input changes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{Host, ReadRange, path};
use rcmake_resolve::GenerationResult;

use crate::{
    commands::Commands,
    graph::Graph,
    syntax::{Edge, Ninja},
};

pub(super) fn emit_regeneration(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    outputs: &[BString],
    result: &GenerationResult,
    regenerate: &[u8],
    verify: Option<&[u8]>,
) {
    let mut seen = HashSet::new();
    let mut listfiles: Vec<_> = result
        .metadata
        .listfiles
        .iter()
        .map(|file| graph.node(file))
        .filter(|file| !outputs.contains(file) && seen.insert(file.clone()))
        .collect();

    if let Some(cmake) = verify {
        let script = graph.node(rcmake_gen_common::glob::SCRIPT.as_bytes());
        let stamp = graph.node(rcmake_gen_common::glob::STAMP.as_bytes());
        let force = graph.node(b"CMakeFiles/VerifyGlobs.force");
        ninja.phony(&force, &[]);
        ninja.edge(
            b"VERIFY_GLOBS",
            Edge {
                outputs: std::slice::from_ref(&stamp),
                implicit_inputs: &[script.clone(), force],
                ..Edge::default()
            },
        );
        let mut command = cmake.to_vec();
        command.extend_from_slice(b" -P ");
        graph.commands.word(&script, &mut command);
        ninja.binding(b"COMMAND", &command);
        listfiles.push(stamp);
    }

    ninja.edge(
        b"RERUN_CMAKE",
        Edge {
            outputs,
            implicit_inputs: &listfiles,
            ..Edge::default()
        },
    );
    ninja.binding(b"COMMAND", regenerate);

    for listfile in &listfiles {
        // Missing listfiles must trigger regeneration so CMake can check them.
        if *listfile != graph.node(rcmake_gen_common::glob::STAMP.as_bytes())
            && !graph.has_target(listfile)
        {
            ninja.phony(listfile, &[]);
        }
    }
}

pub(super) fn source_directory(result: &GenerationResult, build_dir: &Path) -> PathBuf {
    result
        .metadata
        .listfiles
        .iter()
        .map(|file| path::to_native(file))
        .find(|file| {
            file.file_name()
                .is_some_and(|name| name == "CMakeLists.txt")
        })
        .and_then(|file| file.parent().map(Path::to_path_buf))
        .or_else(|| {
            result
                .targets
                .first()
                .map(|target| path::to_native(&target.metadata.source_dir))
        })
        .map(|source| build_dir.join(source))
        .unwrap_or_else(|| build_dir.to_path_buf())
}

pub(super) fn configure_command(
    result: &GenerationResult,
    build_dir: &Path,
    cmake: &[u8],
    commands: &Commands,
    rerun: bool,
) -> Vec<u8> {
    let source_dir = source_directory(result, build_dir);
    let mut command = cmake.to_vec();
    if rerun {
        command.extend_from_slice(b" --regenerate-during-build");
    }
    let option = |prefix: &[u8], directory: &Path, command: &mut Vec<u8>| {
        command.push(b' ');
        let directory = path::from_native(directory);
        if commands.windows() {
            commands.word(&[prefix, &directory].concat(), command);
        } else {
            command.extend_from_slice(prefix);
            commands.word(&directory, command);
        }
    };
    option(b"-S", &source_dir, &mut command);
    option(b"-B", build_dir, &mut command);
    command
}

pub(super) fn configuration_setting(
    host: &dyn Host,
    build_dir: &Path,
    name: &[u8],
) -> Option<BString> {
    host.env_get(name)
        .or_else(|| cache_setting(&read_cache(host, build_dir)?, name))
}

pub(super) fn read_cache(host: &dyn Host, build_dir: &Path) -> Option<Vec<u8>> {
    host.read(
        &build_dir.join("CMakeCache.txt"),
        ReadRange::All {
            limit: 16 * 1024 * 1024,
        },
    )
    .ok()
}

pub(super) fn cache_setting(cache: &[u8], name: &[u8]) -> Option<BString> {
    cache.split(|&byte| byte == b'\n').find_map(|line| {
        let entry = line.strip_prefix(name)?.strip_prefix(b":")?;
        let value = entry.splitn(2, |&byte| byte == b'=').nth(1)?;
        Some(BString::from(value.strip_suffix(b"\r").unwrap_or(value)))
    })
}
