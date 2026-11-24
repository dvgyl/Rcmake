use std::sync::Arc;

use rcmake_host::{RunOutput, Stream};

use super::*;
use crate::generators::tests::{check_checksums, fake_image, invoke, package_host, read, success};

fn check_create_args(args: &[BString], staging: &Path, output: &Path, volume: &str, format: &str) {
    let [
        create,
        volname,
        label,
        srcfolder,
        source,
        overwrite,
        flag,
        kind,
        image,
    ] = args
    else {
        panic!("Expected nine hdiutil arguments.");
    };
    assert_eq!(
        [create, volname, label, srcfolder, overwrite, flag, kind].map(|arg| arg.as_slice()),
        [
            "create",
            "-volname",
            volume,
            "-srcfolder",
            "-ov",
            "-format",
            format
        ]
        .map(str::as_bytes)
    );
    assert_eq!(to_native(source), staging);
    assert_eq!(to_native(image), output);
}

#[test]
fn creates_and_publishes_default_disk_image() {
    let host = package_host();
    host.set_command_handler(|host, request, _| {
        let staging = Path::new("/work/_CPack_Packages/DragNDrop/Sample Package");
        let output = staging.with_extension("dmg");
        let [command] = request.pipeline.as_slice() else {
            panic!("Expected one tool command.");
        };
        assert_eq!(command.program, "hdiutil");
        check_create_args(&command.args, staging, &output, "Sample Package", "UDZO");
        assert_eq!(
            host.read_link(&staging.join("Applications"))?,
            Path::new("/Applications")
        );
        assert_eq!(read(host, &staging.join("readme.txt")), b"payload");
        fake_image(host, request.pipeline.first().unwrap());
        Ok(success())
    })
    .unwrap();

    assert_eq!(
        invoke(host.clone(), "DragNDrop", ""),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    check_checksums(host.as_ref());
}

#[test]
fn stages_finder_files_and_passes_each_requested_format() {
    for format in ["UDRW", "UDRO", "UDCO", "UDZO", "UDBZ", "ULFO", "ULMO"] {
        let host = package_host();
        host.seed_file(Path::new("/work/assets/cover.png"), b"cover")
            .unwrap();
        host.seed_file(Path::new("/work/assets/layout"), b"layout")
            .unwrap();
        host.set_command_handler(move |host, request, _| {
            let command = request.pipeline.first().unwrap();
            let staging = Path::new("/work/_CPack_Packages/DragNDrop/Sample Package");
            assert_eq!(command.program, "hdiutil");
            check_create_args(
                &command.args,
                staging,
                &staging.with_extension("dmg"),
                "Demo Volume",
                format,
            );
            assert_eq!(
                read(host, &staging.join(".background/background.png")),
                b"cover"
            );
            assert_eq!(read(host, &staging.join(".DS_Store")), b"layout");
            assert_eq!(
                host.metadata(&staging.join("Applications"), Follow::No)
                    .unwrap_err()
                    .kind,
                ErrorKind::NotFound
            );
            fake_image(host, command);
            Ok(success())
        })
        .unwrap();
        let settings = format!(
            r#"set(CPACK_DMG_VOLUME_NAME "Demo Volume")
set(CPACK_DMG_FORMAT {format})
set(CPACK_DMG_DISABLE_APPLICATIONS_SYMLINK ON)
set(CPACK_DMG_BACKGROUND_IMAGE assets/cover.png)
set(CPACK_DMG_DS_STORE assets/layout)"#
        );
        assert_eq!(
            invoke(host.clone(), "DragNDrop", &settings),
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
    }
}

#[test]
fn embeds_text_and_rtf_license_resources() {
    let host = package_host();
    for (file, content) in [
        ("English.menu.txt", "English\r\nAgree\r\n"),
        ("English.license.txt", "License\r\n"),
        ("German.menu.txt", "German\nAkzeptieren\n"),
        ("German.license.rtf", "{\\rtf1 License}"),
    ] {
        host.seed_file(&Path::new("/work/license").join(file), content.as_bytes())
            .unwrap();
    }
    host.set_command_handler(|host, request, _| {
        let command = request.pipeline.first().unwrap();
        match command.args.as_slice() {
            [verb, ..] if verb == "create" => fake_image(host, command),
            [verb, flag, xml, placeholder, output] if verb == "udifrez" => {
                assert_eq!(flag, "-xml");
                assert_eq!(placeholder, "resources");
                let xml = String::from_utf8(read(host, &to_native(xml))).unwrap();
                assert!(xml.contains("<key>LPic</key>"));
                assert!(xml.contains("<data>AAAAAgAAAAAAAAADAAEAAA==</data>"));
                assert!(xml.contains("<key>STR#</key>"));
                assert!(xml.contains("<data>AAIHRW5nbGlzaAVBZ3JlZQ==</data>"));
                assert!(xml.contains("<key>TEXT</key>"));
                assert!(xml.contains("<data>TGljZW5zZQ0N</data>"));
                assert!(xml.contains("<key>RTF </key>"));
                host.write(
                    &to_native(output),
                    b"image with license",
                    WriteMode::Truncate,
                )?;
            }
            other => panic!("Unexpected command {other:?}"),
        }
        Ok(success())
    })
    .unwrap();

    assert_eq!(
        invoke(
            host.clone(),
            "DragNDrop",
            "set(CPACK_DMG_SLA_DIR license)\nset(CPACK_DMG_SLA_LANGUAGES English German)"
        ),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert_eq!(
        read(host.as_ref(), Path::new("/work/Sample Package.dmg")),
        b"image with license"
    );
    check_checksums(host.as_ref());
}

#[test]
fn detaches_writable_image_before_conversion_or_failure() {
    for fail in [false, true] {
        let host = package_host();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = calls.clone();
        host.set_command_handler(move |host, request, _| {
            let command = request.pipeline.first().unwrap();
            observed.lock().unwrap().push(command.clone());
            if command.program == "osascript" && fail {
                return Ok(RunOutput {
                    results: vec![rcmake_host::ProcessResult::Exited(1)],
                    stdout: b"Finder setup failed".to_vec(),
                    ..RunOutput::default()
                });
            }
            if command
                .args
                .first()
                .is_some_and(|arg| arg == "create" || arg == "convert")
            {
                fake_image(host, command);
            }
            Ok(success())
        })
        .unwrap();

        let code = invoke(
            host.clone(),
            "DragNDrop",
            "set(CPACK_DMG_DS_STORE_SETUP_SCRIPT setup.applescript)",
        );
        let stderr = host.stream_bytes(Stream::Stderr);
        assert_eq!(code, i32::from(fail), "{stderr}");
        let calls = calls.lock().unwrap();
        let [create, attach, setup, detach, rest @ ..] = calls.as_slice() else {
            panic!("Unexpected commands {calls:?}");
        };
        let image = create.args.last().unwrap();
        assert!(create.args.iter().any(|arg| arg == "UDRW"));
        let [verb, flag, mount, flags, input] = attach.args.as_slice() else {
            panic!("Unexpected attach command {attach:?}");
        };
        assert_eq!(verb, "attach");
        assert_eq!(flag, "-mountpoint");
        assert_eq!(flags, "-noautoopen");
        assert_eq!(input, image);
        assert_eq!(setup.program, "osascript");
        let [script, label] = setup.args.as_slice() else {
            panic!("Expected two setup arguments.");
        };
        assert_eq!(to_native(script), Path::new("/work/setup.applescript"));
        assert_eq!(label, "Sample Package");
        assert_eq!(detach.args, [BString::from("detach"), mount.clone()]);
        if fail {
            assert!(rest.is_empty());
            assert!(stderr.to_string().contains("Finder setup failed"));
        } else {
            let [convert] = rest else {
                panic!("Unexpected commands {rest:?}");
            };
            let [verb, input, format, kind, out, file] = convert.args.as_slice() else {
                panic!("Expected six convert arguments.");
            };
            assert_eq!(
                [verb, format, kind, out].map(|arg| arg.as_slice()),
                ["convert", "-format", "UDZO", "-o"].map(str::as_bytes)
            );
            assert_eq!(input, image);
            assert_eq!(
                to_native(file),
                Path::new("/work/_CPack_Packages/DragNDrop/Sample Package.dmg")
            );
            check_checksums(host.as_ref());
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn creates_native_disk_images_without_mounting() {
    use rcmake_host::NativeHost;

    let host = Arc::new(NativeHost::new().unwrap());
    let temp = std::env::temp_dir();
    let scratch = ScratchDir::new(host.as_ref(), &temp).unwrap();
    let staging = scratch.path.join("staging");
    host.create_dir_all(&staging).unwrap();
    host.write(
        &staging.join("readme.txt"),
        b"payload",
        WriteMode::CreateNew,
    )
    .unwrap();
    let hdiutil = scratch.path.join("hdiutil-create");
    host.write(
            &hdiutil,
            b"#!/bin/sh\n[ \"$1\" = create ] || exit 1\nshift\nexec /usr/bin/hdiutil create -quiet \"$@\"\n",
            WriteMode::CreateNew,
        )
        .unwrap();
    host.set_permissions(&hdiutil, 0o700).unwrap();
    let vars = BTreeMap::from([
        ("CPACK_COMMAND_HDIUTIL".into(), from_native(&hdiutil)),
        ("CPACK_DMG_DISABLE_APPLICATIONS_SYMLINK".into(), "ON".into()),
    ]);
    let cx = PackageContext {
        host: host.as_ref(),
        staging_dir: &staging,
        output_dir: &scratch.path,
        vars: &vars,
        file_name: b"sample",
        grouping: crate::ComponentGrouping::default(),
    };
    let artifacts = DragNDropGenerator.package(&cx).unwrap();
    assert_eq!(artifacts, [scratch.path.join("sample.dmg")]);
    check_udif_footer(host.as_ref(), artifacts.first().unwrap());

    let plist = scratch.path.join("Info.plist");
    let icon = scratch.path.join("sample.icns");
    host.write(
        &plist,
        b"<plist version=\"1.0\"><dict/></plist>",
        WriteMode::CreateNew,
    )
    .unwrap();
    host.write(&icon, b"icns", WriteMode::CreateNew).unwrap();
    let mut bundle_vars = vars.clone();
    bundle_vars.extend([
        ("CPACK_BUNDLE_NAME".into(), "Sample App".into()),
        ("CPACK_BUNDLE_PLIST".into(), from_native(&plist)),
        ("CPACK_BUNDLE_ICON".into(), from_native(&icon)),
    ]);
    let cx = PackageContext {
        vars: &bundle_vars,
        file_name: b"bundle",
        ..cx
    };
    let artifacts = crate::generators::bundle::BundleGenerator
        .package(&cx)
        .unwrap();
    assert_eq!(artifacts, [scratch.path.join("bundle.dmg")]);
    check_udif_footer(host.as_ref(), artifacts.first().unwrap());
}
#[cfg(target_os = "macos")]
fn check_udif_footer(host: &dyn Host, image: &Path) {
    // hdiutil imageinfo rejects -quiet on macOS 27.
    let metadata = host.metadata(image, Follow::No).unwrap();
    assert!(metadata.len > 512);
    let header = host
        .read(
            image,
            ReadRange::Slice {
                offset: metadata.len - 512,
                len: Some(12),
            },
        )
        .unwrap();
    assert_eq!(header, b"koly\0\0\0\x04\0\0\x02\0");
}
