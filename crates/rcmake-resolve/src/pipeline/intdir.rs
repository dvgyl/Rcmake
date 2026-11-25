//! Validate the target intermediate directory strategies.

use rcmake_host::{Host, Stream};
use rcmake_model::ProjectModel;

use crate::ResolveError;

/// Reject an unknown `CMAKE_INTERMEDIATE_DIR_STRATEGY` or
/// `CMAKE_AUTOGEN_INTERMEDIATE_DIR_STRATEGY`.
///
/// The cache entry has priority over the environment variable. The supported
/// strategies are `FULL` and `SHORT`.
pub(super) fn validate(model: &ProjectModel, host: &dyn Host) -> Result<(), ResolveError> {
    for (name, kind) in [
        (b"CMAKE_INTERMEDIATE_DIR_STRATEGY".as_slice(), ""),
        (b"CMAKE_AUTOGEN_INTERMEDIATE_DIR_STRATEGY", "autogen "),
    ] {
        let value = model
            .cache
            .get(name)
            .map(|entry| entry.value.clone())
            .or_else(|| host.env_get(name));
        let Some(value) = value.filter(|value| !value.is_empty()) else {
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
    Ok(())
}
