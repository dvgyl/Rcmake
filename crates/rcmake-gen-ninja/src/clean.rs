use std::collections::BTreeSet;
use std::path::Path;

use rcmake_base::{BString, list};
use rcmake_gen_common::GenError;
use rcmake_genex::{EvalContext, TargetHandle, evaluate, parse};
use rcmake_host::Host;
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::usage::UsageResolver;

use crate::{Graph, generation_error, write_if_changed};

impl Graph<'_> {
    pub(crate) fn set_clean_files(
        &mut self,
        model: Option<&ProjectModel>,
        generated: &[BString],
    ) -> Result<(), GenError> {
        let mut files: BTreeSet<_> = generated.iter().cloned().collect();
        let Some(model) = model else {
            self.clean_files = files;
            return Ok(());
        };
        let mut usage = UsageResolver::new(model);
        let mut add = |dir: DirId, target, value: &BString| -> Result<(), GenError> {
            let context = EvalContext {
                config: self.input.config.clone(),
                head: target,
                current: target,
                ..EvalContext::default()
            };
            let (evaluated, _) = parse(value)
                .and_then(|value| evaluate(&value, &context, &mut usage.host(dir)))
                .map_err(|error| generation_error(self.build_dir, error.to_string()))?;
            for path in list::split(&evaluated, list::Empty::Drop) {
                let path = self.commands.path(&path);
                let absolute = if self.commands.absolute(&path) {
                    path
                } else {
                    self.commands
                        .path(&[model.dir(dir).binary_dir.as_slice(), b"/", &path].concat())
                };
                files.insert(absolute);
            }
            Ok(())
        };
        for (id, dir) in model.dirs() {
            if let Some(value) = dir.props.get(b"ADDITIONAL_CLEAN_FILES") {
                add(id, None, value)?;
            }
        }
        for (id, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
            if let Some(value) = target.props.get(b"ADDITIONAL_CLEAN_FILES")
                && let Ok(index) = u32::try_from(id.index())
            {
                add(target.dir, Some(TargetHandle(index)), value)?;
            }
        }
        self.clean_files = files;
        Ok(())
    }
}

pub(crate) fn write_clean_additional_script(
    host: &dyn Host,
    build_dir: &Path,
    graphs: &[&Graph<'_>],
) -> Result<(), GenError> {
    let mut script =
        Vec::from(b"# Additional clean files\ncmake_minimum_required(VERSION 3.16)\n".as_slice());
    for graph in graphs {
        if graph.clean_files.is_empty() {
            continue;
        }
        let config = graph.config.unwrap_or(graph.input.config.0.as_slice());
        script.extend_from_slice(b"\nif(\"${CONFIG}\" STREQUAL \"\" OR \"${CONFIG}\" STREQUAL \"");
        append_cmake_string(&mut script, config);
        script.extend_from_slice(b"\")\n  file(REMOVE_RECURSE\n");
        let entries: BTreeSet<BString> = graph
            .clean_files
            .iter()
            .map(|file| graph.node(file))
            .collect();
        for entry in entries {
            script.extend_from_slice(b"  \"");
            append_cmake_string(&mut script, &entry);
            script.extend_from_slice(b"\"\n");
        }
        script.extend_from_slice(b"  )\nendif()\n");
    }
    write_if_changed(
        host,
        &build_dir.join("CMakeFiles/clean_additional.cmake"),
        &script,
    )
}

fn append_cmake_string(out: &mut Vec<u8>, value: &[u8]) {
    for &byte in value {
        if matches!(byte, b'\\' | b'"' | b'$') {
            out.push(b'\\');
        }
        out.push(byte);
    }
}
