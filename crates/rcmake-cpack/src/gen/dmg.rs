//! macOS `DragNDrop` generator (`.dmg` through `hdiutil`).

use std::collections::BTreeMap;
use std::fmt::Write;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rcmake_base::{BString, list, truth};
use rcmake_host::path::{from_native, to_native};
use rcmake_host::{FileKind, Follow, Host, ReadRange, WriteMode};

use super::{ScratchDir, copy_asset, option, run_tool};
use crate::config::absolute;
use crate::stage::{clear_output_file, ensure_dir};
use crate::{CpackError, PackageContext, PackageGenerator, error, safe_name};

/// The `DragNDrop` generator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DragNDropGenerator;

impl PackageGenerator for DragNDropGenerator {
    fn name(&self) -> &'static str {
        "DragNDrop"
    }

    fn extension(&self) -> &'static str {
        ".dmg"
    }

    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        create_image(cx, cx.staging_dir)
    }
}

pub(super) fn create_image(
    cx: &PackageContext<'_>,
    staging: &Path,
) -> Result<Vec<PathBuf>, CpackError> {
    let output = cx
        .output_dir
        .join(safe_name(&[cx.file_name, b".dmg"].concat())?);
    let license = license_resources(cx)?;
    stage_layout(cx, staging)?;
    let volume = option(cx, b"CPACK_DMG_VOLUME_NAME").unwrap_or(cx.file_name);
    let format = option(cx, b"CPACK_DMG_FORMAT").unwrap_or(b"UDZO");
    let tool = option(cx, b"CPACK_COMMAND_HDIUTIL").unwrap_or(b"hdiutil");

    if let Some(script) = option(cx, b"CPACK_DMG_DS_STORE_SETUP_SCRIPT") {
        create_with_setup(cx, staging, &output, tool, volume, format, script)?;
    } else {
        clear_output_file(cx.host, &output)?;
        run_tool(
            cx.host,
            cx.output_dir,
            tool,
            create_args(staging, &output, volume, format),
        )?;
    }

    if let Some(license) = license {
        let scratch = ScratchDir::new(cx.host, cx.output_dir)?;
        let xml = scratch.path.join("sla.xml");
        cx.host
            .write(&xml, license.as_bytes(), WriteMode::CreateNew)?;
        // hdiutil requires a placeholder before the image path when it reads XML.
        run_tool(
            cx.host,
            cx.output_dir,
            tool,
            vec![
                "udifrez".into(),
                "-xml".into(),
                from_native(&xml),
                "resources".into(),
                from_native(&output),
            ],
        )?;
    }

    if cx.host.metadata(&output, Follow::No)?.kind != FileKind::File {
        return Err(error(format!(
            "Expected a disk image at {}",
            output.display()
        )));
    }
    Ok(vec![output])
}

fn stage_layout(cx: &PackageContext<'_>, staging: &Path) -> Result<(), CpackError> {
    let host = cx.host;
    if !option(cx, b"CPACK_DMG_DISABLE_APPLICATIONS_SYMLINK").is_some_and(truth::is_on) {
        let link = staging.join("Applications");
        let target = Path::new("/Applications");
        if host.read_link(&link).as_deref() != Ok(target) {
            host.symlink(target, &link)?;
        }
    }
    if let Some(image) = option(cx, b"CPACK_DMG_BACKGROUND_IMAGE") {
        let mut background = PathBuf::from("background");
        if let Some(extension) = to_native(image).extension() {
            background.set_extension(extension);
        }
        let directory = staging.join(".background");
        ensure_dir(host, staging, &directory)?;
        copy_asset(host, image, &directory.join(background))?;
    }
    if let Some(store) = option(cx, b"CPACK_DMG_DS_STORE") {
        copy_asset(host, store, &staging.join(".DS_Store"))?;
    }
    Ok(())
}

fn create_args(staging: &Path, image: &Path, volume: &[u8], format: &[u8]) -> Vec<BString> {
    vec![
        "create".into(),
        "-volname".into(),
        volume.into(),
        "-srcfolder".into(),
        from_native(staging),
        "-ov".into(),
        "-format".into(),
        format.into(),
        from_native(image),
    ]
}

fn create_with_setup(
    cx: &PackageContext<'_>,
    staging: &Path,
    output: &Path,
    tool: &[u8],
    volume: &[u8],
    format: &[u8],
    script: &[u8],
) -> Result<(), CpackError> {
    let host = cx.host;
    let mut scratch = ScratchDir::new(cx.host, cx.output_dir)?;
    let image = scratch.path.join("setup.dmg");
    let mount = scratch.path.join("mount");
    let mut args = create_args(staging, &image, volume, b"UDRW");
    // Reserve free space for Finder to write the new .DS_Store.
    args.splice(1..1, ["-imagekey".into(), "di-size-add=2048".into()]);
    run_tool(host, cx.output_dir, tool, args)?;
    host.create_dir_all(&mount)?;

    // Keep the scratch directory if the image might still be mounted.
    scratch.cleanup = false;
    run_tool(
        host,
        cx.output_dir,
        tool,
        vec![
            "attach".into(),
            "-mountpoint".into(),
            from_native(&mount),
            "-noautoopen".into(),
            from_native(&image),
        ],
    )?;
    let script = absolute(&host.initial_cwd(), script);
    let setup = run_tool(
        host,
        &mount,
        b"osascript",
        vec![from_native(&script), volume.into()],
    );
    let detach = run_tool(
        host,
        cx.output_dir,
        tool,
        vec!["detach".into(), from_native(&mount)],
    );
    scratch.cleanup = detach.is_ok();
    detach?;
    setup?;

    clear_output_file(host, output)?;
    if format == b"UDRW" {
        host.rename(&image, output)?;
        Ok(())
    } else {
        run_tool(
            host,
            cx.output_dir,
            tool,
            vec![
                "convert".into(),
                from_native(&image),
                "-format".into(),
                format.into(),
                "-o".into(),
                from_native(output),
            ],
        )
    }
}

fn license_resources(cx: &PackageContext<'_>) -> Result<Option<String>, CpackError> {
    let Some(directory) = option(cx, b"CPACK_DMG_SLA_DIR") else {
        return Ok(None);
    };
    let languages = list::split(
        option(cx, b"CPACK_DMG_SLA_LANGUAGES")
            .ok_or_else(|| error("Set CPACK_DMG_SLA_LANGUAGES when using CPACK_DMG_SLA_DIR"))?,
        list::Empty::Drop,
    );
    if languages.is_empty() || languages.len() > 100 {
        return Err(error(
            "Specify 1 to 100 languages in CPACK_DMG_SLA_LANGUAGES",
        ));
    }
    let directory = absolute(&cx.host.initial_cwd(), directory);
    let mut resources = BTreeMap::<&str, String>::new();
    // LPic maps each classic Mac region code to a zero-based resource index.
    let count =
        u16::try_from(languages.len()).map_err(|_| error("Too many disk image languages"))?;
    let mut regions = vec![0, 0];
    regions.extend(count.to_be_bytes());
    for (index, language) in (0_u16..).zip(&languages) {
        let region = license_region(language)
            .ok_or_else(|| error(format!("Unknown disk image license language {language}")))?;
        regions.extend(region.to_be_bytes());
        regions.extend(index.to_be_bytes());
        regions.extend([0, 0]);

        let name = BString::from(language.as_slice());
        let path = |suffix: &str| -> Result<PathBuf, CpackError> {
            Ok(directory.join(safe_name(format!("{name}.{suffix}").as_bytes())?))
        };
        let txt = path("license.txt")?;
        let (kind, license) = match read_resource(cx.host, &txt) {
            Err(CpackError::Host(err)) if err.kind == ErrorKind::NotFound => {
                ("RTF ", read_resource(cx.host, &path("license.rtf")?)?)
            }
            result => ("TEXT", result?),
        };
        let mut text = Vec::new();
        for line in resource_lines(&license) {
            text.extend(line);
            text.push(b'\r');
        }
        text.push(b'\r');

        let menu_file = path("menu.txt")?;
        let menu = read_resource(cx.host, &menu_file)?;
        let lines: Vec<_> = resource_lines(&menu).collect();
        let line_count = u16::try_from(lines.len())
            .map_err(|_| error(format!("Too many lines in {}", menu_file.display())))?;
        let mut labels = line_count.to_be_bytes().to_vec();
        for line in lines {
            labels.push(u8::try_from(line.len()).map_err(|_| {
                error(format!(
                    "A menu line exceeds 255 bytes in {}",
                    menu_file.display()
                ))
            })?);
            labels.extend(line);
        }

        let id = 5000 + index;
        write_resource(
            resources.entry(kind).or_default(),
            id,
            &name.to_string(),
            &text,
        )?;
        write_resource(
            resources.entry("STR#").or_default(),
            id,
            &name.to_string(),
            &labels,
        )?;
    }
    write_resource(resources.entry("LPic").or_default(), 5000, "", &regions)?;

    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>");
    for (kind, entries) in resources {
        write!(xml, "<key>{kind}</key><array>{entries}</array>").map_err(resource_error)?;
    }
    xml.push_str("</dict></plist>\n");
    Ok(Some(xml))
}

fn resource_lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split_inclusive(|byte| *byte == b'\n').map(|line| {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        line.strip_suffix(b"\r").unwrap_or(line)
    })
}

fn read_resource(host: &dyn Host, path: &Path) -> Result<Vec<u8>, CpackError> {
    if host.metadata(path, Follow::Yes)?.kind != FileKind::File {
        return Err(error(format!(
            "Expected a regular file at {}",
            path.display()
        )));
    }
    Ok(host.read(path, ReadRange::All { limit: 1024 * 1024 })?)
}

fn resource_error(_: std::fmt::Error) -> CpackError {
    error("Cannot write disk image license resources")
}

fn write_resource(xml: &mut String, id: u16, name: &str, bytes: &[u8]) -> Result<(), CpackError> {
    let name = name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    write!(xml, "<dict><key>Attributes</key><string>0x0000</string><key>ID</key><string>{id}</string><key>Name</key><string>{name}</string><key>Data</key><data>").map_err(resource_error)?;
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let mut word = 0_u32;
        for (&byte, shift) in chunk.iter().zip([16, 8, 0]) {
            word |= u32::from(byte) << shift;
        }
        for shift in [18, 12, 6, 0].into_iter().take(chunk.len() + 1) {
            if let Some(&byte) = ALPHABET.get(((word >> shift) & 63) as usize) {
                xml.push(char::from(byte));
            }
        }
        for _ in chunk.len()..3 {
            xml.push('=');
        }
    }
    xml.push_str("</data></dict>");
    Ok(())
}

fn license_region(language: &[u8]) -> Option<u16> {
    // Classic Mac region numbers for the language identifiers in the SLA list.
    Some(match language.to_ascii_lowercase().as_slice() {
        b"english" | b"en" | b"en_us" | b"en-us" => 0,
        b"french" | b"fr" | b"fr_fr" | b"fr-fr" => 1,
        b"en_gb" | b"en-gb" => 2,
        b"german" | b"de" | b"de_de" | b"de-de" => 3,
        b"italian" | b"it" | b"it_it" | b"it-it" => 4,
        b"dutch" | b"nl" | b"nl_nl" | b"nl-nl" => 5,
        b"swedish" | b"sv" | b"sv_se" | b"sv-se" => 7,
        b"spanish" | b"es" | b"es_es" | b"es-es" => 8,
        b"danish" | b"da" | b"da_dk" | b"da-dk" => 9,
        b"pt_pt" | b"pt-pt" => 10,
        b"fr_ca" | b"fr-ca" => 11,
        b"norwegian" | b"no" | b"nb" | b"nb_no" | b"nb-no" => 12,
        b"hebrew" | b"he" | b"he_il" | b"he-il" => 13,
        b"japanese" | b"ja" | b"ja_jp" | b"ja-jp" => 14,
        b"en_au" | b"en-au" => 15,
        b"arabic" | b"ar" => 16,
        b"finnish" | b"fi" | b"fi_fi" | b"fi-fi" => 17,
        b"greek" | b"el" | b"el_gr" | b"el-gr" => 20,
        b"icelandic" | b"is" => 21,
        b"turkish" | b"tr" | b"tr_tr" | b"tr-tr" => 24,
        b"hindi" | b"hi" => 33,
        b"romanian" | b"ro" => 39,
        b"lithuanian" | b"lt" => 41,
        b"polish" | b"pl" | b"pl_pl" | b"pl-pl" => 42,
        b"hungarian" | b"hu" => 43,
        b"estonian" | b"et" => 44,
        b"latvian" | b"lv" => 45,
        b"russian" | b"ru" | b"ru_ru" | b"ru-ru" => 49,
        b"korean" | b"ko" | b"ko_kr" | b"ko-kr" => 51,
        b"chinese" | b"zh" | b"zh_cn" | b"zh-cn" | b"zh-hans" => 52,
        b"zh_tw" | b"zh-tw" | b"zh-hant" => 53,
        b"thai" | b"th" => 54,
        b"czech" | b"cs" => 56,
        b"slovak" | b"sk" => 57,
        b"ukrainian" | b"uk" => 62,
        b"serbian" | b"sr" => 65,
        b"slovenian" | b"sl" => 66,
        b"croatian" | b"hr" => 68,
        b"portuguese" | b"pt" | b"pt_br" | b"pt-br" => 71,
        b"bulgarian" | b"bg" => 72,
        b"catalan" | b"ca" => 73,
        b"en_ca" | b"en-ca" => 82,
        b"vietnamese" | b"vi" => 97,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_host::{CommandSpec, PureHost, RunOutput, Stream};

    use super::*;
    use crate::generators::tests::{check_checksums, fake_image, invoke, read, success};

    #[test]
    fn creates_and_publishes_default_disk_image() {
        let host = Arc::new(PureHost::default());
        host.set_command_handler(|host, request, _| {
            let staging = Path::new("/work/_CPack_Packages/DragNDrop/Sample Package");
            let output = staging.with_extension("dmg");
            assert_eq!(
                request.pipeline,
                [CommandSpec {
                    program: "hdiutil".into(),
                    args: [
                        "create".into(),
                        "-volname".into(),
                        "Sample Package".into(),
                        "-srcfolder".into(),
                        from_native(staging),
                        "-ov".into(),
                        "-format".into(),
                        "UDZO".into(),
                        from_native(&output),
                    ]
                    .into(),
                }]
            );
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
            let host = Arc::new(PureHost::default());
            host.seed_file(Path::new("/work/assets/cover.png"), b"cover")
                .unwrap();
            host.seed_file(Path::new("/work/assets/layout"), b"layout")
                .unwrap();
            host.set_command_handler(move |host, request, _| {
                let command = request.pipeline.first().unwrap();
                let staging = Path::new("/work/_CPack_Packages/DragNDrop/Sample Package");
                assert_eq!(command.program, "hdiutil");
                assert_eq!(
                    command.args,
                    create_args(
                        staging,
                        &staging.with_extension("dmg"),
                        b"Demo Volume",
                        format.as_bytes()
                    )
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
        let host = Arc::new(PureHost::default());
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
            let host = Arc::new(PureHost::default());
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
            assert_eq!(setup.args, ["/work/setup.applescript", "Sample Package"]);
            assert_eq!(detach.args, [BString::from("detach"), mount.clone()]);
            if fail {
                assert!(rest.is_empty());
                assert!(stderr.to_string().contains("Finder setup failed"));
            } else {
                let [convert] = rest else {
                    panic!("Unexpected commands {rest:?}");
                };
                assert_eq!(
                    convert.args,
                    [
                        "convert".into(),
                        image.clone(),
                        "-format".into(),
                        "UDZO".into(),
                        "-o".into(),
                        "/work/_CPack_Packages/DragNDrop/Sample Package.dmg".into(),
                    ]
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
}
