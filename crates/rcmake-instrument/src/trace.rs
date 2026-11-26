//! Convert instrumentation snippets to Chrome Trace Event Format.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::Path;

use rcmake_base::BString;
use rcmake_host::path;
use rcmake_json::Value;

use crate::{InstrumentError, invalid};

struct Event {
    start: u64,
    duration: u64,
    role: BString,
    name: BString,
    args: Value,
}

fn number(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::UInt(n)) => Some(*n),
        Some(Value::Int(n)) => u64::try_from(*n).ok(),
        _ => None,
    }
}

impl Event {
    fn from_snippet(args: Value) -> Result<Self, InstrumentError> {
        let Value::Object(fields) = &args else {
            return Err(invalid("A trace snippet must be an object."));
        };
        let field = |key: &str| fields.get(key.as_bytes());
        let text = |key| match field(key) {
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        };
        let role = text("role").ok_or_else(|| invalid("The trace snippet has no role."))?;
        let start = number(field("timeStart"))
            .ok_or_else(|| invalid("The trace snippet has no start time."))?;
        let duration = number(field("duration"))
            .ok_or_else(|| invalid("The trace snippet has no duration."))?;
        let detail = match role.as_slice() {
            b"compile" => text("source"),
            b"link" => text("target"),
            b"custom" => text("command"),
            b"test" => text("testName"),
            b"install" => text("workingDir").and_then(|dir| {
                let dir = path::to_native(&dir);
                dir.file_name()
                    .map(|name| path::from_native(Path::new(name)))
            }),
            _ => None,
        };
        let mut name = role.clone();
        if let Some(detail) = detail {
            name.extend_from_slice(b": ");
            name.extend_from_slice(&detail);
        }
        Ok(Self {
            start,
            duration,
            role,
            name,
            args,
        })
    }

    fn json(self, thread: u64) -> Value {
        Value::object_from([
            ("name", Value::String(self.name)),
            ("cat", Value::String(self.role)),
            ("ph", Value::string("X")),
            ("ts", Value::UInt(self.start.saturating_mul(1000))),
            ("dur", Value::UInt(self.duration.saturating_mul(1000))),
            ("pid", Value::UInt(0)),
            ("tid", Value::UInt(thread)),
            ("args", self.args),
        ])
    }
}

pub(crate) fn trace_value(
    snippets: impl IntoIterator<Item = Value>,
) -> Result<Value, InstrumentError> {
    let mut events = snippets
        .into_iter()
        .map(Event::from_snippet)
        .collect::<Result<Vec<_>, _>>()?;
    events.sort_by_key(|event| event.start);
    let mut busy = BinaryHeap::<Reverse<(u64, u64)>>::new();
    let mut free = BinaryHeap::<Reverse<u64>>::new();
    let mut next_thread = 1;
    let events = events
        .into_iter()
        .map(|event| {
            while busy
                .peek()
                .is_some_and(|Reverse((end, _))| *end <= event.start)
            {
                if let Some(Reverse((_, thread))) = busy.pop() {
                    free.push(Reverse(thread));
                }
            }
            let thread = free.pop().map_or_else(
                || {
                    let thread = next_thread;
                    next_thread += 1;
                    thread
                },
                |Reverse(thread)| thread,
            );
            busy.push(Reverse((
                event.start.saturating_add(event.duration),
                thread,
            )));
            event.json(thread)
        })
        .collect();
    Ok(Value::Array(events))
}
