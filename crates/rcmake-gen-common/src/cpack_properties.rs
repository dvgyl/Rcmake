//! Write the install properties that CPack reads.

use std::collections::BTreeMap;

use rcmake_base::{BString, list};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::usage::UsageResolver;

use crate::{GenError, configuration_pattern, genex, quoted_content, script_argument};

/// Render `CPackProperties.cmake`. Return `None` if no install properties are set.
///
/// # Errors
///
/// Return an error if an install property key or generator expression is invalid.
pub fn render_cpack_properties(model: &ProjectModel) -> Result<Option<BString>, GenError> {
    let dir = DirId::ROOT;
    let configs = if model.generator.is_some_and(|kind| kind.multi_config()) {
        model
            .definition(dir, b"CMAKE_CONFIGURATION_TYPES")
            .map_or_else(
                || vec!["Debug".into(), "Release".into(), "RelWithDebInfo".into()],
                |value| list::split(value, list::Empty::Drop),
            )
    } else {
        vec![
            model
                .definition(dir, b"CMAKE_BUILD_TYPE")
                .cloned()
                .unwrap_or_default(),
        ]
    };
    let configs: Vec<_> = configs.into_iter().map(ConfigId).collect();
    render_resolved(model, &configs)
}

pub(crate) fn render_resolved(
    model: &ProjectModel,
    configs: &[ConfigId],
) -> Result<Option<BString>, GenError> {
    let props = &model.global.props;
    let mut records = BTreeMap::<BString, BTreeMap<BString, &BString>>::new();
    for key in props.names() {
        let Some(record) = key.strip_prefix(b"\0INSTALL") else {
            continue;
        };
        // The model stores the native-size file name length before the file name.
        let (size, record) = record
            .split_at_checked(size_of::<usize>())
            .ok_or_else(invalid_key)?;
        let length = usize::from_be_bytes(size.try_into().map_err(|_| invalid_key())?);
        let (file, name) = record.split_at_checked(length).ok_or_else(invalid_key)?;
        if let Some(value) = props.get(&key) {
            records
                .entry(file.into())
                .or_default()
                .insert(name.into(), value);
        }
    }
    if records.is_empty() {
        return Ok(None);
    }
    let fallback = [ConfigId::default()];
    let configs = if configs.is_empty() {
        &fallback
    } else {
        configs
    };
    let multi = configs.len() > 1 || model.generator.is_some_and(|kind| kind.multi_config());
    let mut out = BString::from("# CPack properties\n");
    let mut usage = UsageResolver::new(model);
    for (file, properties) in records {
        for (index, config) in configs.iter().enumerate() {
            if multi {
                out.extend_from_slice(if index == 0 { b"if" } else { b"elseif" });
                out.extend_from_slice(b"(CPACK_BUILD_CONFIG MATCHES ");
                let pattern = configuration_pattern(std::slice::from_ref(&config.0));
                out.push(b'"');
                quoted_content(&mut out, pattern.strip_suffix(b"$").unwrap_or(&pattern));
                out.extend_from_slice(b"$\"");
                out.extend_from_slice(b")\n");
            }
            let context = EvalContext {
                config: config.clone(),
                ..EvalContext::default()
            };
            let mut evaluate = |value: &[u8]| genex(&mut usage, DirId::ROOT, &context, value, None);
            let file = evaluate(&file)?;
            for (property, value) in &properties {
                if multi {
                    out.extend_from_slice(b"  ");
                }
                out.extend_from_slice(b"set_property(INSTALL ");
                script_argument(&mut out, &file);
                out.extend_from_slice(b" PROPERTY ");
                script_argument(&mut out, property);
                out.push(b' ');
                script_argument(&mut out, &evaluate(value)?);
                out.extend_from_slice(b")\n");
            }
        }
        if multi {
            out.extend_from_slice(b"endif()\n");
        }
    }
    Ok(Some(out))
}

fn invalid_key() -> GenError {
    GenError::Invalid("Invalid install property key.".into())
}
