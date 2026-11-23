use crate::collate::{CollationInput, collate, read_modules_json, write_modules_json};
use crate::{LookupMethod, ModmapFormat, ModulesError, p1689, tdi};

#[test]
fn keywords_round_trip() {
    for format in [ModmapFormat::Clang, ModmapFormat::Gcc, ModmapFormat::Msvc] {
        assert_eq!(
            ModmapFormat::from_name(format.as_str().as_bytes()),
            Some(format)
        );
    }
    assert_eq!(ModmapFormat::from_name(b"icc"), None);
    for method in [
        LookupMethod::ByName,
        LookupMethod::IncludeAngle,
        LookupMethod::IncludeQuote,
    ] {
        assert_eq!(
            LookupMethod::from_name(method.as_str().as_bytes()),
            Some(method)
        );
    }
}

#[test]
fn ddi_round_trips_scan_fields() -> Result<(), ModulesError> {
    let ddi = p1689::parse_ddi(
        br#"{
        "version": 1, "revision": 777, "rules": [{
            "work-directory": "/build", "primary-output": "a.o",
            "outputs": ["a.d"],
            "provides": [{"logical-name": "A", "source-path": "a.cxx",
                          "compiled-module-path": "a.pcm", "unique-on-source-path": true}],
            "requires": [{"logical-name": "B", "source-path": "b.hxx",
                          "compiled-module-path": "b.pcm", "unique-on-source-path": true,
                          "lookup-method": "include-angle"}]
        }]}
    "#,
        b"a.ddi",
    )?;
    assert!(
        ddi.rules
            .iter()
            .flat_map(|rule| &rule.provides)
            .all(|module| module.is_interface)
    );
    assert_eq!(ddi.revision, 0);
    assert_eq!(p1689::parse_ddi(&p1689::write_ddi(&ddi)?, b"a.ddi")?, ddi);
    for invalid in [
        br#"{"version":2,"rules":[{}]}"#.as_slice(),
        br#"{"version":1,"rules":[{},{}]}"#,
        br#"{"version":1,"rules":[{"requires":[{"logical-name":"M","unique-on-source-path":true}]}]}"#,
    ] {
        assert!(p1689::parse_ddi(invalid, b"a.ddi").is_err());
    }
    Ok(())
}

#[test]
fn metadata_preserves_install_and_import_details() -> Result<(), ModulesError> {
    let info = tdi::read_tdi(br#"{
        "dir-top-bld":"/build", "linked-target-dirs":["CMakeFiles/lib.dir"],
        "cxx-modules":{"m.o":{"type":"CXX_MODULES","visibility":"PUBLIC","compile-options":["-foo"]}},
        "exports":[{"filesystem-export-name":"example","install":true}],
        "bmi-installation":{"destination":"lib/bmi","script-location":"/build/install.cmake"},
        "cxx-interface-objects":{"rsp-file":"objects.rsp","module-objects":{"M":"m.o"}}
    }"#)?;
    assert_eq!(tdi::read_tdi(&tdi::write_tdi(&info)?)?, info);
    let modules = read_modules_json(
        br#"{
        "modules":{"M":{"bmi":"m.pcm","is-private":false}},
        "usages":{"M":["P"]},
        "references":{"P":{"path":"p.pcm","lookup-method":"by-name"}},
        "interface-objects":{"M":"m.o"},
        "import-error":{"message":"Cannot import M","modules":["M"]}
    }"#,
    )?;
    assert_eq!(read_modules_json(&write_modules_json(&modules)?)?, modules);
    let consumer = p1689::parse_ddi(
        br#"{"version":1,"rules":[{"primary-output":"main.o","requires":[{"logical-name":"M"}]}]}"#,
        b"main.ddi",
    )?;
    assert_eq!(
        collate(&CollationInput {
            ddis: vec![consumer],
            linked: vec![modules],
            ..CollationInput::default()
        }),
        Err(ModulesError::Invalid("Cannot import M".into()))
    );
    Ok(())
}

#[test]
fn cycles_and_duplicate_providers_are_errors() -> Result<(), ModulesError> {
    let a = p1689::parse_ddi(
        br#"{"version":1,"rules":[{"primary-output":"a.o","provides":[{"logical-name":"A"}],"requires":[{"logical-name":"B"}]}]}"#,
        b"a.ddi",
    )?;
    let b = p1689::parse_ddi(
        br#"{"version":1,"rules":[{"primary-output":"b.o","provides":[{"logical-name":"B"}],"requires":[{"logical-name":"A"}]}]}"#,
        b"b.ddi",
    )?;
    let mut input = CollationInput {
        ddis: vec![a.clone(), b],
        ..CollationInput::default()
    };
    assert_eq!(
        collate(&input),
        Err(ModulesError::Cycle(br#""A", "B""#.as_slice().into()))
    );
    input.ddis.push(a);
    let err = collate(&input);
    assert_eq!(
        err,
        Err(ModulesError::Invalid(
            "Multiple sources provide the C++ module `A`".into()
        ))
    );
    Ok(())
}
