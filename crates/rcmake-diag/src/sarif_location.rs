use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_json::Value;

use crate::{Frame, FrameKind, Location};

pub(super) fn record(entry: &mut BTreeMap<BString, Value>, location: &Location) {
    let primary = match location {
        Location::None => return,
        Location::FileLine { file, line } => make_location(file, *line, None, false),
        Location::Backtrace(trace) => {
            let Some(top) = trace.top() else { return };
            let frames: Vec<_> = trace
                .frames()
                .filter(|frame| frame.line > 0 || frame.kind == FrameKind::Deferred)
                .map(|frame| Value::object_from([("location", frame_location(frame))]))
                .collect();
            if !frames.is_empty() {
                entry.insert(
                    "stacks".into(),
                    Value::Array(vec![Value::object_from([("frames", Value::Array(frames))])]),
                );
            }
            frame_location(top)
        }
    };
    entry.insert("locations".into(), Value::Array(vec![primary]));
}

fn frame_location(frame: &Frame) -> Value {
    make_location(
        &frame.file,
        frame.line,
        frame.name.as_deref(),
        frame.kind == FrameKind::Deferred,
    )
}

fn make_location(file: &[u8], line: u32, name: Option<&[u8]>, deferred: bool) -> Value {
    let mut physical = BTreeMap::from([(
        "artifactLocation".into(),
        Value::object_from([("uri", Value::string(file))]),
    )]);
    if line > 0 && !deferred {
        physical.insert(
            "region".into(),
            Value::object_from([("startLine", Value::UInt(line.into()))]),
        );
    }
    let mut result = BTreeMap::from([("physicalLocation".into(), Value::Object(physical))]);
    if let Some(name) = name.filter(|name| !name.is_empty()) {
        result.insert(
            "logicalLocations".into(),
            Value::Array(vec![Value::object_from([
                ("name", Value::string(name)),
                ("kind", Value::string("function")),
            ])]),
        );
    }
    if deferred {
        result.insert(
            "message".into(),
            Value::object_from([("text", Value::string("DEFERRED"))]),
        );
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Backtrace;

    #[test]
    fn records_primary_location_and_orders_stack_frames() {
        let frame = |file: &[u8], line, name: &[u8], kind| Frame {
            file: file.into(),
            line,
            name: Some(name.into()),
            kind,
        };
        let trace = Backtrace::default()
            .push(frame(b"root.cmake", 0, b"", FrameKind::ModuleInclude))
            .push(frame(b"root.cmake", 5, b"caller", FrameKind::Command))
            .push(frame(b"root.cmake", 0, b"deferred", FrameKind::Deferred))
            .push(frame(b"child.cmake", 2, b"message", FrameKind::Command));
        let mut entry = BTreeMap::new();
        record(&mut entry, &Location::None);
        assert!(entry.is_empty());
        record(&mut entry, &Location::Backtrace(trace.clone()));
        let primary = trace.top().map(frame_location);
        assert_eq!(
            entry.get(b"locations".as_slice()),
            Some(&Value::Array(primary.into_iter().collect()))
        );
        let frames = trace
            .frames()
            .take(3)
            .map(|frame| Value::object_from([("location", frame_location(frame))]))
            .collect();
        assert_eq!(
            entry.get(b"stacks".as_slice()),
            Some(&Value::Array(vec![Value::object_from([(
                "frames",
                Value::Array(frames)
            )])]))
        );
        assert!(matches!(
            make_location(b"root.cmake", 0, None, false),
            Value::Object(location) if location.len() == 1
        ));
    }
}
