use rcmake_host::{ReadRange, path::to_native};
use rcmake_model::GeneratorKind;

use super::*;

#[test]
fn app_bundle_copies_configured_plist_and_resources() -> TestResult {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let source = format!("{root}/src");
    let build = format!("{root}/out");
    let mut model = project_with_roots(&source, &build);
    let mut app = target(
        "app",
        TargetKind::Executable,
        &["main.c", "asset.txt", "icon.icns"],
    );
    for (key, value) in [
        ("MACOSX_BUNDLE", "ON"),
        ("OUTPUT_NAME", "renamed"),
        ("MACOSX_BUNDLE_GUI_IDENTIFIER", "org.example.app"),
        ("RESOURCE", "icon.icns;unlisted.txt"),
    ] {
        app.props.set(key.into(), value.into());
    }
    let template = format!("{source}/App.plist.in");
    app.props
        .set("MACOSX_BUNDLE_INFO_PLIST".into(), template.as_str().into());
    let id = model.add_target(app)?;
    let dir = model.dir_mut(DirId::ROOT);
    dir.vars.insert("BUNDLE_LABEL".into(), "Example".into());
    for (file, location) in [("asset.txt", "Resources/sub"), ("icon.icns", "Other")] {
        dir.source_props
            .entry(format!("{source}/{file}").into())
            .or_default()
            .set("MACOSX_PACKAGE_LOCATION".into(), location.into());
    }
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: backtrace(5),
        kind: InstallKind::Targets {
            targets: vec![id],
            export: None,
        },
        options: InstallOptions {
            destination: Some("apps".into()),
            ..Default::default()
        },
    });
    let host = PureHost::default();
    for file in ["main.c", "asset.txt", "icon.icns", "unlisted.txt"] {
        host.seed_file(&Path::new(&source).join(file), b"")?;
    }
    host.seed_file(
        Path::new(&template),
        b"${MACOSX_BUNDLE_EXECUTABLE_NAME}:@MACOSX_BUNDLE_GUI_IDENTIFIER@:@BUNDLE_LABEL@",
    )?;

    for (generator, system, base, resource_dir) in [
        (GeneratorKind::Ninja, "Darwin", "Contents/", "Resources/"),
        (
            GeneratorKind::UnixMakefiles,
            "Darwin",
            "Contents/",
            "Resources/",
        ),
        (GeneratorKind::Ninja, "iOS", "", ""),
    ] {
        model.generator = Some(generator);
        model
            .global
            .props
            .set("CMAKE_SYSTEM_NAME".into(), system.into());
        let result = resolve(&model, &host, &[])?;
        let [graph] = result.graphs.as_slice() else {
            return Err("Expected one build graph.".into());
        };
        let plist = format!("renamed.app/{base}Info.plist");
        let asset = format!("renamed.app/{base}{resource_dir}sub/asset.txt");
        let icon = format!("renamed.app/{base}{resource_dir}icon.icns");
        let resources = graph
            .steps
            .iter()
            .find_map(|step| match step {
                BuildStep::Custom(step) if step.outputs.iter().any(|value| value == &plist) => {
                    Some(step)
                }
                _ => None,
            })
            .ok_or("Missing bundle copy step.")?;
        assert_eq!(
            resources.outputs,
            [plist.as_str(), asset.as_str(), icon.as_str()]
        );
        assert!(graph.steps.iter().any(|step| matches!(
            step,
            BuildStep::Link { inputs, .. } if resources.outputs.iter().all(|path| inputs.contains(path))
        )));
        let plist_input = resources
            .depends
            .iter()
            .find(|path| path.ends_with(b"/Info.plist"))
            .ok_or("Missing configured property list.")?;
        let path = lower::join(build.as_bytes(), plist_input);
        assert_eq!(
            host.read(&to_native(&path), ReadRange::All { limit: 1024 })?,
            b"renamed:org.example.app:Example"
        );
        let wrapper = format!("{build}/renamed.app");
        assert!(result.install.actions.iter().any(|action| matches!(
            &action.kind,
            InstallActionKind::Directory { dirs, .. }
                if dirs.iter().any(|dir| dir == &wrapper)
        )));
    }
    Ok(())
}
