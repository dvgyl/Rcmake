pub(crate) mod arch;

use rcmake_base::path_lex::Lexical;
use rcmake_model::{DirId, ProjectModel};

use super::definition;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    Simulator,
    Catalyst,
}

pub(crate) fn system(model: &ProjectModel, dir: DirId) -> &[u8] {
    definition(model, dir, b"CMAKE_SYSTEM_NAME").map_or(b"Darwin", |value| value.as_slice())
}

pub(crate) fn variant(model: &ProjectModel, dir: DirId) -> Option<Variant> {
    let sdk = definition(model, dir, b"CMAKE_OSX_SYSROOT")?;
    let name = Lexical::new(sdk).filename().to_ascii_lowercase();
    if name
        .windows(b"simulator".len())
        .any(|part| part == b"simulator")
    {
        Some(Variant::Simulator)
    } else if system(model, dir) == b"iOS" && name.starts_with(b"macosx") {
        Some(Variant::Catalyst)
    } else {
        None
    }
}

pub(crate) fn plist_system(platform: &[u8]) -> Option<&'static [u8]> {
    match platform {
        b"macos" => Some(b"Darwin"),
        b"ios" => Some(b"iOS"),
        b"tvos" => Some(b"tvOS"),
        b"watchos" => Some(b"watchOS"),
        b"xros" => Some(b"visionOS"),
        _ => None,
    }
}
