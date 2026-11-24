use std::{collections::BTreeMap, fmt::Write, path::Path};

use rcmake_base::{BString, Limits};
use rcmake_host::{Host, ReadRange, path};
use rcmake_instrument::{InstrumentOption, query};
use rcmake_json::{ReadOptions, Value, parse};

use crate::junit::escape;

type Object = BTreeMap<BString, Value>;

pub(crate) struct Instrumentation {
    verbose: bool,
}

impl Instrumentation {
    pub(crate) fn load(host: &dyn Host, dir: &Path) -> Option<Self> {
        let queries = query::read_queries(host, dir).ok()?;
        let enabled = |option| queries.iter().any(|query| query.options.contains(&option));
        let verbose = enabled(InstrumentOption::CdashVerbose);
        (verbose || enabled(InstrumentOption::CdashSubmit)).then_some(Self { verbose })
    }

    pub(crate) fn configure(&self, host: &dyn Host, dir: &Path, start: i64) -> String {
        let mut xml = String::from("<Commands>\n");
        for snippet in snippets(host, dir, start) {
            match field(&snippet, b"role") {
                b"configure" => self.command(&mut xml, "Configure", &snippet),
                b"generate" => self.command(&mut xml, "Generate", &snippet),
                _ => {}
            }
        }
        xml.push_str("</Commands>\n");
        xml
    }

    pub(crate) fn build(&self, host: &dyn Host, dir: &Path, start: i64) -> String {
        let mut xml = String::from("<Commands>\n");
        let mut targets = BTreeMap::<BString, Vec<_>>::new();
        for snippet in snippets(host, dir, start) {
            let role = match field(&snippet, b"role") {
                b"compile" => "Compile",
                b"link" => "Link",
                b"custom" => "Custom",
                b"cmakeBuild" => "CmakeBuild",
                b"build" => "Build",
                _ => continue,
            };
            let target = field(&snippet, b"target");
            if target.is_empty() {
                self.command(&mut xml, role, &snippet);
            } else {
                targets
                    .entry(target.into())
                    .or_default()
                    .push((role, snippet));
            }
        }
        xml.push_str("</Commands>\n<Targets>\n");
        let target_types = targets_from_content(host, dir);
        for (name, commands) in targets {
            let kind = match target_types.get(&name) {
                Some(Value::Object(target)) => field(target, b"type"),
                _ => b"",
            };
            let kind = if kind.is_empty() {
                b"UNKNOWN".as_slice()
            } else {
                kind
            };
            let _ = writeln!(
                xml,
                "<Target name=\"{}\" type=\"{}\">",
                escape(&name, true),
                escape(kind, true)
            );
            for (role, snippet) in commands {
                self.command(&mut xml, role, &snippet);
            }
            xml.push_str("</Target>\n");
        }
        xml.push_str("</Targets>\n");
        xml
    }

    fn command(&self, xml: &mut String, role: &str, snippet: &Object) {
        let _ = writeln!(xml, "<{role}>");
        let command = field(snippet, b"command");
        if self.verbose {
            let _ = writeln!(xml, "<Command>{}</Command>", escape(command, false));
        } else if !command.is_empty() {
            let program = command
                .split(|byte| byte.is_ascii_whitespace())
                .next()
                .unwrap_or_default();
            let _ = writeln!(
                xml,
                "<Command>{} (truncated)</Command>",
                escape(program, false)
            );
        }
        if let Some(Value::Array(outputs)) = snippet.get(b"outputs".as_slice())
            && !outputs.is_empty()
        {
            xml.push_str("<Outputs>\n");
            for output in outputs.iter().filter_map(string) {
                let _ = writeln!(xml, "<Output>{}</Output>", escape(output, false));
            }
            xml.push_str("</Outputs>\n");
        }
        for (key, element) in [(b"stdout".as_slice(), "Stdout"), (b"stderr", "Stderr")] {
            let value = field(snippet, key);
            if !value.is_empty() {
                let _ = writeln!(xml, "<{element}>{}</{element}>", escape(value, false));
            }
        }
        measurements(xml, snippet);
        let _ = writeln!(xml, "</{role}>");
    }

    pub(crate) fn test(&self, host: &dyn Host, xml: &mut String, path: &Path) {
        if let Some(snippet) = load(host, path) {
            measurements(xml, &snippet);
        }
    }
}

fn targets_from_content(host: &dyn Host, dir: &Path) -> Object {
    let content = dir.join(".cmake/instrumentation/v1/data/content");
    host.read_dir(&content)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.name.starts_with(b"cmake-") && entry.name.ends_with(b".json"))
        .max_by(|a, b| a.name.cmp(&b.name))
        .and_then(|entry| load(host, &content.join(path::to_native(&entry.name))))
        .and_then(|mut entry| match entry.remove(b"targets".as_slice()) {
            Some(Value::Object(targets)) => Some(targets),
            _ => None,
        })
        .unwrap_or_default()
}

pub(crate) fn target_labels(host: &dyn Host, dir: &Path, name: &[u8]) -> Vec<BString> {
    let targets = targets_from_content(host, dir);
    let Some(Value::Object(target)) = targets.get(name) else {
        return Vec::new();
    };
    let Some(Value::Array(labels)) = target.get(b"labels".as_slice()) else {
        return Vec::new();
    };
    labels
        .iter()
        .filter_map(string)
        .map(BString::from)
        .collect()
}

fn snippets(host: &dyn Host, dir: &Path, since: i64) -> Vec<Object> {
    let data = dir.join(".cmake/instrumentation/v1/data");
    let mut entries = host.read_dir(&data).unwrap_or_default();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
        .into_iter()
        .filter(|entry| entry.name.ends_with(b".json"))
        .map(|entry| data.join(path::to_native(&entry.name)))
        .filter(|path| {
            host.metadata(path, rcmake_host::Follow::No)
                .is_ok_and(|info| info.mtime.secs >= since)
        })
        .filter_map(|path| load(host, &path))
        .collect()
}

fn load(host: &dyn Host, path: &Path) -> Option<Object> {
    let bytes = host
        .read(
            path,
            ReadRange::All {
                limit: Limits::default().max_file_read,
            },
        )
        .ok()?;
    match parse(&bytes, &ReadOptions::default()).ok()? {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

fn field<'a>(snippet: &'a Object, key: &[u8]) -> &'a [u8] {
    snippet.get(key).and_then(string).unwrap_or_default()
}

fn measurements(xml: &mut String, snippet: &Object) {
    for (group, entries) in [
        (
            b"dynamicSystemInformation".as_slice(),
            [
                (b"beforeHostMemoryUsed".as_slice(), "BeforeHostMemoryUsed"),
                (b"afterHostMemoryUsed", "AfterHostMemoryUsed"),
                (b"afterCPULoadAverage", "AfterCPULoadAverage"),
            ],
        ),
        (
            b"processMetrics".as_slice(),
            [
                (b"maxRSS".as_slice(), "MaxRSS"),
                (b"userTime", "UserTime"),
                (b"systemTime", "SystemTime"),
            ],
        ),
    ] {
        let Some(Value::Object(values)) = snippet.get(group) else {
            continue;
        };
        for (key, name) in entries {
            let (kind, value) = match values.get(key) {
                Some(Value::UInt(value)) => ("integer", value.to_string()),
                Some(Value::Int(value)) => ("integer", value.to_string()),
                Some(Value::Real(value)) => ("double", value.to_string()),
                _ => continue,
            };
            let _ = writeln!(
                xml,
                "<NamedMeasurement type=\"numeric/{kind}\" name=\"{name}\"><Value>{value}</Value></NamedMeasurement>"
            );
        }
    }
}

fn string(value: &Value) -> Option<&[u8]> {
    match value {
        Value::String(value) => Some(value),
        _ => None,
    }
}
