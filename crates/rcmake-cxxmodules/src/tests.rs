use crate::collate::{CollationInput, collate};
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
fn stubs_report_unsupported() {
    assert!(matches!(
        p1689::parse_ddi(b"{}", b"a.ddi"),
        Err(ModulesError::Unsupported(_))
    ));
    assert!(matches!(
        tdi::read_tdi(b"{}"),
        Err(ModulesError::Unsupported(_))
    ));
    assert!(matches!(
        collate(&CollationInput::default()),
        Err(ModulesError::Unsupported(_))
    ));
}
