use rcmake_resolve::{InstallAction, InstallActionKind};

use super::*;

#[test]
fn strip_action_uses_staged_destination_and_guards() -> Result<(), Box<dyn std::error::Error>> {
    for (dest, prefix) in [
        ("lib with spaces", "${CMAKE_INSTALL_PREFIX}/"),
        ("/opt/lib with spaces", ""),
    ] {
        let mut out = BString::default();
        render_action(
            &mut out,
            &InstallAction {
                component: "Library".into(),
                all_components: false,
                exclude_from_all: false,
                destination: dest.into(),
                configurations: vec!["Release".into()],
                kind: InstallActionKind::Strip {
                    program: "/tool path/strip".into(),
                    flags: vec!["-x".into()],
                    name: "libsample.2.dylib".into(),
                },
                optional: false,
                message: BString::default(),
            },
            &InstallScriptOptions::default(),
        )?;
        let out = String::from_utf8(out.to_vec())?;
        let path = format!("\"$ENV{{DESTDIR}}{prefix}{dest}/libsample.2.dylib\"");
        assert!(out.contains("if(CMAKE_INSTALL_COMPONENT STREQUAL \"Library\""));
        assert!(out.contains("if(CMAKE_INSTALL_CONFIG_NAME MATCHES "));
        assert!(out.contains(&format!("if(EXISTS {path} AND NOT IS_SYMLINK {path})")));
        assert!(out.contains("if(CMAKE_INSTALL_DO_STRIP)"));
        assert!(out.contains(&format!(
            "execute_process(COMMAND \"/tool path/strip\" \"-x\" {path})"
        )));
    }
    Ok(())
}
