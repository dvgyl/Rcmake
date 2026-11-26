use super::*;
use rcmake_model::TargetOrigin;

#[test]
fn evaluates_private_static_dependencies_in_sbom_link_properties()
-> Result<(), Box<dyn std::error::Error>> {
    let mut model = ProjectModel::default();
    let mut library = Target::new(
        "library".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    library.props.set(
        "INTERFACE_LINK_LIBRARIES".into(),
        "$<LINK_ONLY:internal>".into(),
    );
    let id = model.add_target(library)?;
    let library = model.target(id).ok_or("The library is missing.")?;
    let mut usage = UsageResolver::new(&model);
    let link_languages = HashMap::new();
    let links = target_links(
        &model,
        &link_languages,
        &mut usage,
        id,
        library,
        b"",
        PreprocessMode::Build,
    )?;
    assert_eq!(links, [(DirId::ROOT, "internal".into())]);
    Ok(())
}
