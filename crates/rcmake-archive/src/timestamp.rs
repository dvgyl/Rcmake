use std::io::Read;

use jiff::{
    Timestamp,
    civil::{Date, DateTime as CivilDateTime, Time},
    tz::{Offset, TimeZone},
};
use rcmake_host::FileTime;
use tar::Entry;
use zip::{DateTime, extra_fields::ExtraField, read::ZipFile, write::FullFileOptions};

use crate::{ArchiveError, archive_error};

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const TICKS_PER_SECOND: i128 = 10_000_000;
const NTFS_EPOCH_OFFSET: i64 = 11_644_473_600;
const SECONDS_PER_DAY: i64 = 86_400;

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

pub(super) fn add_zip_time(
    options: &mut FullFileOptions<'_>,
    time: FileTime,
) -> Result<(), ArchiveError> {
    if let Ok(secs) = i32::try_from(time.secs) {
        let mut field = vec![1];
        field.extend_from_slice(&secs.to_le_bytes());
        options
            .add_extra_data(0x5455, field.into_boxed_slice(), false)
            .map_err(archive_error)?;
        if time.nanos == 0 {
            return Ok(());
        }
    }

    let ticks = (i128::from(time.secs) + i128::from(NTFS_EPOCH_OFFSET)) * TICKS_PER_SECOND
        + i128::from(time.nanos / 100);
    let ticks = u64::try_from(ticks).map_err(|_| {
        ArchiveError::new("The modification time is outside the ZIP timestamp range")
    })?;
    // The NTFS field has four reserved bytes, a tag, a length, and three file times.
    let mut field = vec![0, 0, 0, 0, 1, 0, 24, 0];
    field.extend_from_slice(&ticks.to_le_bytes());
    field.extend_from_slice(&[0; 16]);
    options
        .add_extra_data(0x000a, field.into_boxed_slice(), false)
        .map_err(archive_error)
}

pub(super) fn zip_time<R: Read>(file: &ZipFile<'_, R>) -> Option<FileTime> {
    let mut unix_time = None;
    for field in file.extra_data_fields() {
        match field {
            ExtraField::Ntfs(field) => {
                let ticks = i128::from(field.mtime());
                return Some(FileTime {
                    secs: (ticks / TICKS_PER_SECOND) as i64 - NTFS_EPOCH_OFFSET,
                    nanos: ((ticks % TICKS_PER_SECOND) * 100) as u32,
                });
            }
            ExtraField::ExtendedTimestamp(field) => {
                if let Some(time) = field.mod_time() {
                    unix_time = Some(seconds(i64::from(i32::from_le_bytes(time.to_le_bytes()))));
                }
            }
        }
    }
    unix_time.or_else(|| file.last_modified().map(dos_to_unix))
}

pub(super) fn zip_dos_time(time: FileTime) -> DateTime {
    let mut days = time.secs.div_euclid(SECONDS_PER_DAY);
    let start = days_to_year(1980);
    if days < start {
        return DateTime::default();
    }
    let end = days_to_year(2108) - 1;
    let secs = if days > end {
        days = end;
        SECONDS_PER_DAY - 1
    } else {
        time.secs.rem_euclid(SECONDS_PER_DAY)
    };
    let mut year = 1980;
    while days >= days_to_year(year + 1) {
        year += 1;
    }
    days -= days_to_year(year);
    let mut month = 1;
    while days >= month_days(year, month) {
        days -= month_days(year, month);
        month += 1;
    }
    DateTime::from_date_and_time(
        year,
        month,
        (days + 1) as u8,
        (secs / 3600) as u8,
        (secs / 60 % 60) as u8,
        (secs % 60) as u8,
    )
    .unwrap_or_default()
}

fn dos_to_unix(time: DateTime) -> FileTime {
    let days_before_month: i64 = (1..time.month())
        .map(|month| month_days(time.year(), month))
        .sum();
    let days = days_to_year(time.year()) + days_before_month + i64::from(time.day()) - 1;
    seconds(
        days * SECONDS_PER_DAY
            + i64::from(time.hour()) * 3600
            + i64::from(time.minute()) * 60
            + i64::from(time.second()),
    )
}

fn days_to_year(year: u16) -> i64 {
    const LAST_PRE_EPOCH_YEAR: i64 = 1969;
    let previous = i64::from(year) - 1;
    (previous - LAST_PRE_EPOCH_YEAR) * 365 + leap_days(previous) - leap_days(LAST_PRE_EPOCH_YEAR)
}

fn leap_days(year: i64) -> i64 {
    year / 4 - year / 100 + year / 400
}

fn month_days(year: u16, month: u8) -> i64 {
    match month {
        2 => {
            28 + i64::from(
                year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)),
            )
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Parse a CMake archive modification time, including epoch seconds prefixed with `@`.
pub fn parse_archive_mtime(host: &dyn rcmake_host::Host, value: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(value).ok()?;
    if let Some(epoch) = text.strip_prefix('@') {
        return u64::try_from(epoch.parse::<i64>().ok()?).ok();
    }
    let text = text.trim_ascii();
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
        let time = if text.len() <= 4
            && !text.is_empty()
            && text.bytes().all(|byte| byte.is_ascii_digit())
        {
            let number = text.parse::<u32>().ok()?;
            let (hour, minute) = if number < 100 {
                (number, 0)
            } else {
                (number / 100, number % 100)
            };
            if hour > 23 || minute > 59 {
                Time::MIN
            } else {
                Time::new(hour as i8, minute as i8, 0, 0).ok()?
            }
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
