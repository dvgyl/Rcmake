use crate::collate::{CollationInput, collate, read_modules_json, write_modules_json};
use crate::{LookupMethod, ModmapFormat, ModulesError, modmap, p1689, tdi};

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
fn ddi_checks_named_modules_and_allows_header_paths() -> Result<(), ModulesError> {
    for name in [
        "",
        "../M",
        r"M\\part",
        r"M\npart",
        r"M\rpart",
        r#"M\"part"#,
        r"M\u0000",
        "Mä",
    ] {
        for kind in ["provides", "requires"] {
            let json =
                format!(r#"{{"version":1,"rules":[{{"{kind}":[{{"logical-name":"{name}"}}]}}]}}"#);
            let result = p1689::parse_ddi(json.as_bytes(), b"scan.ddi");
            assert!(
                matches!(result, Err(ModulesError::Invalid(message)) if message.ends_with(b"Invalid C++ module logical-name")),
                "{kind}: {name:?}"
            );
        }
    }
    let ddi = p1689::parse_ddi(
        br#"{"version":1,"rules":[{
            "provides":[{"logical-name":"M0.part_a:part_b"}],
            "requires":[{"logical-name":"include/sub dir/header.hpp","lookup-method":"include-quote"}]
        }]}"#,
        b"scan.ddi",
    )?;
    assert_eq!(
        p1689::parse_ddi(&p1689::write_ddi(&ddi)?, b"scan.ddi")?,
        ddi
    );
    assert!(p1689::parse_ddi(
        br#"{"version":1,"rules":[{"requires":[{"logical-name":"header\n.hpp","lookup-method":"include-angle"}]}]}"#,
        b"scan.ddi",
    ).is_err());
    Ok(())
}

#[test]
fn modmaps_reject_invalid_bmi_paths() -> Result<(), ModulesError> {
    let mut modules = read_modules_json(
        br#"{
            "modules":{"M":{"bmi":"file name..v1.pcm"}},
            "usages":{"M":["P"]},
            "references":{"P":{"path":"p.pcm","lookup-method":"by-name"}}
        }"#,
    )?;
    let ddi = p1689::parse_ddi(
        br#"{"version":1,"rules":[
            {"provides":[{"logical-name":"M"}],"requires":[{"logical-name":"M"}]}
        ]}"#,
        b"scan.ddi",
    )?;
    let rule = ddi
        .rules
        .first()
        .ok_or_else(|| crate::json::invalid("Missing rule"))?;
    let formats = [ModmapFormat::Clang, ModmapFormat::Gcc, ModmapFormat::Msvc];
    for format in formats {
        modmap::write_modmap(format, rule, &modules)?;
    }
    for path in [
        "../M.pcm",
        r"sub\..\M.pcm",
        "M\0.pcm",
        "M\n.pcm",
        "M\r.pcm",
        "M\".pcm",
    ] {
        let bmi = modules
            .modules
            .get_mut(b"M".as_slice())
            .ok_or_else(|| crate::json::invalid("Missing module"))?;
        let valid = std::mem::replace(&mut bmi.bmi, path.into());
        for format in formats {
            assert!(
                modmap::write_modmap(format, rule, &modules).is_err(),
                "{format:?}: {path:?}"
            );
        }
        let bmi = modules
            .modules
            .get_mut(b"M".as_slice())
            .ok_or_else(|| crate::json::invalid("Missing module"))?;
        bmi.bmi = valid;
        let reference = modules
            .references
            .get_mut(b"P".as_slice())
            .ok_or_else(|| crate::json::invalid("Missing module reference"))?;
        reference.path = path.into();
        for format in [ModmapFormat::Clang, ModmapFormat::Msvc] {
            assert!(
                modmap::write_modmap(format, rule, &modules).is_err(),
                "{format:?}: {path:?}"
            );
        }
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

#[test]
fn bmi_install_script_contains_only_public_modules_of_the_target() -> Result<(), ModulesError> {
    let drive = if cfg!(windows) { "C:" } else { "" };
    let tdi = tdi::read_tdi(
        r#"{
          "module-dir": "/build/modules",
          "cxx-modules": {
            "public.o": {"visibility": "PUBLIC"},
            "private.o": {"visibility": "PRIVATE"}
          },
          "bmi-installation": {
            "destination": "lib/bmi",
            "script-location": "/build/install-bmi.cmake",
            "permissions": "OWNER_READ GROUP_READ",
            "message-level": "MESSAGE_LAZY"
          }
        }"#
        .replace("/build", &format!("{drive}/build"))
        .as_bytes(),
    )?;
    let ddis = [("public.o", "Public"), ("private.o", "Private")]
    .into_iter()
    .map(|(object, module)| {
        p1689::parse_ddi(
            format!(
                r#"{{"version":1,"rules":[{{"primary-output":"{object}","provides":[{{"logical-name":"{module}"}}]}}]}}"#
            )
            .as_bytes(),
            b"unit.ddi",
        )
    })
    .collect::<Result<Vec<_>, _>>()?;
    let forwarded = read_modules_json(
        br#"{"modules":{"External":{"bmi":"/other/External.pcm","is-private":false}}}"#,
    )?;
    let output = collate(&CollationInput {
        tdi,
        ddis,
        forwarded: vec![forwarded],
        modmap_format: Some(ModmapFormat::Clang),
        ..CollationInput::default()
    })?;
    let actual = output
        .modmaps
        .iter()
        .find(|(path, _)| path == &format!("{drive}/build/install-bmi.cmake"))
        .map(|(_, script)| script.as_slice());
    let expected = format!(
        "file(INSTALL DESTINATION \"${{CMAKE_INSTALL_PREFIX}}/lib/bmi\" TYPE FILE MESSAGE_LAZY PERMISSIONS OWNER_READ GROUP_READ FILES \"{drive}/build/modules/Public.pcm\")\n"
    );
    assert_eq!(actual, Some(expected.as_bytes()));
    Ok(())
}
