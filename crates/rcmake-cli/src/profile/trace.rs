//! Write complete events in the Chrome Trace format.

#![forbid(unsafe_code)]

use std::time::Duration;

use rcmake_json::{Value, write_compact};

use super::Span;
use crate::object;

/// Encode the samples for use in Perfetto or Chrome Trace.
pub(super) fn render(spans: &[&Span], pid: u32) -> Vec<u8> {
    let mut out = vec![b'['];
    for (index, span) in spans.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(b",\n");
        }
        let site = &span.site;
        let value = object([
            (
                "args",
                object([
                    ("depth", Value::UInt(u64::from(span.depth))),
                    ("file", json_text(&site.file)),
                    ("line", Value::UInt(u64::from(site.line))),
                ]),
            ),
            ("cat", Value::string(site.kind.category())),
            ("dur", micros(span.elapsed)),
            ("name", json_text(&site.name)),
            ("ph", Value::string("X")),
            ("pid", Value::UInt(u64::from(pid))),
            ("tid", Value::UInt(0)),
            ("ts", micros(span.start)),
        ]);
        out.extend_from_slice(&write_compact(&value));
    }
    out.extend_from_slice(b"]\n");
    out
}

fn micros(duration: Duration) -> Value {
    Value::UInt(u64::try_from(duration.as_micros()).unwrap_or(u64::MAX))
}

fn json_text(bytes: &[u8]) -> Value {
    Value::string(String::from_utf8_lossy(bytes).as_bytes())
}

#[cfg(test)]
mod tests {
    use rcmake_json::{ReadOptions, parse};

    use super::*;
    use crate::profile::{ProfileKind, Site};

    #[test]
    fn emits_complete_event_with_microsecond_units() {
        let span = Span {
            site: Site {
                name: b"test\n\"".as_slice().into(),
                file: b"scripts/\xff.cmake".as_slice().into(),
                line: 23,
                kind: ProfileKind::Function,
            },
            depth: 3,
            start: Duration::from_micros(10),
            elapsed: Duration::from_nanos(17_999),
        };
        let bytes = render(&[&span], 1000);
        let expected = Value::Array(vec![object([
            (
                "args",
                object([
                    ("depth", Value::Int(3)),
                    ("file", Value::string("scripts/\u{fffd}.cmake")),
                    ("line", Value::Int(23)),
                ]),
            ),
            ("cat", Value::string("function")),
            ("dur", Value::Int(17)),
            ("name", Value::string("test\n\"")),
            ("ph", Value::string("X")),
            ("pid", Value::Int(1000)),
            ("tid", Value::Int(0)),
            ("ts", Value::Int(10)),
        ])]);
        assert_eq!(parse(&bytes, &ReadOptions::default()), Ok(expected));
    }
}
