use rcmake_base::list;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext, GenexHost, evaluate, parse};
use rcmake_model::ProjectModel;

use super::{SourcePaths, absolute_path};
use crate::{ResolveError, usage::UsageResolver};

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
        let mut languages = props
            .get(b"ENABLED_LANGUAGES")
            .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
        languages.retain(|language| language != "NONE");
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
            let use_directory = dir
                .final_policies
                .is_some_and(|policies| policies.get(PolicyId::new(70)) == PolicyStatus::New);
            let base = if use_directory {
                &dir.binary_dir
            } else {
                &dir.source_dir
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
            let no_language = languages.is_empty().then_some(None);
            for language in languages.iter().cloned().map(Some).chain(no_language) {
                let context = EvalContext {
                    config: config.clone(),
                    language,
                    current: target,
                    head: target,
                    ..EvalContext::default()
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
