//! Portable end-to-end tests for native installer generators (`productbuild`, `NSIS`,
//! `NSIS64`, `WIX` v3/v4, `IFW`), post-hook staged-tree containment, publication rollback,
//! and failure-path matrices.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rcmake_base::BString;
use rcmake_base::hash::HashAlgo;
use rcmake_host::path::to_native;
#[cfg(target_os = "macos")]
use rcmake_host::path::{from_native, from_native_cmake};
use rcmake_host::{
    CommandSpec, Follow, Host, HostError, HostResult, ProcessResult, PureHost, ReadRange,
    RegistryRoot, RegistryView, RunOutput, RunRequest, Stream, SystemInfo, WriteMode,
};

use super::{
    XmlElement, XmlNode, escape_js_single_quoted, escape_nsis_literal, escape_wix_preprocessor,
    parse_xml_bounded,
};

const DIST_TEMPLATE: &[u8] = include_bytes!(
    "../../../../../modules/cmake-4.5/Modules/Internal/CPack/CPack.distribution.dist.in"
);
const NSIS_TEMPLATE: &[u8] =
    include_bytes!("../../../../../modules/cmake-4.5/Modules/Internal/CPack/NSIS.template.in");
const NSIS_INI_TEMPLATE: &[u8] = include_bytes!(
    "../../../../../modules/cmake-4.5/Modules/Internal/CPack/NSIS.InstallOptions.ini.in"
);
const WIX_V4_TEMPLATE: &[u8] =
    include_bytes!("../../../../../modules/cmake-4.5/Modules/Internal/CPack/WIX.template.in");
const WIX_V3_TEMPLATE: &[u8] = include_bytes!(
    "../../../../../modules/cmake-4.5/Modules/Internal/CPack/WIX-v3/WIX.template.in"
);
const GENERIC_LICENSE: &[u8] =
    include_bytes!("../../../../../modules/cmake-4.5/Templates/CPack.GenericLicense.txt");
const GENERIC_DESC: &[u8] =
    include_bytes!("../../../../../modules/cmake-4.5/Templates/CPack.GenericDescription.txt");
const GENERIC_WELCOME: &[u8] =
    include_bytes!("../../../../../modules/cmake-4.5/Templates/CPack.GenericWelcome.txt");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OsProfile {
    MacOs,
    Linux,
    Windows,
}

const ALL_PROFILES: [OsProfile; 3] = [OsProfile::MacOs, OsProfile::Linux, OsProfile::Windows];

impl OsProfile {
    fn system_info(self) -> SystemInfo {
        let (sysname, machine) = match self {
            Self::MacOs => ("Darwin", "arm64"),
            Self::Linux => ("Linux", "x86_64"),
            Self::Windows => ("Windows", "AMD64"),
        };
        SystemInfo {
            sysname: BString::from(sysname),
            release: BString::from("1.0.0"),
            version: BString::from("TestProfile"),
            machine: BString::from(machine),
            hostname: BString::from("testhost"),
            cpu_count: 4,
            total_memory: 0,
            pid: 4242,
        }
    }

    fn path_sep(self) -> &'static str {
        if self == Self::Windows { ";" } else { ":" }
    }

    fn tool_name(self, base: &str) -> String {
        if self == Self::Windows {
            format!("{base}.exe")
        } else {
            base.to_owned()
        }
    }
}

#[derive(Clone, Debug)]
struct RecordedCall {
    program: BString,
    args: Vec<BString>,
    cwd: PathBuf,
}

fn make_profile_host(profile: OsProfile) -> Arc<PureHost> {
    let exe = if profile == OsProfile::Windows {
        "/tools/bin/cpack.exe"
    } else {
        "/tools/bin/cpack"
    };
    let host = Arc::new(PureHost::new(PathBuf::from("/work"), PathBuf::from(exe)));
    host.set_system(profile.system_info()).unwrap();
    if profile == OsProfile::Windows {
        host.set_registry_host_view(RegistryView::Bits64).unwrap();
    }
    host.env_set(
        b"PATH",
        Some(format!("/tools/bin{}/usr/bin", profile.path_sep()).as_bytes()),
    );
    // Seed vendored BSD-3-Clause templates under /cmake.
    for (rel, bytes) in [
        (
            "Modules/Internal/CPack/CPack.distribution.dist.in",
            DIST_TEMPLATE,
        ),
        ("Modules/Internal/CPack/NSIS.template.in", NSIS_TEMPLATE),
        (
            "Modules/Internal/CPack/NSIS.InstallOptions.ini.in",
            NSIS_INI_TEMPLATE,
        ),
        ("Modules/Internal/CPack/WIX.template.in", WIX_V4_TEMPLATE),
        (
            "Modules/Internal/CPack/WIX-v3/WIX.template.in",
            WIX_V3_TEMPLATE,
        ),
        ("Templates/CPack.GenericLicense.txt", GENERIC_LICENSE),
        ("Templates/CPack.GenericDescription.txt", GENERIC_DESC),
        ("Templates/CPack.GenericWelcome.txt", GENERIC_WELCOME),
    ] {
        host.seed_file(&Path::new("/cmake").join(rel), bytes)
            .unwrap();
    }
    host
}

fn seed_tool(host: &PureHost, profile: OsProfile, name: &str) -> PathBuf {
    let path = Path::new("/tools/bin").join(profile.tool_name(name));
    host.seed_file(&path, b"mock-tool").unwrap();
    host.set_permissions(&path, 0o755).unwrap();
    path
}

fn read_bytes(host: &dyn Host, path: &Path) -> Vec<u8> {
    host.read(
        path,
        ReadRange::All {
            limit: 16 * 1024 * 1024,
        },
    )
    .unwrap()
}

fn read_utf8(host: &dyn Host, path: &Path) -> String {
    String::from_utf8(read_bytes(host, path)).unwrap()
}

fn run_unrestricted(host: Arc<dyn Host>, args: &[&str]) -> i32 {
    let argv: Vec<BString> = std::iter::once("cpack")
        .chain(args.iter().copied())
        .map(BString::from)
        .collect();
    crate::main_with_factory(&argv, host, crate::generators::create_unrestricted)
}

fn run_production(host: Arc<dyn Host>, args: &[&str]) -> i32 {
    let argv: Vec<BString> = std::iter::once("cpack")
        .chain(args.iter().copied())
        .map(BString::from)
        .collect();
    crate::main(&argv, host)
}

fn ok_output(stdout: &[u8]) -> RunOutput {
    RunOutput {
        results: vec![ProcessResult::Exited(0)],
        stdout: stdout.to_vec(),
        ..RunOutput::default()
    }
}

fn fail_output(code: i32, stdout: &[u8]) -> RunOutput {
    RunOutput {
        results: vec![ProcessResult::Exited(code)],
        stdout: stdout.to_vec(),
        ..RunOutput::default()
    }
}

fn timeout_output() -> RunOutput {
    RunOutput {
        results: vec![ProcessResult::TimedOut],
        stdout: b"timed out".to_vec(),
        ..RunOutput::default()
    }
}

fn record_cmd(calls: &Arc<Mutex<Vec<RecordedCall>>>, req: &RunRequest) -> CommandSpec {
    let spec = req.pipeline[0].clone();
    calls.lock().unwrap().push(RecordedCall {
        program: spec.program.clone(),
        args: spec.args.clone(),
        cwd: req.cwd.clone(),
    });
    spec
}

fn find_arg_after<'a>(args: &'a [BString], flag: &[u8]) -> Option<&'a [u8]> {
    args.windows(2)
        .find(|w| w[0].as_slice() == flag)
        .map(|w| w[1].as_slice())
}

fn xml_find_all<'a>(elem: &'a XmlElement, tag: &str) -> Vec<&'a XmlElement> {
    let mut out = Vec::new();
    if elem.name == tag {
        out.push(elem);
    }
    for child in &elem.children {
        if let XmlNode::Element(sub) = child {
            out.extend(xml_find_all(sub, tag));
        }
    }
    out
}

fn xml_attr<'a>(elem: &'a XmlElement, key: &str) -> Option<&'a str> {
    elem.attributes
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn xml_text(elem: &XmlElement) -> String {
    let mut out = String::new();
    for child in &elem.children {
        if let XmlNode::Text(t) = child {
            out.push_str(t);
        }
    }
    out
}

fn check_checksum_files(host: &dyn Host, dir: &Path, filename: &str) {
    let body = read_bytes(host, &dir.join(filename));
    for (ext, algo) in [("md5", HashAlgo::Md5), ("sha256", HashAlgo::Sha256)] {
        let expected = format!("{}  {filename}\n", algo.hex_digest(&body));
        assert_eq!(
            read_utf8(host, &dir.join(format!("{filename}.{ext}"))),
            expected
        );
    }
}

/// Seed a minimal CMake component install project (`cmake_install.cmake`) under `/work/build`.
fn seed_component_project(host: &PureHost, install_rules: &str) {
    host.seed_file(
        Path::new("/work/build/cmake_install.cmake"),
        install_rules.as_bytes(),
    )
    .unwrap();
}

// ===========================================================================
// 1. Production Platform Dispatch Tests
// ===========================================================================

#[test]
fn production_platform_dispatch_matches_cmake_availability() {
    for profile in ALL_PROFILES {
        let cases: &[(&str, bool)] = &[
            ("productbuild", profile == OsProfile::MacOs),
            ("WIX", profile == OsProfile::Windows),
            ("NSIS", true),
            ("NSIS64", true),
            ("IFW", true),
        ];
        for &(gen_name, expected_available) in cases {
            let host = make_profile_host(profile);
            host.seed_file(
                Path::new("/work/CPackConfig.cmake"),
                format!(
                    r#"set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "{gen_name}")
set(CPACK_PACKAGE_NAME "dispatch")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "dispatch-1.0.0")
"#
                )
                .as_bytes(),
            )
            .unwrap();
            let code = run_production(host.clone(), &[]);
            assert_eq!(code, 1, "missing tools or unavailable should fail cleanly");
            let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
            let unavailable =
                stderr.contains(&format!("Could not create CPack generator: {gen_name}"));
            assert_eq!(
                !unavailable, expected_available,
                "profile={profile:?} gen={gen_name} stderr={stderr}"
            );
        }
    }
}

// ===========================================================================
// 2. productbuild Generator Tests (Monolithic, Components, Domains, Oracle)
// ===========================================================================

#[test]
fn productbuild_monolithic_across_all_profiles() {
    for profile in ALL_PROFILES {
        let host = make_profile_host(profile);
        seed_tool(&host, profile, "pkgbuild");
        seed_tool(&host, profile, "productbuild");
        host.seed_file(Path::new("/work/src/bin/hello"), b"hello-app")
            .unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "productbuild")
set(CPACK_PACKAGE_NAME "MyApp")
set(CPACK_PACKAGE_VENDOR "Acme")
set(CPACK_PACKAGE_VERSION "2.3.4")
set(CPACK_PACKAGE_FILE_NAME "MyApp-2.3.4")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_PACKAGE_CHECKSUM "MD5;SHA256")
"#,
        )
        .unwrap();

        let calls = Arc::new(Mutex::new(Vec::<RecordedCall>::new()));
        let calls_ref = Arc::clone(&calls);
        host.set_command_handler(move |h, req, _| {
            let spec = record_cmd(&calls_ref, req);
            let prog = String::from_utf8_lossy(&spec.program).into_owned();
            if prog.contains("pkgbuild") {
                // Verify Contents/ and leaf packages are NOT leaked inside --root payload!
                let root = to_native(find_arg_after(&spec.args, b"--root").unwrap());
                assert!(
                    h.metadata(&root.join("Contents"), Follow::No).is_err(),
                    "Contents/ must never appear inside pkgbuild --root"
                );
                assert!(
                    h.metadata(&root.join("Applications/bin/hello"), Follow::No)
                        .is_ok(),
                    "default /Applications prefix must be honored under --root"
                );
                let out = to_native(spec.args.last().unwrap());
                h.write(&out, b"leaf-pkg-bytes", WriteMode::CreateNew)?;
                Ok(ok_output(b"pkgbuild ok\n"))
            } else if prog.contains("productbuild") {
                let dist = to_native(find_arg_after(&spec.args, b"--distribution").unwrap());
                assert!(h.metadata(&dist, Follow::No).is_ok());
                let out = to_native(spec.args.last().unwrap());
                h.write(&out, b"final-productbuild-pkg", WriteMode::CreateNew)?;
                Ok(ok_output(b"productbuild ok\n"))
            } else {
                panic!("unexpected tool {prog}");
            }
        })
        .unwrap();

        let code = run_unrestricted(host.clone(), &[]);
        assert_eq!(
            code,
            0,
            "profile={profile:?} stderr={}",
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
        );

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert!(
            recorded[0]
                .program
                .ends_with(profile.tool_name("pkgbuild").as_bytes()),
            "unexpected program: {}",
            recorded[0].program
        );
        assert_eq!(
            from_native_cmake(&recorded[0].cwd),
            BString::from("/work/_CPack_Packages/productbuild")
        );
        assert_eq!(
            find_arg_after(&recorded[0].args, b"--identifier"),
            Some(b"com.Acme.MyApp".as_slice())
        );
        assert_eq!(
            find_arg_after(&recorded[0].args, b"--install-location"),
            Some(b"/".as_slice())
        );
        // Monolithic leaf package name matches CMake 4.5.1 (CPACK_PACKAGE_NAME, no .pkg).
        assert!(
            rcmake_base::path_lex::cmake_style(recorded[0].args.last().unwrap())
                .ends_with(b"/Contents/Packages/MyApp")
        );

        let dist_xml = read_bytes(
            host.as_ref(),
            Path::new("/work/_CPack_Packages/productbuild/MyApp-2.3.4/Contents/distribution.dist"),
        );
        let root = parse_xml_bounded(&dist_xml, "distribution.dist").unwrap();
        assert_eq!(root.name, "installer-gui-script");
        let opts = xml_find_all(&root, "options");
        assert_eq!(opts.len(), 1);
        assert_eq!(xml_attr(opts[0], "rootVolumeOnly"), Some("false"));
        assert!(xml_find_all(&root, "domains").is_empty());

        assert_eq!(
            read_bytes(host.as_ref(), Path::new("/work/MyApp-2.3.4.pkg")),
            b"final-productbuild-pkg"
        );
        check_checksum_files(host.as_ref(), Path::new("/work"), "MyApp-2.3.4.pkg");
    }
}

#[test]
fn productbuild_components_domains_scripts_backgrounds_and_signing() {
    let host = make_profile_host(OsProfile::MacOs);
    seed_tool(&host, OsProfile::MacOs, "pkgbuild");
    seed_tool(&host, OsProfile::MacOs, "productbuild");
    host.seed_file(Path::new("/work/scripts/pre.sh"), b"#!/bin/sh\necho pre\n")
        .unwrap();
    host.seed_file(
        Path::new("/work/scripts/post.sh"),
        b"#!/bin/sh\necho post\n",
    )
    .unwrap();
    host.seed_file(Path::new("/work/core.plist"), b"<plist/>")
        .unwrap();
    host.seed_file(Path::new("/work/res/bg.png"), b"PNGDATA")
        .unwrap();
    host.seed_file(Path::new("/work/res/bg_dark.png"), b"PNGDARK")
        .unwrap();
    host.seed_file(Path::new("/work/res/Welcome.html"), b"<html>Welcome</html>")
        .unwrap();
    host.seed_file(Path::new("/work/res/License.rtf"), b"{\\rtf1 Lic}")
        .unwrap();
    host.seed_file(Path::new("/work/res/ReadMe.rtfd/TXT.rtf"), b"{\\rtf1 Read}")
        .unwrap();
    host.seed_file(Path::new("/work/payload/base.txt"), b"base")
        .unwrap();
    host.seed_file(Path::new("/work/payload/core.txt"), b"core")
        .unwrap();
    host.seed_file(Path::new("/work/payload/app.txt"), b"app")
        .unwrap();

    seed_component_project(
        &host,
        r#"
if(CMAKE_INSTALL_COMPONENT STREQUAL "base")
  file(INSTALL "/work/payload/base.txt" DESTINATION "${CMAKE_INSTALL_PREFIX}/lib")
elseif(CMAKE_INSTALL_COMPONENT STREQUAL "core")
  file(INSTALL "/work/payload/core.txt" DESTINATION "${CMAKE_INSTALL_PREFIX}/lib")
elseif(CMAKE_INSTALL_COMPONENT STREQUAL "app")
  file(INSTALL "/work/payload/app.txt" DESTINATION "${CMAKE_INSTALL_PREFIX}/bin")
endif()
"#,
    );

    host.seed_file(
        Path::new("/work/CPackConfig.cmake"),
        br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "productbuild")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_PACKAGE_NAME "Suite & <Co>")
set(CPACK_PACKAGE_VENDOR "Vendor")
set(CPACK_PACKAGE_VERSION "1.2.0")
set(CPACK_PACKAGE_FILE_NAME "Suite-1.2.0")
set(CPACK_PRODUCTBUILD_IDENTIFIER "org.example.suite")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;Suite;ALL;/")
set(CPACK_COMPONENTS_ALL "base;core;app")
set(CPACK_COMPONENT_GROUP_RUNTIME_DISPLAY_NAME "Runtime Group")
set(CPACK_COMPONENT_GROUP_RUNTIME_DESCRIPTION "Runtime <libs> & tools")
set(CPACK_COMPONENT_GROUP_SUB_PARENT_GROUP "runtime")
set(CPACK_COMPONENT_GROUP_SUB_DISPLAY_NAME "Subgroup")
set(CPACK_COMPONENT_BASE_GROUP "runtime")
set(CPACK_COMPONENT_BASE_REQUIRED ON)
set(CPACK_COMPONENT_CORE_GROUP "sub")
set(CPACK_COMPONENT_CORE_DEPENDS "base")
set(CPACK_COMPONENT_CORE_PLIST "/work/core.plist")
set(CPACK_COMPONENT_CORE_ARCHIVE_FILE "custom-core")
set(CPACK_PREFLIGHT_CORE_SCRIPT "/work/scripts/pre.sh")
set(CPACK_POSTFLIGHT_CORE_SCRIPT "/work/scripts/post.sh")
set(CPACK_COMPONENT_APP_DEPENDS "core")
set(CPACK_COMPONENT_APP_DISABLED ON)
set(CPACK_COMPONENT_APP_HIDDEN ON)
set(CPACK_COMPONENT_APP_DOWNLOADED ON)
set(CPACK_DOWNLOAD_SITE "https://downloads.example.test/pkgs/")
set(CPACK_PRODUCTBUILD_DOMAINS ON)
set(CPACK_PRODUCTBUILD_DOMAINS_ANYWHERE OFF)
set(CPACK_PRODUCTBUILD_DOMAINS_USER ON)
set(CPACK_PRODUCTBUILD_DOMAINS_ROOT ON)
set(CPACK_PRODUCTBUILD_RESOURCES_DIR "/work/res")
set(CPACK_PRODUCTBUILD_BACKGROUND "bg.png")
set(CPACK_PRODUCTBUILD_BACKGROUND_ALIGNMENT "bottomleft")
set(CPACK_PRODUCTBUILD_BACKGROUND_SCALING "none")
set(CPACK_PRODUCTBUILD_BACKGROUND_MIME_TYPE "image/png")
set(CPACK_PRODUCTBUILD_BACKGROUND_DARKAQUA "bg_dark.png")
set(CPACK_PRODUCTBUILD_BACKGROUND_DARKAQUA_UTI "public.png")
set(CPACK_RESOURCE_FILE_WELCOME "/work/res/Welcome.html")
set(CPACK_RESOURCE_FILE_LICENSE "/work/res/License.rtf")
set(CPACK_RESOURCE_FILE_README "/work/res/ReadMe.rtfd")
set(CPACK_PKGBUILD_IDENTITY_NAME "Dev ID Installer")
set(CPACK_PKGBUILD_KEYCHAIN_PATH "/work/keys.keychain")
set(CPACK_PRODUCTBUILD_IDENTITY_NAME "Prod ID Installer")
set(CPACK_PRODUCTBUILD_KEYCHAIN_PATH "/work/prod.keychain")
"#,
    )
    .unwrap();

    let calls = Arc::new(Mutex::new(Vec::<RecordedCall>::new()));
    let calls_ref = Arc::clone(&calls);
    host.set_command_handler(move |h, req, _| {
        let spec = record_cmd(&calls_ref, req);
        let prog = String::from_utf8_lossy(&spec.program).into_owned();
        if prog.contains("pkgbuild") {
            let id = String::from_utf8_lossy(find_arg_after(&spec.args, b"--identifier").unwrap())
                .into_owned();
            let scripts_dir = to_native(find_arg_after(&spec.args, b"--scripts").unwrap());
            if id.ends_with(".core") {
                let pre = h
                    .metadata(&scripts_dir.join("preinstall"), Follow::No)
                    .unwrap();
                let post = h
                    .metadata(&scripts_dir.join("postinstall"), Follow::No)
                    .unwrap();
                assert_eq!(pre.mode & 0o777, 0o755);
                assert_eq!(post.mode & 0o777, 0o755);
                assert_eq!(
                    find_arg_after(&spec.args, b"--component-plist")
                        .map(rcmake_base::path_lex::cmake_style),
                    Some(BString::from("/work/core.plist"))
                );
            } else {
                // Component scripts must NOT leak into other components.
                assert!(
                    h.metadata(&scripts_dir.join("preinstall"), Follow::No)
                        .is_err()
                );
            }
            // Output path is before --component-plist when plist is present.
            let out_idx = spec
                .args
                .iter()
                .position(|a| a.as_slice() == b"--component-plist")
                .map_or(spec.args.len() - 1, |i| i - 1);
            let out = to_native(&spec.args[out_idx]);
            h.write(&out, b"comp-pkg", WriteMode::CreateNew)?;
            Ok(ok_output(b""))
        } else {
            let out = to_native(spec.args.last().unwrap());
            h.write(&out, b"dist-pkg", WriteMode::CreateNew)?;
            Ok(ok_output(b""))
        }
    })
    .unwrap();

    assert_eq!(
        run_unrestricted(host.clone(), &[]),
        0,
        "stderr={}",
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
    );

    let recorded = calls.lock().unwrap();
    assert_eq!(recorded.len(), 4); // 3 components + 1 productbuild
    assert_eq!(
        find_arg_after(&recorded[0].args, b"--sign"),
        Some(b"Dev ID Installer".as_slice())
    );
    assert_eq!(
        find_arg_after(&recorded[0].args, b"--keychain").map(rcmake_base::path_lex::cmake_style),
        Some(BString::from("/work/keys.keychain"))
    );
    assert_eq!(
        find_arg_after(&recorded[3].args, b"--identifier"),
        Some(b"org.example.suite".as_slice())
    );
    assert_eq!(
        find_arg_after(&recorded[3].args, b"--sign"),
        Some(b"Prod ID Installer".as_slice())
    );
    assert_eq!(
        find_arg_after(&recorded[3].args, b"--keychain").map(rcmake_base::path_lex::cmake_style),
        Some(BString::from("/work/prod.keychain"))
    );

    let dist_path =
        Path::new("/work/_CPack_Packages/productbuild/Suite-1.2.0/Contents/distribution.dist");
    let root = parse_xml_bounded(&read_bytes(host.as_ref(), dist_path), "dist").unwrap();
    let opts = xml_find_all(&root, "options");
    assert_eq!(xml_attr(opts[0], "rootVolumeOnly"), None);
    let domains = xml_find_all(&root, "domains");
    assert_eq!(domains.len(), 1);
    assert_eq!(xml_attr(domains[0], "enable_anywhere"), Some("false"));
    assert_eq!(xml_attr(domains[0], "enable_currentUserHome"), Some("true"));
    assert_eq!(xml_attr(domains[0], "enable_localSystem"), Some("true"));

    let bgs = xml_find_all(&root, "background");
    assert_eq!(xml_attr(bgs[0], "file"), Some("bg.png"));
    assert_eq!(xml_attr(bgs[0], "alignment"), Some("bottomleft"));
    assert_eq!(xml_attr(bgs[0], "mime-type"), Some("image/png"));
    let dark_bgs = xml_find_all(&root, "background-darkAqua");
    assert_eq!(xml_attr(dark_bgs[0], "file"), Some("bg_dark.png"));
    assert_eq!(xml_attr(dark_bgs[0], "uti"), Some("public.png"));

    let choices = xml_find_all(&root, "choice");
    let app_choice = choices
        .iter()
        .find(|c| xml_attr(c, "id") == Some("appChoice"))
        .unwrap();
    assert_eq!(xml_attr(app_choice, "start_selected"), Some("false"));
    assert_eq!(xml_attr(app_choice, "start_visible"), Some("false"));
    assert_eq!(
        xml_attr(app_choice, "selected"),
        Some(
            "my.choice.selected && choices['coreChoice'].selected && choices['baseChoice'].selected"
        )
    );
    let base_choice = choices
        .iter()
        .find(|c| xml_attr(c, "id") == Some("baseChoice"))
        .unwrap();
    assert_eq!(xml_attr(base_choice, "start_enabled"), Some("false"));
    let base_sel = xml_attr(base_choice, "selected").unwrap();
    assert!(base_sel.contains("|| choices['coreChoice'].selected"));
    assert!(base_sel.contains("|| choices['appChoice'].selected"));

    let pkg_refs: Vec<_> = xml_find_all(&root, "pkg-ref")
        .into_iter()
        .filter(|e| xml_attr(e, "version").is_some())
        .collect();
    assert_eq!(pkg_refs.len(), 3);
    for pref in &pkg_refs {
        // Domains enabled => auth="Admin" omitted.
        assert_eq!(xml_attr(pref, "auth"), None);
    }
    let app_ref = pkg_refs
        .iter()
        .find(|e| xml_attr(e, "id") == Some("org.example.suite.app"))
        .unwrap();
    assert_eq!(
        xml_text(app_ref),
        "https://downloads.example.test/pkgs/Suite-1.2.0-app.pkg"
    );
    let core_ref = pkg_refs
        .iter()
        .find(|e| xml_attr(e, "id") == Some("org.example.suite.core"))
        .unwrap();
    assert_eq!(
        xml_text(core_ref),
        "file:./Contents/Packages/custom-core.pkg"
    );
}

#[test]
fn productbuild_rejects_resource_collision_and_component_cycles() {
    let host = make_profile_host(OsProfile::MacOs);
    seed_tool(&host, OsProfile::MacOs, "pkgbuild");
    seed_tool(&host, OsProfile::MacOs, "productbuild");
    host.seed_file(Path::new("/work/src/a.txt"), b"a").unwrap();
    host.seed_file(Path::new("/work/badres/distribution.dist"), b"collide")
        .unwrap();
    host.seed_file(
        Path::new("/work/CPackConfig.cmake"),
        br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "productbuild")
set(CPACK_PACKAGE_NAME "App")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "App-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_PRODUCTBUILD_RESOURCES_DIR "/work/badres")
"#,
    )
    .unwrap();
    host.set_command_handler(|h, req, _| {
        let out = to_native(req.pipeline[0].args.last().unwrap());
        h.write(&out, b"leaf", WriteMode::CreateNew)?;
        Ok(ok_output(b""))
    })
    .unwrap();
    assert_eq!(run_unrestricted(host.clone(), &[]), 1);
    assert!(
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
            .contains("collides with generated control entry")
    );

    // Component dependency cycle must fail before invoking pkgbuild.
    let host2 = make_profile_host(OsProfile::MacOs);
    seed_tool(&host2, OsProfile::MacOs, "pkgbuild");
    seed_tool(&host2, OsProfile::MacOs, "productbuild");
    seed_component_project(&host2, "");
    host2
        .seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "productbuild")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_PACKAGE_NAME "Cycle")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "Cycle-1.0")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;Cycle;ALL;/")
set(CPACK_COMPONENTS_ALL "a;b")
set(CPACK_COMPONENT_A_DEPENDS "b")
set(CPACK_COMPONENT_B_DEPENDS "a")
"#,
        )
        .unwrap();
    let invoked = Arc::new(AtomicUsize::new(0));
    let invoked_ref = Arc::clone(&invoked);
    host2
        .set_command_handler(move |_, _, _| {
            invoked_ref.fetch_add(1, Ordering::SeqCst);
            Ok(ok_output(b""))
        })
        .unwrap();
    assert_eq!(run_unrestricted(host2.clone(), &[]), 1);
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    assert!(
        String::from_utf8_lossy(&host2.stream_bytes(Stream::Stderr)).contains("Cycle detected")
    );
}

// ===========================================================================
// 3. NSIS and NSIS64 Generator Tests
// ===========================================================================

#[test]
fn nsis_version_probe_prefix_and_validation_across_profiles() {
    for profile in ALL_PROFILES {
        let expected_flag = if profile == OsProfile::Windows {
            b"/VERSION".as_slice()
        } else {
            b"-VERSION".as_slice()
        };
        // Reject < 3.03, retaining original version string in diagnostic.
        for &(bad_ver, expect_substring) in &[
            ("v3.02\n", "v3.02"),
            ("v2.51\n", "v2.51"),
            ("garbage output\n", "unrecognized version output"),
        ] {
            let host = make_profile_host(profile);
            seed_tool(&host, profile, "makensis");
            host.seed_file(Path::new("/work/src/a.txt"), b"x").unwrap();
            host.seed_file(
                Path::new("/work/CPackConfig.cmake"),
                br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "NSIS")
set(CPACK_PACKAGE_NAME "App")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "App-1.0.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
"#,
            )
            .unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let calls_ref = Arc::clone(&calls);
            host.set_command_handler(move |_, req, _| {
                record_cmd(&calls_ref, req);
                Ok(ok_output(bad_ver.as_bytes()))
            })
            .unwrap();

            assert_eq!(run_unrestricted(host.clone(), &[]), 1);
            let recorded = calls.lock().unwrap();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].args, vec![BString::from(expected_flag)]);
            let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
            assert!(
                stderr.contains(expect_substring),
                "profile={profile:?} bad_ver={bad_ver:?} stderr={stderr}"
            );
        }

        // Accept >= 3.03 and legacy v*.cvs.
        for good_ver in ["v3.03\n", "v3.10\n", "v2.46.cvs\n"] {
            let host = make_profile_host(profile);
            seed_tool(&host, profile, "makensis");
            host.seed_file(Path::new("/work/src/a.txt"), b"x").unwrap();
            host.seed_file(
                Path::new("/work/CPackConfig.cmake"),
                br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "NSIS64")
set(CPACK_PACKAGE_NAME "App")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "App-1.0.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_NSIS_EXECUTABLE_PRE_ARGUMENTS "V3;-DEXTRA=1")
set(CPACK_NSIS_EXECUTABLE_POST_ARGUMENTS "NOCD")
set(CPACK_PACKAGE_CHECKSUM "SHA256")
"#,
            )
            .unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let calls_ref = Arc::clone(&calls);
            host.set_command_handler(move |h, req, _| {
                let spec = record_cmd(&calls_ref, req);
                if spec.args.len() == 1
                    && (spec.args[0].as_slice() == b"-VERSION"
                        || spec.args[0].as_slice() == b"/VERSION")
                {
                    return Ok(ok_output(good_ver.as_bytes()));
                }
                let nsi = to_native(&spec.args[2]);
                assert!(h.metadata(&nsi, Follow::No).is_ok());
                let out = req.cwd.join("App-1.0.0.exe");
                h.write(&out, b"nsis-exe-bytes", WriteMode::CreateNew)?;
                Ok(ok_output(b"Processed 1 file\n"))
            })
            .unwrap();

            assert_eq!(
                run_unrestricted(host.clone(), &[]),
                0,
                "profile={profile:?} ver={good_ver:?} stderr={}",
                String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
            );
            let recorded = calls.lock().unwrap();
            assert_eq!(recorded.len(), 2);
            let pfx = if profile == OsProfile::Windows {
                "/"
            } else {
                "-"
            };
            assert_eq!(recorded[1].args[0], BString::from(format!("{pfx}V3")));
            assert_eq!(recorded[1].args[1], BString::from("-DEXTRA=1"));
            assert!(recorded[1].args[2].ends_with(b"project.nsi"));
            assert_eq!(recorded[1].args[3], BString::from(format!("{pfx}NOCD")));

            let nsi_text = read_utf8(
                host.as_ref(),
                Path::new("/work/_CPack_Packages/NSIS64/project.nsi"),
            );
            assert!(nsi_text.contains(r#"InstallDir "$PROGRAMFILES64\App 1.0.0""#));
            assert!(
                host.metadata(
                    Path::new("/work/_CPack_Packages/NSIS64/NSISOutput.log"),
                    Follow::No
                )
                .is_ok()
            );
            assert_eq!(
                read_bytes(host.as_ref(), Path::new("/work/App-1.0.0.exe")),
                b"nsis-exe-bytes"
            );
        }
    }
}

#[test]
fn nsis_components_reserved_dirs_shortcuts_downloads_and_escaping() {
    let host = make_profile_host(OsProfile::Windows);
    // Test Windows registry discovery for NSIS64 (not in PATH!).
    host.env_set(b"PATH", Some(b"/empty"));
    host.seed_file(Path::new("/nsis64/makensis.exe"), b"mock-nsis")
        .unwrap();
    host.seed_registry_value(
        RegistryRoot::LocalMachine,
        b"SOFTWARE\\NSIS\\Unicode",
        None,
        RegistryView::Bits64,
        b"/nsis64",
    )
    .unwrap();

    host.seed_file(Path::new("/work/img/hdr.bmp"), b"BMP")
        .unwrap();
    host.seed_file(Path::new("/work/payload/a.txt"), b"alpha")
        .unwrap();
    host.seed_file(Path::new("/work/payload/b.txt"), b"beta")
        .unwrap();
    host.seed_file(Path::new("/work/payload/c.txt"), b"gamma")
        .unwrap();
    seed_component_project(
        &host,
        r#"
if(CMAKE_INSTALL_COMPONENT STREQUAL "CON")
  file(INSTALL "/work/payload/a.txt" DESTINATION "${CMAKE_INSTALL_PREFIX}/bin")
elseif(CMAKE_INSTALL_COMPONENT STREQUAL "EndsWithDot.")
  file(INSTALL "/work/payload/b.txt" DESTINATION "${CMAKE_INSTALL_PREFIX}/share")
elseif(CMAKE_INSTALL_COMPONENT STREQUAL "dlcomp")
  file(INSTALL "/work/payload/c.txt" DESTINATION "${CMAKE_INSTALL_PREFIX}/opt")
endif()
"#,
    );

    host.seed_file(
        Path::new("/work/CPackConfig.cmake"),
        br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "NSIS64")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_PACKAGE_NAME "My$App \"Pro\"")
set(CPACK_PACKAGE_VENDOR "Acme")
set(CPACK_PACKAGE_VERSION "3.2.1")
set(CPACK_PACKAGE_FILE_NAME "MyApp-3.2.1")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;MyApp;ALL;/")
set(CPACK_COMPONENTS_ALL "CON;EndsWithDot.;dlcomp")
set(CPACK_ALL_INSTALL_TYPES "Full;Minimal")
set(CPACK_INSTALL_TYPE_FULL_DISPLAY_NAME "Full Install")
set(CPACK_COMPONENT_CON_DISPLAY_NAME "Console $1")
set(CPACK_COMPONENT_CON_DESCRIPTION "Line 1
Line 2 with $var and \"quote\"")
set(CPACK_COMPONENT_CON_REQUIRED ON)
set(CPACK_COMPONENT_ENDSWITHDOT._DEPENDS "CON")
set(CPACK_COMPONENT_ENDSWITHDOT._INSTALL_TYPES "Full")
set(CPACK_COMPONENT_DLCOMP_DOWNLOADED ON)
set(CPACK_DOWNLOAD_SITE "https://cdn.example.test/nsis")
set(CPACK_NSIS_CON_INSTALL_DIRECTORY "$INSTDIR\\CustomCon")
set(CPACK_NSIS_MUI_HEADERIMAGE "/work/img/hdr.bmp")
set(CPACK_NSIS_MANIFEST_DPI_AWARE ON)
set(CPACK_NSIS_BRANDING_TEXT "My $Brand")
set(CPACK_NSIS_BRANDING_TEXT_TRIM_POSITION "RIGHT")
set(CPACK_NSIS_IGNORE_LICENSE_PAGE ON)
set(CPACK_NSIS_IGNORE_COMPONENTS_PAGE ON)
set(CPACK_NSIS_CRC_CHECK "off")
set(CPACK_NSIS_COMPRESSOR "/SOLID lzma")
set(CPACK_PACKAGE_EXECUTABLES "mytool;My Tool")
set(CPACK_CREATE_DESKTOP_LINKS "mytool")
set(CPACK_NSIS_MENU_LINKS "https://example.test;Website;doc/manual.html;Manual")
set(CPACK_NSIS_EXTRA_INSTALL_COMMANDS "DetailPrint \"raw-extra\"")
"#,
    )
    .unwrap();

    host.set_command_handler(|h, req, _| {
        let spec = &req.pipeline[0];
        if spec.args == [BString::from("/VERSION")] {
            return Ok(ok_output(b"v3.08\n"));
        }
        h.write(
            &req.cwd.join("MyApp-3.2.1.exe"),
            b"exe",
            WriteMode::CreateNew,
        )?;
        Ok(ok_output(b"ok"))
    })
    .unwrap();

    assert_eq!(
        run_unrestricted(host.clone(), &[]),
        0,
        "stderr={}",
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
    );

    let nsi = read_utf8(
        host.as_ref(),
        Path::new("/work/_CPack_Packages/NSIS64/project.nsi"),
    );
    // Mirror Tests/CPackNSISGenerator/RunCPackVerifyResult.cmake checks:
    assert!(nsi.contains(r#"!define MUI_HEADERIMAGE_BITMAP "\work\img\hdr.bmp""#));
    assert!(nsi.contains("ManifestDPIAware true"));
    assert!(nsi.contains(r#"BrandingText /TRIMRIGHT "My $$Brand""#));
    assert!(nsi.contains("CRCCheck off"));
    assert!(nsi.contains("SetCompressor /SOLID lzma"));
    assert!(!nsi.contains("MUI_PAGE_LICENSE"));
    assert!(!nsi.contains("MUI_PAGE_COMPONENTS"));
    // Reserved directory names sanitized on Windows host (_CON, EndsWithDot._).
    assert!(nsi.contains(r#"File /r "${INST_DIR}\_CON\*.*""#));
    assert!(nsi.contains(r#"File /r "${INST_DIR}\EndsWithDot._\*.*""#));
    // Custom component install directory + escaping.
    assert!(nsi.contains(r#"SetOutPath "$INSTDIR\CustomCon""#));
    assert!(nsi.contains(r#"Line 1$\r$\nLine 2 with $$var and $\"quote$\""#));
    // Raw channel kept verbatim.
    assert!(nsi.contains(r#"DetailPrint "raw-extra""#));
    // Shortcuts & URL links.
    assert!(nsi.contains(r#"CreateShortCut "$SMPROGRAMS\$STARTMENU_FOLDER\My Tool.lnk""#));
    assert!(nsi.contains(r#"CreateShortCut "$DESKTOP\My Tool.lnk""#));
    assert!(nsi.contains(r#"WriteINIStr "$SMPROGRAMS\$STARTMENU_FOLDER\Website.url""#));
    // Downloaded component created zip archive under CPackUploads and enabled CPACK_USES_DOWNLOAD.
    assert!(nsi.contains("!define CPACK_USES_DOWNLOAD"));
    assert!(nsi.contains("Call DownloadFile"));
    assert!(
        host.metadata(
            Path::new("/work/CPackUploads/MyApp-3.2.1-dlcomp.zip"),
            Follow::No
        )
        .is_ok()
    );
}

#[test]
fn nsis_rejects_bad_trim_compressor_crc_glob_and_absolute_dest() {
    let bad_configs: &[(&str, &str)] = &[
        (
            r#"set(CPACK_NSIS_BRANDING_TEXT "x")
set(CPACK_NSIS_BRANDING_TEXT_TRIM_POSITION "BOGUS")"#,
            "Unsupported branding text trim position",
        ),
        (
            r#"set(CPACK_NSIS_COMPRESSOR "zstd")"#,
            "Unsupported CPACK_NSIS_COMPRESSOR",
        ),
        (
            r#"set(CPACK_NSIS_CRC_CHECK "maybe")"#,
            "Invalid CPACK_NSIS_CRC_CHECK",
        ),
    ];
    for &(extra, expected_err) in bad_configs {
        let host = make_profile_host(OsProfile::Linux);
        seed_tool(&host, OsProfile::Linux, "makensis");
        host.seed_file(Path::new("/work/src/a.txt"), b"a").unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            format!(
                r#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "NSIS")
set(CPACK_PACKAGE_NAME "App")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "App-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
{extra}
"#
            )
            .as_bytes(),
        )
        .unwrap();
        host.set_command_handler(|_, _, _| Ok(ok_output(b"v3.08\n")))
            .unwrap();
        assert_eq!(run_unrestricted(host.clone(), &[]), 1);
        let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
        assert!(stderr.contains(expected_err), "stderr={stderr}");
    }

    // Glob wildcard in staged file name rejected.
    let host_glob = make_profile_host(OsProfile::Linux);
    seed_tool(&host_glob, OsProfile::Linux, "makensis");
    host_glob
        .seed_file(Path::new("/work/src/wild*.txt"), b"x")
        .unwrap();
    host_glob
        .seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "NSIS")
set(CPACK_PACKAGE_NAME "App")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "App-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
"#,
        )
        .unwrap();
    host_glob
        .set_command_handler(|_, _, _| Ok(ok_output(b"v3.08\n")))
        .unwrap();
    assert_eq!(run_unrestricted(host_glob.clone(), &[]), 1);
    assert!(
        String::from_utf8_lossy(&host_glob.stream_bytes(Stream::Stderr))
            .contains("Unsafe NSIS glob character")
    );
}

// ===========================================================================
// 4. WiX v3 and v4 Generator Tests
// ===========================================================================

#[test]
fn wix_v3_and_v4_end_to_end_with_patch_props_rtf_and_scopes() {
    for profile in ALL_PROFILES {
        // --- WiX v4 + AppWiX-style patch + perUser scope + CPackProperties ---
        let host = make_profile_host(profile);
        seed_tool(&host, profile, "wix");
        host.seed_file(
            Path::new("/work/license.txt"),
            "Copyright 2026 {Acme} \\ Café 🚀\nSecond line".as_bytes(),
        )
        .unwrap();
        host.seed_file(Path::new("/work/payload/my_libapp.exe"), b"MZ")
            .unwrap();
        host.seed_file(Path::new("/work/extra/extra.wxs"), b"<Wix/>")
            .unwrap();
        host.seed_file(
            Path::new("/work/patch.xml"),
            br##"<CPackWiXPatch>
  <CPackWiXFragment Id="#PRODUCT">
    <Property Id="PATCHED_PROP" Value="42"/>
  </CPackWiXFragment>
  <CPackWiXFragment Id="CM_CP_applications.bin.my_libapp.exe">
    <Environment Id="MY_ENV" Name="MY_VAR" Value="1" Action="set" System="no"/>
  </CPackWiXFragment>
</CPackWiXPatch>"##,
        )
        .unwrap();
        seed_component_project(
            &host,
            r#"
if(CMAKE_INSTALL_COMPONENT STREQUAL "applications")
  file(INSTALL "/work/payload/my_libapp.exe" DESTINATION "${CMAKE_INSTALL_PREFIX}/bin")
endif()
"#,
        );
        host.seed_file(
            Path::new("/work/props.cmake"),
            br#"
set_property(INSTALL "bin/my_libapp.exe" PROPERTY CPACK_NEVER_OVERWRITE ON)
set_property(INSTALL "bin/my_libapp.exe" PROPERTY CPACK_PERMANENT ON)
set_property(INSTALL "bin/my_libapp.exe" PROPERTY CPACK_WIX_ACL "Everyone=GenericRead,GenericExecute")
set_property(INSTALL "bin/my_libapp.exe" PROPERTY CPACK_START_MENU_SHORTCUTS "My App")
set_property(INSTALL "bin/my_libapp.exe" PROPERTY CPACK_DESKTOP_SHORTCUTS "My App Desktop")
"#,
        )
        .unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "WIX")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_WIX_VERSION "4")
set(CPACK_WIX_INSTALL_SCOPE "perUser")
set(CPACK_WIX_TEST_FIXED_GUIDS "11111111-1111-4111-8111-111111111111;22222222-2222-4222-8222-222222222222")
set(CPACK_PACKAGE_NAME "App$WiX")
set(CPACK_PACKAGE_VENDOR "Acme$Inc")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "AppWiX-1.0.0")
set(CPACK_RESOURCE_FILE_LICENSE "/work/license.txt")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;AppWiX;ALL;/")
set(CPACK_COMPONENTS_ALL "applications")
set(CPACK_WIX_PATCH_FILE "/work/patch.xml")
set(CPACK_WIX_EXTRA_SOURCES "/work/extra/extra.wxs")
set(CPACK_WIX_CULTURES "en-US;de-DE")
set(CPACK_WIX_CUSTOM_XMLNS "util=http://wixtoolset.org/schemas/v4/wxs/util")
set(CPACK_WIX_CAB_PER_COMPONENT ON)
set(CPACK_PRE_BUILD_SCRIPTS "/work/props.cmake")
set(CPACK_PACKAGE_CHECKSUM "SHA256")
"#,
        )
        .unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_ref = Arc::clone(&calls);
        host.set_command_handler(move |h, req, _| {
            let spec = record_cmd(&calls_ref, req);
            let out = to_native(find_arg_after(&spec.args, b"-out").unwrap());
            h.write(&out, b"wix-v4-msi-bytes", WriteMode::CreateNew)?;
            Ok(ok_output(b"wix build ok\n"))
        })
        .unwrap();

        assert_eq!(
            run_unrestricted(host.clone(), &[]),
            0,
            "profile={profile:?} stderr={}",
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
        );
        // Implicit upgrade GUID warning emitted.
        assert!(
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).contains(
                "CPACK_WIX_UPGRADE_GUID implicitly set to 22222222-2222-4222-8222-222222222222"
            )
        );

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].args[0], BString::from("build"));
        assert!(
            recorded[0]
                .args
                .contains(&BString::from("WixToolset.UI.wixext"))
        );
        assert!(recorded[0].args.contains(&BString::from("-culture")));
        assert!(recorded[0].args.contains(&BString::from("-i")));

        let wix_dir = Path::new("/work/_CPack_Packages/WIX");
        let vars_wxi = read_utf8(host.as_ref(), &wix_dir.join("cpack_variables.wxi"));
        assert!(vars_wxi.contains("App$$WiX"));
        assert!(vars_wxi.contains("CPACK_WIX_INSTALL_SCOPE=\"perUser\""));
        assert!(vars_wxi.contains("CPACK_WIX_CAB_PER_COMPONENT=\"ON\""));

        let prod_wxi = read_utf8(host.as_ref(), &wix_dir.join("product_fragment.wxi"));
        assert!(prod_wxi.contains("PATCHED_PROP"));
        assert!(prod_wxi.contains("media1.cab"));

        let dirs_root = parse_xml_bounded(
            &read_bytes(host.as_ref(), &wix_dir.join("directories.wxs")),
            "dirs",
        )
        .unwrap();
        assert!(
            xml_find_all(&dirs_root, "StandardDirectory")
                .iter()
                .any(|e| xml_attr(e, "Id") == Some("LocalAppDataFolder"))
        );
        assert!(
            xml_find_all(&dirs_root, "RemoveFolder")
                .iter()
                .any(|e| xml_attr(e, "Directory") == Some("INSTALL_ROOT"))
        );

        let files_root = parse_xml_bounded(
            &read_bytes(host.as_ref(), &wix_dir.join("files.wxs")),
            "files",
        )
        .unwrap();
        let comp = xml_find_all(&files_root, "Component")
            .into_iter()
            .find(|e| xml_attr(e, "Id") == Some("CM_CP_applications.bin.my_libapp.exe"))
            .unwrap();
        assert_eq!(xml_attr(comp, "NeverOverwrite"), Some("yes"));
        assert_eq!(xml_attr(comp, "Permanent"), Some("yes"));
        assert_eq!(xml_attr(comp, "DiskId"), Some("1"));
        assert!(!xml_find_all(comp, "Environment").is_empty());
        let perms = xml_find_all(comp, "Permission");
        assert_eq!(xml_attr(perms[0], "User"), Some("Everyone"));
        assert_eq!(xml_attr(perms[0], "GenericRead"), Some("yes"));
        // Per-user scope emits HKCU RegistryValue KeyPath on file component.
        let reg = xml_find_all(comp, "RegistryValue");
        assert_eq!(xml_attr(reg[0], "Root"), Some("HKCU"));
        assert_eq!(xml_attr(reg[0], "KeyPath"), Some("yes"));

        // RTF conversion escaped braces, backslash, non-ASCII, and surrogate pair.
        let rtf_bytes = read_bytes(host.as_ref(), &wix_dir.join("License.rtf"));
        assert_eq!(rtf_bytes.last(), Some(&0));
        let rtf_text = String::from_utf8_lossy(&rtf_bytes[..rtf_bytes.len() - 1]);
        assert!(rtf_text.contains(r#"\{Acme\} \\ Caf\u233? \u-10179?\u-8576?\par"#));

        let main_wxs = read_utf8(host.as_ref(), &wix_dir.join("main.wxs"));
        assert!(main_wxs.contains("xmlns:ui=\"http://wixtoolset.org/schemas/v4/wxs/ui\""));
        assert!(main_wxs.contains("xmlns:util=\"http://wixtoolset.org/schemas/v4/wxs/util\""));
        assert!(host.metadata(&wix_dir.join("wix.log"), Follow::No).is_ok());
        assert_eq!(
            read_bytes(host.as_ref(), Path::new("/work/AppWiX-1.0.0.msi")),
            b"wix-v4-msi-bytes"
        );
    }

    // --- WiX v3: candle (with duplicate basename collision) + light ---
    let host3 = make_profile_host(OsProfile::Windows);
    seed_tool(&host3, OsProfile::Windows, "candle");
    seed_tool(&host3, OsProfile::Windows, "light");
    host3
        .seed_file(Path::new("/work/src/app.exe"), b"EXE")
        .unwrap();
    host3
        .seed_file(Path::new("/work/a/dup.wxs"), b"<Wix/>")
        .unwrap();
    host3
        .seed_file(Path::new("/work/b/dup.wxs"), b"<Wix/>")
        .unwrap();
    host3
        .seed_file(Path::new("/work/extra.wixobj"), b"OBJ")
        .unwrap();
    host3
        .seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "WIX")
set(CPACK_WIX_VERSION "3")
set(CPACK_WIX_UPGRADE_GUID "{AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE}")
set(CPACK_PACKAGE_NAME "Legacy")
set(CPACK_PACKAGE_VERSION "2.0.0")
set(CPACK_PACKAGE_FILE_NAME "Legacy-2.0.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_WIX_EXTRA_SOURCES "/work/a/dup.wxs;/work/b/dup.wxs")
set(CPACK_WIX_EXTRA_OBJECTS "/work/extra.wixobj")
set(CPACK_WIX_CULTURES "en-US")
"#,
        )
        .unwrap();

    let calls3 = Arc::new(Mutex::new(Vec::new()));
    let calls3_ref = Arc::clone(&calls3);
    host3
        .set_command_handler(move |h, req, _| {
            let spec = record_cmd(&calls3_ref, req);
            let out = to_native(find_arg_after(&spec.args, b"-out").unwrap());
            h.write(&out, b"wix3-out", WriteMode::CreateNew)?;
            Ok(ok_output(b"ok"))
        })
        .unwrap();
    assert_eq!(
        run_unrestricted(host3.clone(), &[]),
        0,
        "stderr={}",
        String::from_utf8_lossy(&host3.stream_bytes(Stream::Stderr))
    );
    let recorded3 = calls3.lock().unwrap();
    // 4 generated + 2 extra candle runs + 1 light run = 7 invocations
    assert_eq!(recorded3.len(), 7);
    let candle_outs: Vec<String> = recorded3[..6]
        .iter()
        .map(|c| String::from_utf8_lossy(find_arg_after(&c.args, b"-out").unwrap()).into_owned())
        .collect();
    assert!(candle_outs.iter().any(|p| p.ends_with("dup.wixobj")));
    assert!(candle_outs.iter().any(|p| p.ends_with("dup1.wixobj")));
    // External candle source gets -I<work_dir>.
    assert!(recorded3[4].args.iter().any(|a| a.starts_with(b"-I")));
    // Light gets -cultures:en-US, WixUIExtension, and extra.wixobj.
    let light_args = &recorded3[6].args;
    assert!(light_args.contains(&BString::from("-cultures:en-US")));
    assert!(light_args.contains(&BString::from("WixUIExtension")));
    assert!(light_args.contains(&BString::from("/work/extra.wixobj")));
}

#[test]
fn wix_rejects_v4_none_scope_dtd_patch_duplicate_and_unapplied_fragments() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "set(CPACK_WIX_VERSION \"4\")\nset(CPACK_WIX_INSTALL_SCOPE \"NONE\")\n",
            "",
            "not supported with WiX version 4",
        ),
        (
            "set(CPACK_WIX_VERSION \"4\")\nset(CPACK_WIX_PATCH_FILE \"/work/p.xml\")\n",
            "<!DOCTYPE x [<!ENTITY e SYSTEM \"file:///etc/passwd\">]><CPackWiXPatch/>",
            "DTD and ENTITY declarations are forbidden",
        ),
        (
            "set(CPACK_WIX_VERSION \"4\")\nset(CPACK_WIX_PATCH_FILE \"/work/p.xml\")\n",
            "<CPackWiXPatch><CPackWiXFragment Id=\"#PRODUCT\"/><CPackWiXFragment Id=\"#PRODUCT\"/></CPackWiXPatch>",
            "Invalid reuse of 'CPackWiXFragment' 'Id'",
        ),
        (
            "set(CPACK_WIX_VERSION \"4\")\nset(CPACK_WIX_PATCH_FILE \"/work/p.xml\")\n",
            "<CPackWiXPatch><CPackWiXFragment Id=\"NO_SUCH_ID\"/></CPackWiXPatch>",
            "did not have matching IDs",
        ),
    ];
    for &(cfg_extra, patch_xml, expected_err) in cases {
        let host = make_profile_host(OsProfile::Windows);
        seed_tool(&host, OsProfile::Windows, "wix");
        host.seed_file(Path::new("/work/src/a.txt"), b"a").unwrap();
        if !patch_xml.is_empty() {
            host.seed_file(Path::new("/work/p.xml"), patch_xml.as_bytes())
                .unwrap();
        }
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            format!(
                r#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "WIX")
set(CPACK_PACKAGE_NAME "App")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "App-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
{cfg_extra}
"#
            )
            .as_bytes(),
        )
        .unwrap();
        let invoked = Arc::new(AtomicUsize::new(0));
        let invoked_ref = Arc::clone(&invoked);
        host.set_command_handler(move |_, _, _| {
            invoked_ref.fetch_add(1, Ordering::SeqCst);
            Ok(ok_output(b""))
        })
        .unwrap();
        assert_eq!(run_unrestricted(host.clone(), &[]), 1);
        assert_eq!(invoked.load(Ordering::SeqCst), 0);
        let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
        assert!(stderr.contains(expected_err), "stderr={stderr}");
    }
}

// ===========================================================================
// 5. IFW Generator Tests
// ===========================================================================

#[test]
fn ifw_extensions_version_gating_product_images_repos_and_secret_redaction() {
    // Verify per-platform default extension (.dmg / .run / .exe).
    for (profile, expected_ext) in [
        (OsProfile::MacOs, ".dmg"),
        (OsProfile::Linux, ".run"),
        (OsProfile::Windows, ".exe"),
    ] {
        let host = make_profile_host(profile);
        seed_tool(&host, profile, "binarycreator");
        seed_tool(&host, profile, "repogen");
        host.seed_file(Path::new("/work/src/bin/app"), b"bin")
            .unwrap();
        host.seed_file(Path::new("/work/img/p1.png"), b"PNG1")
            .unwrap();
        host.seed_file(Path::new("/work/img/p2.png"), b"PNG2")
            .unwrap();
        host.seed_file(Path::new("/work/res/extra.txt"), b"res")
            .unwrap();
        host.seed_file(
            Path::new("/work/res/app.qrc"),
            b"<RCC><qresource><file>extra.txt</file></qresource></RCC>",
        )
        .unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_IFW_FRAMEWORK_VERSION "4.2.0")
set(CPACK_PACKAGE_NAME "QtApp")
set(CPACK_PACKAGE_VERSION "2.0.0")
set(CPACK_PACKAGE_FILE_NAME "QtApp-2.0.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_IFW_PACKAGE_WIZARD_SHOW_PAGE_LIST OFF)
set(CPACK_IFW_PACKAGE_RUN_PROGRAM "@TargetDir@/bin/app")
set(CPACK_IFW_PACKAGE_RUN_PROGRAM_ARGUMENTS "--quiet;--ok")
set(CPACK_IFW_PACKAGE_RUN_PROGRAM_DESCRIPTION "Launch QtApp")
set(CPACK_IFW_PACKAGE_PRODUCT_IMAGES "/work/img/p1.png;/work/img/p2.png")
set(CPACK_IFW_PACKAGE_PRODUCT_IMAGE_URLS "https://example.test/1;https://example.test/2")
set(CPACK_IFW_PACKAGE_RESOURCES "/work/res/app.qrc")
set(CPACK_IFW_ARCHIVE_FORMAT "zip")
set(CPACK_IFW_ARCHIVE_COMPRESSION "5")
set(CPACK_IFW_REPOSITORIES_ALL "mainrepo;updadd;updrm;updrep")
set(CPACK_IFW_REPOSITORY_MAINREPO_URL "https://repo.example.test/main")
set(CPACK_IFW_REPOSITORY_MAINREPO_USERNAME "alice")
set(CPACK_IFW_REPOSITORY_MAINREPO_PASSWORD "SUPER_SECRET_PASS")
set(CPACK_IFW_REPOSITORY_UPDADD_ADD ON)
set(CPACK_IFW_REPOSITORY_UPDADD_URL "https://repo.example.test/add")
set(CPACK_IFW_REPOSITORY_UPDRM_REMOVE ON)
set(CPACK_IFW_REPOSITORY_UPDRM_URL "https://repo.example.test/old")
set(CPACK_IFW_REPOSITORY_UPDREP_REPLACE ON)
set(CPACK_IFW_REPOSITORY_UPDREP_OLD_URL "https://repo.example.test/from")
set(CPACK_IFW_REPOSITORY_UPDREP_NEW_URL "https://repo.example.test/to")
set(CPACK_PACKAGE_CHECKSUM "SHA256")
"#,
        )
        .unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_ref = Arc::clone(&calls);
        host.set_command_handler(move |h, req, _| {
            let spec = record_cmd(&calls_ref, req);
            let prog = String::from_utf8_lossy(&spec.program).into_owned();
            if prog.contains("repogen") {
                let repo_dir = to_native(spec.args.last().unwrap());
                h.create_dir_all(&repo_dir)?;
                h.write(
                    &repo_dir.join("Updates.xml"),
                    b"<?xml version=\"1.0\"?>\n<Updates><Checksum>true</Checksum><PackageUpdate><Name>root</Name></PackageUpdate></Updates>\n",
                    WriteMode::CreateNew,
                )?;
                Ok(ok_output(b"repogen ok"))
            } else {
                let out = to_native(spec.args.last().unwrap());
                h.write(&out, b"ifw-installer-bytes", WriteMode::CreateNew)?;
                Ok(ok_output(b"binarycreator ok"))
            }
        })
        .unwrap();

        assert_eq!(
            run_unrestricted(host.clone(), &[]),
            0,
            "profile={profile:?} stderr={}",
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
        );

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 2); // repogen + binarycreator
        assert!(
            recorded[0]
                .args
                .contains(&BString::from("--archive-format"))
        );
        assert!(recorded[0].args.contains(&BString::from("--compression")));
        assert!(!recorded[0].args.contains(&BString::from("-c"))); // >= 2.0 omits -c
        assert!(recorded[1].args.contains(&BString::from("-r")));

        let stage_dir = Path::new("/work/_CPack_Packages/IFW/QtApp-2.0.0");
        let config_root = parse_xml_bounded(
            &read_bytes(host.as_ref(), &stage_dir.join("config/config.xml")),
            "cfg",
        )
        .unwrap();
        // Verify @ApplicationsDir@ / @RootDir@ preserved verbatim and ProductImages emitted.
        let target_dir = xml_text(xml_find_all(&config_root, "TargetDir")[0]);
        assert!(target_dir.starts_with("@RootDir@") || target_dir.starts_with("@ApplicationsDir@"));
        assert_eq!(
            xml_text(xml_find_all(&config_root, "WizardShowPageList")[0]),
            "false"
        );
        let imgs = xml_find_all(&config_root, "ProductImage");
        assert_eq!(imgs.len(), 2);
        assert_eq!(xml_text(xml_find_all(imgs[0], "Image")[0]), "p1.png");
        assert_eq!(
            xml_text(xml_find_all(imgs[0], "Url")[0]),
            "https://example.test/1"
        );
        // Verify Updates.xml was safely patched with add/remove/replace.
        let updates_root = parse_xml_bounded(
            &read_bytes(host.as_ref(), &stage_dir.join("repository/Updates.xml")),
            "Updates.xml",
        )
        .unwrap();
        let repo_updates = xml_find_all(&updates_root, "Repository");
        assert_eq!(repo_updates.len(), 3);
        assert_eq!(xml_attr(repo_updates[0], "action"), Some("add"));
        assert_eq!(xml_attr(repo_updates[1], "action"), Some("remove"));
        assert_eq!(xml_attr(repo_updates[2], "action"), Some("replace"));

        // Verify installer published with exact extension, and repository/ NOT published.
        let expected_file = format!("QtApp-2.0.0{expected_ext}");
        assert_eq!(
            read_bytes(host.as_ref(), &Path::new("/work").join(&expected_file)),
            b"ifw-installer-bytes"
        );
        assert!(
            host.metadata(Path::new("/work/repository"), Follow::No)
                .is_err()
        );
    }

    // Verify repository password redaction on tool failure in both IFWOutput.log and stderr.
    let host_fail = make_profile_host(OsProfile::Linux);
    seed_tool(&host_fail, OsProfile::Linux, "binarycreator");
    seed_tool(&host_fail, OsProfile::Linux, "repogen");
    host_fail
        .seed_file(Path::new("/work/src/a.txt"), b"a")
        .unwrap();
    host_fail
        .seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_PACKAGE_NAME "SecApp")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "SecApp-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_IFW_REPOSITORIES_ALL "r1")
set(CPACK_IFW_REPOSITORY_R1_URL "https://repo.example.test")
set(CPACK_IFW_REPOSITORY_R1_PASSWORD "TOP_SECRET_PWD_999")
"#,
        )
        .unwrap();
    host_fail
        .set_command_handler(|_, _, _| {
            Ok(fail_output(
                1,
                b"auth error for password=TOP_SECRET_PWD_999!\n",
            ))
        })
        .unwrap();
    assert_eq!(run_unrestricted(host_fail.clone(), &[]), 1);
    let stderr = String::from_utf8_lossy(&host_fail.stream_bytes(Stream::Stderr)).into_owned();
    let log_text = read_utf8(
        host_fail.as_ref(),
        Path::new("/work/_CPack_Packages/IFW/IFWOutput.log"),
    );
    assert!(
        !stderr.contains("TOP_SECRET_PWD_999"),
        "stderr leaked secret: {stderr}"
    );
    assert!(
        !log_text.contains("TOP_SECRET_PWD_999"),
        "log leaked secret: {log_text}"
    );
    assert!(log_text.contains("***"));
}

#[test]
fn ifw_shared_package_coalescing_overlap_and_qrc_traversal_rejection() {
    // 1) A shared IFW package merges disjoint files. An overlapping file takes the content
    // of the later component in install order.
    for (same_file, expect_ok) in [(false, true), (true, true)] {
        let host = make_profile_host(OsProfile::Linux);
        seed_tool(&host, OsProfile::Linux, "binarycreator");
        host.seed_file(Path::new("/work/payload/a.txt"), b"AAA")
            .unwrap();
        host.seed_file(Path::new("/work/payload/b.txt"), b"BBB")
            .unwrap();
        let b_dest = if same_file {
            "file(INSTALL \"/work/payload/b.txt\" DESTINATION \"${CMAKE_INSTALL_PREFIX}/bin\" RENAME \"a.txt\")"
        } else {
            "file(INSTALL \"/work/payload/b.txt\" DESTINATION \"${CMAKE_INSTALL_PREFIX}/bin\")"
        };
        seed_component_project(
            &host,
            &format!(
                r#"
if(CMAKE_INSTALL_COMPONENT STREQUAL "c1")
  file(INSTALL "/work/payload/a.txt" DESTINATION "${{CMAKE_INSTALL_PREFIX}}/bin")
elseif(CMAKE_INSTALL_COMPONENT STREQUAL "c2")
  {b_dest}
endif()
"#
            ),
        );
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_IFW_FRAMEWORK_VERSION "3.0.0")
set(CPACK_PACKAGE_NAME "Coal")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "Coal-1.0")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;Coal;ALL;/")
set(CPACK_COMPONENTS_ALL "c1;c2")
set(CPACK_IFW_COMPONENT_C1_NAME "org.shared.pkg")
set(CPACK_IFW_COMPONENT_C2_NAME "org.shared.pkg")
set(CPACK_IFW_COMPONENT_C1_DEPENDS "my-dep:>=1.2")
"#,
        )
        .unwrap();
        host.set_command_handler(|h, req, _| {
            let out = to_native(req.pipeline[0].args.last().unwrap());
            h.write(&out, b"ok", WriteMode::CreateNew)?;
            Ok(ok_output(b""))
        })
        .unwrap();

        let code = run_unrestricted(host.clone(), &[]);
        assert_eq!(
            code == 0,
            expect_ok,
            "same_file={same_file} stderr={}",
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
        );
        if expect_ok {
            // Hyphenated dependency on QtIFW 3.0 (< 3.1) emits warning.
            assert!(
                String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
                    .contains("requires QtIFW 3.1 or later")
            );
            let pkg_root = parse_xml_bounded(
                &read_bytes(
                    host.as_ref(),
                    Path::new(
                        "/work/_CPack_Packages/IFW/Coal-1.0/packages/org.shared.pkg/meta/package.xml",
                    ),
                ),
                "package.xml",
            )
            .unwrap();
            assert_eq!(
                xml_text(xml_find_all(&pkg_root, "Dependencies")[0]),
                "my-dep:>=1.2"
            );
            let shared = Path::new(
                "/work/_CPack_Packages/IFW/Coal-1.0/packages/org.shared.pkg/data/bin/a.txt",
            );
            let expected: &[u8] = if same_file { b"BBB" } else { b"AAA" };
            assert_eq!(read_bytes(host.as_ref(), shared), expected);
        }
    }

    // 1b) Default framework version (1.9.9) emits <2.0 policy + repogen -c, and --online-only / -e modes work.
    {
        let host = make_profile_host(OsProfile::MacOs);
        seed_tool(&host, OsProfile::MacOs, "binarycreator");
        seed_tool(&host, OsProfile::MacOs, "repogen");
        host.seed_file(Path::new("/work/src/a.txt"), b"a").unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_IFW_PACKAGE_FILE_EXTENSION ".custom")
set(CPACK_PACKAGE_NAME "LegacyIfw")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "LegacyIfw-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_IFW_DOWNLOAD_ALL ON)
set(CPACK_DOWNLOAD_SITE "https://repo.example.test/v1")
"#,
        )
        .unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_ref = Arc::clone(&calls);
        host.set_command_handler(move |h, req, _| {
            let spec = record_cmd(&calls_ref, req);
            let prog = String::from_utf8_lossy(&spec.program).into_owned();
            if prog.contains("repogen") {
                let dir = to_native(spec.args.last().unwrap());
                h.create_dir_all(&dir)?;
                Ok(ok_output(b""))
            } else {
                let out = to_native(spec.args.last().unwrap());
                h.write(&out, b"online-installer", WriteMode::CreateNew)?;
                Ok(ok_output(b""))
            }
        })
        .unwrap();
        assert_eq!(run_unrestricted(host.clone(), &[]), 0);
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        // QtIFW < 2.0 passes -c to repogen!
        assert!(recorded[0].args.contains(&BString::from("-c")));
        assert!(recorded[1].args.contains(&BString::from("--online-only")));
        let cfg_xml = read_utf8(
            host.as_ref(),
            Path::new("/work/_CPack_Packages/IFW/LegacyIfw-1.0/config/config.xml"),
        );
        assert!(cfg_xml.contains("QtIFW less 2.0"));
        assert!(cfg_xml.contains("<AllowNonAsciiCharacters>true</AllowNonAsciiCharacters>"));
        assert_eq!(
            read_bytes(host.as_ref(), Path::new("/work/LegacyIfw-1.0.custom")),
            b"online-installer"
        );
    }

    // 1c) IFW package dependency cycle rejected before invoking binarycreator.
    {
        let host = make_profile_host(OsProfile::Linux);
        seed_tool(&host, OsProfile::Linux, "binarycreator");
        seed_component_project(&host, "");
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_PACKAGE_NAME "IfwCycle")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "IfwCycle-1.0")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;IfwCycle;ALL;/")
set(CPACK_COMPONENTS_ALL "x;y")
set(CPACK_IFW_COMPONENT_X_DEPENDS "y")
set(CPACK_IFW_COMPONENT_Y_DEPENDS "x")
"#,
        )
        .unwrap();
        let invoked = Arc::new(AtomicUsize::new(0));
        let invoked_ref = Arc::clone(&invoked);
        host.set_command_handler(move |_, _, _| {
            invoked_ref.fetch_add(1, Ordering::SeqCst);
            Ok(ok_output(b""))
        })
        .unwrap();
        assert_eq!(run_unrestricted(host.clone(), &[]), 1);
        assert_eq!(invoked.load(Ordering::SeqCst), 0);
        assert!(
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).contains("Cycle detected")
        );
    }

    // 2) QRC path traversal rejected before invoking binarycreator.
    let host_qrc = make_profile_host(OsProfile::Linux);
    seed_tool(&host_qrc, OsProfile::Linux, "binarycreator");
    host_qrc
        .seed_file(Path::new("/work/src/a.txt"), b"a")
        .unwrap();
    host_qrc
        .seed_file(
            Path::new("/work/res/bad.qrc"),
            b"<RCC><qresource><file>../../etc/passwd</file></qresource></RCC>",
        )
        .unwrap();
    host_qrc
        .seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_PACKAGE_NAME "Qrc")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "Qrc-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_IFW_PACKAGE_RESOURCES "/work/res/bad.qrc")
"#,
        )
        .unwrap();
    let invoked = Arc::new(AtomicUsize::new(0));
    let invoked_ref = Arc::clone(&invoked);
    host_qrc
        .set_command_handler(move |_, _, _| {
            invoked_ref.fetch_add(1, Ordering::SeqCst);
            Ok(ok_output(b""))
        })
        .unwrap();
    assert_eq!(run_unrestricted(host_qrc.clone(), &[]), 1);
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    assert!(
        String::from_utf8_lossy(&host_qrc.stream_bytes(Stream::Stderr))
            .contains("escapes base directory")
    );
}

// ===========================================================================
// 6. Post-Hook Staged-Tree Containment Regression Tests
// ===========================================================================

#[test]
fn audit9_post_hook_symlinked_parent_escape_blocks_all_generators() {
    for (gen_name, extra_cfg, tools) in [
        ("productbuild", "", &["pkgbuild", "productbuild"][..]),
        ("NSIS", "", &["makensis"][..]),
        ("WIX", "set(CPACK_WIX_VERSION \"4\")\n", &["wix"][..]),
        ("IFW", "", &["binarycreator"][..]),
    ] {
        let host = make_profile_host(OsProfile::Linux);
        for tool in tools {
            seed_tool(&host, OsProfile::Linux, tool);
        }
        host.seed_file(
            Path::new("/work/outside/secret.txt"),
            b"HARMLESS_OUTSIDE_MARKER",
        )
        .unwrap();
        host.seed_file(Path::new("/work/payload/item.txt"), b"inside")
            .unwrap();
        seed_component_project(
            &host,
            r#"
if(CMAKE_INSTALL_COMPONENT STREQUAL "comp")
  file(INSTALL "/work/payload/item.txt" DESTINATION "${CMAKE_INSTALL_PREFIX}/sub")
endif()
"#,
        );
        // Pre-build hook swaps the recorded parent directory `sub` with a symlink to `/work/outside`!
        let pre_hook = if gen_name == "productbuild" {
            r#"
file(REMOVE_RECURSE "${CPACK_TEMPORARY_DIRECTORY}/comp/Applications/sub")
file(CREATE_LINK "/work/outside" "${CPACK_TEMPORARY_DIRECTORY}/comp/Applications/sub" SYMBOLIC)
"#
        } else {
            r#"
file(REMOVE_RECURSE "${CPACK_TEMPORARY_DIRECTORY}/comp/sub")
file(CREATE_LINK "/work/outside" "${CPACK_TEMPORARY_DIRECTORY}/comp/sub" SYMBOLIC)
"#
        };
        host.seed_file(Path::new("/work/pre_hook.cmake"), pre_hook.as_bytes())
            .unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            format!(
                r#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "{gen_name}")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_PACKAGE_NAME "Audit9")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "Audit9-1.0.0")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;Audit9;ALL;/")
set(CPACK_COMPONENTS_ALL "comp")
set(CPACK_PRE_BUILD_SCRIPTS "/work/pre_hook.cmake")
{extra_cfg}
"#
            )
            .as_bytes(),
        )
        .unwrap();

        let build_invocations = Arc::new(AtomicUsize::new(0));
        let build_inv_ref = Arc::clone(&build_invocations);
        host.set_command_handler(move |_, req, _| {
            let spec = &req.pipeline[0];
            // Allow NSIS version probe in prepare_config, count any actual build call!
            if spec.args == [BString::from("-VERSION")] || spec.args == [BString::from("/VERSION")]
            {
                return Ok(ok_output(b"v3.08\n"));
            }
            build_inv_ref.fetch_add(1, Ordering::SeqCst);
            Ok(ok_output(b""))
        })
        .unwrap();

        let code = run_unrestricted(host.clone(), &[]);
        assert_eq!(
            code, 1,
            "Audit #9 failed to block post-hook symlinked parent escape for {gen_name}"
        );
        assert_eq!(
            build_invocations.load(Ordering::SeqCst),
            0,
            "Audit #9: external build tool must NEVER be invoked for {gen_name} after parent escape!"
        );
    }
}

#[test]
fn audit9_chained_payload_symlink_escape_rejected() {
    let host = make_profile_host(OsProfile::MacOs);
    seed_tool(&host, OsProfile::MacOs, "pkgbuild");
    seed_tool(&host, OsProfile::MacOs, "productbuild");
    host.seed_file(Path::new("/work/outside/marker"), b"OUTSIDE")
        .unwrap();
    host.seed_file(Path::new("/work/src/ok.txt"), b"ok")
        .unwrap();
    // Pre-build hook introduces chained symlinks: a -> b, b -> ../../../outside/marker
    host.seed_file(
        Path::new("/work/hook.cmake"),
        br#"
file(CREATE_LINK "b" "${CPACK_TEMPORARY_DIRECTORY}/Applications/a" SYMBOLIC)
file(CREATE_LINK "../../../../outside/marker" "${CPACK_TEMPORARY_DIRECTORY}/Applications/b" SYMBOLIC)
"#,
    )
    .unwrap();
    host.seed_file(
        Path::new("/work/CPackConfig.cmake"),
        br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "productbuild")
set(CPACK_PACKAGE_NAME "Chain")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "Chain-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_PRE_BUILD_SCRIPTS "/work/hook.cmake")
"#,
    )
    .unwrap();
    let invoked = Arc::new(AtomicUsize::new(0));
    let invoked_ref = Arc::clone(&invoked);
    host.set_command_handler(move |_, _, _| {
        invoked_ref.fetch_add(1, Ordering::SeqCst);
        Ok(ok_output(b""))
    })
    .unwrap();

    assert_eq!(run_unrestricted(host.clone(), &[]), 1);
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    assert!(
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
            .contains("escapes staging root")
    );
}

// ===========================================================================
// 7. Publication Fault-Injection and Rollback Tests
// ===========================================================================

/// Host wrapper injecting controlled I/O faults during publication staging and rename.
struct FaultInjectHost {
    inner: Arc<PureHost>,
    fail_write_from_tmp: AtomicBool,
    fail_write_checksum_tmp: AtomicBool,
    partial_write_before_fail: AtomicBool,
    fail_rename_dest_suffix: Mutex<Option<String>>,
    fail_rename_from_prefix: Mutex<Option<String>>,
}

impl Host for FaultInjectHost {
    rcmake_host::forward_host!(inner, env, process);
    rcmake_host::forward_host!(@method inner, metadata(p: &Path, follow: Follow) -> HostResult<rcmake_host::Metadata>);
    rcmake_host::forward_host!(@method inner, metadata_fresh(p: &Path, follow: Follow) -> HostResult<rcmake_host::Metadata>);
    rcmake_host::forward_host!(@ inner, read);
    rcmake_host::forward_host!(@ inner, read_with_origin);
    rcmake_host::forward_host!(@ inner, read_chunks);
    rcmake_host::forward_host!(@method inner, read_dir(p: &Path) -> HostResult<Vec<rcmake_host::DirEntry>>);
    rcmake_host::forward_host!(@method inner, create_dir_all(p: &Path) -> HostResult<()>);
    rcmake_host::forward_host!(@method inner, create_private_dir(p: &Path) -> HostResult<()>);
    rcmake_host::forward_host!(@method inner, remove_file(p: &Path) -> HostResult<()>);
    rcmake_host::forward_host!(@method inner, remove_dir(p: &Path) -> HostResult<()>);
    rcmake_host::forward_host!(@method inner, remove_dir_all(p: &Path) -> HostResult<()>);
    rcmake_host::forward_host!(@method inner, rename_noreplace(from: &Path, to: &Path) -> HostResult<()>);
    rcmake_host::forward_host!(@method inner, lock_file(path: &Path, timeout: Option<core::time::Duration>) -> HostResult<Box<dyn rcmake_host::FileLockGuard>>);
    rcmake_host::forward_host!(@method inner, copy_file(from: &Path, to: &Path, opts: rcmake_host::CopyOptions) -> HostResult<()>);
    rcmake_host::forward_host!(@method inner, symlink(target: &Path, link: &Path) -> HostResult<()>);
    rcmake_host::forward_host!(@method inner, hard_link(src: &Path, dst: &Path) -> HostResult<()>);
    rcmake_host::forward_host!(@ inner, read_link);
    rcmake_host::forward_host!(@method inner, canonicalize(p: &Path) -> HostResult<PathBuf>);
    rcmake_host::forward_host!(@method inner, canonicalize_fresh(p: &Path) -> HostResult<PathBuf>);
    rcmake_host::forward_host!(@method inner, set_permissions(p: &Path, mode: u32) -> HostResult<()>);
    rcmake_host::forward_host!(@method inner, set_times(p: &Path, atime: Option<rcmake_host::FileTime>, mtime: Option<rcmake_host::FileTime>) -> HostResult<()>);
    rcmake_host::forward_host!(@method inner, set_symlink_times(path: &Path, atime: Option<rcmake_host::FileTime>, mtime: Option<rcmake_host::FileTime>) -> HostResult<()>);
    rcmake_host::forward_host!(@ inner, open_read);
    rcmake_host::forward_host!(@ inner, scratch_file);
    rcmake_host::forward_host!(@ inner, scratch_file_in);

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        let from_name = from
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let from_fails = self
            .fail_rename_from_prefix
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|prefix| from_name.is_some_and(|name| name.starts_with(prefix.as_str())));
        if from_fails
            || self
                .fail_rename_dest_suffix
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|suffix| to.to_string_lossy().ends_with(suffix.as_str()))
        {
            return Err(HostError::from_io(
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected publication rename fault",
                ),
                Some(to.to_path_buf()),
            ));
        }
        self.inner.rename(from, to)
    }

    fn write(&self, path: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if self.fail_write_checksum_tmp.load(Ordering::SeqCst) && name.starts_with(".rcmake-pub-") {
            if self.partial_write_before_fail.load(Ordering::SeqCst) {
                let _ = self.inner.write(path, b"partial", mode);
            }
            return Err(HostError::from_io(
                std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "injected checksum staging fault",
                ),
                Some(path.to_path_buf()),
            ));
        }
        self.inner.write(path, data, mode)
    }

    fn write_from(
        &self,
        path: &Path,
        mode: WriteMode,
        input: &mut dyn Read,
        len: u64,
    ) -> HostResult<()> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if self.fail_write_from_tmp.load(Ordering::SeqCst) && name.starts_with(".rcmake-pub-") {
            if self.partial_write_before_fail.load(Ordering::SeqCst) {
                let _ = self.inner.write(path, b"partial", mode);
            }
            return Err(HostError::from_io(
                std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "injected installer staging fault",
                ),
                Some(path.to_path_buf()),
            ));
        }
        self.inner.write_from(path, mode, input, len)
    }

    fn registry_host_view(&self) -> Option<RegistryView> {
        self.inner.registry_host_view()
    }
    fn registry_value(
        &self,
        root: RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: RegistryView,
    ) -> HostResult<Option<BString>> {
        self.inner.registry_value(root, subkey, name, view)
    }
    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        self.inner.write_out(stream, bytes);
    }
    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        self.inner.read_stdin(limit)
    }
    fn is_terminal(&self, stream: Stream) -> bool {
        self.inner.is_terminal(stream)
    }
    fn terminal_width(&self, stream: Stream) -> Option<usize> {
        self.inner.terminal_width(stream)
    }
    fn now(&self) -> rcmake_host::FileTime {
        self.inner.now()
    }
    fn monotonic(&self) -> std::time::Duration {
        self.inner.monotonic()
    }
    fn sleep(&self, duration: std::time::Duration) {
        self.inner.sleep(duration);
    }
    fn time_zone(&self) -> rcmake_host::TimeZone {
        self.inner.time_zone()
    }
    fn current_exe(&self) -> HostResult<PathBuf> {
        self.inner.current_exe()
    }
    fn initial_cwd(&self) -> PathBuf {
        self.inner.initial_cwd()
    }
    fn system(&self) -> SystemInfo {
        self.inner.system()
    }
    fn effective_uid(&self) -> Option<u32> {
        self.inner.effective_uid()
    }
}

#[test]
fn audit10_atomic_publication_preserves_previous_installer_and_checksums_on_fault() {
    for fail_on_checksum in [false, true] {
        let inner = make_profile_host(OsProfile::Linux);
        seed_tool(&inner, OsProfile::Linux, "binarycreator");
        inner
            .seed_file(Path::new("/work/src/a.txt"), b"v2")
            .unwrap();
        // Pre-existing published installer + checksum from previous release!
        let prev_installer = b"PREVIOUS_GOOD_INSTALLER_V1";
        let prev_sha = format!(
            "{}  App-1.0.run\n",
            HashAlgo::Sha256.hex_digest(prev_installer)
        );
        inner
            .seed_file(Path::new("/work/App-1.0.run"), prev_installer)
            .unwrap();
        inner
            .seed_file(Path::new("/work/App-1.0.run.sha256"), prev_sha.as_bytes())
            .unwrap();
        inner
            .seed_file(
                Path::new("/work/CPackConfig.cmake"),
                br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_PACKAGE_NAME "App")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "App-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_PACKAGE_CHECKSUM "SHA256")
"#,
            )
            .unwrap();
        inner
            .set_command_handler(|h, req, _| {
                let out = to_native(req.pipeline[0].args.last().unwrap());
                h.write(&out, b"NEW_INSTALLER_V2", WriteMode::CreateNew)?;
                Ok(ok_output(b"ok"))
            })
            .unwrap();

        let wrapper = Arc::new(FaultInjectHost {
            inner: Arc::clone(&inner),
            fail_write_from_tmp: AtomicBool::new(!fail_on_checksum),
            fail_write_checksum_tmp: AtomicBool::new(fail_on_checksum),
            partial_write_before_fail: AtomicBool::new(true),
            fail_rename_dest_suffix: Mutex::new(None),
            fail_rename_from_prefix: Mutex::new(None),
        });

        assert_eq!(run_unrestricted(wrapper.clone(), &[]), 1);

        // Previous published installer and checksum MUST remain 100% intact!
        assert_eq!(
            read_bytes(inner.as_ref(), Path::new("/work/App-1.0.run")),
            prev_installer
        );
        assert_eq!(
            read_utf8(inner.as_ref(), Path::new("/work/App-1.0.run.sha256")),
            prev_sha
        );
        // Zero stray temporary staging files left behind in /work.
        for entry in inner.read_dir(Path::new("/work")).unwrap() {
            assert!(
                !entry.name.starts_with(b".rcmake-pub-"),
                "stray temp file left behind: {}",
                entry.name
            );
        }

        // Now clear the fault and verify repeat run atomically replaces both files.
        wrapper.fail_write_from_tmp.store(false, Ordering::SeqCst);
        wrapper
            .fail_write_checksum_tmp
            .store(false, Ordering::SeqCst);
        assert_eq!(run_unrestricted(wrapper, &[]), 0);
        assert_eq!(
            read_bytes(inner.as_ref(), Path::new("/work/App-1.0.run")),
            b"NEW_INSTALLER_V2"
        );
        let expected_v2_sha = format!(
            "{}  App-1.0.run\n",
            HashAlgo::Sha256.hex_digest(b"NEW_INSTALLER_V2")
        );
        assert_eq!(
            read_utf8(inner.as_ref(), Path::new("/work/App-1.0.run.sha256")),
            expected_v2_sha
        );
    }
}

// ===========================================================================
// 8. Failure-Path Matrix Across All Four Generators
// ===========================================================================

#[derive(Clone, Copy, Debug)]
enum FailureMode {
    ExitZeroNoOutput,
    ExitZeroStaleNotRecreated,
    OutputIsSymlink,
    OutputIsDirectory,
    TimedOut,
    NonZeroExit,
}

#[test]
fn failure_path_matrix_across_all_installer_generators() {
    let modes = [
        FailureMode::ExitZeroNoOutput,
        FailureMode::ExitZeroStaleNotRecreated,
        FailureMode::OutputIsSymlink,
        FailureMode::OutputIsDirectory,
        FailureMode::TimedOut,
        FailureMode::NonZeroExit,
    ];
    let gens: &[(&str, &str, &str, &[&str])] = &[
        ("productbuild", ".pkg", "", &["pkgbuild", "productbuild"]),
        ("NSIS", ".exe", "", &["makensis"]),
        ("WIX", ".msi", "set(CPACK_WIX_VERSION \"4\")\n", &["wix"]),
        ("IFW", ".run", "", &["binarycreator"]),
    ];

    for &(gen_name, ext, extra_cfg, tools) in gens {
        for mode in modes {
            let host = make_profile_host(OsProfile::Linux);
            for tool in tools {
                seed_tool(&host, OsProfile::Linux, tool);
            }
            host.seed_file(Path::new("/work/src/f.txt"), b"x").unwrap();
            host.seed_file(
                Path::new("/work/CPackConfig.cmake"),
                format!(
                    r#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "{gen_name}")
set(CPACK_PACKAGE_NAME "FailTest")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "FailTest-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
{extra_cfg}
"#
                )
                .as_bytes(),
            )
            .unwrap();

            let expected_out = PathBuf::from(format!(
                "/work/_CPack_Packages/{gen_name}/FailTest-1.0{ext}"
            ));
            if matches!(mode, FailureMode::ExitZeroStaleNotRecreated) {
                host.seed_file(&expected_out, b"STALE_ARTIFACT").unwrap();
            }

            let expected_out_clone = expected_out.clone();
            let gen_owned = gen_name.to_owned();
            host.set_command_handler(move |h, req, _| {
                let spec = &req.pipeline[0];
                if spec.args == [BString::from("-VERSION")] {
                    return Ok(ok_output(b"v3.08\n"));
                }
                let prog = String::from_utf8_lossy(&spec.program).into_owned();
                // For productbuild, let leaf pkgbuild succeed so we test the final tool phase
                // (except NonZeroExit/TimedOut which can also fail at pkgbuild and must stop productbuild).
                if gen_owned == "productbuild" && prog.contains("pkgbuild") {
                    match mode {
                        FailureMode::TimedOut => return Ok(timeout_output()),
                        FailureMode::NonZeroExit => return Ok(fail_output(2, b"pkgbuild err\n")),
                        _ => {
                            let leaf = to_native(spec.args.last().unwrap());
                            h.write(&leaf, b"leaf", WriteMode::CreateNew)?;
                            return Ok(ok_output(b""));
                        }
                    }
                }
                match mode {
                    FailureMode::ExitZeroNoOutput | FailureMode::ExitZeroStaleNotRecreated => {
                        Ok(ok_output(b"pretended ok"))
                    }
                    FailureMode::OutputIsSymlink => {
                        h.seed_file(Path::new("/work/dummy_target"), b"t")?;
                        h.symlink(Path::new("/work/dummy_target"), &expected_out_clone)?;
                        Ok(ok_output(b"symlinked"))
                    }
                    FailureMode::OutputIsDirectory => {
                        h.create_dir_all(&expected_out_clone)?;
                        Ok(ok_output(b"dir"))
                    }
                    FailureMode::TimedOut => Ok(timeout_output()),
                    FailureMode::NonZeroExit => Ok(fail_output(42, b"tool failed\n")),
                }
            })
            .unwrap();

            let code = run_unrestricted(host.clone(), &[]);
            assert_eq!(
                code, 1,
                "gen={gen_name} mode={mode:?} unexpectedly succeeded!"
            );
            assert!(
                host.metadata(
                    &PathBuf::from(format!("/work/FailTest-1.0{ext}")),
                    Follow::No
                )
                .is_err(),
                "gen={gen_name} mode={mode:?} must never publish on failure"
            );
        }
    }
}

// ===========================================================================
// 9. Unit Checks for Shared Escaping, Bounded XML, and PathToId Helpers
// ===========================================================================

#[test]
fn shared_escaping_and_xml_security_unit_checks() {
    assert_eq!(
        escape_nsis_literal("a$b\"c\nd", true).unwrap(),
        r#"a$$b$\"c$\r$\nd"#
    );
    assert!(escape_nsis_literal("line1\nline2", false).is_err());
    assert_eq!(
        escape_wix_preprocessor("cost $5 and $(var)"),
        "cost $$5 and $$(var)"
    );
    assert_eq!(escape_js_single_quoted("a'b\\c").unwrap(), "a\\'b\\\\c");
    assert!(parse_xml_bounded(b"<!DOCTYPE a><a/>", "test").is_err());
    assert!(parse_xml_bounded(b"<a>&custom;</a>", "test").is_err());
    let parsed = parse_xml_bounded(
        b"<?xml version=\"1.0\"?><a k=\"&amp;&#10;\">hi&lt;</a>",
        "t",
    )
    .unwrap();
    assert_eq!(xml_attr(&parsed, "k"), Some("&\n"));
    assert_eq!(xml_text(&parsed), "hi<");

    // GUID validation & normalization.
    assert_eq!(
        crate::generators::wix::validate_guid("{abcdef01-2345-4678-9abc-def012345678}").unwrap(),
        "ABCDEF01-2345-4678-9ABC-DEF012345678"
    );
    assert!(crate::generators::wix::validate_guid("not-a-guid").is_err());

    // PathToId hashing, collision suffixes, and non-ASCII paths.
    let mut alloc = crate::generators::wix::render::PathIdAllocator::default();
    assert_eq!(alloc.id_for_path("bin/app.exe").unwrap(), "P_bin.app.exe");
    // Ambiguity collision produces _2 suffix (bin/a-b vs bin/a+b both normalize to bin.a_b).
    assert_eq!(alloc.id_for_path("bin/a-b").unwrap(), "P_bin.a_b");
    assert_eq!(alloc.id_for_path("bin/a+b").unwrap(), "P_bin.a_b_2");
    // >33% non-ASCII / special characters switches to H_<sha1>_<leaf>.
    let non_ascii = alloc.id_for_path("café/日本語.txt").unwrap();
    assert!(non_ascii.starts_with("H_"), "got {non_ascii}");
    // >60 chars switches to H_<sha1>_...
    let long_path = "very/deeply/nested/directory/structure/that/exceeds/sixty/characters/file.txt";
    let long_id = alloc.id_for_path(long_path).unwrap();
    assert!(long_id.starts_with("H_"), "got {long_id}");
}

// ===========================================================================
// 10. Optional macOS Live Oracle Parity Check for productbuild
// ===========================================================================

#[cfg(target_os = "macos")]
#[test]
fn productbuild_matches_live_macos_oracle_distribution_xml() {
    let host = Arc::new(rcmake_host::NativeHost::new().unwrap());
    let Some(reference_cpack) = host.env_get(b"RCMAKE_TEST_REFERENCE_CPACK") else {
        return;
    };
    let oracle_cpack = to_native(&reference_cpack);
    if host.metadata(&oracle_cpack, Follow::Yes).is_err() {
        return;
    }

    let base = std::env::temp_dir().join(format!(
        "rcmake-cpack-productbuild-parity-{}",
        host.system().pid
    ));
    let _ = host.remove_dir_all(&base);
    struct DirGuard {
        host: Arc<rcmake_host::NativeHost>,
        path: PathBuf,
    }
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = self.host.remove_dir_all(&self.path);
        }
    }
    let _guard = DirGuard {
        host: Arc::clone(&host),
        path: base.clone(),
    };

    let mock_bin = base.join("mock_bin");
    host.create_dir_all(&mock_bin).unwrap();
    let mock_script =
        b"#!/bin/sh\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf 'mockpkg' > \"$out\"\n";
    let pkgbuild_mock = mock_bin.join("pkgbuild");
    let productbuild_mock = mock_bin.join("productbuild");
    host.write(&pkgbuild_mock, mock_script, WriteMode::CreateNew)
        .unwrap();
    host.write(&productbuild_mock, mock_script, WriteMode::CreateNew)
        .unwrap();
    host.set_permissions(&pkgbuild_mock, 0o755).unwrap();
    host.set_permissions(&productbuild_mock, 0o755).unwrap();

    let src_dir = base.join("src");
    host.create_dir_all(&src_dir).unwrap();
    host.write(&src_dir.join("hello.txt"), b"hello", WriteMode::CreateNew)
        .unwrap();

    let cmake_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(|p| p.join("modules/cmake-4.5"))
        .unwrap_or_default();
    if host.metadata(&cmake_root, Follow::Yes).is_err() {
        return;
    }

    let run_dir = |name: &str| {
        let dir = base.join(name);
        host.create_dir_all(&dir).unwrap();
        let cfg = format!(
            r#"set(CMAKE_ROOT "{cmake}")
set(CPACK_GENERATOR "productbuild")
set(CPACK_PACKAGE_NAME "ParityApp")
set(CPACK_PACKAGE_VENDOR "ParityCo")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_DESCRIPTION "Parity description")
set(CPACK_PACKAGE_FILE_NAME "ParityApp-1.0.0")
set(CPACK_RESOURCE_FILE_LICENSE "{cmake}/Templates/CPack.GenericLicense.txt")
set(CPACK_RESOURCE_FILE_README "{cmake}/Templates/CPack.GenericDescription.txt")
set(CPACK_RESOURCE_FILE_WELCOME "{cmake}/Templates/CPack.GenericWelcome.txt")
set(CPACK_INSTALLED_DIRECTORIES "{src};/")
set(CPACK_COMMAND_PKGBUILD "{pkg}")
set(CPACK_COMMAND_PRODUCTBUILD "{prod}")
"#,
            cmake = from_native_cmake(&cmake_root),
            src = from_native_cmake(&src_dir),
            pkg = from_native_cmake(&pkgbuild_mock),
            prod = from_native_cmake(&productbuild_mock),
        );
        host.write(
            &dir.join("CPackConfig.cmake"),
            cfg.as_bytes(),
            WriteMode::CreateNew,
        )
        .unwrap();
        dir
    };

    let oracle_dir = run_dir("oracle");
    let native_dir = run_dir("native");

    let oracle_res = host
        .run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: from_native(&oracle_cpack),
                    args: vec![
                        "--config".into(),
                        from_native(&oracle_dir.join("CPackConfig.cmake")),
                        "-B".into(),
                        from_native(&oracle_dir),
                    ],
                }],
                cwd: oracle_dir.clone(),
                env: host.env_all(),
                stdin: rcmake_host::Input::Null,
                stdout: rcmake_host::Output::Capture,
                stderr: rcmake_host::Output::SharedWithStdout,
                timeout: Some(std::time::Duration::from_secs(30)),
                tee: false,
            },
            &mut |_, _| {},
        )
        .unwrap();
    assert_eq!(
        oracle_res.results,
        vec![ProcessResult::Exited(0)],
        "oracle cpack failed: {}",
        String::from_utf8_lossy(&oracle_res.stdout)
    );

    let native_code = crate::main(
        &[
            "cpack".into(),
            "--config".into(),
            from_native(&native_dir.join("CPackConfig.cmake")),
            "-B".into(),
            from_native(&native_dir),
        ],
        host.clone(),
    );
    assert_eq!(native_code, 0);

    let oracle_xml_path = [
        oracle_dir
            .join("_CPack_Packages/Darwin/productbuild/ParityApp-1.0.0/Contents/distribution.dist"),
        oracle_dir.join("_CPack_Packages/productbuild/ParityApp-1.0.0/Contents/distribution.dist"),
    ]
    .into_iter()
    .find(|p| host.metadata(p, Follow::No).is_ok())
    .unwrap();
    let native_xml_path =
        native_dir.join("_CPack_Packages/productbuild/ParityApp-1.0.0/Contents/distribution.dist");

    let o_xml = parse_xml_bounded(&read_bytes(host.as_ref(), &oracle_xml_path), "oracle").unwrap();
    let n_xml = parse_xml_bounded(&read_bytes(host.as_ref(), &native_xml_path), "native").unwrap();
    assert_eq!(o_xml.name, n_xml.name);
    let o_opts: BTreeMap<_, _> = xml_find_all(&o_xml, "options")[0]
        .attributes
        .iter()
        .cloned()
        .collect();
    let n_opts: BTreeMap<_, _> = xml_find_all(&n_xml, "options")[0]
        .attributes
        .iter()
        .cloned()
        .collect();
    assert_eq!(o_opts, n_opts);
    assert_eq!(
        xml_text(xml_find_all(&o_xml, "title")[0]),
        xml_text(xml_find_all(&n_xml, "title")[0])
    );
    assert!(
        host.metadata(&native_dir.join("ParityApp-1.0.0.pkg"), Follow::No)
            .is_ok()
    );
}

// ===========================================================================
// 11. Review Regression Tests (P1 #2-#7 and CPack P2s)
// ===========================================================================

#[test]
fn publish_rolls_back_installer_and_checksums_on_rename_fault() {
    for had_previous in [true, false] {
        let inner = make_profile_host(OsProfile::Linux);
        seed_tool(&inner, OsProfile::Linux, "binarycreator");
        inner
            .seed_file(Path::new("/work/src/a.txt"), b"v2")
            .unwrap();
        let prev_installer = b"PREVIOUS_GOOD_INSTALLER_V1";
        let prev_md5 = format!(
            "{}  App-1.0.run\n",
            HashAlgo::Md5.hex_digest(prev_installer)
        );
        let prev_sha = format!(
            "{}  App-1.0.run\n",
            HashAlgo::Sha256.hex_digest(prev_installer)
        );
        if had_previous {
            inner
                .seed_file(Path::new("/work/App-1.0.run"), prev_installer)
                .unwrap();
            inner
                .seed_file(Path::new("/work/App-1.0.run.md5"), prev_md5.as_bytes())
                .unwrap();
            inner
                .seed_file(Path::new("/work/App-1.0.run.sha256"), prev_sha.as_bytes())
                .unwrap();
        }
        inner
            .seed_file(
                Path::new("/work/CPackConfig.cmake"),
                br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_PACKAGE_NAME "App")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "App-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_PACKAGE_CHECKSUM "MD5;SHA256")
"#,
            )
            .unwrap();
        inner
            .set_command_handler(|h, req, _| {
                let out = to_native(req.pipeline[0].args.last().unwrap());
                h.write(&out, b"NEW_INSTALLER_V2", WriteMode::CreateNew)?;
                Ok(ok_output(b"ok"))
            })
            .unwrap();

        let wrapper = Arc::new(FaultInjectHost {
            inner: Arc::clone(&inner),
            fail_write_from_tmp: AtomicBool::new(false),
            fail_write_checksum_tmp: AtomicBool::new(false),
            partial_write_before_fail: AtomicBool::new(false),
            fail_rename_dest_suffix: Mutex::new(Some(".sha256".to_owned())),
            fail_rename_from_prefix: Mutex::new(None),
        });

        assert_eq!(run_unrestricted(wrapper, &[]), 1);

        let stdout = String::from_utf8_lossy(&inner.stream_bytes(Stream::Stdout)).into_owned();
        assert!(
            !stdout.contains("- package: /work/App-1.0.run generated."),
            "must not emit package success log on failed checksum rename: {stdout}"
        );
        if had_previous {
            assert_eq!(
                read_bytes(inner.as_ref(), Path::new("/work/App-1.0.run")),
                prev_installer
            );
            assert_eq!(
                read_utf8(inner.as_ref(), Path::new("/work/App-1.0.run.md5")),
                prev_md5
            );
            assert_eq!(
                read_utf8(inner.as_ref(), Path::new("/work/App-1.0.run.sha256")),
                prev_sha
            );
        } else {
            assert!(
                inner
                    .metadata(Path::new("/work/App-1.0.run"), Follow::No)
                    .is_err()
            );
            assert!(
                inner
                    .metadata(Path::new("/work/App-1.0.run.md5"), Follow::No)
                    .is_err()
            );
            assert!(
                inner
                    .metadata(Path::new("/work/App-1.0.run.sha256"), Follow::No)
                    .is_err()
            );
        }
        for entry in inner.read_dir(Path::new("/work")).unwrap() {
            assert!(
                !entry.name.starts_with(b".rcmake-pub-"),
                "stray publication temp entry left behind: {}",
                entry.name
            );
        }
    }
}

#[test]
fn failed_rollback_keeps_the_backup_that_it_could_not_restore() {
    let inner = make_profile_host(OsProfile::Linux);
    seed_tool(&inner, OsProfile::Linux, "binarycreator");
    inner
        .seed_file(Path::new("/work/src/a.txt"), b"v2")
        .unwrap();
    let prev_installer = b"PREVIOUS_GOOD_INSTALLER_V1";
    let prev_sha = format!(
        "{}  App-1.0.run\n",
        HashAlgo::Sha256.hex_digest(prev_installer)
    );
    inner
        .seed_file(Path::new("/work/App-1.0.run"), prev_installer)
        .unwrap();
    inner
        .seed_file(Path::new("/work/App-1.0.run.sha256"), prev_sha.as_bytes())
        .unwrap();
    inner
        .seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_PACKAGE_NAME "App")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "App-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_PACKAGE_CHECKSUM "SHA256")
"#,
        )
        .unwrap();
    inner
        .set_command_handler(|h, req, _| {
            let out = to_native(req.pipeline[0].args.last().unwrap());
            h.write(&out, b"NEW_INSTALLER_V2", WriteMode::CreateNew)?;
            Ok(ok_output(b"ok"))
        })
        .unwrap();
    let wrapper = Arc::new(FaultInjectHost {
        inner: Arc::clone(&inner),
        fail_write_from_tmp: AtomicBool::new(false),
        fail_write_checksum_tmp: AtomicBool::new(false),
        partial_write_before_fail: AtomicBool::new(false),
        fail_rename_dest_suffix: Mutex::new(Some(".sha256".to_owned())),
        fail_rename_from_prefix: Mutex::new(Some("backup-".to_owned())),
    });

    assert_eq!(run_unrestricted(wrapper, &[]), 1);

    let stderr = String::from_utf8_lossy(&inner.stream_bytes(Stream::Stderr)).into_owned();
    let kept = stderr
        .split("(previous file kept at ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_else(|| panic!("no kept backup path in: {stderr}"));
    assert!(
        stderr
            .replace('\\', "/")
            .contains("rollback failed for: /work/App-1.0.run: "),
        "{stderr}"
    );
    assert_eq!(read_bytes(inner.as_ref(), Path::new(kept)), prev_installer);
    let stage = Path::new(kept).parent().unwrap();
    let names: Vec<_> = inner
        .read_dir(stage)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, [BString::from("backup-0")]);
    assert_eq!(
        read_utf8(inner.as_ref(), Path::new("/work/App-1.0.run.sha256")),
        prev_sha
    );
}

#[test]
fn post_hook_ancestor_symlink_swap_rejected_for_installers_and_archives() {
    for (gen_name, extra_cfg, tools) in [
        ("productbuild", "", &["pkgbuild", "productbuild"][..]),
        ("NSIS", "", &["makensis"][..]),
        ("WIX", "set(CPACK_WIX_VERSION \"4\")\n", &["wix"][..]),
        ("IFW", "", &["binarycreator"][..]),
        ("TGZ", "", &[][..]),
    ] {
        let host = make_profile_host(OsProfile::Linux);
        for tool in tools {
            seed_tool(&host, OsProfile::Linux, tool);
        }
        host.seed_file(Path::new("/work/src/real.txt"), b"real")
            .unwrap();
        // Prepare an outside tree that has a regular directory at the staging leaf name.
        host.seed_file(
            Path::new("/work/evil_work/Swap-1.0/secret.txt"),
            b"OUTSIDE_SECRET",
        )
        .unwrap();
        // Pre-build hook swaps the ancestor `_CPack_Packages/<gen>` with a symlink to `/work/evil_work`.
        host.seed_file(
            Path::new("/work/swap_ancestor.cmake"),
            br#"
file(RENAME "${CPACK_TOPLEVEL_DIRECTORY}" "${CPACK_TOPLEVEL_DIRECTORY}_orig")
file(CREATE_LINK "/work/evil_work" "${CPACK_TOPLEVEL_DIRECTORY}" SYMBOLIC)
"#,
        )
        .unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            format!(
                r#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "{gen_name}")
set(CPACK_PACKAGE_NAME "Swap")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "Swap-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_PRE_BUILD_SCRIPTS "/work/swap_ancestor.cmake")
{extra_cfg}
"#
            )
            .as_bytes(),
        )
        .unwrap();

        let build_calls = Arc::new(AtomicUsize::new(0));
        let build_calls_ref = Arc::clone(&build_calls);
        host.set_command_handler(move |_, req, _| {
            let spec = &req.pipeline[0];
            if spec.args == [BString::from("-VERSION")] || spec.args == [BString::from("/VERSION")]
            {
                return Ok(ok_output(b"v3.08\n"));
            }
            build_calls_ref.fetch_add(1, Ordering::SeqCst);
            Ok(ok_output(b""))
        })
        .unwrap();

        assert_eq!(
            run_unrestricted(host.clone(), &[]),
            1,
            "ancestor symlink swap must fail for {gen_name}"
        );
        assert_eq!(
            build_calls.load(Ordering::SeqCst),
            0,
            "external build tool must not run after ancestor swap for {gen_name}"
        );
    }

    // Post-build hook replacing the work directory with a symlink must also be rejected.
    let host_post = make_profile_host(OsProfile::Linux);
    host_post
        .seed_file(Path::new("/work/src/real.txt"), b"real")
        .unwrap();
    host_post
        .seed_file(
            Path::new("/work/evil_post/PostSwap-1.0.tar.gz"),
            b"EVIL_ARCHIVE",
        )
        .unwrap();
    host_post
        .seed_file(
            Path::new("/work/post_swap.cmake"),
            br#"
file(RENAME "${CPACK_TOPLEVEL_DIRECTORY}" "${CPACK_TOPLEVEL_DIRECTORY}_orig")
file(CREATE_LINK "/work/evil_post" "${CPACK_TOPLEVEL_DIRECTORY}" SYMBOLIC)
"#,
        )
        .unwrap();
    host_post
        .seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "TGZ")
set(CPACK_PACKAGE_NAME "PostSwap")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "PostSwap-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_POST_BUILD_SCRIPTS "/work/post_swap.cmake")
"#,
        )
        .unwrap();
    assert_eq!(run_unrestricted(host_post.clone(), &[]), 1);
    assert!(
        host_post
            .metadata(Path::new("/work/PostSwap-1.0.tar.gz"), Follow::No)
            .is_err()
    );
}

#[test]
fn symlink_validator_resolves_links_before_parent_traversal() {
    let host = make_profile_host(OsProfile::Linux);
    let root = Path::new("/work/staging");
    host.create_dir_all(&root.join("dir/sub/nested")).unwrap();
    host.seed_file(Path::new("/work/secret.txt"), b"SECRET")
        .unwrap();
    host.seed_file(&root.join("outside"), b"harmless-in-root")
        .unwrap();
    host.seed_file(&root.join("dir/inside.txt"), b"ok-in-root")
        .unwrap();
    let canon = host.canonicalize_fresh(root).unwrap();

    // Case 1: dir/link -> sub/nested followed by dir/link/../../outside where dir/outside escapes.
    // Lexical collapse turns dir/link/../../outside into `outside` (harmless file inside root),
    // while true component-by-component resolution resolves to `dir/outside` -> escape!
    host.symlink(Path::new("sub/nested"), &root.join("dir/link"))
        .unwrap();
    host.symlink(Path::new("../../secret.txt"), &root.join("dir/outside"))
        .unwrap();
    host.symlink(Path::new("dir/link/../../outside"), &root.join("escape1"))
        .unwrap();
    let err1 = crate::source::validate_contained_symlink(
        host.as_ref(),
        root,
        &canon,
        &root.join("escape1"),
    )
    .unwrap_err();
    assert!(
        err1.to_string().contains("escapes staging root"),
        "unexpected error: {err1}"
    );

    // Case 2: dir/up -> .. followed by dir/up/../outside escapes immediately after resolving dir/up.
    host.symlink(Path::new(".."), &root.join("dir/up")).unwrap();
    host.symlink(Path::new("dir/up/../outside"), &root.join("escape2"))
        .unwrap();
    let err2 = crate::source::validate_contained_symlink(
        host.as_ref(),
        root,
        &canon,
        &root.join("escape2"),
    )
    .unwrap_err();
    assert!(
        err2.to_string().contains("escapes staging root"),
        "unexpected error: {err2}"
    );

    // Case 3: Chained symlink a -> b/c, b -> ../outside escapes.
    host.symlink(Path::new("../secret.txt"), &root.join("b"))
        .unwrap();
    host.symlink(Path::new("b/c"), &root.join("a")).unwrap();
    let err3 =
        crate::source::validate_contained_symlink(host.as_ref(), root, &canon, &root.join("a"))
            .unwrap_err();
    assert!(
        err3.to_string().contains("escapes staging root"),
        "unexpected error: {err3}"
    );

    // Case 4: Valid contained link through dir/link/../../inside.txt succeeds.
    host.symlink(Path::new("dir/link/../../inside.txt"), &root.join("valid"))
        .unwrap();
    assert!(
        crate::source::validate_contained_symlink(host.as_ref(), root, &canon, &root.join("valid"))
            .is_ok()
    );
}

#[test]
fn ifw_all_in_one_merges_components_without_component_prefix() {
    let host = make_profile_host(OsProfile::Linux);
    seed_tool(&host, OsProfile::Linux, "binarycreator");
    host.seed_file(Path::new("/work/payload/app"), b"app-bin")
        .unwrap();
    host.seed_file(Path::new("/work/payload/api.h"), b"header")
        .unwrap();
    seed_component_project(
        &host,
        r#"
if(CMAKE_INSTALL_COMPONENT STREQUAL "Runtime")
  file(INSTALL "/work/payload/app" DESTINATION "${CMAKE_INSTALL_PREFIX}/bin")
elseif(CMAKE_INSTALL_COMPONENT STREQUAL "Devel")
  file(INSTALL "/work/payload/api.h" DESTINATION "${CMAKE_INSTALL_PREFIX}/include")
endif()
"#,
    );
    host.seed_file(
        Path::new("/work/CPackConfig.cmake"),
        br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_PACKAGE_NAME "AllInOne")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "AllInOne-1.0.0")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;AllInOne;ALL;/")
set(CPACK_COMPONENTS_ALL "Runtime;Devel")
set(CPACK_COMPONENTS_GROUPING "ALL_COMPONENTS_IN_ONE")
"#,
    )
    .unwrap();
    host.set_command_handler(|h, req, _| {
        let out = to_native(req.pipeline[0].args.last().unwrap());
        h.write(&out, b"ifw-all-in-one", WriteMode::CreateNew)?;
        Ok(ok_output(b""))
    })
    .unwrap();

    assert_eq!(
        run_unrestricted(host.clone(), &[]),
        0,
        "stderr={}",
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
    );
    let data_dir = Path::new("/work/_CPack_Packages/IFW/AllInOne-1.0.0/packages/AllInOne/data");
    assert_eq!(
        read_bytes(host.as_ref(), &data_dir.join("bin/app")),
        b"app-bin"
    );
    assert_eq!(
        read_bytes(host.as_ref(), &data_dir.join("include/api.h")),
        b"header"
    );
    assert!(
        host.metadata(&data_dir.join("Runtime"), Follow::No)
            .is_err(),
        "component staging dir prefix Runtime/ must not appear in all-in-one IFW data/"
    );
    assert!(
        host.metadata(&data_dir.join("Devel"), Follow::No).is_err(),
        "component staging dir prefix Devel/ must not appear in all-in-one IFW data/"
    );
}

#[test]
fn productbuild_component_includes_hook_added_and_replaced_files() {
    let host = make_profile_host(OsProfile::MacOs);
    seed_tool(&host, OsProfile::MacOs, "pkgbuild");
    seed_tool(&host, OsProfile::MacOs, "productbuild");
    host.seed_file(Path::new("/work/payload/orig.txt"), b"original-v1")
        .unwrap();
    seed_component_project(
        &host,
        r#"
if(CMAKE_INSTALL_COMPONENT STREQUAL "app")
  file(INSTALL "/work/payload/orig.txt" DESTINATION "${CMAKE_INSTALL_PREFIX}/bin")
endif()
"#,
    );
    host.seed_file(
        Path::new("/work/pre_hook.cmake"),
        br#"
file(WRITE "${CPACK_TEMPORARY_DIRECTORY}/app/Applications/bin/orig.txt" "replaced-v2")
file(WRITE "${CPACK_TEMPORARY_DIRECTORY}/app/Applications/bin/hook_added.txt" "added-by-hook")
file(WRITE "${CPACK_TEMPORARY_DIRECTORY}/emptycomp/Applications/share/from_hook.txt" "hook-only-comp")
"#,
    )
    .unwrap();
    host.seed_file(
        Path::new("/work/CPackConfig.cmake"),
        br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "productbuild")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_PACKAGE_NAME "HookPkg")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "HookPkg-1.0.0")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;HookPkg;ALL;/")
set(CPACK_COMPONENTS_ALL "app;emptycomp")
set(CPACK_PRE_BUILD_SCRIPTS "/work/pre_hook.cmake")
"#,
    )
    .unwrap();

    let saw_app = Arc::new(AtomicBool::new(false));
    let saw_empty = Arc::new(AtomicBool::new(false));
    let saw_app_ref = Arc::clone(&saw_app);
    let saw_empty_ref = Arc::clone(&saw_empty);
    host.set_command_handler(move |h, req, _| {
        let spec = &req.pipeline[0];
        let prog = String::from_utf8_lossy(&spec.program).into_owned();
        if prog.contains("pkgbuild") {
            let id = String::from_utf8_lossy(find_arg_after(&spec.args, b"--identifier").unwrap())
                .into_owned();
            let root = to_native(find_arg_after(&spec.args, b"--root").unwrap());
            if id.ends_with(".app") {
                assert_eq!(
                    read_bytes(h, &root.join("Applications/bin/orig.txt")),
                    b"replaced-v2"
                );
                assert_eq!(
                    read_bytes(h, &root.join("Applications/bin/hook_added.txt")),
                    b"added-by-hook"
                );
                saw_app_ref.store(true, Ordering::SeqCst);
            } else if id.ends_with(".emptycomp") {
                assert_eq!(
                    read_bytes(h, &root.join("Applications/share/from_hook.txt")),
                    b"hook-only-comp"
                );
                saw_empty_ref.store(true, Ordering::SeqCst);
            }
            let out = to_native(spec.args.last().unwrap());
            h.write(&out, b"leaf-pkg", WriteMode::CreateNew)?;
            Ok(ok_output(b""))
        } else {
            let out = to_native(spec.args.last().unwrap());
            h.write(&out, b"final-pkg", WriteMode::CreateNew)?;
            Ok(ok_output(b""))
        }
    })
    .unwrap();

    assert_eq!(
        run_unrestricted(host.clone(), &[]),
        0,
        "stderr={}",
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
    );
    assert!(saw_app.load(Ordering::SeqCst));
    assert!(saw_empty.load(Ordering::SeqCst));
}

#[test]
fn deep_acyclic_dependency_chain_and_group_depth_limit() {
    // 1) 5,000-component acyclic dependency chain must not overflow the stack across catalog,
    // productbuild dependency expression expansion, NSIS dependency macro expansion, or IFW.
    const CHAIN_LEN: usize = 5_000;
    let mut vars = BTreeMap::new();
    let mut names = Vec::with_capacity(CHAIN_LEN);
    for i in 0..CHAIN_LEN {
        let name = format!("c{i}");
        names.push(name);
    }
    vars.insert(
        BString::from("CPACK_INSTALL_CMAKE_PROJECTS"),
        BString::from("/work/build;Deep;ALL;/"),
    );
    vars.insert(BString::from("CPACK_GENERATOR"), BString::from("NSIS"));
    vars.insert(
        BString::from("CPACK_COMPONENTS_ALL"),
        BString::from(names.join(";")),
    );
    for i in 0..CHAIN_LEN - 1 {
        vars.insert(
            BString::from(format!("CPACK_COMPONENT_C{i}_DEPENDS")),
            BString::from(format!("c{}", i + 1)),
        );
    }
    let cfg = crate::config::CpackConfig { vars };
    let catalog = super::ComponentCatalog::from_config(&cfg, &[]).unwrap();
    assert_eq!(catalog.components.len(), CHAIN_LEN);

    let first = catalog.components.get(&BString::from("c0")).unwrap();
    let mut visited = std::collections::BTreeSet::new();
    let mut js_expr = String::new();
    crate::generators::productbuild::append_dep_expr(&catalog, first, &mut visited, &mut js_expr)
        .unwrap();
    assert!(js_expr.contains("c4999Choice"));

    visited.clear();
    let mut nsis_deps = String::new();
    crate::generators::nsis::append_select_deps(&catalog, first, &mut visited, &mut nsis_deps)
        .unwrap();
    assert!(nsis_deps.contains("${c4999}"));

    crate::generators::ifw::check_deep_package_chain(CHAIN_LEN, false).unwrap();
    let cyc_err = crate::generators::ifw::check_deep_package_chain(CHAIN_LEN, true).unwrap_err();
    assert!(cyc_err.to_string().contains("Cycle detected"));

    // 2) 5,000-deep acyclic group hierarchy must fail cleanly with a depth-limit configuration error.
    let mut gvars = BTreeMap::new();
    gvars.insert(
        BString::from("CPACK_INSTALL_CMAKE_PROJECTS"),
        BString::from("/work/build;DeepG;ALL;/"),
    );
    gvars.insert(BString::from("CPACK_GENERATOR"), BString::from("WIX"));
    gvars.insert(BString::from("CPACK_COMPONENTS_ALL"), BString::from("leaf"));
    gvars.insert(
        BString::from("CPACK_COMPONENT_LEAF_GROUP"),
        BString::from("g0"),
    );
    for i in 0..CHAIN_LEN - 1 {
        gvars.insert(
            BString::from(format!("CPACK_COMPONENT_GROUP_G{i}_PARENT_GROUP")),
            BString::from(format!("g{}", i + 1)),
        );
    }
    let gcfg = crate::config::CpackConfig { vars: gvars };
    let gerr = super::ComponentCatalog::from_config(&gcfg, &[]).unwrap_err();
    assert!(
        gerr.to_string().contains("exceeds maximum depth"),
        "unexpected group depth error: {gerr}"
    );
}

#[test]
fn nsis_registry_order_and_decimal_version_parity() {
    // Decimal version rule: 3.1 >= 3.03 is accepted; 3.029 and 3.02 are rejected.
    assert!(crate::generators::nsis::validate_nsis_version("v3.1\n").is_ok());
    assert!(crate::generators::nsis::validate_nsis_version("v3.03\n").is_ok());
    assert!(crate::generators::nsis::validate_nsis_version("v3.031\n").is_ok());
    assert!(crate::generators::nsis::validate_nsis_version("v3.029\n").is_err());
    assert!(crate::generators::nsis::validate_nsis_version("v3.02\n").is_err());
    assert!(crate::generators::nsis::validate_nsis_version("v3.0\n").is_err());

    // NSIS checks native-view Unicode key BEFORE 32-bit standard key.
    let host = make_profile_host(OsProfile::Windows);
    host.env_set(b"PATH", Some(b"/empty"));
    host.seed_file(Path::new("/nsis32-std/makensis.exe"), b"std")
        .unwrap();
    host.seed_file(Path::new("/nsis-native-uni/makensis.exe"), b"uni")
        .unwrap();
    host.seed_registry_value(
        RegistryRoot::LocalMachine,
        b"SOFTWARE\\NSIS",
        None,
        RegistryView::Bits32,
        b"/nsis32-std",
    )
    .unwrap();
    host.seed_registry_value(
        RegistryRoot::LocalMachine,
        b"SOFTWARE\\NSIS\\Unicode",
        None,
        RegistryView::Bits64,
        b"/nsis-native-uni",
    )
    .unwrap();
    host.seed_file(Path::new("/work/src/a.txt"), b"a").unwrap();
    host.seed_file(
        Path::new("/work/CPackConfig.cmake"),
        br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "NSIS")
set(CPACK_PACKAGE_NAME "RegOrder")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "RegOrder-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
"#,
    )
    .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_ref = Arc::clone(&calls);
    host.set_command_handler(move |h, req, _| {
        let spec = record_cmd(&calls_ref, req);
        if spec.args == [BString::from("/VERSION")] {
            return Ok(ok_output(b"v3.1\n"));
        }
        h.write(
            &req.cwd.join("RegOrder-1.0.exe"),
            b"exe",
            WriteMode::CreateNew,
        )?;
        Ok(ok_output(b"ok"))
    })
    .unwrap();

    assert_eq!(
        run_unrestricted(host.clone(), &[]),
        0,
        "stderr={}",
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
    );
    let recorded = calls.lock().unwrap();
    assert_eq!(
        rcmake_base::path_lex::cmake_style(&recorded[0].program),
        BString::from("/nsis-native-uni/makensis.exe")
    );

    // NSIS64 checks 64-bit keys first before 32-bit Unicode key.
    let host64 = make_profile_host(OsProfile::Windows);
    host64.env_set(b"PATH", Some(b"/empty"));
    host64
        .seed_file(Path::new("/nsis64-std/makensis.exe"), b"64std")
        .unwrap();
    host64
        .seed_file(Path::new("/nsis32-uni/makensis.exe"), b"32uni")
        .unwrap();
    host64
        .seed_registry_value(
            RegistryRoot::LocalMachine,
            b"SOFTWARE\\NSIS",
            None,
            RegistryView::Bits64,
            b"/nsis64-std",
        )
        .unwrap();
    host64
        .seed_registry_value(
            RegistryRoot::LocalMachine,
            b"SOFTWARE\\NSIS\\Unicode",
            None,
            RegistryView::Bits32,
            b"/nsis32-uni",
        )
        .unwrap();
    host64
        .seed_file(Path::new("/work/src/a.txt"), b"a")
        .unwrap();
    host64
        .seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "NSIS64")
set(CPACK_PACKAGE_NAME "RegOrder64")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "RegOrder64-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
"#,
        )
        .unwrap();
    let calls64 = Arc::new(Mutex::new(Vec::new()));
    let calls64_ref = Arc::clone(&calls64);
    host64
        .set_command_handler(move |h, req, _| {
            let spec = record_cmd(&calls64_ref, req);
            if spec.args == [BString::from("/VERSION")] {
                return Ok(ok_output(b"v3.1\n"));
            }
            h.write(
                &req.cwd.join("RegOrder64-1.0.exe"),
                b"exe64",
                WriteMode::CreateNew,
            )?;
            Ok(ok_output(b"ok"))
        })
        .unwrap();
    assert_eq!(run_unrestricted(host64.clone(), &[]), 0);
    assert_eq!(
        rcmake_base::path_lex::cmake_style(&calls64.lock().unwrap()[0].program),
        BString::from("/nsis64-std/makensis.exe")
    );
}

#[test]
fn wix_legacy_license_bytes_and_xml_whitespace_patch() {
    let host = make_profile_host(OsProfile::Windows);
    seed_tool(&host, OsProfile::Windows, "wix");
    // Legacy non-UTF-8 license bytes (0xA9 Latin-1 copyright, 0xFF invalid lead byte).
    host.seed_file(
        Path::new("/work/legacy_license.txt"),
        b"Copyright \xA9 2026 \xFF\n",
    )
    .unwrap();
    host.seed_file(Path::new("/work/src/app.exe"), b"MZ")
        .unwrap();
    // Patch text with NBSP (\u{00A0}) surrounded by XML whitespace.
    let patch_xml = "<CPackWiXPatch>\n  <CPackWiXFragment Id=\"#PRODUCT\">\n    <CustomAction Id=\"CA1\">  \u{00A0}KeepNBSP\u{00A0}\r\n\t</CustomAction>\n  </CPackWiXFragment>\n</CPackWiXPatch>\n";
    host.seed_file(Path::new("/work/patch.xml"), patch_xml.as_bytes())
        .unwrap();
    host.seed_file(
        Path::new("/work/CPackConfig.cmake"),
        br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "WIX")
set(CPACK_WIX_VERSION "4")
set(CPACK_WIX_TEST_FIXED_GUIDS "11111111-1111-4111-8111-111111111111;22222222-2222-4222-8222-222222222222")
set(CPACK_PACKAGE_NAME "LegacyWix")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "LegacyWix-1.0.0")
set(CPACK_RESOURCE_FILE_LICENSE "/work/legacy_license.txt")
set(CPACK_WIX_PATCH_FILE "/work/patch.xml")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
"#,
    )
    .unwrap();
    host.set_command_handler(|h, req, _| {
        let out = to_native(find_arg_after(&req.pipeline[0].args, b"-out").unwrap());
        h.write(&out, b"msi", WriteMode::CreateNew)?;
        Ok(ok_output(b"ok"))
    })
    .unwrap();

    assert_eq!(
        run_unrestricted(host.clone(), &[]),
        0,
        "stderr={}",
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
    );
    let rtf_bytes = read_bytes(
        host.as_ref(),
        Path::new("/work/_CPack_Packages/WIX/License.rtf"),
    );
    let rtf_str = String::from_utf8_lossy(&rtf_bytes);
    assert!(
        rtf_str.contains(
            r"Copyright \cf1 [INVALID-BYTE-169]\cf0  2026 \cf1 [INVALID-BYTE-255]\cf0 \par"
        ),
        "unexpected RTF: {rtf_str}"
    );
    let prod_wxi = read_utf8(
        host.as_ref(),
        Path::new("/work/_CPack_Packages/WIX/product_fragment.wxi"),
    );
    assert!(
        prod_wxi.contains(">\u{00A0}KeepNBSP\u{00A0}</CustomAction>"),
        "NBSP was unexpectedly stripped in patch text: {prod_wxi}"
    );
}

#[test]
fn ifw_updates_warning_qrc_isolation_and_extended_redaction() {
    // 1) Missing Updates.xml warns and continues; empty/missing QRC skipped; two QRCs with same
    // asset path are isolated in distinct directories without overwriting each other.
    {
        let host = make_profile_host(OsProfile::Linux);
        seed_tool(&host, OsProfile::Linux, "binarycreator");
        seed_tool(&host, OsProfile::Linux, "repogen");
        host.seed_file(Path::new("/work/src/a.txt"), b"a").unwrap();
        host.seed_file(Path::new("/work/empty.qrc"), b"<RCC><qresource/></RCC>")
            .unwrap();
        host.seed_file(
            Path::new("/work/q1/res.qrc"),
            b"<RCC><qresource><file>asset.txt</file></qresource></RCC>",
        )
        .unwrap();
        host.seed_file(Path::new("/work/q1/asset.txt"), b"FROM_Q1")
            .unwrap();
        host.seed_file(
            Path::new("/work/q2/res.qrc"),
            b"<RCC><qresource><file>asset.txt</file></qresource></RCC>",
        )
        .unwrap();
        host.seed_file(Path::new("/work/q2/asset.txt"), b"FROM_Q2")
            .unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_IFW_FRAMEWORK_VERSION "4.2.0")
set(CPACK_PACKAGE_NAME "IfwP2")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "IfwP2-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_IFW_PACKAGE_RESOURCES "/work/missing.qrc;/work/empty.qrc;/work/q1/res.qrc;/work/q2/res.qrc")
set(CPACK_IFW_REPOSITORIES_ALL "r1;radd")
set(CPACK_IFW_REPOSITORY_R1_URL "https://repo.example.test/main")
set(CPACK_IFW_REPOSITORY_RADD_ADD ON)
set(CPACK_IFW_REPOSITORY_RADD_URL "https://repo.example.test/extra")
"#,
        )
        .unwrap();
        let qrc_arg = Arc::new(Mutex::new(String::new()));
        let qrc_arg_ref = Arc::clone(&qrc_arg);
        host.set_command_handler(move |h, req, _| {
            let spec = &req.pipeline[0];
            let prog = String::from_utf8_lossy(&spec.program).into_owned();
            if prog.contains("repogen") {
                // Intentionally omit Updates.xml to verify warning-and-continue behavior.
                let repo_dir = to_native(spec.args.last().unwrap());
                h.create_dir_all(&repo_dir)?;
                Ok(ok_output(b"repogen ok"))
            } else {
                if let Some(r) = find_arg_after(&spec.args, b"-r") {
                    *qrc_arg_ref.lock().unwrap() = String::from_utf8_lossy(r).into_owned();
                }
                let out = to_native(spec.args.last().unwrap());
                h.write(&out, b"ifw-ok", WriteMode::CreateNew)?;
                Ok(ok_output(b"bc ok"))
            }
        })
        .unwrap();

        assert_eq!(
            run_unrestricted(host.clone(), &[]),
            0,
            "stderr={}",
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
        );
        let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
        let norm_stderr = stderr.replace('\\', "/");
        assert!(norm_stderr.contains("Problem patch IFW \"Updates\" file"));
        assert!(norm_stderr.contains("/work/missing.qrc"), "{stderr}");
        assert!(norm_stderr.contains("/work/empty.qrc"), "{stderr}");

        let qrc_list: Vec<String> = qrc_arg
            .lock()
            .unwrap()
            .split(',')
            .map(str::to_owned)
            .collect();
        assert_eq!(qrc_list.len(), 2, "expected 2 valid QRCs, got {qrc_list:?}");
        let p0 = PathBuf::from(&qrc_list[0])
            .parent()
            .unwrap()
            .join("asset.txt");
        let p1 = PathBuf::from(&qrc_list[1])
            .parent()
            .unwrap()
            .join("asset.txt");
        assert_ne!(p0, p1);
        assert_eq!(read_bytes(host.as_ref(), &p0), b"FROM_Q1");
        assert_eq!(read_bytes(host.as_ref(), &p1), b"FROM_Q2");
    }

    // 2) Updates.xml patching preserves nonblank whitespace in text nodes.
    {
        let host = make_profile_host(OsProfile::Linux);
        seed_tool(&host, OsProfile::Linux, "binarycreator");
        seed_tool(&host, OsProfile::Linux, "repogen");
        host.seed_file(Path::new("/work/src/a.txt"), b"a").unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_PACKAGE_NAME "IfwText")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "IfwText-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_IFW_REPOSITORIES_ALL "r1;radd")
set(CPACK_IFW_REPOSITORY_R1_URL "https://repo.example.test/main")
set(CPACK_IFW_REPOSITORY_RADD_ADD ON)
set(CPACK_IFW_REPOSITORY_RADD_URL "https://repo.example.test/extra")
"#,
        )
        .unwrap();
        host.set_command_handler(|h, req, _| {
            let spec = &req.pipeline[0];
            let prog = String::from_utf8_lossy(&spec.program).into_owned();
            if prog.contains("repogen") {
                let repo_dir = to_native(spec.args.last().unwrap());
                h.create_dir_all(&repo_dir)?;
                h.write(
                    &repo_dir.join("Updates.xml"),
                    b"<Updates><Checksum>true</Checksum><Description>  keep edges  </Description></Updates>",
                    WriteMode::CreateNew,
                )?;
                Ok(ok_output(b""))
            } else {
                let out = to_native(spec.args.last().unwrap());
                h.write(&out, b"ok", WriteMode::CreateNew)?;
                Ok(ok_output(b""))
            }
        })
        .unwrap();
        assert_eq!(run_unrestricted(host.clone(), &[]), 0);
        let patched = read_utf8(
            host.as_ref(),
            Path::new("/work/_CPack_Packages/IFW/IfwText-1.0/repository/Updates.xml"),
        );
        assert!(
            patched.contains("<Description>  keep edges  </Description>"),
            "nonblank text whitespace was stripped: {patched}"
        );
    }

    // 3) QRC referencing a symlink pointing outside the QRC base directory is rejected.
    {
        let host = make_profile_host(OsProfile::Linux);
        seed_tool(&host, OsProfile::Linux, "binarycreator");
        host.seed_file(Path::new("/work/src/a.txt"), b"a").unwrap();
        host.seed_file(Path::new("/work/outside/secret.txt"), b"SECRET")
            .unwrap();
        host.seed_file(
            Path::new("/work/qdir/link.qrc"),
            b"<RCC><qresource><file>escape.txt</file></qresource></RCC>",
        )
        .unwrap();
        host.symlink(
            Path::new("/work/outside/secret.txt"),
            Path::new("/work/qdir/escape.txt"),
        )
        .unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_PACKAGE_NAME "QrcLink")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "QrcLink-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_IFW_PACKAGE_RESOURCES "/work/qdir/link.qrc")
"#,
        )
        .unwrap();
        let invoked = Arc::new(AtomicUsize::new(0));
        let invoked_ref = Arc::clone(&invoked);
        host.set_command_handler(move |_, _, _| {
            invoked_ref.fetch_add(1, Ordering::SeqCst);
            Ok(ok_output(b""))
        })
        .unwrap();
        assert_eq!(run_unrestricted(host.clone(), &[]), 1);
        assert_eq!(invoked.load(Ordering::SeqCst), 0);
        assert!(
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
                .contains("escapes base directory")
        );
    }

    // 4) XML-escaped passwords and URL-embedded credentials are redacted in logs and stderr.
    {
        let host = make_profile_host(OsProfile::Linux);
        seed_tool(&host, OsProfile::Linux, "binarycreator");
        seed_tool(&host, OsProfile::Linux, "repogen");
        host.seed_file(Path::new("/work/src/a.txt"), b"a").unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_PACKAGE_NAME "RedactExt")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "RedactExt-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_IFW_REPOSITORIES_ALL "r1")
set(CPACK_IFW_REPOSITORY_R1_URL "https://bob:URL_SECRET_77&x@repo.example.test/main")
set(CPACK_IFW_REPOSITORY_R1_PASSWORD "p&w<\"'1")
"#,
        )
        .unwrap();
        host.set_command_handler(|_, _, _| {
            Ok(fail_output(
                1,
                b"xml=<Password>p&amp;w&lt;&quot;&apos;1</Password> url=https://bob:URL_SECRET_77&amp;x@repo.example.test/main raw=https://bob:URL_SECRET_77&x@repo.example.test/main\n",
            ))
        })
        .unwrap();
        assert_eq!(run_unrestricted(host.clone(), &[]), 1);
        let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
        let log_text = read_utf8(
            host.as_ref(),
            Path::new("/work/_CPack_Packages/IFW/IFWOutput.log"),
        );
        for leaked in ["p&w", "p&amp;w", "URL_SECRET_77", "bob:URL_SECRET_77"] {
            assert!(!stderr.contains(leaked), "stderr leaked {leaked}: {stderr}");
            assert!(
                !log_text.contains(leaked),
                "IFWOutput.log leaked {leaked}: {log_text}"
            );
        }
    }
}

fn seed_all_in_one(host: &PureHost, install_rules: &str) {
    seed_tool(host, OsProfile::Linux, "binarycreator");
    seed_component_project(host, install_rules);
    host.seed_file(
        Path::new("/work/CPackConfig.cmake"),
        br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_PACKAGE_NAME "AllInOne")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "AllInOne-1.0.0")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;AllInOne;ALL;/")
set(CPACK_COMPONENTS_ALL "Runtime;Devel")
set(CPACK_COMPONENTS_GROUPING "ALL_COMPONENTS_IN_ONE")
"#,
    )
    .unwrap();
    host.set_command_handler(|h, req, _| {
        let out = to_native(req.pipeline[0].args.last().unwrap());
        h.write(&out, b"ifw-all-in-one", WriteMode::CreateNew)?;
        Ok(ok_output(b""))
    })
    .unwrap();
}

#[test]
fn ifw_all_in_one_later_component_replaces_a_shared_file() {
    let host = make_profile_host(OsProfile::Linux);
    host.seed_file(Path::new("/work/payload/runtime/shared.txt"), b"runtime")
        .unwrap();
    host.seed_file(Path::new("/work/payload/devel/shared.txt"), b"devel")
        .unwrap();
    seed_all_in_one(
        &host,
        r#"
if(CMAKE_INSTALL_COMPONENT STREQUAL "Runtime")
  file(INSTALL "/work/payload/runtime/shared.txt" DESTINATION "${CMAKE_INSTALL_PREFIX}/share")
elseif(CMAKE_INSTALL_COMPONENT STREQUAL "Devel")
  file(INSTALL "/work/payload/devel/shared.txt" DESTINATION "${CMAKE_INSTALL_PREFIX}/share")
endif()
"#,
    );

    assert_eq!(
        run_unrestricted(host.clone(), &[]),
        0,
        "stderr={}",
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
    );
    let data_dir = Path::new("/work/_CPack_Packages/IFW/AllInOne-1.0.0/packages/AllInOne/data");
    assert_eq!(
        read_bytes(host.as_ref(), &data_dir.join("share/shared.txt")),
        b"devel"
    );
}

#[test]
fn ifw_rejects_a_merged_link_that_another_component_redirects_outside() {
    let host = make_profile_host(OsProfile::Linux);
    seed_all_in_one(
        &host,
        r#"
file(MAKE_DIRECTORY "${CMAKE_INSTALL_PREFIX}/lib")
if(CMAKE_INSTALL_COMPONENT STREQUAL "Runtime")
  file(CREATE_LINK ".." "${CMAKE_INSTALL_PREFIX}/lib/q" SYMBOLIC)
elseif(CMAKE_INSTALL_COMPONENT STREQUAL "Devel")
  file(CREATE_LINK "q/../../x" "${CMAKE_INSTALL_PREFIX}/lib/m" SYMBOLIC)
endif()
"#,
    );
    let invoked = Arc::new(AtomicUsize::new(0));
    let invoked_ref = Arc::clone(&invoked);
    host.set_command_handler(move |_, _, _| {
        invoked_ref.fetch_add(1, Ordering::SeqCst);
        Ok(ok_output(b""))
    })
    .unwrap();

    assert_eq!(run_unrestricted(host.clone(), &[]), 1);
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
    assert!(stderr.contains("escapes staging root"), "{stderr}");
}

#[test]
fn component_payload_validates_links_across_its_merged_roots() {
    let host = make_profile_host(OsProfile::MacOs);
    let staging = Path::new("/work/stage");
    for (root, link, target) in [("one", "q", ".."), ("two", "m", "q/../../x")] {
        let lib = staging.join(root).join("lib");
        host.create_dir_all(&lib).unwrap();
        host.symlink(Path::new(target), &lib.join(link)).unwrap();
    }
    let canon_staging = host.canonicalize(staging).unwrap();
    let staged_roots = ["one", "two"]
        .into_iter()
        .map(|name| crate::StagedRoot {
            root: staging.join(name),
            scanned: staging.join(name),
            canon_root: canon_staging.join(name),
            canon_scanned: canon_staging.join(name),
            files: Vec::new(),
        })
        .collect();
    let component = crate::InstalledComponent {
        name: "app".into(),
        staged_roots,
        ..crate::InstalledComponent::default()
    };
    let err = crate::generators::installer::copy_component_payload(
        host.as_ref(),
        staging,
        &canon_staging,
        &component,
        Path::new("/work/payload"),
        crate::source::SymlinkPolicy::PreserveContained,
        false,
        true,
    )
    .unwrap_err();
    assert!(err.to_string().contains("escapes staging root"), "{err}");
}

#[test]
fn ifw_skips_a_malformed_resource_descriptor_and_still_rejects_escapes() {
    for (descriptor, succeeds) in [
        (&b"<RCC><qresource><file>asset.txt</file>"[..], true),
        (
            b"<RCC><qresource><file>../outside.txt</file></qresource></RCC>",
            false,
        ),
    ] {
        let host = make_profile_host(OsProfile::Linux);
        seed_tool(&host, OsProfile::Linux, "binarycreator");
        host.seed_file(Path::new("/work/src/a.txt"), b"a").unwrap();
        host.seed_file(Path::new("/work/res/asset.txt"), b"asset")
            .unwrap();
        host.seed_file(Path::new("/work/outside.txt"), b"outside")
            .unwrap();
        host.seed_file(Path::new("/work/res/app.qrc"), descriptor)
            .unwrap();
        host.seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "IFW")
set(CPACK_PACKAGE_NAME "Res")
set(CPACK_PACKAGE_VERSION "1.0")
set(CPACK_PACKAGE_FILE_NAME "Res-1.0")
set(CPACK_INSTALLED_DIRECTORIES "/work/src;/")
set(CPACK_IFW_PACKAGE_RESOURCES "/work/res/app.qrc")
"#,
        )
        .unwrap();
        host.set_command_handler(|h, req, _| {
            assert!(find_arg_after(&req.pipeline[0].args, b"-r").is_none());
            let out = to_native(req.pipeline[0].args.last().unwrap());
            h.write(&out, b"ifw", WriteMode::CreateNew)?;
            Ok(ok_output(b""))
        })
        .unwrap();

        let code = run_unrestricted(host.clone(), &[]);
        let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
        if succeeds {
            assert_eq!(code, 0, "{stderr}");
            assert!(
                stderr.replace('\\', "/").contains(
                    "Can't copy resources from \"/work/res/app.qrc\". Resource will be skipped."
                ),
                "{stderr}"
            );
        } else {
            assert_eq!(code, 1);
            assert!(stderr.contains("escapes"), "{stderr}");
        }
    }
}

#[test]
fn installer_tool_defaults_to_no_timeout_honors_env_override_and_truncates_over_16mib() {
    use std::time::Duration;

    use super::{MAX_TOOL_OUTPUT_BYTES, TOOL_TIMEOUT_ENV, build_tool_timeout, run_installer_tool};

    let host = make_profile_host(OsProfile::Linux);
    assert_eq!(build_tool_timeout(host.as_ref()), None);
    host.env_set(TOOL_TIMEOUT_ENV, Some(b"0"));
    assert_eq!(build_tool_timeout(host.as_ref()), None);
    host.env_set(TOOL_TIMEOUT_ENV, Some(b"45"));
    assert_eq!(
        build_tool_timeout(host.as_ref()),
        Some(Duration::from_secs(45))
    );
    host.env_set(TOOL_TIMEOUT_ENV, None);

    let seen_timeout = Arc::new(Mutex::new(Some(Duration::from_secs(999))));
    let seen_ref = Arc::clone(&seen_timeout);
    host.set_command_handler(move |_, req, on_output| {
        *seen_ref.lock().unwrap() = req.timeout;
        let chunk = vec![b'A'; 1024 * 1024];
        for _ in 0..17 {
            on_output(Stream::Stdout, &chunk);
        }
        Ok(ok_output(b""))
    })
    .unwrap();

    let outcome = run_installer_tool(
        host.as_ref(),
        Path::new("/work"),
        b"/usr/bin/tool",
        &[],
        build_tool_timeout(host.as_ref()),
    )
    .expect("oversized tool output must truncate cleanly instead of failing");
    assert_eq!(*seen_timeout.lock().unwrap(), None);
    assert!(matches!(outcome.status, ProcessResult::Exited(0)));
    assert!(outcome.output.len() > MAX_TOOL_OUTPUT_BYTES);
    assert!(
        outcome
            .output
            .ends_with(b"\n... [output truncated at 16 MiB]\n")
    );
}

#[test]
fn nsis_propagates_non_notfound_component_dir_io_errors() {
    use std::io::ErrorKind;

    struct DenyComponentStat {
        inner: Arc<PureHost>,
        denied_suffix: String,
    }

    impl Host for DenyComponentStat {
        rcmake_host::forward_host!(inner, env, process);
        rcmake_host::forward_host!(@method inner, metadata(p: &Path, follow: Follow) -> HostResult<rcmake_host::Metadata>);
        fn metadata_fresh(&self, p: &Path, follow: Follow) -> HostResult<rcmake_host::Metadata> {
            if rcmake_host::path::from_native_cmake(p)
                .to_string()
                .ends_with(&self.denied_suffix)
            {
                return Err(HostError::from_io(
                    std::io::Error::new(ErrorKind::PermissionDenied, "denied"),
                    Some(p.to_path_buf()),
                ));
            }
            self.inner.metadata_fresh(p, follow)
        }
        rcmake_host::forward_host!(@ inner, read);
        rcmake_host::forward_host!(@ inner, read_with_origin);
        rcmake_host::forward_host!(@ inner, read_chunks);
        rcmake_host::forward_host!(@method inner, write(p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, read_dir(p: &Path) -> HostResult<Vec<rcmake_host::DirEntry>>);
        rcmake_host::forward_host!(@method inner, create_dir_all(p: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, create_private_dir(p: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, remove_file(p: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, remove_dir(p: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, remove_dir_all(p: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, rename(from: &Path, to: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, rename_noreplace(from: &Path, to: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, lock_file(path: &Path, timeout: Option<core::time::Duration>) -> HostResult<Box<dyn rcmake_host::FileLockGuard>>);
        rcmake_host::forward_host!(@method inner, copy_file(from: &Path, to: &Path, opts: rcmake_host::CopyOptions) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, symlink(target: &Path, link: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, hard_link(src: &Path, dst: &Path) -> HostResult<()>);
        rcmake_host::forward_host!(@ inner, read_link);
        rcmake_host::forward_host!(@method inner, canonicalize(p: &Path) -> HostResult<PathBuf>);
        rcmake_host::forward_host!(@method inner, canonicalize_fresh(p: &Path) -> HostResult<PathBuf>);
        rcmake_host::forward_host!(@method inner, set_permissions(p: &Path, mode: u32) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, set_times(p: &Path, atime: Option<rcmake_host::FileTime>, mtime: Option<rcmake_host::FileTime>) -> HostResult<()>);
        rcmake_host::forward_host!(@method inner, set_symlink_times(path: &Path, atime: Option<rcmake_host::FileTime>, mtime: Option<rcmake_host::FileTime>) -> HostResult<()>);
        rcmake_host::forward_host!(@ inner, open_read);
        rcmake_host::forward_host!(@ inner, scratch_file);
        rcmake_host::forward_host!(@ inner, scratch_file_in);

        fn registry_host_view(&self) -> Option<RegistryView> {
            self.inner.registry_host_view()
        }
        fn registry_value(
            &self,
            root: RegistryRoot,
            subkey: &[u8],
            name: Option<&[u8]>,
            view: RegistryView,
        ) -> HostResult<Option<BString>> {
            self.inner.registry_value(root, subkey, name, view)
        }
        fn write_out(&self, stream: Stream, bytes: &[u8]) {
            self.inner.write_out(stream, bytes);
        }
        fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
            self.inner.read_stdin(limit)
        }
        fn is_terminal(&self, stream: Stream) -> bool {
            self.inner.is_terminal(stream)
        }
        fn terminal_width(&self, stream: Stream) -> Option<usize> {
            self.inner.terminal_width(stream)
        }
        fn now(&self) -> rcmake_host::FileTime {
            self.inner.now()
        }
        fn monotonic(&self) -> std::time::Duration {
            self.inner.monotonic()
        }
        fn sleep(&self, duration: std::time::Duration) {
            self.inner.sleep(duration);
        }
        fn time_zone(&self) -> rcmake_host::TimeZone {
            self.inner.time_zone()
        }
        fn current_exe(&self) -> HostResult<PathBuf> {
            self.inner.current_exe()
        }
        fn initial_cwd(&self) -> PathBuf {
            self.inner.initial_cwd()
        }
        fn system(&self) -> SystemInfo {
            self.inner.system()
        }
    }

    let inner = make_profile_host(OsProfile::Windows);
    seed_tool(&inner, OsProfile::Windows, "makensis");
    seed_component_project(
        &inner,
        r#"
if(CMAKE_INSTALL_COMPONENT STREQUAL "Runtime")
  file(WRITE "${CMAKE_INSTALL_PREFIX}/bin.txt" "ok")
endif()
"#,
    );
    inner
        .seed_file(
            Path::new("/work/CPackConfig.cmake"),
            br#"
set(CMAKE_ROOT "/cmake")
set(CPACK_GENERATOR "NSIS")
set(CPACK_CMAKE_GENERATOR "Ninja")
set(CPACK_PACKAGE_NAME "NsisErr")
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "NsisErr-1.0.0")
set(CPACK_INSTALL_CMAKE_PROJECTS "/work/build;NsisErr;ALL;/")
set(CPACK_COMPONENTS_ALL "Runtime;Devel")
"#,
        )
        .unwrap();
    inner
        .set_command_handler(|_, req, _| {
            if req
                .pipeline
                .first()
                .is_some_and(|cmd| cmd.args == [BString::from("/VERSION")])
            {
                return Ok(ok_output(b"v3.08\n"));
            }
            Ok(ok_output(b"ok"))
        })
        .unwrap();

    let wrapper = Arc::new(DenyComponentStat {
        inner: Arc::clone(&inner),
        denied_suffix: "/NsisErr-1.0.0/Devel".to_owned(),
    });
    assert_eq!(run_unrestricted(wrapper, &[]), 1);
    let stderr = String::from_utf8_lossy(&inner.stream_bytes(Stream::Stderr)).into_owned();
    assert!(
        stderr.to_ascii_lowercase().contains("denied")
            || stderr.to_ascii_lowercase().contains("permission"),
        "expected permission error to propagate instead of skipping: {stderr}"
    );
}
