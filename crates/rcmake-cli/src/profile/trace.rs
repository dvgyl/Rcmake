//! Write native complete events in the Chrome Trace format.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Write as _;
use std::time::Duration;

use rcmake_json::{Value, write_compact};

use super::Span;
use crate::object;

/// Encode a bounded trace and return the number of samples omitted from this export.
pub(super) fn render(
    spans: &[Span],
    pid: u32,
    limit: usize,
    dropped_samples: usize,
) -> (Vec<u8>, usize) {
    let reserve = omitted_event(pid, dropped_samples, spans.len()).len() + 2;
    let mut out = vec![b'['];
    let mut emitted = 0;
    for (index, span) in spans.iter().enumerate() {
        let event = complete_event(span, index, pid);
        if out.len() + event.len() + reserve + 4 > limit {
            break;
        }
        append_event(&mut out, &event);
        emitted += 1;
    }

    let omitted = spans.len() - emitted;
    if dropped_samples > 0 || omitted > 0 {
        let notice = omitted_event(pid, dropped_samples, omitted);
        if out.len() + notice.len() + 4 <= limit {
            append_event(&mut out, &notice);
        }
    }
    out.extend_from_slice(b"]\n");
    (out, omitted)
}

fn append_event(out: &mut Vec<u8>, event: &[u8]) {
    if out.len() > 1 {
        out.extend_from_slice(b",\n");
    }
    out.extend_from_slice(event);
}

fn complete_event(span: &Span, index: usize, pid: u32) -> Vec<u8> {
    let site = &span.site;
    let mut args = BTreeMap::from([
        ("depth".into(), Value::UInt(u64::from(span.depth))),
        ("file".into(), json_text(&site.file)),
        ("line".into(), Value::UInt(u64::from(site.line))),
        ("sample_id".into(), Value::UInt(index as u64)),
    ]);
    if let Some(parent) = span.parent {
        args.insert("parent_id".into(), Value::UInt(parent as u64));
    }
    let value = object([
        ("args", Value::Object(args)),
        ("cat", Value::string(site.kind.category())),
        ("name", json_text(&site.name)),
        ("ph", Value::string("X")),
        ("pid", Value::UInt(u64::from(pid))),
        ("tid", Value::UInt(u64::from(span.tid))),
    ]);
    let mut bytes = Vec::from(write_compact(&value));
    let _ = bytes.pop();
    bytes.extend_from_slice(b",\"dur\":");
    write_micros(&mut bytes, span.elapsed);
    bytes.extend_from_slice(b",\"ts\":");
    write_micros(&mut bytes, span.start);
    bytes.push(b'}');
    bytes
}

fn write_micros(out: &mut Vec<u8>, duration: Duration) {
    // Format the decimal directly so the value stays exact to one nanosecond.
    let _ = write!(
        out,
        "{}.{:03}",
        duration.as_micros(),
        duration.as_nanos() % 1000
    );
}

fn omitted_event(pid: u32, dropped_samples: usize, omitted: usize) -> Vec<u8> {
    Vec::from(write_compact(&object([
        (
            "args",
            object([
                ("recording_limit", Value::UInt(dropped_samples as u64)),
                ("trace_byte_limit", Value::UInt(omitted as u64)),
            ]),
        ),
        ("name", Value::string("Rcmake profile samples omitted")),
        ("ph", Value::string("M")),
        ("pid", Value::UInt(u64::from(pid))),
        ("tid", Value::UInt(0)),
    ])))
}

fn json_text(bytes: &[u8]) -> Value {
    Value::string(String::from_utf8_lossy(bytes).as_bytes())
}

#[cfg(test)]
mod tests {
    use rcmake_json::{ReadOptions, parse};

    use super::*;
    use crate::profile::{ProfileKind, ProfileRecorder, ProfileSample, resolve_spans};

    #[test]
    fn emits_fractional_microseconds_and_escapes_source() -> Result<(), Box<dyn std::error::Error>>
    {
        let recorder = ProfileRecorder::new();
        recorder.record(ProfileSample {
            name: b"test\n\"",
            file: b"scripts/\xff.cmake",
            line: 23,
            kind: ProfileKind::Function,
            depth: 3,
            start: Duration::from_nanos(12_200),
            elapsed: Duration::from_nanos(700),
        });
        let mut spans = recorder
            .state
            .lock()
            .map_err(|_| "Cannot read the profile samples.")?
            .spans
            .clone()
            .into_sorted_vec();
        resolve_spans(&mut spans);
        let (bytes, omitted) = render(&spans, 1000, 4096, 0);
        assert_eq!(omitted, 0);
        let text = std::str::from_utf8(&bytes)?;
        assert!(text.contains("\"dur\":0.700"));
        assert!(text.contains("\"ts\":12.200"));
        let expected = Value::Array(vec![object([
            (
                "args",
                object([
                    ("depth", Value::Int(3)),
                    ("file", Value::string("scripts/\u{fffd}.cmake")),
                    ("line", Value::Int(23)),
                    ("sample_id", Value::Int(0)),
                ]),
            ),
            ("cat", Value::string("function")),
            ("dur", Value::Real(0.7)),
            ("name", Value::string("test\n\"")),
            ("ph", Value::string("X")),
            ("pid", Value::Int(1000)),
            ("tid", Value::Int(0)),
            ("ts", Value::Real(12.2)),
        ])]);
        assert_eq!(parse(&bytes, &ReadOptions::default()), Ok(expected));
        Ok(())
    }

    #[test]
    fn labels_samples_omitted_at_each_limit() -> Result<(), Box<dyn std::error::Error>> {
        let recorder = ProfileRecorder::new();
        for line in 0..10 {
            recorder.record(ProfileSample {
                name: b"command",
                file: b"large.cmake",
                line,
                kind: ProfileKind::Function,
                depth: 0,
                start: Duration::from_secs(u64::from(line)),
                elapsed: Duration::from_millis(1),
            });
        }
        let mut spans = recorder
            .state
            .lock()
            .map_err(|_| "Cannot read the profile samples.")?
            .spans
            .clone()
            .into_sorted_vec();
        resolve_spans(&mut spans);
        let limit = 512;
        let (bytes, omitted) = render(&spans, 1000, limit, 17);
        assert!(omitted > 0 && omitted < spans.len());
        assert!(bytes.len() <= limit);
        assert!(matches!(
            parse(&bytes, &ReadOptions::default()),
            Ok(Value::Array(events)) if events.len() == 11 - omitted
        ));
        let text = std::str::from_utf8(&bytes)?;
        assert!(text.contains("\"recording_limit\":17"));
        assert!(text.contains(&format!("\"trace_byte_limit\":{omitted}")));
        Ok(())
    }
}
