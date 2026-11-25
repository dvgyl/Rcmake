use super::*;

#[test]
fn install_export_can_reference_an_excluded_subdirectory_export() -> TestResult {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let source = format!("{root}/src");
    let build = format!("{root}/out");
    let mut model = project_with_roots(&source, &build);
    let sub = model.add_directory(
        DirId::ROOT,
        format!("{source}/dep").into(),
        format!("{build}/dep").into(),
        Default::default(),
    )?;
    model.dir_mut(sub).exclude_from_all = true;
    let mut dependency = target("dep", TargetKind::InterfaceLibrary, &[]);
    dependency.dir = sub;
    let dependency = model.add_target(dependency)?;
    model.add_alias(sub, "Dep::dep".into(), dependency)?;

    let mut consumer = target("consumer", TargetKind::InterfaceLibrary, &[]);
    consumer.usage.interface_link_libraries = Some(vec![link_item("Dep::dep")]);
    let consumer = model.add_target(consumer)?;

    for (dir, target, set, namespace) in [
        (sub, dependency, "DepTargets", "Dep::"),
        (DirId::ROOT, consumer, "ConsumerTargets", ""),
    ] {
        let index = model.add_install(InstallRule {
            dir,
            backtrace: backtrace(4),
            kind: InstallKind::Targets {
                targets: vec![target],
                export: Some(set.into()),
            },
            options: InstallOptions::default(),
        });
        model.exports.insert(
            set.into(),
            ExportSet {
                name: set.into(),
                targets: vec![target],
                install_rules: vec![index],
            },
        );
        model.add_install(InstallRule {
            dir,
            backtrace: backtrace(5),
            kind: InstallKind::Export {
                set: set.into(),
                namespace: namespace.into(),
                file: None,
                cxx_modules_directory: None,
            },
            options: InstallOptions {
                destination: Some(format!("lib/cmake/{set}").into()),
                ..Default::default()
            },
        });
    }

    let host = PureHost::default();
    let result = resolve(&model, &host, &[])?;
    let [plan] = result.exports.as_slice() else {
        return Err("Expected one parent export plan.".into());
    };
    let [export] = plan.targets.as_slice() else {
        return Err("Expected one exported target.".into());
    };
    assert_eq!(plan.set, "ConsumerTargets");
    assert_eq!(
        export
            .properties
            .get(b"INTERFACE_LINK_LIBRARIES".as_slice()),
        Some(&"Dep::dep".into())
    );
    assert!(!result.install.directories.contains_key(&sub));
    assert_eq!(result.install.actions.len(), 1);
    assert!(
        result
            .install
            .actions
            .iter()
            .any(|action| matches!(action.kind, InstallActionKind::Export { plan: 0 }))
    );

    let rule = model
        .installs
        .iter_mut()
        .find(|rule| rule.dir == sub && matches!(rule.kind, InstallKind::Export { .. }))
        .ok_or("Missing dependency export rule.")?;
    rule.kind = InstallKind::Code("".into());
    let error = resolve(&model, &host, &[])
        .err()
        .ok_or("Expected export resolution to fail without an install export.")?;
    assert!(error.to_string().contains("not in any export set"));
    Ok(())
}
