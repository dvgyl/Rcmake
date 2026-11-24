use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcmake_base::{BString, Limits};
use rcmake_eval::Interp;
use rcmake_host::{Host, HostError, HostResult, WriteMode};
use rcmake_json::{Value, write_compact};

pub(crate) struct Recorder {
    pid: u64,
    output: Mutex<Output>,
}

struct Output {
    json: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl Recorder {
    pub(crate) fn new(host: &dyn Host) -> Self {
        Self {
            pid: u64::from(host.system().pid),
            output: Mutex::new(Output {
                json: b"[".to_vec(),
                limit: Limits::default().max_value_bytes,
                overflowed: false,
            }),
        }
    }

    pub(crate) fn attach(self: &Arc<Self>, interp: &mut Interp) {
        let recorder = Arc::clone(self);
        interp.set_command_profiler(Some(Arc::new(move |name, file, line, start, elapsed| {
            recorder.event(name, file, line, start, elapsed);
        })));
    }

    fn event(&self, name: &[u8], file: &[u8], line: u32, start: Duration, elapsed: Duration) {
        let mut location = BString::from(file);
        location.push(b':');
        location.extend_from_slice(line.to_string().as_bytes());
        let micros = |duration: Duration| {
            Value::UInt(u64::try_from(duration.as_micros()).unwrap_or(u64::MAX))
        };
        let event = crate::object([
            (
                "args",
                crate::object([("location", Value::String(location))]),
            ),
            ("cat", Value::string("script")),
            ("dur", micros(elapsed)),
            ("name", Value::string(name)),
            ("ph", Value::string("X")),
            ("pid", Value::UInt(self.pid)),
            ("tid", Value::UInt(0)),
            ("ts", micros(start)),
        ]);
        let bytes = write_compact(&event);
        if let Ok(mut output) = self.output.lock() {
            if output
                .json
                .len()
                .saturating_add(bytes.len())
                .saturating_add(3)
                > output.limit
            {
                output.overflowed = true;
            } else if !output.overflowed {
                if output.json.len() > 1 {
                    output.json.extend_from_slice(b",\n");
                }
                output.json.extend_from_slice(&bytes);
            }
        }
    }

    pub(crate) fn write(&self, host: &dyn Host, path: &Path) -> HostResult<()> {
        let mut output = self
            .output
            .lock()
            .map_err(|_| HostError::unsupported("Profiling recorder stopped unexpectedly"))?;
        if output.overflowed {
            return Err(HostError::unsupported(
                "Profiling output exceeds the size limit",
            ));
        }
        output.json.push(b']');
        host.write(path, &output.json, WriteMode::Truncate)
    }
}
