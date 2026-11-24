#![allow(clippy::expect_used, clippy::panic)]

use std::path::Path;

use rcmake_host::{PureHost, WriteMode};
use rcmake_json::ReadOptions;

use super::*;

fn from_json(input: &str) -> Value {
    rcmake_json::parse(input.as_bytes(), &ReadOptions::default()).expect("valid test JSON")
}

fn from_files(host: &PureHost, files: &[(&str, &str)]) -> Result<Presets, PresetsError> {
    let docs = files
        .iter()
        .map(|(name, input)| (PathBuf::from(name), from_json(input)))
        .collect();
    load_from_values(Path::new("/project"), host, &docs)
}

fn root(input: &str) -> Result<Presets, PresetsError> {
    from_files(&PureHost::default(), &[("CMakePresets.json", input)])
}

fn names(presets: &Presets, kind: PresetKind, env: &EnvSnapshot) -> Vec<BString> {
    presets
        .list_visible(kind, env)
        .into_iter()
        .map(|preset| preset.name)
        .collect()
}

fn named(presets: &Presets, kind: PresetKind, name: &str) -> ResolvedPreset {
    resolve(presets, kind, name.as_bytes(), &EnvSnapshot::default()).expect("visible preset")
}

#[test]
fn schema_versions_and_root_metadata() {
    for version in 1..=13 {
        let presets = root(&format!(
            r#"{{"version":{version},"cmakeMinimumRequired":{{"major":4,"minor":5,"patch":1}},"configurePresets":[{{"name":"default","generator":"Ninja","binaryDir":"build"}}]}}"#
        )).expect("supported schema");
        assert_eq!(presets.version, version);
        assert_eq!(
            presets
                .files
                .get(Path::new("/project/CMakePresets.json"))
                .and_then(|file| file.cmake_minimum_required),
            Some(CmakeVersion {
                major: 4,
                minor: 5,
                patch: 1
            })
        );
        assert_eq!(
            named(&presets, PresetKind::Configure, "default").binary_dir,
            Some("/project/build".into())
        );
    }
    for version in ["-1", "0", "14", "100", "\"13\"", "null"] {
        assert!(root(&format!(r#"{{"version":{version}}}"#)).is_err());
    }
    assert_eq!(
        root("{}").expect_err("missing version").message,
        "No \"version\" field"
    );
    assert!(
        root(r#"{"version":13,"cmakeMinimumRequired":{"major":4,"minor":5,"patch":2}}"#).is_err()
    );
    assert!(root(r#"{"version":13,"unexpected":true}"#).is_err());
    assert!(
        root(r#"{"version":2,"configurePresets":[{"name":"missing-generator"}]}"#)
            .expect_err("version 2 requires a generator")
            .message
            .contains("missing field \"generator\"")
    );
    let presets = root(
        r#"{"version":13,"$schema":"schema.json","$comment":["root"],
         "vendor":{"example.org/tool/1.0":{"feature":true}},
         "configurePresets":[{"name":"cfg","vendor":{"example.org/tool/1.0":true}}]}"#,
    )
    .expect("root metadata");
    let file = presets
        .files
        .get(Path::new("/project/CMakePresets.json"))
        .expect("file metadata");
    assert_eq!(file.schema_uri, Some("schema.json".into()));
    assert!(file.vendor.contains_key(b"example.org/tool/1.0".as_slice()));
    assert!(
        presets
            .preset_vendor(PresetKind::Configure, b"cfg")
            .is_some()
    );
}

#[test]
fn schema_feature_gates() {
    let cases = [
        (2, r#""buildPresets":[]"#),
        (2, r#""testPresets":[]"#),
        (
            3,
            r#""configurePresets":[{"name":"cfg","hidden":true,"condition":true}]"#,
        ),
        (
            3,
            r#""configurePresets":[{"name":"cfg","hidden":true,"installDir":"install","toolchainFile":"t.cmake"}]"#,
        ),
        (4, r#""include":[]"#),
        (
            4,
            r#""buildPresets":[{"name":"b","hidden":true,"resolvePackageReferences":"only"}]"#,
        ),
        (
            5,
            r#""testPresets":[{"name":"t","hidden":true,"output":{"testOutputTruncation":"head"}}]"#,
        ),
        (
            6,
            r#""testPresets":[{"name":"t","hidden":true,"output":{"outputJUnitFile":"report.xml"}}]"#,
        ),
        (6, r#""packagePresets":[]"#),
        (6, r#""workflowPresets":[]"#),
        (
            7,
            r#""configurePresets":[{"name":"cfg","hidden":true,"trace":{"mode":"on"}}]"#,
        ),
        (8, r#""$schema":"schema.json""#),
        (
            10,
            r#""$comment":"root","configurePresets":[{"name":"cfg","graphviz":"deps.dot","$comment":"preset"}]"#,
        ),
        (
            11,
            r#""testPresets":[{"name":"t","hidden":true,"execution":{"jobs":""}}]"#,
        ),
        (
            12,
            r#""configurePresets":[{"name":"cfg","warnings":{"author":true,"installAbsoluteDestination":true},"errors":{"unusedCli":true,"uninitialized":true}}]"#,
        ),
        (
            12,
            r#""testPresets":[{"name":"t","hidden":true,"execution":{"testPassthroughArguments":["-v"]}}]"#,
        ),
        (
            13,
            r#""configurePresets":[{"name":"cfg","warnings":{"strict":true,"nonTargetDirective":true},"errors":{"strict":true,"nonTargetDirective":true}}]"#,
        ),
    ];
    for (version, fields) in cases {
        let document = |version| format!(r#"{{"version":{version},{fields}}}"#);
        assert!(
            root(&document(version - 1)).is_err(),
            "{fields} before v{version}"
        );
        assert!(root(&document(version)).is_ok(), "{fields} in v{version}");
    }
    assert!(
        root(r#"{"version":12,"configurePresets":[{"name":"cfg","warnings":{"dev":false}}]}"#)
            .is_err()
    );
    for jobs in ["-1", r#""x""#, "null"] {
        let build = format!(
            r#"{{"version":13,"buildPresets":[{{"name":"b","hidden":true,"jobs":{jobs}}}]}}"#
        );
        let test = format!(
            r#"{{"version":13,"testPresets":[{{"name":"t","hidden":true,"execution":{{"jobs":{jobs}}}}}]}}"#
        );
        assert!(root(&build).is_err());
        assert!(root(&test).is_err());
    }
}

#[test]
fn include_order_reachability_and_user_layer() {
    let presets = from_files(
        &PureHost::default(),
        &[
            (
                "CMakePresets.json",
                r#"{"version":4,"include":["sub/a.json","sub/../common.json"],
             "configurePresets":[{"name":"project","inherits":"common","generator":"Ninja"}]}"#,
            ),
            (
                "CMakeUserPresets.json",
                r#"{"version":13,"include":["local.json"],
             "configurePresets":[{"name":"user","inherits":"project"}]}"#,
            ),
            (
                "local.json",
                r#"{"version":3,"configurePresets":[{"name":"local"}]}"#,
            ),
            (
                "sub/a.json",
                r#"{"version":4,"include":["../common.json"],"configurePresets":[{"name":"a"}]}"#,
            ),
            (
                "common.json",
                r#"{"version":3,"configurePresets":[{"name":"common","binaryDir":"out"}]}"#,
            ),
        ],
    )
    .expect("combined file graph");
    assert_eq!(presets.files.len(), 5);
    assert_eq!(
        names(&presets, PresetKind::Configure, &EnvSnapshot::default()),
        ["user", "local", "project", "a", "common"].map(BString::from)
    );
    assert_eq!(
        named(&presets, PresetKind::Configure, "user").binary_dir,
        Some("/project/out".into())
    );

    let error = from_files(
        &PureHost::default(),
        &[
            (
                "CMakePresets.json",
                r#"{"version":3,"configurePresets":[{"name":"a","inherits":"b"}]}"#,
            ),
            (
                "CMakeUserPresets.json",
                r#"{"version":3,"configurePresets":[{"name":"b"}]}"#,
            ),
        ],
    )
    .expect_err("project cannot see user presets");
    assert_eq!(
        error.message,
        r#"Configure preset "a" inherits preset "b", but "b" is not reachable from the preset's file"#
    );
}

#[test]
fn graph_errors_report_cycles_duplicates_and_missing_files() {
    let error = from_files(
        &PureHost::default(),
        &[
            (
                "CMakePresets.json",
                r#"{"version":4,"include":["sub/a.json"]}"#,
            ),
            (
                "sub/a.json",
                r#"{"version":4,"include":["../CMakePresets.json"]}"#,
            ),
        ],
    )
    .expect_err("include cycle");
    assert_eq!(
        error.message,
        "Cyclic include among presets files: /project/CMakePresets.json"
    );
    assert_eq!(
        root(r#"{"version":4,"include":["missing.json"]}"#)
            .expect_err("missing include")
            .message,
        "File not found: /project/missing.json"
    );
    assert_eq!(
        root(r#"{"version":13,"configurePresets":[{"name":"a"},{"name":"a"}]}"#)
            .expect_err("duplicate")
            .message,
        r#"Duplicate preset: "a""#
    );
    assert_eq!(
        root(r#"{"version":13,"configurePresets":[{"name":"a","inherits":"b"}]}"#)
            .expect_err("missing parent")
            .message,
        r#"Invalid configure preset: "a": Could not find inherited preset "b""#
    );
    assert_eq!(root(r#"{"version":13,"configurePresets":[{"name":"a","inherits":"b"},{"name":"b","inherits":"a"}]}"#).expect_err("inheritance cycle").message,
        r#"Cyclic preset inheritance for configure preset "a""#);
}

#[test]
fn include_macros_and_file_dir_provenance() {
    let host = PureHost::default();
    host.env_set(b"INCLUDE", Some(b"sub/base.json"));
    let base = |version| {
        format!(
            r#"{{"version":{version},"configurePresets":[{{"name":"base","hidden":true,"binaryDir":"${{fileDir}}/${{presetName}}" }}]}}"#
        )
    };
    for (version, include) in [
        (7, "$penv{INCLUDE}"),
        (8, "$penv{INCLUDE}"),
        (9, "${fileDir}/sub/base.json"),
    ] {
        let project = format!(
            r#"{{"version":{version},"include":["{include}"],"configurePresets":[{{"name":"derived","inherits":"base"}}]}}"#
        );
        let presets = from_files(
            &host,
            &[
                ("CMakePresets.json", &project),
                ("sub/base.json", &base(11)),
            ],
        )
        .expect("include macro");
        assert_eq!(
            named(&presets, PresetKind::Configure, "derived").binary_dir,
            Some("/project/derived".into())
        );
    }
    let presets = from_files(&host, &[
        ("CMakePresets.json", r#"{"version":13,"include":["${sourceDir}/sub/base.json"],"configurePresets":[{"name":"derived","inherits":"base"}]}"#),
        ("sub/base.json", &base(12)),
    ]).expect("inherited v12 fileDir");
    assert_eq!(
        named(&presets, PresetKind::Configure, "derived").binary_dir,
        Some("/project/sub/derived".into())
    );
    for (version, include) in [
        (6, "$penv{INCLUDE}"),
        (7, "${fileDir}/sub/base.json"),
        (9, "$env{INCLUDE}"),
        (13, "${presetName}"),
    ] {
        let project = format!(r#"{{"version":{version},"include":["{include}"]}}"#);
        assert!(
            from_files(
                &host,
                &[
                    ("CMakePresets.json", &project),
                    ("sub/base.json", &base(13))
                ]
            )
            .is_err()
        );
    }
}

#[test]
fn multiple_inheritance_and_null_overrides() {
    let presets = root(r#"{
      "version":13,"configurePresets":[
        {"name":"first","hidden":true,"displayName":"template","generator":"Ninja",
          "environment":{"KEEP":"first","REMOVE":"unused","PARENT":"parent"},
          "cacheVariables":{"KEEP":"first","REMOVE":"unused","DISABLE":true},
          "warnings":{"author":true,"deprecated":true}},
        {"name":"second","hidden":true,"generator":"Unix Makefiles",
          "environment":{"KEEP":"second","FALLBACK":"second"},
          "cacheVariables":{"KEEP":"second","OTHER":false},"warnings":{"strict":true}},
        {"name":"child","inherits":["first","second"],"environment":{"REMOVE":null,"PARENT":"child"},
          "cacheVariables":{"REMOVE":null,"DISABLE":{"type":"STRING","value":false}},
          "warnings":{"deprecated":false}}]}"#).expect("valid inheritance");
    let preset = named(&presets, PresetKind::Configure, "child");
    assert_eq!(preset.generator, Some("Ninja".into()));
    assert_eq!(preset.display_name, None);
    assert_eq!(preset.environment.get(b"REMOVE".as_slice()), Some(&None));
    assert_eq!(
        preset.environment.get(b"KEEP".as_slice()),
        Some(&Some("first".into()))
    );
    assert_eq!(
        preset.environment.get(b"FALLBACK".as_slice()),
        Some(&Some("second".into()))
    );
    assert_eq!(
        preset.environment.get(b"PARENT".as_slice()),
        Some(&Some("child".into()))
    );
    let PresetSettings::Configure(settings) = preset.settings else {
        panic!("configure settings")
    };
    let cache = settings.cache_variables.expect("cache variables");
    assert!(!cache.contains_key(b"REMOVE".as_slice()));
    assert_eq!(
        cache
            .get(b"KEEP".as_slice())
            .map(|variable| &variable.value),
        Some(&"first".into())
    );
    assert_eq!(
        cache.get(b"DISABLE".as_slice()),
        Some(&CacheVariable {
            kind: Some("STRING".into()),
            value: "FALSE".into()
        })
    );
    assert_eq!(
        cache.get(b"OTHER".as_slice()),
        Some(&CacheVariable {
            kind: Some("BOOL".into()),
            value: "FALSE".into()
        })
    );
    assert_eq!(
        settings.warnings,
        Some(ConfigureWarnings {
            author: Some(true),
            strict: Some(true),
            deprecated: Some(false),
            ..ConfigureWarnings::default()
        })
    );
    assert_eq!(
        names(&presets, PresetKind::Configure, &EnvSnapshot::default()),
        [BString::from("child")]
    );
    assert!(
        presets
            .resolve(PresetKind::Configure, b"first", &EnvSnapshot::default())
            .is_err()
    );
}

#[test]
fn conditions_support_all_operators_and_short_circuit() {
    let presets = root(
        r#"{
        "version":13,"configurePresets":[
          {"name":"all","condition":{"type":"allOf","conditions":[
            {"type":"const","value":true},
            {"type":"equals","lhs":"${presetName}","rhs":"all"},
            {"type":"notEquals","lhs":"left","rhs":"right"},
            {"type":"inList","string":"x","list":["y","x","${invalid}"]},
            {"type":"notInList","string":"x","list":["y","z"]},
            {"type":"matches","string":"release-123","regex":"^release-[0-9]+$"},
            {"type":"notMatches","string":"release-123","regex":"^debug"},
            {"type":"not","condition":false},
            {"type":"anyOf","conditions":[false,true,{"type":"equals","lhs":"${invalid}","rhs":""}]}
          ]}},
          {"name":"never","condition":false},
          {"name":"inherited","inherits":"never","condition":true},
          {"name":"always","hidden":true,"condition":true},
          {"name":"second-parent","inherits":["always","never"]},
          {"name":"parent-false","inherits":["never","always"]},
          {"name":"null-override","inherits":"never","condition":null},
          {"name":"null","condition":null},
          {"name":"null-parent","inherits":["null","never"]},
          {"name":"any-empty","condition":{"type":"anyOf","conditions":[]}},
          {"name":"all-empty","condition":{"type":"allOf","conditions":[]}}
        ]}"#,
    )
    .expect("valid conditions");
    assert_eq!(
        names(&presets, PresetKind::Configure, &EnvSnapshot::default()),
        [
            "all",
            "inherited",
            "second-parent",
            "null-override",
            "null",
            "all-empty"
        ]
        .map(BString::from)
    );
    assert!(root(r#"{"version":13,"configurePresets":[{"name":"bad","condition":{"type":"not","condition":null}}]}"#).is_err());
}

#[test]
fn expansion_uses_parent_environment_and_detects_cycles() {
    let presets = root(r#"{
      "version":13,"configurePresets":[
        {"name":"cfg","generator":"Ninja","binaryDir":"build/${configurePresetName}",
         "installDir":"install/${hostSystemName}","toolchainFile":"${sourceParentDir}/tool.cmake",
         "graphviz":"${sourceDirName}.dot",
         "environment":{"PATH":"bin${pathListSep}$penv{PATH}","PASS":"$env{LATE}","LATE":"${generator}",
           "FALLBACK":null,"LOOKUP":"$env{FALLBACK}","DOLLAR":"${dollar}{stay}","LITERAL":"$unknown{key}"}},
        {"name":"bad","environment":{"A":"$env{B}","B":"$env{A}"}},
        {"name":"self","environment":{"A":"$env{A}"}},
        {"name":"vendor","binaryDir":"$vendor{ext.value}"},
        {"name":"unknown","binaryDir":"${notDefined}"},
        {"name":"unclosed","binaryDir":"${sourceDir"}
      ]}"#).expect("valid schema");
    let env = EnvSnapshot {
        vars: BTreeMap::from([
            ("PATH".into(), "system".into()),
            ("FALLBACK".into(), "process".into()),
        ]),
    };
    let preset = presets
        .resolve(PresetKind::Configure, b"cfg", &env)
        .expect("macros expanded");
    assert_eq!(preset.binary_dir, Some("/project/build/cfg".into()));
    let PresetSettings::Configure(settings) = preset.settings else {
        panic!("configure settings")
    };
    assert_eq!(settings.install_dir, Some("/project/install/PureOS".into()));
    assert_eq!(settings.toolchain_file, Some("//tool.cmake".into()));
    assert_eq!(settings.graphviz, Some("project.dot".into()));
    for (key, expected) in [
        (
            "PATH",
            if cfg!(windows) {
                "bin;system"
            } else {
                "bin:system"
            },
        ),
        ("PASS", "Ninja"),
        ("DOLLAR", "${stay}"),
        ("LITERAL", "$unknown{key}"),
        ("LOOKUP", "process"),
    ] {
        assert_eq!(
            preset.environment.get(key.as_bytes()),
            Some(&Some(expected.into()))
        );
    }
    for name in ["bad", "self", "unknown", "unclosed"] {
        let error = presets
            .resolve(PresetKind::Configure, name.as_bytes(), &env)
            .expect_err("invalid expansion");
        assert!(error.message.contains("Invalid macro expansion"));
    }
    assert_eq!(
        names(&presets, PresetKind::Configure, &env),
        [BString::from("cfg")]
    );
    let old = root(r#"{"version":12,"configurePresets":[{"name":"cfg","binaryDir":"${configurePresetName}"}]}"#)
        .expect("schema accepts a string");
    assert!(old.resolve(PresetKind::Configure, b"cfg", &env).is_err());
}

#[test]
fn resolves_build_test_package_and_workflow_settings() {
    let presets = root(r#"{
      "version":13,
      "configurePresets":[{"name":"cfg","generator":"Ninja","binaryDir":"out","environment":{"FROM_CFG":"${presetName}","TOOL":"cc"}}],
      "buildPresets":[{"name":"build","configurePreset":"cfg","jobs":0,"targets":["${configurePresetName}","${generator}"],
        "nativeToolOptions":["$env{TOOL}"],"resolvePackageReferences":"only","cleanFirst":true}],
      "testPresets":[{"name":"test","configurePreset":"cfg","inheritConfigureEnvironment":false,
        "output":{"outputJUnitFile":"${configurePresetName}.xml","verbosity":"extra","testOutputTruncation":"middle"},
        "filter":{"include":{"index":{"start":1,"specificTests":[3,5]}},"exclude":{"fixtures":{"any":"${presetName}"}}},
        "execution":{"jobs":"","resourceSpecFile":"${fileDir}/resources.json","showOnly":"json-v1-raw",
          "repeat":{"mode":"until-pass","count":3},"testPassthroughArguments":["-v"]}}],
      "packagePresets":[{"name":"package","configurePreset":"cfg","generators":["TGZ"],"configurations":["Release"],
        "variables":{"CPACK_VERBATIM_VARIABLES":"ON"},"configFile":"CPackConfig.cmake","packageName":"example",
        "packageVersion":"1","packageDirectory":"dist","vendorName":"example.org","output":{"verbose":true},
        "environment":{"CONFIGURE":"${configurePresetName}"}}],
      "workflowPresets":[{"name":"all","steps":[{"type":"configure","name":"cfg"},{"type":"build","name":"build"},
        {"type":"test","name":"test"},{"type":"package","name":"package"}]}]
    }"#).expect("valid presets");
    let env = EnvSnapshot::default();
    let build = presets
        .resolve(PresetKind::Build, b"build", &env)
        .expect("build preset");
    assert_eq!(build.configure_preset, Some("cfg".into()));
    assert_eq!(build.binary_dir, Some("/project/out".into()));
    assert_eq!(
        build.environment.get(b"FROM_CFG".as_slice()),
        Some(&Some("build".into()))
    );
    let PresetSettings::Build(build) = build.settings else {
        panic!("build settings")
    };
    assert_eq!(build.targets, Some(vec!["cfg".into(), "Ninja".into()]));
    assert_eq!(build.native_tool_options, Some(vec!["cc".into()]));
    assert_eq!(
        build.resolve_package_references,
        Some(PackageReferences::Only)
    );

    let test = named(&presets, PresetKind::Test, "test");
    assert!(test.environment.is_empty());
    let PresetSettings::Test(test) = test.settings else {
        panic!("test settings")
    };
    assert_eq!(
        test.output.and_then(|output| output.output_junit_file),
        Some("cfg.xml".into())
    );
    let execution = test.execution.expect("test execution");
    assert_eq!(execution.jobs, Some(TestJobs::Automatic));
    assert_eq!(
        execution.resource_spec_file,
        Some("/project/resources.json".into())
    );
    assert_eq!(
        execution.repeat,
        Some(TestRepeat {
            mode: RepeatMode::UntilPass,
            count: 3
        })
    );
    assert_eq!(
        execution.test_passthrough_arguments,
        Some(vec!["-v".into()])
    );

    let package = named(&presets, PresetKind::Package, "package");
    assert_eq!(
        package.environment.get(b"CONFIGURE".as_slice()),
        Some(&Some("cfg".into()))
    );
    let PresetSettings::Package(package) = package.settings else {
        panic!("package settings")
    };
    assert_eq!(package.generators, Some(vec!["TGZ".into()]));
    assert_eq!(package.output.and_then(|output| output.verbose), Some(true));
    assert_eq!(package.package_directory, Some("dist".into()));

    let workflow = presets
        .resolve(PresetKind::Workflow, b"all", &env)
        .expect("workflow");
    let PresetSettings::Workflow(workflow) = workflow.settings else {
        panic!("workflow settings")
    };
    assert_eq!(workflow.steps.len(), 4);
    assert_eq!(
        workflow.steps.last().map(|step| step.kind),
        Some(PresetKind::Package)
    );
}

#[test]
fn invalid_references_and_workflow_steps_are_rejected() {
    for (input, message) in [
        (
            r#"{"version":13,"buildPresets":[{"name":"b","configurePreset":"missing"}]}"#,
            "references unknown configure preset",
        ),
        (
            r#"{"version":13,"configurePresets":[{"name":"b"}],"buildPresets":[{"name":"b","configurePreset":"${presetName}"}]}"#,
            r#"references unknown configure preset "${presetName}""#,
        ),
        (
            r#"{"version":13,"testPresets":[{"name":"t"}]}"#,
            "must either be hidden",
        ),
        (
            r#"{"version":13,"workflowPresets":[{"name":"w","steps":[]}]}"#,
            "No workflow steps",
        ),
        (
            r#"{"version":13,"workflowPresets":[{"name":"w","steps":[{"type":"configure","name":"missing"}]}]}"#,
            "Invalid workflow step",
        ),
        (
            r#"{"version":13,"configurePresets":[{"name":"a"},{"name":"b"}],"buildPresets":[{"name":"b","configurePreset":"b"}],
            "workflowPresets":[{"name":"w","steps":[{"type":"configure","name":"a"},{"type":"build","name":"b"}]}]}"#,
            "uses configure preset \"b\"",
        ),
    ] {
        assert!(
            root(input)
                .expect_err("invalid reference")
                .message
                .contains(message)
        );
    }
}

#[test]
fn load_reads_project_user_and_includes_through_host() {
    let host = PureHost::default();
    let project = Path::new("/project");
    host.create_dir_all(project).expect("create project");
    for (name, content) in [
        (
            "CMakePresets.json",
            r#"{"version":13,"include":["ci.json"],"configurePresets":[{"name":"common","hidden":true,"binaryDir":"out"}]}"#,
        ),
        (
            "CMakeUserPresets.json",
            r#"{"version":3,"configurePresets":[{"name":"local","inherits":"common"}]}"#,
        ),
        (
            "ci.json",
            r#"{"version":13,"configurePresets":[{"name":"ci"}]}"#,
        ),
    ] {
        host.write(&project.join(name), content.as_bytes(), WriteMode::Truncate)
            .expect("write fixture");
    }
    let presets = load(project, &host).expect("load through host");
    assert_eq!(
        names(&presets, PresetKind::Configure, &EnvSnapshot::default()),
        ["local", "ci"].map(BString::from)
    );
    host.write(
        &project.join("ci.json"),
        b"{/* invalid comment */\"version\":13}",
        WriteMode::Truncate,
    )
    .expect("write broken JSON");
    assert!(
        load(project, &host)
            .expect_err("invalid JSON")
            .message
            .contains("ci.json")
    );
}

#[test]
fn parsed_values_do_not_require_json_text() {
    let host = PureHost::default();
    let preset = Value::Object(BTreeMap::from([("name".into(), Value::string("cfg"))]));
    let root = Value::Object(BTreeMap::from([
        ("version".into(), Value::Int(13)),
        ("configurePresets".into(), Value::Array(vec![preset])),
    ]));
    let presets = load_from_values(
        Path::new("/project"),
        &host,
        &BTreeMap::from([(PathBuf::from("CMakePresets.json"), root)]),
    )
    .expect("load a parsed tree");
    assert_eq!(
        names(&presets, PresetKind::Configure, &EnvSnapshot::default()),
        [BString::from("cfg")]
    );
}

#[test]
fn include_symlinks_share_file_identity_and_retain_the_first_path() {
    let host = PureHost::default();
    let project = Path::new("/project");
    host.create_dir_all(&project.join("sub"))
        .expect("create project");
    for (name, text) in [
        (
            "CMakePresets.json",
            r#"{"version":13,"include":["alias.json","sub/base.json"]}"#,
        ),
        (
            "sub/base.json",
            r#"{"version":13,"include":["leaf.json"],"configurePresets":[{"name":"base","binaryDir":"${fileDir}/out"}]}"#,
        ),
        (
            "leaf.json",
            r#"{"version":13,"configurePresets":[{"name":"leaf"}]}"#,
        ),
    ] {
        host.write(&project.join(name), text.as_bytes(), WriteMode::Truncate)
            .expect("write file");
    }
    host.symlink(Path::new("sub/base.json"), &project.join("alias.json"))
        .expect("create alias");
    let presets = load(project, &host).expect("load one copy of base");
    assert_eq!(
        names(&presets, PresetKind::Configure, &EnvSnapshot::default()),
        ["base", "leaf"].map(BString::from)
    );
    assert_eq!(
        named(&presets, PresetKind::Configure, "base").binary_dir,
        Some("/project/out".into())
    );

    host.write(
        &project.join("leaf.json"),
        br#"{"version":13,"include":["sub/base.json"]}"#,
        WriteMode::Truncate,
    )
    .expect("create include cycle");
    assert_eq!(
        load(project, &host)
            .expect_err("cycle through alias")
            .message,
        "Could not read presets from /project:\nCyclic include among presets files: /project/sub/base.json"
    );
}

#[test]
fn reports_json_value_locations_in_included_files() {
    let host = PureHost::default();
    let project = Path::new("/project");
    host.create_dir_all(project).expect("create project");
    host.write(
        &project.join("CMakePresets.json"),
        br#"{"version":13,"include":["base.json"]}"#,
        WriteMode::Truncate,
    )
    .expect("write root");
    host.write(
        &project.join("base.json"),
        br#"{
  "version": 13,
  "configurePresets": [
    {"name": "p", "b\u0069naryDir": []}
  ]
}"#,
        WriteMode::Truncate,
    )
    .expect("write include");
    assert_eq!(
        load(project, &host).expect_err("invalid directory").message,
        concat!(
            "Could not read presets from /project:\n",
            "base.json:4: \"binaryDir\" expected a string\n",
            "    {\"name\": \"p\", \"b\\u0069naryDir\": []}\n",
            "                                    ^",
        )
    );
}

#[test]
fn validates_hidden_macros_and_skips_vendor_macros_during_load() {
    let host = PureHost::default();
    let project = Path::new("/project");
    let file = project.join("CMakePresets.json");
    host.create_dir_all(project).expect("create project");
    host.write(
        &file,
        br#"{"version":13,"configurePresets":[{"name":"good"},{"name":"hidden","hidden":true,"binaryDir":"${unknown}"}]}"#,
        WriteMode::Truncate,
    )
    .expect("write invalid macro");
    assert_eq!(
        load(project, &host).expect_err("invalid macro").message,
        "Could not read presets from /project:\nInvalid macro expansion in \"hidden\""
    );
    host.write(
        &file,
        br#"{"version":13,"configurePresets":[{"name":"good"},{"name":"vendor","binaryDir":"$vendor{ext.value}"}]}"#,
        WriteMode::Truncate,
    )
    .expect("write vendor macro");
    let presets = load(project, &host).expect("load vendor macro");
    assert_eq!(
        names(&presets, PresetKind::Configure, &EnvSnapshot::default()),
        [BString::from("good")]
    );
}

#[test]
fn explicit_file_uses_the_working_directory_and_its_own_includes() {
    let host = PureHost::default();
    let project = Path::new("/project/source");
    for (path, contents) in [
        ("/project/source/CMakePresets.json", "invalid"),
        ("/project/source/CMakeUserPresets.json", "invalid"),
        (
            "/work/ci/custom.json",
            r#"{"version":13,"include":["base.json"],"configurePresets":[{"name":"custom","inherits":"base"}]}"#,
        ),
        (
            "/work/ci/base.json",
            r#"{"version":12,"configurePresets":[{"name":"base","hidden":true,"binaryDir":"${sourceDir}/build","cacheVariables":{"META":"${fileDir}"}}]}"#,
        ),
    ] {
        let file = Path::new(path);
        host.create_dir_all(file.parent().expect("parent directory"))
            .expect("create directory");
        host.write(file, contents.as_bytes(), WriteMode::Truncate)
            .expect("write file");
    }
    let presets =
        load_file(project, Path::new("ci/custom.json"), &host).expect("load explicit file");
    assert_eq!(presets.version, 13);
    let preset = named(&presets, PresetKind::Configure, "custom");
    assert_eq!(preset.binary_dir, Some("/project/source/build".into()));
    let PresetSettings::Configure(settings) = preset.settings else {
        panic!("configure settings")
    };
    assert_eq!(
        settings.cache_variables.expect("cache variables")[b"META".as_slice()].value,
        BString::from("/work/ci")
    );
}
