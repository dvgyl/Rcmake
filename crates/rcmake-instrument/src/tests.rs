use crate::{
    CustomContentType, DATA_VERSION, Hook, InstrumentError, InstrumentOption, query, trace,
};

#[test]
fn keywords_round_trip() {
    for hook in Hook::ALL {
        assert_eq!(Hook::from_name(hook.as_str().as_bytes()), Some(hook));
    }
    for option in InstrumentOption::ALL {
        assert_eq!(
            InstrumentOption::from_name(option.as_str().as_bytes()),
            Some(option)
        );
    }
    assert_eq!(Hook::from_name(b"postConfigure"), None);
    assert_eq!(
        CustomContentType::from_name(b"JSON"),
        Some(CustomContentType::Json)
    );
    assert_eq!((DATA_VERSION.major, DATA_VERSION.minor), (1, 2));
}

#[test]
fn stubs_report_unsupported() {
    assert!(matches!(
        query::parse_query(b"{\"version\":1}"),
        Err(InstrumentError::Unsupported(_))
    ));
    assert!(matches!(
        trace::write_trace(&[]),
        Err(InstrumentError::Unsupported(_))
    ));
}
