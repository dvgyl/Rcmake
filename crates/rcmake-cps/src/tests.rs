use crate::{CpsComponentType, CpsError, CpsPackage, naming, reader, writer};

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
fn stubs_report_unsupported() {
    assert!(matches!(
        writer::write_package(&CpsPackage::default()),
        Err(CpsError::Unsupported(_))
    ));
    assert!(matches!(
        reader::read_package(b"{}", b"/p.cps"),
        Err(CpsError::Unsupported(_))
    ));
    assert!(matches!(
        naming::package_file_name(b"Pkg", None, false),
        Err(CpsError::Unsupported(_))
    ));
}
