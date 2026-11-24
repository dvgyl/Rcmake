use rcmake_base::{BString, ByteSlice, list};
use rcmake_model::{BT, FileSet, FileSetKind, ProjectModel};

use crate::ResolveError;

pub(super) fn prepare(model: &ProjectModel) -> Result<Option<ProjectModel>, ResolveError> {
    let mut prepared = None;
    for (target_id, target) in model.targets() {
        for &input_id in &target.file_sets {
            let Some(input) = model.file_set(input_id) else {
                continue;
            };
            let Some(rule_name) = input.props.get(b"_RCMAKE_RULE") else {
                continue;
            };
            let input_dir_id = model.file_set_rule_dir(input_id).unwrap_or(target.dir);
            let input_dir = model.dir(input_dir_id);
            let Some(rule) = model.custom_rule(input_dir_id, rule_name) else {
                continue;
            };
            let Some(output) = rule.get(b"OUTPUT_FILE_SET") else {
                continue;
            };
            let entries = list::split(output, list::Empty::Keep);
            let [name, kind] = entries.as_slice() else {
                continue;
            };
            let variables = [
                (b"<RULE>".as_slice(), rule_name.as_slice()),
                (b"<TARGET>", target.name.as_slice()),
                (b"<FILE_SET>", input.name.as_slice()),
            ];
            let name = expand(name.clone(), &variables);
            if name.contains(&b'<') || name.contains(&b'>') {
                continue;
            }
            let kind = FileSetKind::from_name(kind).unwrap_or(if kind == "SYCL_HEADERS" {
                FileSetKind::Headers
            } else {
                FileSetKind::Sources
            });
            let existing = prepared.as_ref().unwrap_or(model);
            let output_id = existing
                .target(target_id)
                .into_iter()
                .flat_map(|target| &target.file_sets)
                .copied()
                .find(|&id| existing.file_set(id).is_some_and(|set| set.name == name));
            if output_id
                .and_then(|id| existing.file_set(id))
                .is_some_and(|set| set.kind != kind || !set.base_dirs.is_empty())
            {
                continue;
            }
            let output_dir = model.dir(target.dir);
            let directories = [
                (
                    b"<CURRENT_SOURCE_DIR>".as_slice(),
                    input_dir.source_dir.as_slice(),
                ),
                (b"<CURRENT_BINARY_DIR>", input_dir.binary_dir.as_slice()),
                (
                    b"<OUTPUT_CURRENT_SOURCE_DIR>",
                    output_dir.source_dir.as_slice(),
                ),
                (
                    b"<OUTPUT_CURRENT_BINARY_DIR>",
                    output_dir.binary_dir.as_slice(),
                ),
            ];
            let raw = rule
                .get(b"OUTPUT_FILE_SET_BASE_DIRS")
                .cloned()
                .unwrap_or_else(|| "<CURRENT_BINARY_DIR>".into());
            let bases = expand(expand(raw, &variables), &directories);
            let base_dirs = list::split(&bases, list::Empty::Drop)
                .into_iter()
                .map(|dir| BT::new(dir, target.backtrace.clone()))
                .collect();
            let prepared = prepared.get_or_insert_with(|| model.clone());
            if let Some(id) = output_id {
                if let Some(set) = prepared.file_set_mut(id) {
                    set.base_dirs = base_dirs;
                }
            } else {
                prepared
                    .add_file_set(FileSet {
                        target: target_id,
                        name,
                        kind,
                        scope: input.scope,
                        base_dirs,
                        files: Vec::new(),
                        props: Default::default(),
                    })
                    .map_err(|error| ResolveError::Message(error.to_string().into()))?;
            }
        }
    }
    Ok(prepared)
}

fn expand(mut value: BString, variables: &[(&[u8], &[u8])]) -> BString {
    for &(marker, replacement) in variables {
        let extra = replacement.len().saturating_sub(marker.len());
        let growth = value.find_iter(marker).count().saturating_mul(extra);
        if value.len().saturating_add(growth) > 65_536 {
            break;
        }
        value = value.replace(marker, replacement).into();
    }
    value
}
