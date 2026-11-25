use std::collections::BTreeSet;

use rcmake_base::{BString, list};
use rcmake_gen_common::GenError;
use rcmake_genex::{EvalContext, TargetHandle, evaluate, parse};
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::usage::UsageResolver;

use crate::{Graph, generation_error};

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

pub(crate) fn clean_command<'a>(
    graph: &Graph<'_>,
    cmake: &[u8],
    tool: Vec<u8>,
    additional_files: impl IntoIterator<Item = &'a BString>,
) -> Vec<u8> {
    let mut files = additional_files
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .peekable();
    if files.peek().is_none() {
        return tool;
    }
    let mut remove = cmake.to_vec();
    remove.extend_from_slice(b" -E rm -rf --");
    for file in files {
        remove.push(b' ');
        graph.commands.word(file, &mut remove);
    }
    graph.commands.chain(&remove, &tool)
}
