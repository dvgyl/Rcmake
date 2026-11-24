use std::io::Read;

use jiff::{
    Timestamp,
    civil::{Date, DateTime as CivilDateTime, Time},
    fmt::rfc2822,
    tz::{Offset, TimeZone},
};
use rcmake_host::FileTime;
use tar::Entry;
use zip::DateTime;

use crate::{ArchiveError, archive_error};

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const TICKS_PER_SECOND: i128 = 10_000_000;
const NTFS_EPOCH_OFFSET: i64 = 11_644_473_600;

pub(super) fn seconds(secs: i64) -> FileTime {
    FileTime { secs, nanos: 0 }
}

pub(super) fn pax_time(time: FileTime) -> Option<String> {
    if time.secs >= 0 && time.nanos == 0 {
        return None;
    }
    let nanos = i128::from(time.secs) * NANOS_PER_SECOND + i128::from(time.nanos);
    let sign = if nanos < 0 { "-" } else { "" };
    let nanos = nanos.abs();
    Some(format!(
        "{sign}{}.{:09}",
        nanos / NANOS_PER_SECOND,
        nanos % NANOS_PER_SECOND
    ))
}

pub(super) fn tar_time<R: Read>(entry: &mut Entry<'_, R>) -> Result<FileTime, ArchiveError> {
    if let Some(fields) = entry.pax_extensions().map_err(archive_error)? {
        for field in fields {
            let field = field.map_err(archive_error)?;
            if field.key_bytes() == b"mtime" {
                return parse_pax_time(field.value().map_err(archive_error)?);
            }
        }
    }
    let time = entry.header().mtime().map_err(archive_error)?;
    Ok(seconds(i64::from_ne_bytes(time.to_ne_bytes())))
}

fn parse_pax_time(value: &str) -> Result<FileTime, ArchiveError> {
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let whole = whole.parse::<u64>().map_err(archive_error)?;
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ArchiveError::new("Invalid PAX modification time"));
    }
    let mut nanos = 0;
    for byte in fraction.bytes().chain(std::iter::repeat(b'0')).take(9) {
        nanos = nanos * 10 + i128::from(byte - b'0');
    }
    let nanos = i128::from(whole) * NANOS_PER_SECOND + nanos;
    let nanos = if value.starts_with('-') {
        -nanos
    } else {
        nanos
    };
    Ok(FileTime {
        secs: i64::try_from(nanos.div_euclid(NANOS_PER_SECOND)).map_err(archive_error)?,
        nanos: nanos.rem_euclid(NANOS_PER_SECOND) as u32,
    })
}

pub(super) fn zip_time_fields(time: FileTime) -> Result<Vec<u8>, ArchiveError> {
    let mut fields = Vec::new();
    if let Ok(secs) = i32::try_from(time.secs) {
        fields.extend_from_slice(&[0x55, 0x54, 5, 0, 1]);
        fields.extend_from_slice(&secs.to_le_bytes());
        if time.nanos == 0 {
            return Ok(fields);
        }
    }
    let ticks = (i128::from(time.secs) + i128::from(NTFS_EPOCH_OFFSET)) * TICKS_PER_SECOND
        + i128::from(time.nanos / 100);
    let ticks = u64::try_from(ticks).map_err(|_| {
        ArchiveError::new("The modification time is outside the ZIP timestamp range")
    })?;
    // The NTFS field contains four reserved bytes, a tag, a length, and three file times.
    fields.extend_from_slice(&[0x0a, 0, 32, 0, 0, 0, 0, 0, 1, 0, 24, 0]);
    fields.extend_from_slice(&ticks.to_le_bytes());
    fields.extend_from_slice(&[0; 16]);
    Ok(fields)
}

pub(super) fn ntfs_time(ticks: u64) -> FileTime {
    let ticks = i128::from(ticks);
    FileTime {
        secs: (ticks / TICKS_PER_SECOND) as i64 - NTFS_EPOCH_OFFSET,
        nanos: ((ticks % TICKS_PER_SECOND) * 100) as u32,
    }
}

pub(super) fn zip_dos_time(time: FileTime, zone: &TimeZone) -> DateTime {
    let Ok(timestamp) = Timestamp::from_second(time.secs) else {
        return DateTime::default();
    };
    let date_time = timestamp.to_zoned(zone.clone()).datetime();
    if date_time.year() > 2107 {
        return DateTime::from_date_and_time(2107, 12, 31, 23, 59, 58).unwrap_or_default();
    }
    DateTime::from_date_and_time(
        date_time.year() as u16,
        date_time.month() as u8,
        date_time.day() as u8,
        date_time.hour() as u8,
        date_time.minute() as u8,
        date_time.second() as u8,
    )
    .unwrap_or_default()
}

pub(super) fn dos_to_unix(time: DateTime, zone: &TimeZone) -> Option<FileTime> {
    let date = Date::new(
        time.year().try_into().ok()?,
        time.month() as i8,
        time.day() as i8,
    )
    .ok()?;
    let clock = Time::new(
        time.hour() as i8,
        time.minute() as i8,
        time.second() as i8,
        0,
    )
    .ok()?;
    let zoned = date.to_datetime(clock).to_zoned(zone.clone()).ok()?;
    Some(seconds(zoned.timestamp().as_second()))
}

/// Parse a CMake archive modification time, including epoch seconds prefixed with `@`.
pub fn parse_archive_mtime(host: &dyn rcmake_host::Host, value: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(value).ok()?;
    if let Some(epoch) = text.strip_prefix('@') {
        return u64::try_from(epoch.parse::<i64>().ok()?).ok();
    }
    let text = text.trim_ascii();
    if text.contains('.') {
        return None;
    }
    if let Ok(date_time) = rfc2822::parse(text) {
        return u64::try_from(date_time.timestamp().as_second()).ok();
    }
    let upper = text.to_ascii_uppercase();
    let utc = ["UTC", "GMT", "Z"]
        .into_iter()
        .find_map(|suffix| upper.strip_suffix(suffix));
    let (text, zone) = if let Some(text) = utc {
        (text.trim_ascii_end(), TimeZone::UTC)
    } else if let Some((local, offset)) = mtime_offset(text) {
        (local, TimeZone::fixed(offset))
    } else {
        (text, host.time_zone())
    };
    let date_time = if text.contains(' ')
        && let Ok(date_time) = text.parse::<CivilDateTime>()
    {
        date_time
    } else if !text.contains([' ', 'T', 't'])
        && let Ok(date) = text.parse::<Date>()
    {
        date.at(0, 0, 0, 0)
    } else {
        if !text.contains(' ') && text.contains(['T', 't']) {
            return None;
        }
        let time = if text == "24" {
            Time::MIN
        } else if text.len() <= 4 && text.bytes().all(|byte| byte.is_ascii_digit()) {
            let number = text.parse::<u32>().ok()?;
            let (hour, minute) = if number < 100 {
                (number, 0)
            } else {
                (number / 100, number % 100)
            };
            Time::new(i8::try_from(hour).ok()?, i8::try_from(minute).ok()?, 0, 0).ok()?
        } else {
            text.parse::<Time>().ok()?
        };
        let now = Timestamp::from_second(host.now().secs)
            .ok()?
            .to_zoned(zone.clone());
        now.date().to_datetime(time)
    };
    u64::try_from(date_time.to_zoned(zone).ok()?.timestamp().as_second()).ok()
}

fn mtime_offset(text: &str) -> Option<(&str, Offset)> {
    let (local, offset) = text.split_at_checked(text.len().checked_sub(5)?)?;
    let (sign, digits) = offset.split_at_checked(1)?;
    if !local.contains(':')
        || !matches!(sign, "+" | "-")
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let (hours, minutes) = digits.split_at_checked(2)?;
    let hours = hours.parse::<i32>().ok()?;
    let minutes = minutes.parse::<i32>().ok()?;
    if minutes > 59 {
        return None;
    }
    let seconds = hours * 3_600 + minutes * 60;
    let seconds = if sign == "-" { -seconds } else { seconds };
    Some((local.trim_ascii_end(), Offset::from_seconds(seconds).ok()?))
}
