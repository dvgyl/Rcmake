use super::*;

const VS17: GeneratorKind = GeneratorKind::VisualStudio(VsVersion::Vs17);
const VS18: GeneratorKind = GeneratorKind::VisualStudio(VsVersion::Vs18);

fn xcode_27_2() -> XcodeVersion {
    XcodeVersion::parse(b"27.2")
}

fn message(result: Result<GeneratorSpec, GeneratorSpecError>) -> String {
    result.err().map(|err| err.to_string()).unwrap_or_default()
}

#[test]
fn names_round_trip() {
    for kind in GeneratorKind::ALL {
        assert_eq!(GeneratorKind::from_name(kind.name().as_bytes()), Some(kind));
    }
    assert_eq!(GeneratorKind::from_name(b"ninja"), None);
    assert_eq!(GeneratorKind::from_name(b"Visual Studio 16 2019"), None);
    assert_eq!(GeneratorKind::from_name(b"xcode"), None);
}

#[test]
fn method_table_matches_design() {
    use GeneratorKind::{Ninja, NinjaMultiConfig, UnixMakefiles, Xcode};
    let multi: Vec<bool> = GeneratorKind::ALL.map(GeneratorKind::multi_config).into();
    assert_eq!(multi, [false, false, true, true, true, true]);
    let ide: Vec<bool> = GeneratorKind::ALL.map(GeneratorKind::is_ide).into();
    assert_eq!(ide, [false, false, false, true, true, true]);
    assert_eq!(UnixMakefiles.find_make_module(), Some("CMakeUnixFindMake"));
    assert_eq!(Ninja.find_make_module(), Some("CMakeNinjaFindMake"));
    assert_eq!(
        NinjaMultiConfig.find_make_module(),
        Some("CMakeNinjaFindMake")
    );
    assert_eq!(Xcode.find_make_module(), Some("CMakeFindXCode"));
    assert_eq!(VS17.find_make_module(), None);
    assert!(Ninja.default_configuration_types().is_empty());
    assert!(UnixMakefiles.default_configuration_types().is_empty());
    assert_eq!(
        NinjaMultiConfig.default_configuration_types(),
        ["Debug", "Release", "RelWithDebInfo"]
    );
    for kind in [Xcode, VS17, VS18] {
        assert_eq!(
            kind.default_configuration_types(),
            ["Debug", "Release", "MinSizeRel", "RelWithDebInfo"]
        );
    }
    assert_eq!(Ninja.cfg_int_dir(), ".");
    assert_eq!(UnixMakefiles.cfg_int_dir(), ".");
    assert_eq!(NinjaMultiConfig.cfg_int_dir(), "${CONFIGURATION}");
    assert_eq!(
        Xcode.cfg_int_dir(),
        "$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)"
    );
    assert_eq!(VS18.cfg_int_dir(), "$(Configuration)");
    assert_eq!(Ninja.toolset_support(), ToolsetSupport::None);
    assert_eq!(Xcode.toolset_support(), ToolsetSupport::Xcode);
    assert_eq!(VS17.toolset_support(), ToolsetSupport::VisualStudio);
    assert!(!Xcode.supports_platform() && !Xcode.supports_instance());
    assert!(VS17.supports_platform() && VS18.supports_instance());
    assert_eq!(VsVersion::Vs17.default_toolset(), "v143");
    assert_eq!(VsVersion::Vs18.default_toolset(), "v145");
    assert_eq!(VsVersion::Vs17.tools_version(), "17.0");
    assert_eq!(VsVersion::Vs18.tools_version(), "18.0");
    assert!(VsVersion::Vs18.writes_slnx() && !VsVersion::Vs17.writes_slnx());
}

#[test]
fn available_generators_follow_host() {
    let names: Vec<&str> = GeneratorKind::available()
        .map(GeneratorKind::name)
        .collect();
    let mut expected = vec!["Unix Makefiles", "Ninja", "Ninja Multi-Config"];
    if cfg!(target_os = "macos") {
        expected.push("Xcode");
    }
    if cfg!(any(windows, feature = "vs-cross")) {
        expected.extend(["Visual Studio 17 2022", "Visual Studio 18 2026"]);
    }
    assert_eq!(names, expected);
}

#[test]
fn xcode_version_number() {
    assert_eq!(xcode_27_2().number, 272);
    assert_eq!(XcodeVersion::parse(b"12").number, 120);
    assert_eq!(XcodeVersion::parse(b"15.4.1").number, 154);
    assert_eq!(XcodeVersion::parse(b"").number, 0);
    assert_eq!(
        XcodeBuildSystem::default_for(&xcode_27_2()),
        XcodeBuildSystem::Twelve
    );
    assert_eq!(
        XcodeBuildSystem::default_for(&XcodeVersion::parse(b"11.7")),
        XcodeBuildSystem::One
    );
}

#[test]
fn xcode_oracle_error_texts() {
    let xcode = xcode_27_2();
    let parse = |t: &str, a: &str| {
        GeneratorSpec::parse(
            GeneratorKind::Xcode,
            t.as_bytes(),
            a.as_bytes(),
            b"",
            Some(&xcode),
        )
    };
    assert_eq!(
        message(parse("buildsystem=1", "")),
        "Generator\n  Xcode\ntoolset specification field\n  buildsystem=1\nis not allowed with Xcode 27.2."
    );
    assert_eq!(
        message(parse("bogus=1", "")),
        "Generator\n  Xcode\ngiven toolset specification that contains invalid field 'bogus'."
    );
    assert_eq!(
        message(parse("", "arm64")),
        "Generator\n  Xcode\ndoes not support platform specification, but platform\n  arm64\nwas specified."
    );
    assert_eq!(
        message(parse("buildsystem=3", "")),
        "Generator\n  Xcode\ntoolset specification field\n  buildsystem=3\nvalue is unknown.  It must be '1' or '12'."
    );
    assert_eq!(
        message(parse("foo,bar", "")),
        "Generator\n  Xcode\ngiven toolset specification\n  foo,bar\nthat contains a field after the first ',' with no '='."
    );
    assert_eq!(
        message(parse("buildsystem=12,buildsystem=12", "")),
        "Generator\n  Xcode\ngiven toolset specification\n  buildsystem=12,buildsystem=12\nthat contains duplicate field key 'buildsystem'."
    );
    let old = XcodeVersion::parse(b"11.0");
    assert_eq!(
        parse_xcode_toolset(GeneratorKind::Xcode, b"buildsystem=12", Some(&old))
            .err()
            .map(|err| err.to_string()),
        Some(
            "Generator\n  Xcode\ntoolset specification field\n  buildsystem=12\nis not allowed with Xcode 11.0."
                .to_owned()
        )
    );
}

#[test]
fn xcode_accepts_toolset_and_build_system() {
    let xcode = xcode_27_2();
    let spec = GeneratorSpec::parse(
        GeneratorKind::Xcode,
        b"foo,buildsystem=12",
        b"",
        b"",
        Some(&xcode),
    );
    assert_eq!(
        spec.map(|spec| spec.parsed),
        Ok(ParsedToolset::Xcode {
            toolset: Some("foo".into()),
            build_system: Some("12".into()),
        })
    );
    assert_eq!(
        parse_xcode_toolset(GeneratorKind::Xcode, b"", Some(&xcode)),
        Ok(ParsedToolset::Xcode {
            toolset: None,
            build_system: None
        })
    );
    assert_eq!(
        parse_xcode_toolset(GeneratorKind::Xcode, b"buildsystem=1", None).map(|_| ()),
        Ok(())
    );
}

#[test]
fn non_ide_generators_reject_every_option() {
    assert_eq!(
        message(GeneratorSpec::parse(
            GeneratorKind::Ninja,
            b"v143",
            b"",
            b"",
            None
        )),
        "Generator\n  Ninja\ndoes not support toolset specification, but toolset\n  v143\nwas specified."
    );
    assert_eq!(
        message(GeneratorSpec::parse(
            GeneratorKind::UnixMakefiles,
            b"",
            b"x64",
            b"",
            None
        )),
        "Generator\n  Unix Makefiles\ndoes not support platform specification, but platform\n  x64\nwas specified."
    );
    // The instance is checked first.
    assert_eq!(
        message(GeneratorSpec::parse(
            GeneratorKind::Xcode,
            b"bogus=1",
            b"x64",
            b"/opt/vs",
            None
        )),
        "Generator\n  Xcode\ndoes not support instance specification, but instance\n  /opt/vs\nwas specified."
    );
    assert_eq!(
        GeneratorSpec::parse(GeneratorKind::NinjaMultiConfig, b"", b"", b"", None),
        Ok(GeneratorSpec::default())
    );
}

#[test]
fn vs_toolset_fields() {
    let parsed = parse_vs_toolset(
        VS17,
        b"ClangCL,host=x64,version=14.44,cuda=12.4,VCTargetsPath=C:/t,fortran=ifx,customFlagTableDir=/f",
    );
    assert_eq!(
        parsed,
        Ok(VsToolset {
            name: Some("ClangCL".into()),
            host: Some("x64".into()),
            version: Some("14.44".into()),
            cuda: Some("12.4".into()),
            vctargets: Some("C:/t".into()),
            fortran: Some("ifx".into()),
            custom_flag_table_dir: Some("/f".into()),
        })
    );
    assert_eq!(
        parse_vs_toolset(VS18, b",,host=ARM64"),
        Ok(VsToolset {
            host: Some("ARM64".into()),
            ..VsToolset::default()
        })
    );
}

#[test]
fn vs_oracle_error_texts() {
    let err = |result: Result<VsToolset, GeneratorSpecError>| {
        result.err().map(|err| err.to_string()).unwrap_or_default()
    };
    assert_eq!(
        err(parse_vs_toolset(VS17, b"v143,host=arm")),
        "Generator\n  Visual Studio 17 2022\ngiven toolset specification\n  v143,host=arm\nthat contains invalid field 'host=arm'."
    );
    assert_eq!(
        err(parse_vs_toolset(VS18, b"v145,bogus")),
        "Generator\n  Visual Studio 18 2026\ngiven toolset specification\n  v145,bogus\nthat contains a field after the first ',' with no '='."
    );
    assert_eq!(
        err(parse_vs_toolset(VS17, b"version=1,version=2")),
        "Generator\n  Visual Studio 17 2022\ngiven toolset specification\n  version=1,version=2\nthat contains duplicate field key 'version'."
    );
    assert_eq!(
        parse_vs_platform(VS17, b"x64,sdk=10")
            .err()
            .map(|err| err.to_string()),
        Some(
            "Generator\n  Visual Studio 17 2022\ngiven platform specification\n  x64,sdk=10\nthat contains invalid field 'sdk=10'."
                .to_owned()
        )
    );
    assert_eq!(
        parse_vs_platform(VS17, b"x64,version=10.0.22621.0"),
        Ok(VsPlatform {
            name: Some("x64".into()),
            version: Some("10.0.22621.0".into()),
        })
    );
}

#[test]
fn vs_spec_keeps_raw_values() {
    let spec = GeneratorSpec::parse(VS17, b"v143", b"Win32", b"C:/VS", None);
    assert_eq!(
        spec,
        Ok(GeneratorSpec {
            toolset: "v143".into(),
            platform: "Win32".into(),
            instance: "C:/VS".into(),
            parsed: ParsedToolset::Vs(VsToolset {
                name: Some("v143".into()),
                ..VsToolset::default()
            }),
            parsed_platform: Some(VsPlatform {
                name: Some("Win32".into()),
                version: None,
            }),
        })
    );
}
