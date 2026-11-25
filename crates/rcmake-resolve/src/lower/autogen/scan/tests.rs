use rcmake_diag::{Backtrace, PolicyMap};
use rcmake_host::{HostError, PureHost};
use rcmake_model::{DirId, ImportedVisibility, TargetKind, TargetOrigin};

use super::*;

#[test]
fn rcc_probes_short_help_and_uses_stdout() -> Result<(), Box<dyn std::error::Error>> {
    let host = PureHost::new("/".into(), "/cmake".into());
    for (stdout, stderr, expected) in [
        ("See (--list).", "-list", Some("--list")),
        ("Use -list.", "--list", Some("-list")),
        ("Usage", "--list", None),
    ] {
        host.set_command_handler(move |_, request, _| {
            let [command] = request.pipeline.as_slice() else {
                return Err(HostError::unsupported("Expected one rcc command."));
            };
            if command.program != b"/qt/rcc" || command.args != ["-h"] {
                return Err(HostError::unsupported("Expected rcc -h."));
            }
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(1)],
                stdout: stdout.as_bytes().into(),
                stderr: stderr.as_bytes().into(),
            })
        })?;
        assert_eq!(
            rcc_list_options(&host, b"/qt/rcc", b"/")?,
            expected.into_iter().map(BString::from).collect::<Vec<_>>(),
        );
    }
    Ok(())
}

#[test]
fn rcc_checks_imported_paths_and_preserves_explicit_overrides()
-> Result<(), Box<dyn std::error::Error>> {
    let host = PureHost::new("/".into(), "/cmake".into());
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/build".into());
    let make_target = |name: &str, origin| {
        Target::new(
            name.into(),
            TargetKind::Executable,
            origin,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        )
    };
    let mut rcc = make_target(
        "Qt6::rcc",
        TargetOrigin::Imported(ImportedVisibility::Global),
    );
    rcc.props
        .set("IMPORTED_LOCATION".into(), "/qt/missing".into());
    model.add_target(rcc)?;
    let mut app = make_target("app", TargetOrigin::Normal);
    app.props.set("AUTORCC".into(), "ON".into());
    let id = model.add_target(app.clone())?;
    let find = |app: &Target| {
        ToolDiscovery {
            model: &model,
            host: &host,
            id,
            target: app,
            version: QtVersion(6, 8),
            configurations: &[],
            per_config: false,
        }
        .find(Tool::Rcc)
    };
    let Err(error) = find(&app) else {
        return Err("Expected an invalid imported rcc path.".into());
    };
    assert!(error.to_string().contains("Qt6::rcc"));
    assert!(error.to_string().contains("/qt/missing"));

    app.props
        .set("AUTORCC_EXECUTABLE".into(), "/custom/rcc".into());
    let plan = find(&app)?;
    assert_eq!(plan.executable.default, "/custom/rcc");
    assert!(plan.list_options.default.is_empty());
    Ok(())
}

#[test]
fn resource_parser_preserves_order_and_duplicates() -> Result<(), &'static str> {
    assert_eq!(
        parse_resource_entries(b"<RCC><qresource><file alias=\"a\">one.txt</file><file>one.txt</file><file> two.txt </file></qresource></RCC>")?,
        ["one.txt", "one.txt", "one.txt", " two.txt ", " two.txt "],
    );
    assert_eq!(
        parse_resource_entries(b"<file>first</file><file>second</file>")?,
        ["first", "second"]
    );
    Ok(())
}

#[test]
fn source_kinds_cover_headers_and_module_sources() {
    assert_eq!(source_kind(b"hh", false), Some(ScanKind::Header));
    assert_eq!(source_kind(b"cpp", false), Some(ScanKind::Source));
    assert_eq!(source_kind(b"cpp", true), Some(ScanKind::Module));
    assert_eq!(source_kind(b"qrc", false), None);
    assert_eq!(
        version_from_output(b"Qt Meta Object Compiler version 6.8.1"),
        Some(QtVersion(6, 8))
    );
}

#[test]
fn source_options_replace_existing_values() {
    let mut opts = ["-name", "default", "-compress", "1"]
        .map(BString::from)
        .to_vec();
    merge_options(
        &mut opts,
        vec!["--name".into(), "resource".into()],
        &[b"name", b"compress"],
        true,
    );
    assert_eq!(opts, ["-name", "resource", "-compress", "1"]);
}
