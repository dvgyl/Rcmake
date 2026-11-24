//! Write the install properties that CPack reads.

use rcmake_base::{BString, list};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::usage::UsageResolver;

use crate::{GenError, configuration_pattern, genex, quoted_content, script_argument};

/// Render `CPackProperties.cmake`. Return `None` if no install properties are set.
///
/// # Errors
///
/// Return an error if a generator expression is invalid.
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
    let fallback = [ConfigId::default()];
    let configs = if configs.is_empty() {
        &fallback
    } else {
        configs
    };
    let multi = configs.len() > 1 || model.generator.is_some_and(|kind| kind.multi_config());
    let mut out = BString::default();
    let mut usage = UsageResolver::new(model);
    for (file, properties) in &model.install_props {
        let mut names = properties.names();
        if names.is_empty() {
            continue;
        }
        names.sort_unstable();
        if out.is_empty() {
            out.extend_from_slice(b"# CPack properties\n");
        }
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
            let file = evaluate(file)?;
            for property in &names {
                let Some(value) = properties.get(property) else {
                    continue;
                };
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
    Ok((!out.is_empty()).then_some(out))
}
