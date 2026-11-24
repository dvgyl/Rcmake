use rcmake_diag::PolicyStatus;
use rcmake_genex::{ConfigId, EvalContext, GenexHost, evaluate, parse};
use rcmake_host::path::from_native_cmake;
use rcmake_model::ProjectModel;

use super::{SourcePaths, absolute_path};
use crate::{
    ResolveError,
    file_sets::{file_generate_contexts, file_generate_policy},
    usage::UsageResolver,
};

impl SourcePaths<'_> {
    pub(super) fn index_file_generations(
        &mut self,
        model: &ProjectModel,
        config: &ConfigId,
        usage: &mut UsageResolver<'_>,
    ) -> Result<(), ResolveError> {
        let props = &model.global.props;
        let Some(count) = props
            .get(b"_RCMAKE_FILE_GENERATE_COUNT")
            .and_then(|value| number(value))
        else {
            return Ok(());
        };
        let contexts = file_generate_contexts(model, std::slice::from_ref(config));
        let cwd = from_native_cmake(&self.host.initial_cwd());
        let directories: Vec<_> = model.dirs().collect();
        for index in 0..count {
            let key = format!("_RCMAKE_FILE_GENERATE_{index}");
            let get = |field: &str| props.get(format!("{key}_{field}").as_bytes());
            let directory = get("DIRECTORY")
                .and_then(|value| number(value))
                .and_then(|dir| directories.get(dir));
            let (Some(&(id, dir)), Some(output)) = (directory, get("ARG1")) else {
                break;
            };
            let base = if file_generate_policy(model, id, index) == PolicyStatus::New {
                &dir.binary_dir
            } else {
                &cwd
            };
            let ast = parse(output)?;
            let mut host = usage.host(id);
            let mut options = (4..).map_while(|arg| get(&format!("ARG{arg}")));
            let mut target = None;
            while let Some(option) = options.next() {
                match option.as_slice() {
                    b"TARGET" => {
                        target = options.next().and_then(|name| host.find_target(name));
                        break;
                    }
                    b"CONDITION" | b"NEWLINE_STYLE" => {
                        options.next();
                    }
                    _ => {}
                }
            }
            for context in &contexts {
                let context = EvalContext {
                    current: target,
                    head: target,
                    ..(*context).clone()
                };
                let output = evaluate(&ast, &context, &mut host)?.0;
                if !output.is_empty() {
                    self.generated
                        .entry(absolute_path(base, &output))
                        .or_default()
                        .insert(dir.binary_dir.clone());
                }
            }
        }
        Ok(())
    }
}

fn number(value: &[u8]) -> Option<usize> {
    std::str::from_utf8(value).ok()?.parse().ok()
}
