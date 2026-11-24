use std::path::{Path, PathBuf};

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_genex::ConfigId;
use rcmake_host::{
    Host, PureHost, ReadRange,
    path::{from_native, from_native_cmake},
};
use rcmake_model::{
    BT, BuildExport, DirId, DiscoverySpec, ExportKind, ExportSet, FileSet, FileSetKind,
    GeneratorKind, InstallKind, InstallOptions, InstallRule, PackageInfoSpec, ProjectModel,
    SbomSpec, Target, TargetKind, TargetOrigin, TestEntry, TestKind, UsageScope,
};
use rcmake_resolve::{ArtifactSet, CompileGroup, GenerationResult, ResolvedTarget, TargetMetadata};

#[cfg(unix)]
use crate::ctest_file;
use crate::{GenError, install_script, write_supplementary_outputs};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[cfg(unix)]
const PROJECT: &[u8] = br#"cmake_minimum_required(VERSION 4.5)
project(Sdk LANGUAGES C)
add_library(dep INTERFACE)
add_library(core STATIC code.c)
set_property(TARGET core PROPERTY SPDX_LICENSE MIT)
target_link_libraries(core PUBLIC dep)
target_compile_definitions(core INTERFACE FLAG FIXED=1)
target_compile_options(core INTERFACE -ffast-math)
target_sources(core PUBLIC FILE_SET HEADERS BASE_DIRS "${CMAKE_CURRENT_SOURCE_DIR}/headers" FILES "${CMAKE_CURRENT_SOURCE_DIR}/headers/api/core.h")
install(TARGETS dep core EXPORT sdk FILE_SET HEADERS DESTINATION include)
install(PACKAGE_INFO Sdk EXPORT sdk DEFAULT_TARGETS core DEFAULT_CONFIGURATIONS Debug VERSION 2.1)
export(PACKAGE_INFO Sdk EXPORT sdk DEFAULT_TARGETS core DEFAULT_CONFIGURATIONS Debug VERSION 2.1)
install(SBOM Bill EXPORTS sdk DESCRIPTION "Example" HOMEPAGE_URL https://example.com VERSION 2.1 DEFAULT_LICENSE BSD-2-Clause DATA_LICENSE CC0-1.0 PACKAGE_URL pkg:generic/sdk)
export(SBOM Bill EXPORTS sdk DESCRIPTION "Example" HOMEPAGE_URL https://example.com VERSION 2.1 DEFAULT_LICENSE BSD-2-Clause DATA_LICENSE CC0-1.0 PACKAGE_URL pkg:generic/sdk)
set_property(INSTALL "api/core.h" PROPERTY CPACK_TEST "a;b;$<CONFIG>")
set_property(INSTALL "api/core.h" PROPERTY NOTE [=[a"$x\]=])
enable_testing()
set_property(DIRECTORY PROPERTY TEST_INCLUDE_FILES "plain.cmake;config-$<CONFIG>.cmake")
add_test(NAME first COMMAND tool "$<CONFIG>" BUILD_DEPENDS code.c)
discover_tests(COMMAND tool "$<CONFIG>" DISCOVERY_ARGS --list DISCOVERY_MATCH "^(.*)$" TEST_NAME "\\1" TEST_ARGS --run "\\1" TEST_PROPERTIES LABELS discovery BUILD_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/code.c")
add_test(NAME last COMMAND tool "$<CONFIG>")
"#;

fn add_install(model: &mut ProjectModel, kind: InstallKind) -> usize {
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Backtrace::default(),
        kind,
        options: InstallOptions::default(),
    })
}

fn add_export(model: &mut ProjectModel, kind: ExportKind) {
    model.build_exports.push(BuildExport {
        dir: DirId::ROOT,
        backtrace: Backtrace::default(),
        kind,
        set: None,
        targets: Vec::new(),
        namespace: BString::default(),
        file: None,
        append: false,
        cxx_modules_directory: None,
    });
}

fn install_property(model: &mut ProjectModel, path: &[u8], property: &[u8], value: &[u8]) {
    model
        .install_props
        .entry(path.into())
        .or_default()
        .set(property.into(), value.into());
}

fn fixture(
    source: &Path,
    build: &Path,
    configs: &[&str],
) -> TestResult<(ProjectModel, GenerationResult)> {
    let mut model = ProjectModel::default();
    model.set_root_dirs(from_native(source), from_native(build));
    let multi = configs.len() > 1;
    model.generator = Some(if multi {
        GeneratorKind::NinjaMultiConfig
    } else {
        GeneratorKind::Ninja
    });
    let mut library = |name: &str, kind| {
        model.add_target(Target::new(
            name.into(),
            kind,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        ))
    };
    let dep = library("dep", TargetKind::InterfaceLibrary)?;
    let core = library("core", TargetKind::StaticLibrary)?;
    let props = &mut model.target_mut(core).ok_or("missing core target")?.props;
    for (key, value) in [
        ("SPDX_LICENSE", "MIT"),
        ("INTERFACE_COMPILE_DEFINITIONS", "FLAG;FIXED=1"),
        ("INTERFACE_COMPILE_OPTIONS", "-ffast-math"),
        ("INTERFACE_LINK_LIBRARIES", "dep"),
    ] {
        props.set(key.into(), value.into());
    }
    model.add_file_set(FileSet {
        target: core,
        name: "HEADERS".into(),
        kind: FileSetKind::Headers,
        scope: UsageScope::Public,
        base_dirs: vec![BT::new("headers".into(), Default::default())],
        files: vec![BT::new("headers/api/core.h".into(), Default::default())],
        props: Default::default(),
    })?;
    let targets = vec![dep, core];
    let index = add_install(
        &mut model,
        InstallKind::Targets {
            targets: targets.clone(),
            export: Some("sdk".into()),
        },
    );
    for (arg, value) in ["FILE_SET", "HEADERS", "DESTINATION", "include"]
        .into_iter()
        .enumerate()
    {
        model.global.props.set(
            format!("_RCMAKE_INSTALL_{index}_ARG{arg}").into(),
            value.into(),
        );
    }
    model.exports.insert(
        "sdk".into(),
        ExportSet {
            name: "sdk".into(),
            targets,
            install_rules: vec![index],
        },
    );

    let package = PackageInfoSpec {
        package: "Sdk".into(),
        export: "sdk".into(),
        default_targets: vec!["core".into()],
        default_configurations: vec!["Debug".into()],
        version: Some("2.1".into()),
        ..Default::default()
    };
    add_install(
        &mut model,
        InstallKind::PackageInfo(Box::new(package.clone())),
    );
    add_export(&mut model, ExportKind::PackageInfo(package));
    let spec = SbomSpec {
        name: "Bill".into(),
        exports: vec!["sdk".into()],
        description: Some("Example".into()),
        homepage_url: Some("https://example.com".into()),
        version: Some("2.1".into()),
        default_license: Some("BSD-2-Clause".into()),
        data_license: Some("CC0-1.0".into()),
        package_url: Some("pkg:generic/sdk".into()),
        ..Default::default()
    };
    add_install(&mut model, InstallKind::Sbom(Box::new(spec.clone())));
    add_export(&mut model, ExportKind::Sbom(spec));

    install_property(&mut model, b"api/core.h", b"NOTE", b"a\"$x\\");
    install_property(&mut model, b"api/core.h", b"CPACK_TEST", b"a;b;$<CONFIG>");
    model.dir_mut(DirId::ROOT).props.set(
        "TEST_INCLUDE_FILES".into(),
        "plain.cmake;config-$<CONFIG>.cmake".into(),
    );
    for (name, line, command) in [
        ("first", 19, "add_test"),
        ("discovery", 20, "discover_tests"),
        ("last", 21, "add_test"),
    ] {
        let frame = |line, name: Option<&str>| Frame {
            file: from_native(&source.join("CMakeLists.txt"))
                .as_slice()
                .into(),
            line,
            name: name.map(|name| name.as_bytes().into()),
            kind: FrameKind::Command,
        };
        let backtrace = Backtrace::default()
            .push(frame(0, None))
            .push(frame(line, Some(command)));
        let discover = name == "discovery";
        model.add_test(TestEntry {
            name: name.into(),
            dir: DirId::ROOT,
            backtrace: backtrace.clone(),
            command: vec!["tool".into(), "$<CONFIG>".into()],
            configurations: Vec::new(),
            working_directory: None,
            command_expand_lists: false,
            old_style: false,
            policies: Default::default(),
            props: Default::default(),
            build_depends: match name {
                "discovery" => vec![BT::new(from_native(&source.join("code.c")), backtrace)],
                "first" => vec![BT::new("code.c".into(), backtrace)],
                _ => Vec::new(),
            },
            kind: if discover {
                TestKind::Discover(Box::new(DiscoverySpec {
                    discovery_args: vec!["--list".into()],
                    discovery_match: "^(.*)$".into(),
                    test_name: "\\1".into(),
                    test_args: vec!["--run".into(), "\\1".into()],
                    test_props: vec![
                        ("LABELS".into(), "discovery".into()),
                        (
                            "_CMAKE_DEFAULT_FIXTURE_REPEAT_MODE".into(),
                            "AROUND_EACH_REPEAT".into(),
                        ),
                    ],
                    ..Default::default()
                }))
            } else {
                TestKind::Single
            },
        })?;
    }
    let mut result = GenerationResult::default();
    for name in configs {
        let config = ConfigId((*name).into());
        let artifact = if multi {
            build.join(name).join("libcore.a")
        } else {
            build.join("libcore.a")
        };
        result.configs.push(config.clone());
        result.targets.push(ResolvedTarget {
            id: core,
            config,
            metadata: TargetMetadata {
                name: "core".into(),
                kind: TargetKind::StaticLibrary,
                source_dir: from_native(source),
                binary_dir: from_native(build),
                backtrace: Default::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: ArtifactSet {
                file: Some(from_native_cmake(&artifact)),
                ..Default::default()
            },
            compile_groups: vec![CompileGroup {
                language: "C".into(),
                ..Default::default()
            }],
            compile_units: Vec::new(),
            link: None,
            dependencies: Vec::new(),
            cxx_modules: None,
            ide: None,
        });
    }
    result.install = rcmake_resolve::lower::lower_install_manifest(&model, &result.targets);
    Ok((model, result))
}

fn read(host: &dyn Host, path: &Path) -> TestResult<BString> {
    Ok(host.read(path, ReadRange::All { limit: 4_000_000 })?.into())
}

fn export_stage(base: &Path, kind: &str, destination: &[u8]) -> PathBuf {
    base.join("CMakeFiles")
        .join(kind)
        .join(HashAlgo::Md5.hex_digest(destination))
}

#[test]
fn stages_noconfig_cps_and_explicit_sbom_and_preserves_header_paths() -> TestResult {
    let source = Path::new("/source");
    let build = Path::new("/out");
    let host = PureHost::default();
    let (mut model, result) = fixture(source, build, &[""])?;
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_INSTALL_SBOM_FORMATS".into(), "spdx".into());
    let manifest = write_supplementary_outputs(&model, &result, &host, build)?;
    let destination = if cfg!(windows) {
        b"cps/Sdk".as_slice()
    } else {
        b"lib/cps/Sdk".as_slice()
    };
    let stage = export_stage(build, "Export", destination);
    assert!(
        read(&host, &stage.join("Sdk@noconfig.cps"))?
            .to_string()
            .contains("@prefix@/lib/libcore.a")
    );
    assert!(
        read(&host, &build.join("cps/Sdk/Sdk.cps"))?
            .to_string()
            .contains("\"noconfig\"")
    );
    let script = install_script::render(&manifest, b"/out")?.to_string();
    assert!(script.contains("${CMAKE_INSTALL_PREFIX}/include/api"));
    assert!(script.contains(r#"CMAKE_INSTALL_CONFIG_NAME MATCHES "^()$""#));
    assert!(script.contains("Bill-noconfig.spdx.json"));
    assert!(!script.contains("sdk-noconfig.spdx.json"));
    Ok(())
}

#[test]
fn automatic_sbom_validates_formats_and_uses_project_metadata() -> TestResult {
    let source = Path::new("/source");
    let build = Path::new("/out");
    let host = PureHost::default();
    let (mut model, result) = fixture(source, build, &["Debug"])?;
    model
        .installs
        .retain(|rule| !matches!(rule.kind, InstallKind::Sbom(_)));
    let vars = &mut model.dir_mut(DirId::ROOT).vars;
    vars.insert("PROJECT_NAME".into(), "Sdk".into());
    vars.insert("Sdk_SPDX_LICENSE".into(), "BSD-3-Clause".into());
    vars.insert("CMAKE_INSTALL_SBOM_FORMATS".into(), "spdx-3.0+json".into());
    write_supplementary_outputs(&model, &result, &host, build)?;
    let stage = export_stage(build, "sbom", b"lib/sbom/sdk");
    let contents = read(&host, &stage.join("sdk-Debug.spdx.json"))?.to_string();
    assert!(contents.contains("urn:sdk#DataLicenseExpression"));
    assert!(contents.contains("BSD-3-Clause"));
    assert!(contents.contains("urn:core#DeclaredLicenseRelationship"));
    for format in ["invalid", "spdx;spdx-3.0+json"] {
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert("CMAKE_INSTALL_SBOM_FORMATS".into(), format.into());
        let Err(GenError::Invalid(message)) =
            write_supplementary_outputs(&model, &result, &host, build)
        else {
            return Err(format!("expected an invalid format error for {format}").into());
        };
        assert!(message.starts_with(b"\nCMake Error in CMakeLists.txt:\n"));
        assert!(message.to_string().contains(&format!(
            r#"CMAKE_INSTALL_SBOM_FORMATS given invalid format "{format}"."#,
        )));
    }
    Ok(())
}

#[test]
fn stages_appendix_files_with_lower_case_package_names() -> TestResult {
    let source = Path::new("/source");
    let build = Path::new("/out");
    let host = PureHost::default();
    let (mut model, result) = fixture(source, build, &["Debug"])?;
    let spec = PackageInfoSpec {
        package: "Sdk".into(),
        export: "sdk".into(),
        appendix: Some("Tools".into()),
        lower_case_file: true,
        ..Default::default()
    };
    add_install(&mut model, InstallKind::PackageInfo(Box::new(spec.clone())));
    add_export(&mut model, ExportKind::PackageInfo(spec));
    write_supplementary_outputs(&model, &result, &host, build)?;
    let destination = if cfg!(windows) {
        b"cps/sdk".as_slice()
    } else {
        b"lib/cps/sdk".as_slice()
    };
    let stage = export_stage(build, "Export", destination);
    assert!(
        read(&host, &stage.join("sdk-Tools@debug.cps"))?
            .to_string()
            .contains("\"Debug\"")
    );
    assert!(
        read(&host, &build.join("cps/sdk/sdk-Tools.cps"))?
            .to_string()
            .contains("\"Sdk\"")
    );
    Ok(())
}

#[cfg(unix)]
fn add_bmi_fixture(
    model: &mut ProjectModel,
    result: &mut GenerationResult,
    build: &Path,
    host: &dyn Host,
) -> TestResult {
    use rcmake_resolve::{BmiInstall, BuildGraph, CompileUnit, CxxModuleInfo};

    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_CXX_SCANDEP_SOURCE".into(), "scanner <SOURCE>".into());
    let facts = result
        .targets
        .first_mut()
        .ok_or("missing resolved core target")?;
    facts.cxx_modules = Some(CxxModuleInfo {
        tdi: "modules/CXXDependInfo.json".into(),
        dyndep: "modules/CXX.dd".into(),
        module_dir: "modules".into(),
        bmi_install: Some(BmiInstall {
            destination: "lib/bmi".into(),
            component: "Unspecified".into(),
            ..Default::default()
        }),
        ..Default::default()
    });
    let mut target = facts.clone();
    target.compile_units.push(CompileUnit {
        object: "modules/core.o".into(),
        scan: true,
        ..Default::default()
    });
    let steps = rcmake_resolve::lower::lower_modules(model, &target)?;
    result.graphs.push(BuildGraph {
        config: facts.config.clone(),
        steps,
        ..Default::default()
    });
    for (name, contents) in [
        ("modules/core.o.ddi", br#"{"rules":[{"provides":[{"logical-name":"core"}]}]}"#.as_slice()),
        ("modules/CXXModules.json", br#"{"modules":{"core":{"bmi":"modules/core.pcm"},"forwarded":{"bmi":"modules/forwarded.pcm"}}}"#),
        ("modules/core.pcm", b"BMI content"),
    ] {
        crate::export::write_output(host, &build.join(name), contents)?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn cmake_451_matches_package_install_outputs() -> TestResult {
    use std::time::Duration;

    use rcmake_host::{CommandSpec, Input, NativeHost, Output, ProcessResult, RunRequest};
    use rcmake_json::{ReadOptions, Value};

    let host = NativeHost::new()?;
    let Some(oracle) = host.env_get(b"RCMAKE_TEST_REFERENCE_CMAKE") else {
        return Ok(());
    };
    let root = PathBuf::from(format!(
        "/tmp/rcmake-package-install-oracle-{}",
        host.system().pid
    ));
    struct Cleanup<'a>(&'a dyn Host, &'a Path);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.remove_dir_all(self.1);
        }
    }
    host.create_dir_all(&root)?;
    let _cleanup = Cleanup(&host, &root);
    let source = root.join("source");
    let build = root.join("oracle");
    let output = root.join("output");
    let install = root.join("installed");
    for (name, contents) in [
        ("CMakeLists.txt", PROJECT),
        ("code.c", b"void demo(void) {}\n".as_slice()),
        ("headers/api/core.h", b"void demo(void);\n"),
    ] {
        crate::export::write_output(&host, &source.join(name), contents)?;
    }
    let run = |args: Vec<BString>| -> TestResult {
        let report = host.run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: oracle.clone(),
                    args,
                }],
                cwd: root.clone(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(40)),
                tee: false,
            },
            &mut |_, _| {},
        )?;
        assert_eq!(
            report.results,
            [ProcessResult::Exited(0)],
            "{}\n{}",
            BString::from(report.stdout),
            BString::from(report.stderr)
        );
        Ok(())
    };
    fn normalize_spdx(node: &mut Value) {
        match node {
            Value::Object(fields) => {
                if fields.get(b"type".as_slice()) == Some(&Value::string("CreationInfo")) {
                    fields.remove(b"created".as_slice());
                }
                for (key, value) in fields {
                    normalize_spdx(value);
                    if matches!(key.as_slice(), b"rootElement" | b"element" | b"@graph")
                        && let Value::Array(items) = value
                    {
                        items.sort_by_cached_key(rcmake_json::write_compact);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(normalize_spdx),
            _ => {}
        }
    }
    let json = |contents: &BString| -> TestResult<Value> {
        let mut value = rcmake_json::parse(contents, &ReadOptions::default())?;
        normalize_spdx(&mut value);
        Ok(value)
    };
    let cases: [(&str, &[&str]); 2] = [
        ("Ninja", &[""]),
        ("Ninja Multi-Config", &["Debug", "Release"]),
    ];
    for (generator, configs) in cases {
        let config = configs.first().ok_or("missing install configuration")?;
        let build = build.join(if configs.len() > 1 { "multi" } else { "single" });
        run(vec![
            "-S".into(),
            from_native(&source),
            "-B".into(),
            from_native(&build),
            "-G".into(),
            generator.into(),
            "-DCMAKE_CONFIGURATION_TYPES=Debug;Release".into(),
        ])?;
        let (mut model, mut result) = fixture(&source, &build, configs)?;
        add_bmi_fixture(&mut model, &mut result, &build, &host)?;
        let oracle_root = export_stage(&build, "Export", b"lib/cps/Sdk");
        let mut expected = Vec::new();
        for path in [oracle_root.join("Sdk.cps"), build.join("cps/Sdk/Sdk.cps")] {
            expected.push((path.clone(), read(&host, &path)?));
        }
        for config in configs {
            let label = if config.is_empty() {
                "noconfig"
            } else {
                config
            };
            let path = oracle_root.join(format!("Sdk@{}.cps", label.to_ascii_lowercase()));
            expected.push((path.clone(), read(&host, &path)?));
            let bill = format!("Bill-{label}.spdx.json");
            for path in [
                export_stage(&build, "sbom", b"lib/sbom/Bill").join(&bill),
                build.join("sbom/Bill").join(&bill),
            ] {
                expected.push((path.clone(), read(&host, &path)?));
            }
        }
        let cpack = build.join("CPackProperties.cmake");
        expected.push((cpack.clone(), read(&host, &cpack)?));
        let manifest = write_supplementary_outputs(&model, &result, &host, &build)?;
        for (path, oracle) in expected {
            let actual = read(&host, &path)?;
            if path.extension().is_some_and(|ext| ext == "json") {
                assert_eq!(json(&actual)?, json(&oracle)?, "{}", path.display());
            } else {
                assert_eq!(actual, oracle, "{}", path.display());
            }
        }
        let actual = ctest_file::render_resolved_with_host(&model, DirId::ROOT, &result, &host)?;
        let expected = read(&host, &build.join("CTestTestfile.cmake"))?;
        // Both quoting forms pass the same regular expression to CMake.
        assert_eq!(actual.to_string().replace(r"\$", "$"), expected.to_string());
        host.create_dir_all(&output)?;
        let prefix = from_native(&install);
        install_script::write_install_script(
            &manifest,
            &output,
            &install_script::InstallScriptOptions {
                prefix: &prefix,
                configuration: config.as_bytes(),
                ..Default::default()
            },
            &host,
        )?;
        run(vec![
            "-P".into(),
            from_native(&output.join("cmake_install.cmake")),
        ])?;
        assert_eq!(
            read(&host, &install.join("lib/bmi/core.pcm"))?,
            b"BMI content"
        );
        assert_eq!(
            read(&host, &install.join("include/api/core.h"))?,
            b"void demo(void);\n"
        );
        assert_eq!(
            read(&host, &install.join("lib/cps/Sdk/Sdk.cps"))?,
            read(&host, &oracle_root.join("Sdk.cps"))?
        );
    }
    Ok(())
}
