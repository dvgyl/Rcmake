use rcmake_genex::PreprocessMode;
use rcmake_model::{DirId, ProjectModel};

use super::export_property_items;

#[test]
fn export_items_preserve_selected_interface_content() -> Result<(), Box<dyn std::error::Error>> {
    let value = b"$<BUILD_LOCAL_INTERFACE:LOCAL>;$<BUILD_INTERFACE:$<BUILD_LOCAL_INTERFACE:NESTED>;BUILD>;$<INSTALL_INTERFACE:INSTALL>;$<$<CONFIG:Debug>:$<BUILD_LOCAL_INTERFACE:LOCAL>D>";
    let build = export_property_items(value, PreprocessMode::Build)?;
    assert_eq!(
        build,
        [
            "$<BUILD_LOCAL_INTERFACE:NESTED>",
            "BUILD",
            "$<$<CONFIG:Debug>:D>"
        ]
    );
    let install = export_property_items(value, PreprocessMode::Install)?;
    assert_eq!(install, ["INSTALL", "$<$<CONFIG:Debug>:D>"]);
    Ok(())
}

#[test]
fn export_paths_use_the_model_dialect() {
    let mut model = ProjectModel::default();
    assert_eq!(super::export_windows_paths(&model, DirId::ROOT), None);
    for (system, windows) in [
        ("Linux", false),
        ("WindowsStore", true),
        ("Darwin", false),
        ("CYGWIN", false),
    ] {
        model
            .global
            .props
            .set("CMAKE_SYSTEM_NAME".into(), system.into());
        assert_eq!(
            super::export_windows_paths(&model, DirId::ROOT),
            Some(windows)
        );
    }
    model.generator = Some(rcmake_model::GeneratorKind::VisualStudio(
        rcmake_model::VsVersion::Vs18,
    ));
    assert_eq!(
        super::export_windows_paths(&model, DirId::ROOT),
        Some(false)
    );
    model.global.props.unset(b"CMAKE_SYSTEM_NAME");
    assert_eq!(super::export_windows_paths(&model, DirId::ROOT), Some(true));
}

#[test]
fn windows_host_paths_stay_absolute_when_targeting_linux() {
    use crate::{sources::SourcePaths, usage::paths::usage_path_is_full};
    use rcmake_host::PureHost;

    let host = PureHost::new("C:/src".into(), "C:/bin/cmake".into());
    let paths = SourcePaths::new(&host);
    let mut model = ProjectModel::default();
    model.set_root_dirs("C:/src".into(), "C:/build".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_SYSTEM_NAME".into(), "Linux".into());
    let check = |model: &ProjectModel| {
        assert!(usage_path_is_full(
            model,
            &paths,
            DirId::ROOT,
            b"C:/src/include"
        ));
        assert_eq!(super::export_windows_paths(model, DirId::ROOT), Some(true));
    };
    check(&model);

    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_HOST_SYSTEM_NAME".into(), "Windows".into());
    check(&model);
    model.set_root_dirs("/src".into(), "/build".into());
    check(&model);
}
