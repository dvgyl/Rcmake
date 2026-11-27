//! macOS `Bundle` generator (application bundle inside a `.dmg`).

use std::path::PathBuf;

use rcmake_host::path::from_native;

use super::{ScratchDir, copy_asset, dmg, option, run_tool};
use crate::config::absolute;
use crate::{CpackError, PackageContext, PackageGenerator, error, safe_name};

/// The `Bundle` generator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BundleGenerator;

impl PackageGenerator for BundleGenerator {
    fn name(&self) -> &'static str {
        "Bundle"
    }

    fn extension(&self) -> &'static str {
        ".dmg"
    }

    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        let _canon_work = cx.expected_work_root()?;
        let canon_staging = cx.expected_staging_root()?;
        let name = required(cx, "CPACK_BUNDLE_NAME")?;
        let plist = required(cx, "CPACK_BUNDLE_PLIST")?;
        let icon = required(cx, "CPACK_BUNDLE_ICON")?;
        let executable = safe_name(name)?;
        let scratch = ScratchDir::new(cx.host, cx.output_dir)?;
        let bundle = scratch.path.join(safe_name(&[name, b".app"].concat())?);
        let contents = bundle.join("Contents");
        let macos = contents.join("MacOS");
        let resources = contents.join("Resources");
        let host = cx.host;
        host.create_dir_all(&macos)?;
        host.create_dir_all(&resources)?;
        crate::source::copy_checked_tree_expected(
            host,
            cx.staging_dir,
            &canon_staging,
            &resources,
            crate::source::SymlinkPolicy::PreserveContained,
            false,
        )?;
        copy_asset(host, plist, &contents.join("Info.plist"))?;
        copy_asset(
            host,
            icon,
            &resources.join(safe_name(&[name, b".icns"].concat())?),
        )?;
        if let Some(command) = option(cx, b"CPACK_BUNDLE_STARTUP_COMMAND") {
            let path = macos.join(executable);
            copy_asset(host, command, &path)?;
            host.set_permissions(&path, 0o755)?;
        }
        if let Some(identity) = option(cx, b"CPACK_BUNDLE_APPLE_CERT_APP") {
            let tool = option(cx, b"CPACK_COMMAND_CODESIGN").unwrap_or(b"codesign");
            let mut args = vec!["--deep".into(), "-f".into(), "-s".into(), identity.into()];
            if let Some(entitlements) = option(cx, b"CPACK_BUNDLE_APPLE_ENTITLEMENTS") {
                args.extend([
                    "--entitlements".into(),
                    from_native(&absolute(&host.initial_cwd(), entitlements)),
                ]);
            }
            args.push(from_native(&bundle));
            run_tool(host, cx.output_dir, tool, args)?;
        }
        dmg::create_image(cx, &scratch.path)
    }
}

fn required<'a>(cx: &PackageContext<'a>, name: &str) -> Result<&'a [u8], CpackError> {
    option(cx, name.as_bytes()).ok_or_else(|| error(format!("{name} must be set.")))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, OnceLock};

    use rcmake_host::path::to_native;
    use rcmake_host::{Follow, Host, Stream};

    use crate::generators::tests::{
        check_checksums, fake_image, invoke, package_host, read, success,
    };

    const REQUIRED: &str = r#"set(CPACK_BUNDLE_NAME "Sample App")
set(CPACK_BUNDLE_PLIST assets/Info.plist)
set(CPACK_BUNDLE_ICON assets/icon.icns)"#;

    #[test]
    fn assembles_signs_and_packages_app() {
        let host = package_host();
        for (path, data) in [
            ("Info.plist", "<plist/>"),
            ("icon.icns", "icon"),
            ("launch", "#!/bin/sh\n"),
            ("entitlements.plist", "<plist/>"),
        ] {
            host.seed_file(&Path::new("/work/assets").join(path), data.as_bytes())
                .unwrap();
        }
        host.seed_file(Path::new("/work/payload/lib/data"), b"nested payload")
            .unwrap();
        host.symlink(Path::new("data"), Path::new("/work/payload/lib/link"))
            .unwrap();
        let observed = Arc::new(OnceLock::new());
        let signed = observed.clone();
        host.set_command_handler(move |host, request, _| {
            let command = request.pipeline.first().unwrap();
            if command.program == "codesign" {
                let bundle = to_native(command.args.last().unwrap());
                assert_eq!(bundle.file_name().unwrap(), "Sample App.app");
                let [deep, force, sign, identity, entitlements, file, _] = command.args.as_slice()
                else {
                    panic!("Expected seven codesign arguments.");
                };
                assert_eq!(
                    [deep, force, sign, identity, entitlements].map(|arg| arg.as_slice()),
                    [
                        "--deep",
                        "-f",
                        "-s",
                        "Developer ID Application: Example",
                        "--entitlements"
                    ]
                    .map(str::as_bytes)
                );
                assert_eq!(
                    to_native(file),
                    Path::new("/work/assets/entitlements.plist")
                );
                signed.set(bundle).unwrap();
                return Ok(success());
            }
            assert_eq!(command.program, "hdiutil");
            let bundle = signed.get().unwrap();
            let staging = bundle.parent().unwrap();
            let [
                create,
                volname,
                volume,
                srcfolder,
                source,
                overwrite,
                format,
                kind,
                output,
            ] = command.args.as_slice()
            else {
                panic!("Expected nine hdiutil arguments.");
            };
            assert_eq!(
                [create, volname, volume, srcfolder, overwrite, format, kind]
                    .map(|arg| arg.as_slice()),
                [
                    "create",
                    "-volname",
                    "App Volume",
                    "-srcfolder",
                    "-ov",
                    "-format",
                    "UDRO"
                ]
                .map(str::as_bytes)
            );
            assert_eq!(to_native(source), staging);
            assert_eq!(
                to_native(output),
                Path::new("/work/_CPack_Packages/Bundle/Sample Package.dmg")
            );
            let contents = bundle.join("Contents");
            let resources = contents.join("Resources");
            assert_eq!(read(host, &contents.join("Info.plist")), b"<plist/>");
            assert_eq!(read(host, &resources.join("Sample App.icns")), b"icon");
            assert_eq!(read(host, &resources.join("readme.txt")), b"payload");
            assert_eq!(read(host, &resources.join("lib/data")), b"nested payload");
            assert_eq!(
                host.read_link(&resources.join("lib/link"))?,
                Path::new("data")
            );
            assert_eq!(
                host.read_link(&staging.join("Applications"))?,
                Path::new("/Applications")
            );
            let executable = contents.join("MacOS/Sample App");
            assert_eq!(read(host, &executable), b"#!/bin/sh\n");
            assert_eq!(host.metadata(&executable, Follow::No)?.mode & 0o777, 0o755);
            assert!(
                host.metadata(&staging.join("readme.txt"), Follow::No)
                    .is_err()
            );
            fake_image(host, command);
            Ok(success())
        })
        .unwrap();

        let options = format!(
            r#"{REQUIRED}
set(CPACK_BUNDLE_STARTUP_COMMAND assets/launch)
set(CPACK_BUNDLE_APPLE_CERT_APP "Developer ID Application: Example")
set(CPACK_BUNDLE_APPLE_ENTITLEMENTS assets/entitlements.plist)
set(CPACK_DMG_VOLUME_NAME "App Volume")
set(CPACK_DMG_FORMAT UDRO)"#
        );
        assert_eq!(
            invoke(host.clone(), "Bundle", &options),
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
        check_checksums(host.as_ref());
        assert!(host.metadata(observed.get().unwrap(), Follow::No).is_err());
    }

    #[test]
    fn reports_each_missing_bundle_option() {
        for name in [
            "CPACK_BUNDLE_NAME",
            "CPACK_BUNDLE_PLIST",
            "CPACK_BUNDLE_ICON",
        ] {
            let host = package_host();
            host.set_command_handler(|_, request, _| panic!("Unexpected command {request:?}"))
                .unwrap();

            let options = format!("{REQUIRED}\nunset({name})");
            assert_eq!(invoke(host.clone(), "Bundle", &options), 1);
            assert!(
                host.stream_bytes(Stream::Stderr)
                    .to_string()
                    .contains(&format!("{name} must be set."))
            );
        }
    }
}
