//! Validate the target intermediate directory strategies.

use rcmake_host::{Host, Stream};
use rcmake_model::{CacheEntry, CacheType, ProjectModel};

use crate::ResolveError;

/// Reject an unknown `CMAKE_INTERMEDIATE_DIR_STRATEGY` or
/// `CMAKE_AUTOGEN_INTERMEDIATE_DIR_STRATEGY`.
///
/// The cache entry has priority over the environment variable. The supported
/// strategies are `FULL` and `SHORT`.
pub(super) fn prepare(
    model: &ProjectModel,
    host: &dyn Host,
) -> Result<Option<ProjectModel>, ResolveError> {
    let mut initialized = None;
    for (name, kind) in [
        (b"CMAKE_INTERMEDIATE_DIR_STRATEGY".as_slice(), ""),
        (b"CMAKE_AUTOGEN_INTERMEDIATE_DIR_STRATEGY", "autogen "),
    ] {
        let value = if let Some(entry) = model.cache.get(name) {
            entry.value.clone()
        } else if let Some(value) = host.env_get(name) {
            initialized
                .get_or_insert_with(|| model.clone())
                .cache
                .insert(
                    name.into(),
                    CacheEntry {
                        value: value.clone(),
                        ty: CacheType::String,
                        help: Default::default(),
                        advanced: false,
                        props: Default::default(),
                    },
                );
            value
        } else {
            continue;
        };
        if matches!(value.as_slice(), b"FULL" | b"SHORT") {
            continue;
        }
        let body = format!("Unsupported {kind}intermediate directory strategy '{value}'");
        host.write_out(
            Stream::Stderr,
            format!("CMake Error:\n  {body}\n\n\n").as_bytes(),
        );
        return Err(ResolveError::Reported(body.into()));
    }
    Ok(initialized)
}
