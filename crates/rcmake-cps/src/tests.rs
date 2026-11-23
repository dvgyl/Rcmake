use std::{collections::BTreeMap, error::Error};

use rcmake_base::BString;

use crate::{
    CPS_VERSION, CpsComponent, CpsComponentType, CpsConfigSupplement, CpsError, CpsPackage,
    CpsRequirement, CpsTargetFileSet, CpsUsage, naming, reader, writer,
};

#[test]
fn component_type_keywords_round_trip() {
    for name in [
        "archive",
        "dylib",
        "module",
        "executable",
        "interface",
        "symbolic",
        "jar",
        "custom",
    ] {
        let ty = CpsComponentType::from_name(name.as_bytes());
        assert_eq!(ty.as_bytes(), name.as_bytes());
    }
    assert_eq!(
        CpsComponentType::from_name(b"custom"),
        CpsComponentType::Unknown("custom".into())
    );
}

#[test]
fn file_names_and_directories() -> Result<(), CpsError> {
    assert_eq!(
        naming::package_file_name(b"Mixed", None, false)?,
        "Mixed.cps"
    );
    assert_eq!(
        naming::package_file_name(b"Mixed", Some(b"ExtraBits"), true)?,
        "mixed-ExtraBits.cps"
    );
    assert_eq!(
        naming::supplement_file_name(b"Mixed", Some(b"ExtraBits"), b"RelWithDebInfo", true)?,
        "mixed-ExtraBits@relwithdebinfo.cps"
    );
    assert_eq!(
        naming::supplement_file_name(b"Mixed", None, b"", false)?,
        "Mixed@noconfig.cps"
    );
    assert_eq!(
        naming::install_directory(b"Mixed", b"lib64", false, false)?,
        "lib64/cps/Mixed"
    );
    assert_eq!(
        naming::install_directory(b"Mixed", b"lib64", true, true)?,
        "cps/mixed"
    );
    assert_eq!(
        naming::build_directory(b"Mixed", b"/build/", false)?,
        "/build/cps/Mixed"
    );
    assert!(naming::package_file_name(b"../Pkg", None, false).is_err());
    assert!(naming::supplement_file_name(b"Pkg", None, b"../Debug", false).is_err());
    Ok(())
}

#[test]
fn version_bounds_and_schemas() {
    for (version, accepted) in [
        ("0.12", false),
        ("0.13", true),
        ("0.14.8", true),
        ("0.15.0", true),
        ("0.16", false),
        ("1.0", false),
        ("0.15.", false),
        ("0.15oops", false),
    ] {
        let text = format!(
            r#"{{"name":"Pkg","cps_path":"@prefix@","cps_version":"{version}","components":{{}}}}"#
        );
        let result = reader::read_package(text.as_bytes(), b"/cps/Pkg.cps");
        assert_eq!(result.is_ok(), accepted, "{version}");
        if let Err(error) = result {
            assert!(error.to_string().contains("Unsupported cps_version"));
        }
    }
    for (schema, version, accepted) in [
        ("simple", "1.0-rc1", true),
        ("simple", "1..0", false),
        ("simple", "1.2x", false),
        ("custom", "VII", true),
        ("roman", "VII", true),
    ] {
        let text = format!(
            r#"{{"name":"Pkg","cps_path":"@prefix@","cps_version":"0.15.0","components":{{}},"version_schema":"{schema}","version":"{version}"}}"#
        );
        assert_eq!(
            reader::read_package(text.as_bytes(), b"/Pkg.cps").is_ok(),
            accepted,
            "{schema} {version}",
        );
    }
    assert!(reader::read_package(b"{}", b"/Pkg.cps").is_err());
}

fn headers(root: BString) -> CpsTargetFileSet {
    CpsTargetFileSet {
        ty: "includes".into(),
        name: Some("api".into()),
        root,
        files: vec!["foo.h".into()],
    }
}

fn archive(usage: CpsUsage) -> CpsComponent {
    CpsComponent {
        ty: CpsComponentType::Archive,
        usage,
        license: None,
        configurations: BTreeMap::new(),
    }
}

#[test]
fn package_round_trip() -> Result<(), CpsError> {
    let mut component = archive(CpsUsage {
        includes: BTreeMap::from([
            ("*".into(), vec!["/opt/include".into()]),
            ("cpp".into(), vec!["/opt/include/cpp".into()]),
        ]),
        definitions: BTreeMap::from([(
            "*".into(),
            BTreeMap::from([("DEFINED".into(), None), ("EMPTY".into(), Some("".into()))]),
        )]),
        compile_features: vec!["c++23".into()],
        compile_flags: BTreeMap::from([("cpp".into(), vec!["-O2".into()])]),
        compile_requires: vec![":headers".into()],
        link_directories: vec!["/opt/lib".into()],
        link_flags: vec!["--as-needed".into()],
        link_features: vec!["feature".into()],
        link_libraries: vec!["m".into()],
        link_requires: vec![":static".into()],
        requires: vec!["Dep:core".into()],
        file_sets: vec![headers("/opt/include".into())],
        ..CpsUsage::default()
    });
    component.license = Some("MIT".into());
    component.configurations.insert(
        "Debug".into(),
        CpsUsage {
            location: Some("/opt/lib/foo.a".into()),
            link_location: Some("/opt/lib/foo.lib".into()),
            link_name: Some("foo".into()),
            link_languages: vec!["cpp".into()],
            dyld_requires: vec![":private".into()],
            ..CpsUsage::default()
        },
    );
    let package = CpsPackage {
        name: "Foo".into(),
        cps_version: CPS_VERSION.into(),
        cps_path: Some("@prefix@".into()),
        version: Some("1.0.0".into()),
        compat_version: Some("1.0".into()),
        version_schema: Some("simple".into()),
        description: Some("Résumé".into()),
        website: Some("https://example.invalid".into()),
        license: Some("MIT".into()),
        default_license: Some("BSD-3-Clause".into()),
        configurations: vec!["Release".into(), "Debug".into()],
        default_components: vec!["foo".into()],
        components: BTreeMap::from([("foo".into(), component)]),
        requires: BTreeMap::from([(
            "Dep".into(),
            CpsRequirement {
                components: vec!["core".into()],
                version: Some("1.0".into()),
                hints: vec!["/opt/dep".into()],
                domains: vec!["pkg-config".into()],
            },
        )]),
        ..CpsPackage::default()
    };
    let text = writer::write_package(&package)?;
    assert_eq!(reader::read_package(&text, b"/opt/Foo.cps")?, package);
    Ok(())
}

#[test]
fn resolves_paths_and_merges_companion_files() -> Result<(), Box<dyn Error>> {
    let file = br#"{
        "name":"Foo","cps_version":"0.13","cps_path":"@prefix@/lib/cps/Foo",
        "requires":{"Dep":null},
        "components":{"main":{"type":"dylib",
            "link_directories":["@prefix@/lib"],
            "link_libraries":["m","@prefix@/lib/libp.a"],
            "configurations":{"Debug":{"compile_flags":["-g"]}}
        }}
    }"#;
    let appendix = br#"{
        "name":"Foo","default_license":"MIT",
        "requires":{"Dep":{"version":"1.0","components":["a"],"hints":["@prefix@/dep"]}},
        "components":{"extra":{"type":"interface","includes":["relative","@prefix@/include"]}}
    }"#;
    let config = br#"{
        "name":"Foo","configuration":"DEBUG",
        "components":{"main":{"location":"@prefix@/lib/foo.so","link_location":"@prefix@/lib/foo.a",
            "compile_flags":{"*":["-O0"]},"file_sets":[
                {"type":"includes","root":"@prefix@/include","files":["foo.h"],
                 "extensions":{"cmake":{"name@v1":"api"}}}
            ]},"extra":{"includes":{"cpp":["@prefix@/include/extra"]}}}
    }"#;
    let mut package = reader::read_package(file, b"/stage/lib/cps/Foo/Foo.cps")?;
    reader::merge_appendix(&mut package, appendix, b"/stage/lib/cps/Foo/Foo-extra.cps")?;
    let path = b"/stage/lib/cps/Foo/Foo@debug.cps";
    let supplement = reader::read_supplement(config, path)?;
    reader::merge_supplement(&mut package, supplement, path)?;

    let main = package
        .components
        .get(b"main".as_slice())
        .ok_or("Missing main component.")?;
    let extra = package
        .components
        .get(b"extra".as_slice())
        .ok_or("Missing extra component.")?;
    let debug = main
        .configurations
        .get(b"Debug".as_slice())
        .ok_or("Missing Debug configuration.")?;
    assert_eq!(main.usage.link_directories, ["/stage/lib"]);
    assert_eq!(main.usage.link_libraries, ["m", "/stage/lib/libp.a"]);
    assert_eq!(
        debug.location.as_ref().map(|path| path.as_slice()),
        Some(b"/stage/lib/foo.so".as_slice())
    );
    assert_eq!(
        debug.link_location.as_ref().map(|path| path.as_slice()),
        Some(b"/stage/lib/foo.a".as_slice())
    );
    assert_eq!(
        debug.compile_flags.get(b"*".as_slice()),
        Some(&vec!["-g".into(), "-O0".into()])
    );
    assert_eq!(debug.file_sets, vec![headers("/stage/include".into())]);
    assert_eq!(extra.license, Some("MIT".into()));
    assert_eq!(
        extra.usage.includes.get(b"*".as_slice()),
        Some(&vec![
            "/stage/lib/cps/Foo/relative".into(),
            "/stage/include".into()
        ])
    );
    assert_eq!(extra.configurations.len(), 1);
    let dependency = package
        .requires
        .get(b"Dep".as_slice())
        .ok_or("Missing dependency.")?;
    assert_eq!(dependency.hints, ["/stage/dep"]);
    assert_eq!(dependency.components, ["a"]);

    let original = package.clone();
    assert!(
        reader::merge_appendix(&mut package, appendix, b"/stage/lib/cps/Foo/Foo-extra.cps")
            .is_err()
    );
    assert_eq!(package, original);
    let path = b"/stage/lib/cps/Foo/Foo@release.cps";
    let unknown = reader::read_supplement(
        br#"{
        "name":"Foo","configuration":"Release","components":{"main":{},"undefined":{}}
    }"#,
        path,
    )?;
    assert!(reader::merge_supplement(&mut package, unknown, path).is_err());
    assert_eq!(package, original);
    Ok(())
}

#[test]
fn resolves_windows_paths_and_rejects_a_mismatched_prefix() -> Result<(), CpsError> {
    let text = br#"{
        "name":"Foo","cps_version":"0.15.0","cps_path":"@prefix@/cps/Foo",
        "components":{"app":{"type":"executable","location":"@prefix@\\bin\\app.exe"}}
    }"#;
    let package = reader::read_package(text, br"C:\Stage\CPS\FOO\Foo.cps")?;
    assert_eq!(
        package
            .components
            .values()
            .next()
            .and_then(|app| app.usage.location.as_ref()),
        Some(&"C:/Stage/bin/app.exe".into())
    );
    assert!(reader::read_package(text, b"/stage/elsewhere/Foo.cps").is_err());
    Ok(())
}

#[test]
fn supplement_uses_noconfig_and_omits_schema_version() -> Result<(), CpsError> {
    let supplement = CpsConfigSupplement {
        name: "Foo".into(),
        cps_version: CPS_VERSION.into(),
        ..CpsConfigSupplement::default()
    };
    let text = writer::write_supplement(&supplement)?;
    assert_eq!(
        text,
        "{\n  \"components\" : {},\n  \"configuration\" : \"noconfig\",\n  \"name\" : \"Foo\"\n}"
    );
    let parsed = reader::read_supplement(&text, b"/Foo@noconfig.cps")?;
    assert_eq!(parsed.configuration, "noconfig");
    assert!(parsed.cps_version.is_empty());
    Ok(())
}

#[allow(
    clippy::disallowed_methods,
    reason = "The live oracle test runs CMake in a temporary directory."
)]
mod oracle {
    use std::{
        env,
        ffi::OsStr,
        fs, io,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use super::*;

    const SOURCE: &str = r#"
cmake_minimum_required(VERSION 4.5)
project(CpsProbe LANGUAGES C)
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/foo.c" "int foo(void) { return 0; }")
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/foo.h" "int foo(void);")
add_library(foo STATIC "${CMAKE_CURRENT_BINARY_DIR}/foo.c")
target_compile_definitions(foo INTERFACE FOO EMPTY= WITH=on)
target_compile_options(foo INTERFACE -fbase)
target_include_directories(foo INTERFACE $<INSTALL_INTERFACE:include>)
target_sources(foo INTERFACE FILE_SET api TYPE HEADERS
  BASE_DIRS "${CMAKE_CURRENT_BINARY_DIR}" FILES "${CMAKE_CURRENT_BINARY_DIR}/foo.h")
set_target_properties(foo PROPERTIES SPDX_LICENSE MIT)
add_library(Extra INTERFACE)
target_link_libraries(Extra INTERFACE foo)
install(TARGETS foo EXPORT root ARCHIVE DESTINATION lib FILE_SET api DESTINATION include)
install(TARGETS Extra EXPORT part)
set(metadata LOWER_CASE_FILE VERSION 1.2 COMPAT_VERSION 1.0 VERSION_SCHEMA simple
  DEFAULT_TARGETS foo DEFAULT_CONFIGURATIONS Release Debug DESCRIPTION "Résumé"
  LICENSE MIT DEFAULT_LICENSE BSD-3-Clause HOMEPAGE_URL "https://example.invalid")
install(PACKAGE_INFO Mixed EXPORT root ${metadata})
install(PACKAGE_INFO Mixed EXPORT part APPENDIX ExtraBits LOWER_CASE_FILE)
export(PACKAGE_INFO Mixed EXPORT root ${metadata})
export(PACKAGE_INFO Mixed EXPORT part APPENDIX ExtraBits LOWER_CASE_FILE)
"#;

    #[test]
    fn install_and_export_match_cmake_4_5_1_byte_for_byte() -> Result<(), Box<dyn Error>> {
        let Some(cmake) = env::var_os("RCMAKE_TEST_REFERENCE_CMAKE") else {
            return Ok(());
        };
        let temp = TempDir::create()?;
        let root = &temp.0;
        fs::write(root.join("CMakeLists.txt"), SOURCE)?;
        run(root, &cmake, &["--version"])?;
        let version = fs::read_to_string(root.join("cmake.log"))?;
        assert_eq!(version.lines().next(), Some("cmake version 4.5.1"));
        run(
            root,
            &cmake,
            &[
                "-S",
                ".",
                "-B",
                "build",
                "-G",
                "Ninja",
                "-DCMAKE_BUILD_TYPE=Debug",
                "-DCMAKE_INSTALL_LIBDIR=lib",
            ],
        )?;
        run(root, &cmake, &["--build", "build"])?;
        run(
            root,
            &cmake,
            &["--install", "build", "--prefix", "installed"],
        )?;

        let build = root.join("build");
        let installed = root.join("installed");
        let install_dir = installed.join("lib/cps/mixed");
        let build_dir = build.join("cps/mixed");
        let mut package = CpsPackage {
            name: "Mixed".into(),
            cps_path: Some("@prefix@/lib/cps/mixed".into()),
            version: Some("1.2".into()),
            compat_version: Some("1.0".into()),
            version_schema: Some("simple".into()),
            description: Some("Résumé".into()),
            website: Some("https://example.invalid".into()),
            license: Some("MIT".into()),
            default_license: Some("BSD-3-Clause".into()),
            configurations: vec!["Release".into(), "Debug".into()],
            default_components: vec!["foo".into()],
            ..CpsPackage::default()
        };
        let mut foo = archive(CpsUsage {
            includes: BTreeMap::from([("*".into(), vec!["@prefix@/include".into(); 2])]),
            file_sets: vec![headers("@prefix@/include".into())],
            definitions: BTreeMap::from([(
                "*".into(),
                BTreeMap::from([
                    ("EMPTY".into(), Some("".into())),
                    ("FOO".into(), None),
                    ("WITH".into(), Some("on".into())),
                ]),
            )]),
            compile_flags: BTreeMap::from([("*".into(), vec!["-fbase".into()])]),
            ..CpsUsage::default()
        });
        foo.license = Some("MIT".into());
        package.components.insert("foo".into(), foo.clone());
        let installed_usage = CpsUsage {
            location: Some("@prefix@/lib/libfoo.a".into()),
            link_languages: vec!["c".into()],
            ..CpsUsage::default()
        };
        let supplement = CpsConfigSupplement {
            name: "Mixed".into(),
            configuration: "Debug".into(),
            components: BTreeMap::from([("foo".into(), installed_usage.clone())]),
            ..CpsConfigSupplement::default()
        };
        let appendix = CpsPackage {
            name: "Mixed".into(),
            cps_path: package.cps_path.clone(),
            components: BTreeMap::from([(
                "Extra".into(),
                CpsComponent {
                    ty: CpsComponentType::Interface,
                    usage: CpsUsage {
                        requires: vec![":foo".into()],
                        ..CpsUsage::default()
                    },
                    license: None,
                    configurations: BTreeMap::new(),
                },
            )]),
            ..CpsPackage::default()
        };
        let root_name = naming::package_file_name(b"Mixed", None, true)?;
        let appendix_name = naming::package_file_name(b"Mixed", Some(b"ExtraBits"), true)?;
        let config_name = naming::supplement_file_name(b"Mixed", None, b"Debug", true)?;
        equal(
            &install_dir.join(root_name.to_string()),
            writer::write_package(&package)?,
        )?;
        equal(
            &install_dir.join(appendix_name.to_string()),
            writer::write_package(&appendix)?,
        )?;
        equal(
            &install_dir.join(config_name.to_string()),
            writer::write_supplement(&supplement)?,
        )?;

        let build_path = BString::from(build.as_os_str().as_encoded_bytes());
        foo.usage.includes = BTreeMap::from([("*".into(), vec![build_path.clone()])]);
        foo.usage.file_sets = vec![headers(build_path)];
        foo.configurations.insert(
            "Debug".into(),
            CpsUsage {
                location: Some(build.join("libfoo.a").as_os_str().as_encoded_bytes().into()),
                ..installed_usage
            },
        );
        package.components.insert("foo".into(), foo);
        equal(
            &build_dir.join(root_name.to_string()),
            writer::write_build_package(&package)?,
        )?;
        equal(
            &build_dir.join(appendix_name.to_string()),
            writer::write_build_package(&appendix)?,
        )?;
        Ok(())
    }

    fn equal(path: &Path, expected: BString) -> io::Result<()> {
        let actual = BString::from(fs::read(path)?);
        assert_eq!(actual, expected, "{}", path.display());
        Ok(())
    }

    fn run(root: &Path, exe: &OsStr, args: &[&str]) -> io::Result<()> {
        let log = root.join("cmake.log");
        let output = fs::File::create(&log)?;
        let mut child = Command::new(exe)
            .args(args)
            .current_dir(root)
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output))
            .spawn()?;
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "CMake {args:?} failed.\n{}",
                        fs::read_to_string(log)?
                    )))
                };
            }
            if start.elapsed() >= Duration::from_secs(30) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("CMake {args:?} exceeded 30 seconds."),
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn create() -> Result<Self, Box<dyn Error>> {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = env::temp_dir().join(format!("rcmake-cps-{}-{stamp}", std::process::id()));
            fs::create_dir(&path)?;
            Ok(Self(path.canonicalize()?))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
