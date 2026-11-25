//! Timestamp formatting for CMake commands.

use jiff::{Timestamp, fmt::strtime};
use rcmake_base::{BString, parse_source_date_epoch};

use crate::{Host, TimeZone};

/// Error from a malformed or out-of-range `SOURCE_DATE_EPOCH` value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("Cannot parse SOURCE_DATE_EPOCH as integer")]
pub struct InvalidSourceDateEpoch;

/// Read `SOURCE_DATE_EPOCH` as signed seconds. An unset or empty value returns `None`.
///
/// # Errors
/// Return an error if a nonempty value cannot be parsed as a signed 64-bit integer.
pub fn source_date_epoch(host: &dyn Host) -> Result<Option<i64>, InvalidSourceDateEpoch> {
    host.env_get(b"SOURCE_DATE_EPOCH")
        .filter(|value| !value.is_empty())
        .map(|value| parse_source_date_epoch(&value).ok_or(InvalidSourceDateEpoch))
        .transpose()
}

/// Format an instant with the CMake 4.5.1 timestamp specifiers.
///
/// An empty format selects the default date and time format. UTC adds a trailing
/// `Z` to the default format. Copy unsupported specifiers, including `%p` and
/// `%W`, to the result. Return `None` if the instant cannot be represented.
#[must_use]
pub fn format_cmake_timestamp(
    secs: i64,
    nanos: u32,
    zone: TimeZone,
    utc: bool,
    fmt: &[u8],
) -> Option<BString> {
    let instant = Timestamp::new(secs, i32::try_from(nanos).ok()?).ok()?;
    let zoned = instant.to_zoned(if utc { TimeZone::UTC } else { zone });
    let fmt: &[u8] = if fmt.is_empty() {
        if utc {
            b"%Y-%m-%dT%H:%M:%SZ"
        } else {
            b"%Y-%m-%dT%H:%M:%S"
        }
    } else {
        fmt
    };
    let mut out = BString::default();
    let mut bytes = fmt.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        match bytes.next() {
            Some(b'%') | None => out.push(b'%'),
            Some(b'f') => out.extend_from_slice(format!("{:06}", nanos / 1000).as_bytes()),
            Some(b's') => out.extend_from_slice(secs.to_string().as_bytes()),
            Some(b'Z') if utc => out.extend_from_slice(b"GMT"),
            Some(b'Z') => out.extend_from_slice(
                zoned
                    .time_zone()
                    .to_offset_info(instant)
                    .abbreviation()
                    .as_bytes(),
            ),
            Some(
                spec @ (b'd' | b'H' | b'I' | b'j' | b'm' | b'b' | b'B' | b'M' | b'S' | b'U' | b'V'
                | b'w' | b'a' | b'A' | b'y' | b'Y' | b'z'),
            ) => out.extend_from_slice(strtime::format([b'%', spec], &zoned).ok()?.as_bytes()),
            Some(spec) => out.extend_from_slice(&[b'%', spec]),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_default_timestamps_with_local_offset_or_utc() -> Result<(), jiff::Error> {
        let zone = TimeZone::posix("EST5")?;
        assert_eq!(
            format_cmake_timestamp(0, 0, zone.clone(), false, b""),
            Some("1969-12-31T19:00:00".into())
        );
        assert_eq!(
            format_cmake_timestamp(0, 0, zone.clone(), false, b"%Z %z"),
            Some("EST -0500".into())
        );
        assert_eq!(
            format_cmake_timestamp(0, 0, zone, true, b""),
            Some("1970-01-01T00:00:00Z".into())
        );
        Ok(())
    }

    #[test]
    fn formats_cmake_451_specifiers_and_preserves_unknown_bytes() {
        assert_eq!(
            format_cmake_timestamp(
                0, 123_456_789, TimeZone::UTC, true,
                b"%a %A %b %B %d %H %I %j %m %M %S %U %V %w %y %Y %z %Z %f %s %% %p %W %Q %\xff %",
            ),
            Some(BString::from(
                b"Thu Thursday Jan January 01 00 12 001 01 00 00 00 01 4 70 1970 +0000 GMT 123456 0 % %p %W %Q %\xff %".as_slice()
            ))
        );
        assert_eq!(
            format_cmake_timestamp(-1, 0, TimeZone::UTC, true, b"%s %f"),
            Some("-1 000000".into())
        );
    }

    #[test]
    fn rejects_unrepresentable_instants() {
        assert_eq!(
            format_cmake_timestamp(i64::MAX, 0, TimeZone::UTC, true, b""),
            None
        );
        assert_eq!(
            format_cmake_timestamp(0, 1_000_000_000, TimeZone::UTC, true, b""),
            None
        );
    }

    #[test]
    fn source_date_epoch_distinguishes_absence_from_invalid_input() {
        let host = crate::PureHost::default();
        assert_eq!(source_date_epoch(&host), Ok(None));
        for (input, expected) in [
            (b"".as_slice(), Ok(None)),
            (b" +42", Ok(Some(42))),
            (b"-1", Ok(Some(-1))),
            (b"0", Ok(Some(0))),
            (b"42 ", Err(InvalidSourceDateEpoch)),
            (b"9223372036854775808", Err(InvalidSourceDateEpoch)),
        ] {
            host.env_set(b"SOURCE_DATE_EPOCH", Some(input));
            assert_eq!(source_date_epoch(&host), expected);
        }
    }
}
