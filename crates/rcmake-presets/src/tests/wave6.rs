use super::*;

fn package_settings(preset: ResolvedPreset) -> PackageSettings {
    match preset.settings {
        PresetSettings::Package(settings) => settings,
        _ => panic!("package settings"),
    }
}

fn exclude_filter(presets: &Presets, name: &str) -> TestExclude {
    match named(presets, PresetKind::Test, name).settings {
        PresetSettings::Test(settings) => settings
            .filter
            .and_then(|filter| filter.exclude)
            .expect("exclude filter"),
        _ => panic!("test settings"),
    }
}

#[test]
fn package_fields_expand_macros_once() {
    let presets = root(
        r#"{
      "version":13,
      "configurePresets":[{"name":"cfg","binaryDir":"out","environment":{"FROM_CFG":"cfg-value"}}],
      "packagePresets":[
        {"name":"base","hidden":true,"configFile":"${fileDir}/$env{PACK_CONFIG}",
         "environment":{"PACK_CONFIG":"CPackConfig.cmake"}},
        {"name":"pkg","configurePreset":"cfg","inherits":"base",
         "environment":{"VERSION":"shadow"},
         "packageName":"$env{FROM_CFG}",
         "packageVersion":"$env{VERSION}-$penv{VERSION}",
         "packageDirectory":"${sourceDir}/dist/${presetName}",
         "vendorName":"$penv{RAW}",
         "variables":{"${presetName}":"$env{PACK_CONFIG}","CPACK_X":"${configurePresetName}",
           "CPACK_RAW":"$env{RAW}"}}
      ]}"#,
    )
    .expect("valid package presets");
    let env = EnvSnapshot {
        vars: BTreeMap::from([
            ("VERSION".into(), "1.2".into()),
            ("RAW".into(), "${sourceDir}/$env{VERSION}".into()),
        ]),
    };
    let package = package_settings(
        presets
            .resolve(PresetKind::Package, b"pkg", &env)
            .expect("package preset"),
    );
    assert_eq!(
        package.config_file,
        Some("/project/CPackConfig.cmake".into())
    );
    assert_eq!(package.package_name, Some("cfg-value".into()));
    assert_eq!(package.package_version, Some("shadow-1.2".into()));
    assert_eq!(package.package_directory, Some("/project/dist/pkg".into()));
    assert_eq!(
        package.vendor_name,
        Some("${sourceDir}/$env{VERSION}".into())
    );
    assert_eq!(
        package.variables,
        Some(BTreeMap::from([
            ("${presetName}".into(), "CPackConfig.cmake".into()),
            ("CPACK_RAW".into(), "${sourceDir}/$env{VERSION}".into()),
            ("CPACK_X".into(), "cfg".into()),
        ]))
    );

    let invalid = root(
        r#"{"version":13,"configurePresets":[{"name":"cfg"}],
        "packagePresets":[{"name":"pkg","configurePreset":"cfg","packageName":"${notDefined}"}]}"#,
    )
    .expect("schema accepts a string");
    let error = invalid
        .resolve(PresetKind::Package, b"pkg", &EnvSnapshot::default())
        .expect_err("invalid package macro");
    assert!(error.message.contains("Invalid macro expansion"));
}

#[test]
fn generator_macro_looks_up_presets_by_name() {
    let presets = root(
        r#"{
      "version":13,
      "configurePresets":[
        {"name":"cfg","generator":"Ninja","binaryDir":"out"},
        {"name":"other","generator":"Unix Makefiles","binaryDir":"other"},
        {"name":"same","generator":"Xcode","binaryDir":"same"}
      ],
      "buildPresets":[
        {"name":"same","configurePreset":"other","targets":["${generator}"]},
        {"name":"base","hidden":true,"configurePreset":"cfg"}
      ],
      "testPresets":[{"name":"same","configurePreset":"cfg",
        "output":{"outputJUnitFile":"${generator}.xml"}}],
      "packagePresets":[
        {"name":"cfg","configurePreset":"other","packageName":"package-${generator}"},
        {"name":"pkg","configurePreset":"cfg","packageName":"package-${generator}"},
        {"name":"child","inherits":"hidden-pkg","configurePreset":"cfg"},
        {"name":"hidden-pkg","hidden":true,"packageVersion":"v${generator}"}
      ]
    }"#,
    )
    .expect("valid presets");
    let package_name = |name: &str| package_settings(named(&presets, PresetKind::Package, name));
    assert_eq!(
        package_name("cfg").package_name,
        Some("package-Ninja".into())
    );
    assert_eq!(package_name("pkg").package_name, Some("package-".into()));
    assert_eq!(package_name("child").package_version, Some("v".into()));
    let PresetSettings::Build(build) = named(&presets, PresetKind::Build, "same").settings else {
        panic!("build settings")
    };
    assert_eq!(build.targets, Some(vec!["Unix Makefiles".into()]));
    let PresetSettings::Test(test) = named(&presets, PresetKind::Test, "same").settings else {
        panic!("test settings")
    };
    assert_eq!(
        test.output.and_then(|output| output.output_junit_file),
        Some("Unix Makefiles.xml".into())
    );
    let configure = named(&presets, PresetKind::Configure, "same");
    assert_eq!(configure.generator, Some("Xcode".into()));
}

#[test]
fn fixtures_exclusion_replaces_inherited_object() {
    let presets = root(
        r#"{
      "version":13,
      "configurePresets":[{"name":"cfg","binaryDir":"out"}],
      "testPresets":[
        {"name":"setup-base","hidden":true,
         "filter":{"exclude":{"name":"skip","fixtures":{"setup":"DB","any":"ANY"}}}},
        {"name":"cleanup-base","hidden":true,"filter":{"exclude":{"fixtures":{"cleanup":"OTHER"}}}},
        {"name":"cleanup","configurePreset":"cfg","inherits":"setup-base",
         "filter":{"include":{"name":"^main$"},"exclude":{"fixtures":{"cleanup":"DB"}}}},
        {"name":"empty","configurePreset":"cfg","inherits":"setup-base",
         "filter":{"exclude":{"fixtures":{}}}},
        {"name":"inherit","configurePreset":"cfg","inherits":"setup-base",
         "filter":{"include":{"name":"^main$"}}},
        {"name":"first","configurePreset":"cfg","inherits":["cleanup-base","setup-base"]}
      ]}"#,
    )
    .expect("valid test presets");

    let cleanup = exclude_filter(&presets, "cleanup");
    assert_eq!(cleanup.name, Some("skip".into()));
    assert_eq!(
        cleanup.fixtures,
        Some(TestFixtures {
            any: None,
            setup: None,
            cleanup: Some("DB".into()),
        })
    );

    let empty = exclude_filter(&presets, "empty");
    assert_eq!(empty.name, Some("skip".into()));
    assert_eq!(empty.fixtures, Some(TestFixtures::default()));

    let inherit = exclude_filter(&presets, "inherit");
    assert_eq!(inherit.name, Some("skip".into()));
    assert_eq!(
        inherit.fixtures,
        Some(TestFixtures {
            any: Some("ANY".into()),
            setup: Some("DB".into()),
            cleanup: None,
        })
    );

    let first = exclude_filter(&presets, "first");
    assert_eq!(first.name, Some("skip".into()));
    assert_eq!(
        first.fixtures,
        Some(TestFixtures {
            any: None,
            setup: None,
            cleanup: Some("OTHER".into()),
        })
    );
}

#[test]
fn build_uses_disabled_configure_preset_binary_dir() {
    let presets = root(
        r#"{
      "version":13,
      "configurePresets":[
        {"name":"off","binaryDir":"${sourceDir}/out/$env{SUB}","environment":{"SUB":"tree"},
         "condition":false},
        {"name":"on","binaryDir":"on"},
        {"name":"hidden","hidden":true,"binaryDir":"hidden"},
        {"name":"vendor","binaryDir":"vendor",
         "condition":{"type":"equals","lhs":"$vendor{x}","rhs":"y"}},
        {"name":"bad-macro","binaryDir":"${notDefined}","condition":false}
      ],
      "buildPresets":[
        {"name":"b","configurePreset":"off"},
        {"name":"b-on","configurePreset":"on"},
        {"name":"b-hidden","configurePreset":"hidden"},
        {"name":"b-vendor","configurePreset":"vendor"},
        {"name":"b-bad","configurePreset":"bad-macro"},
        {"name":"b-self-off","configurePreset":"off","condition":false}
      ],
      "testPresets":[{"name":"t","configurePreset":"off"}],
      "packagePresets":[{"name":"p","configurePreset":"off"}]
    }"#,
    )
    .expect("valid presets");
    let env = EnvSnapshot::default();

    let build = presets
        .resolve(PresetKind::Build, b"b", &env)
        .expect("build preset with a disabled configure preset");
    assert_eq!(build.configure_preset, Some("off".into()));
    assert_eq!(build.binary_dir, Some("/project/out/tree".into()));
    for kind in [PresetKind::Test, PresetKind::Package] {
        let name: &[u8] = if kind == PresetKind::Test { b"t" } else { b"p" };
        assert_eq!(
            presets
                .resolve(kind, name, &env)
                .expect("dependent preset")
                .binary_dir,
            Some("/project/out/tree".into())
        );
    }

    assert_eq!(
        presets
            .resolve(PresetKind::Configure, b"off", &env)
            .expect_err("direct configure selection")
            .message,
        "Cannot use disabled configure preset in /project: \"off\""
    );
    assert_eq!(
        presets
            .resolve_ignoring_condition(PresetKind::Configure, b"off", &env)
            .expect("expanded settings")
            .binary_dir,
        Some("/project/out/tree".into())
    );

    for (name, message) in [
        (
            "b-hidden",
            "Cannot use hidden configure preset in /project: \"hidden\"",
        ),
        (
            "b-vendor",
            "Could not evaluate configure preset \"vendor\": Invalid macro expansion",
        ),
        (
            "b-self-off",
            "Cannot use disabled build preset in /project: \"b-self-off\"",
        ),
    ] {
        assert_eq!(
            presets
                .resolve(PresetKind::Build, name.as_bytes(), &env)
                .expect_err("unusable build preset")
                .message,
            message
        );
    }
    assert!(
        presets
            .resolve(PresetKind::Build, b"b-bad", &env)
            .expect_err("invalid configure macro")
            .message
            .contains("Invalid macro expansion")
    );
    assert_eq!(
        names(&presets, PresetKind::Build, &env),
        [BString::from("b-on")]
    );
    for kind in [PresetKind::Test, PresetKind::Package] {
        assert!(names(&presets, kind, &env).is_empty());
    }
    assert_eq!(
        names(&presets, PresetKind::Configure, &env),
        [BString::from("on")]
    );

    for input in [
        r#"{"version":13,"buildPresets":[{"name":"b","configurePreset":"missing"}]}"#,
        r#"{"version":13,"configurePresets":[{"name":"c"}],"buildPresets":[{"name":"b","configurePreset":1}]}"#,
    ] {
        assert!(root(input).is_err());
    }
    let unreachable = from_files(
        &PureHost::default(),
        &[
            (
                "CMakePresets.json",
                r#"{"version":13,"buildPresets":[{"name":"b","configurePreset":"cfg"}]}"#,
            ),
            (
                "CMakeUserPresets.json",
                r#"{"version":13,"configurePresets":[{"name":"cfg","binaryDir":"out","condition":false}]}"#,
            ),
        ],
    )
    .expect_err("project presets cannot see user presets");
    assert!(unreachable.message.contains("is not reachable"));
    let user_layer = from_files(
        &PureHost::default(),
        &[
            (
                "CMakePresets.json",
                r#"{"version":13,"configurePresets":[{"name":"cfg","binaryDir":"out","condition":false}]}"#,
            ),
            (
                "CMakeUserPresets.json",
                r#"{"version":13,"buildPresets":[{"name":"b","configurePreset":"cfg"}]}"#,
            ),
        ],
    )
    .expect("user presets reach project presets");
    assert_eq!(
        user_layer
            .resolve(PresetKind::Build, b"b", &env)
            .expect("reachable disabled configure preset")
            .binary_dir,
        Some("/project/out".into())
    );
}

#[test]
fn configure_condition_is_evaluated_before_settings() {
    let host = PureHost::default();
    let project = Path::new("/project");
    let file = project.join("CMakePresets.json");
    host.create_dir_all(project).expect("create project");
    host.write(
        &file,
        br#"{"version":13,"configurePresets":[
          {"name":"good"},
          {"name":"vendor","binaryDir":"${unknownAuditMacro}",
           "condition":{"type":"equals","lhs":"$vendor{x}","rhs":"test"}},
          {"name":"env-vendor","binaryDir":"${unknownAuditMacro}","environment":{"X":"$vendor{y}"}}
        ]}"#,
        WriteMode::Truncate,
    )
    .expect("write vendor condition");
    let presets = load(project, &host).expect("vendor condition skips settings");
    let env = EnvSnapshot::default();
    assert_eq!(
        names(&presets, PresetKind::Configure, &env),
        [BString::from("good")]
    );
    for name in ["vendor", "env-vendor"] {
        assert_eq!(
            presets
                .resolve(PresetKind::Configure, name.as_bytes(), &env)
                .expect_err("unsupported macro")
                .message,
            format!("Could not evaluate configure preset \"{name}\": Invalid macro expansion")
        );
    }

    host.write(
        &file,
        br#"{"version":13,"configurePresets":[
          {"name":"good"},
          {"name":"off","binaryDir":"${unknownAuditMacro}","condition":false}
        ]}"#,
        WriteMode::Truncate,
    )
    .expect("write false condition");
    assert_eq!(
        load(project, &host)
            .expect_err("false condition still expands settings")
            .message,
        "Could not read presets from /project:\nInvalid macro expansion in \"off\""
    );

    host.write(
        &file,
        br#"{"version":13,"configurePresets":[
          {"name":"good"},
          {"name":"bad","condition":{"type":"equals","lhs":"${unknownAuditMacro}","rhs":"x"}}
        ]}"#,
        WriteMode::Truncate,
    )
    .expect("write invalid condition");
    let error = load(project, &host).expect_err("invalid condition macro");
    assert!(error.message.contains("Invalid condition"));
    assert!(error.message.contains("Invalid macro expansion in \"bad\""));
}
