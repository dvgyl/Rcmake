//! Deterministic Visual Studio project, solution, and folder identifiers.

use md5::{Digest, Md5};
use rcmake_model::ProjectModel;

const NAMESPACE: [u8; 16] = [
    0xee, 0x30, 0xc4, 0xbe, 0x51, 0x92, 0x4f, 0xb0, 0xb3, 0x35, 0x72, 0x2a, 0x2d, 0xff, 0xe7, 0x60,
];

/// Get the cached GUID for a name, or calculate the CMake version-3 UUID.
///
/// CMake uses a non-empty `<name>_GUID_CMAKE` cache value exactly as given.
/// For example, `include_external_msproject(ext ext.proj GUID aaa-bbb)`
/// stores `aaa-bbb`, and the solution then refers to `{aaa-bbb}`.
#[must_use]
pub fn for_name(model: &ProjectModel, binary_dir: &str, name: &str) -> String {
    let key = format!("{name}_GUID_CMAKE");
    if let Some(cached) = model.cache.get(key.as_bytes())
        && !cached.value.is_empty()
    {
        return format!("{{{}}}", String::from_utf8_lossy(&cached.value));
    }

    let mut hash = Md5::new();
    hash.update(NAMESPACE);
    hash.update(binary_dir.as_bytes());
    hash.update(b"|");
    hash.update(name.as_bytes());
    let mut bytes: [u8; 16] = hash.finalize().into();
    let [_, _, _, _, _, _, version, _, variant, ..] = &mut bytes;
    *version = (*version & 0x0f) | 0x30;
    *variant = (*variant & 0x3f) | 0x80;

    use std::fmt::Write;
    let mut result = String::from("{");
    for (index, byte) in bytes.into_iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            result.push('-');
        }
        let _ = write!(result, "{byte:02X}");
    }
    result.push('}');
    result
}

pub(crate) fn bare(guid: &str) -> String {
    guid.trim_matches(['{', '}']).to_ascii_lowercase()
}
