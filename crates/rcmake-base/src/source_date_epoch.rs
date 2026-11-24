/// Parse a CMake `SOURCE_DATE_EPOCH` value as signed decimal seconds.
///
/// Accept leading ASCII whitespace and an optional sign. Return `None` for
/// empty input, trailing bytes, or overflow. Callers decide how to handle an
/// empty value and timestamps that the date formatter cannot represent.
#[must_use]
pub fn parse_source_date_epoch(raw: &[u8]) -> Option<i64> {
    let first = raw
        .iter()
        .position(|&byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))?;
    std::str::from_utf8(raw.get(first..)?).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_source_date_epoch;

    #[test]
    fn parses_signed_decimal_seconds() {
        for (raw, expected) in [
            (b"-1".as_slice(), Some(-1)),
            (b"-86400", Some(-86400)),
            (b" 123", Some(123)),
            (b"\x0b\x0c\t+5", Some(5)),
            (b"+5", Some(5)),
            (b"9223372036854775807", Some(i64::MAX)),
            (b"-9223372036854775808", Some(i64::MIN)),
            (b"9223372036854775808", None),
            (b"123abc", None),
            (b"123 ", None),
            (b"+", None),
            (b"", None),
            (b" ", None),
        ] {
            assert_eq!(parse_source_date_epoch(raw), expected);
        }
    }
}
