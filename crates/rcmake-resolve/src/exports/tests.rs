use rcmake_genex::PreprocessMode;

use super::export_property_items;

#[test]
fn export_items_drop_build_local_interface_content() -> Result<(), Box<dyn std::error::Error>> {
    let value = b"$<BUILD_LOCAL_INTERFACE:LOCAL>;$<BUILD_INTERFACE:BUILD>;$<INSTALL_INTERFACE:INSTALL>;$<$<CONFIG:Debug>:$<BUILD_LOCAL_INTERFACE:NESTED>D>";
    let build = export_property_items(value, PreprocessMode::Build)?;
    assert_eq!(build, ["BUILD", "$<$<CONFIG:Debug>:D>"]);
    let install = export_property_items(value, PreprocessMode::Install)?;
    assert_eq!(install, ["INSTALL", "$<$<CONFIG:Debug>:D>"]);
    Ok(())
}
