//! Source breakpoints (`cmDebuggerBreakpointManager`) and the source key function.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::BString;
use rcmake_eval::debug::CommandSpan;
use rcmake_host::Host;
use rcmake_json::Value;

use crate::protocol::object;

/// Most breakpoints in one `setBreakpoints` request.
pub const MAX_BREAKPOINTS_PER_SOURCE: usize = 4096;

/// Most sources with breakpoints or pending breakpoints.
pub const MAX_SOURCES: usize = 1024;

/// Most parsed sources whose command spans the adapter keeps.
pub const MAX_LOADED_SOURCES: usize = 1 << 16;

/// The one key for a source path, used for breakpoint requests, parsed sources, and
/// command lookups (`cmSystemTools::ToNormalizedPathOnDisk`).
///
/// The key is the lexically collapsed path (`.` and `..` removed). On a file system
/// that ignores case (macOS and Windows), each existing component takes its spelling on
/// disk. The function does not resolve symbolic links, so a logical path stays logical.
#[must_use]
pub fn source_key(host: &dyn Host, path: &[u8]) -> BString {
    let collapsed = collapse(path);
    if cfg!(any(target_os = "macos", target_os = "ios", windows)) {
        disk_spelling(host, &collapsed)
    } else {
        collapsed
    }
}

/// Remove `.` components, empty components, and `..` after a normal component.
fn collapse(path: &[u8]) -> BString {
    let absolute = path.first() == Some(&b'/');
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|&byte| byte == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                if parts.last().is_some_and(|last| *last != b"..") {
                    parts.pop();
                } else if !absolute {
                    parts.push(part);
                }
            }
            _ => parts.push(part),
        }
    }
    let mut out = Vec::with_capacity(path.len());
    if absolute {
        out.push(b'/');
    }
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(part);
    }
    if out.is_empty() && !path.is_empty() {
        out.push(b'.');
    }
    BString::from(out)
}

/// Replace each component of an absolute `path` with the spelling of the directory
/// entry that matches it without case. A missing component stops the lookup.
fn disk_spelling(host: &dyn Host, path: &[u8]) -> BString {
    let Some(rest) = path.strip_prefix(b"/") else {
        return BString::from(path);
    };
    let mut out = b"/".to_vec();
    let mut parts = rest.split(|&byte| byte == b'/').peekable();
    let mut found = true;
    while let Some(part) = parts.next() {
        let mut name = part.to_vec();
        if found {
            let dir = rcmake_host::path::to_native(&out);
            match host.read_dir(Path::new(&dir)) {
                Ok(entries) => {
                    if !entries.iter().any(|entry| entry.name.as_slice() == part) {
                        match entries
                            .iter()
                            .find(|entry| entry.name.eq_ignore_ascii_case(part))
                        {
                            Some(entry) => name = entry.name.to_vec(),
                            None => found = false,
                        }
                    }
                }
                Err(_) => found = false,
            }
        }
        out.extend_from_slice(&name);
        if parts.peek().is_some() {
            out.push(b'/');
        }
    }
    BString::from(out)
}

#[derive(Clone, Copy, Debug)]
struct SourceBreakpoint {
    id: i64,
    line: i64,
    valid: bool,
}

/// Breakpoints and parsed command spans for each source key.
#[derive(Default)]
pub struct BreakpointManager {
    spans: BTreeMap<BString, Vec<CommandSpan>>,
    breakpoints: BTreeMap<BString, Vec<SourceBreakpoint>>,
    pending: BTreeSet<BString>,
    next_id: i64,
}

impl BreakpointManager {
    /// `CalibrateBreakpointLine`: the start line of the first command that starts at or
    /// after `line`. A line at or after the end line of the last command moves to the
    /// start of the last command. Otherwise `0` (not verified).
    fn calibrate(spans: &[CommandSpan], line: i64) -> i64 {
        if let Some(span) = spans.iter().find(|span| i64::from(span.start) >= line) {
            return i64::from(span.start);
        }
        match spans.last() {
            Some(last) if i64::from(last.end) <= line => i64::from(last.start),
            _ => 0,
        }
    }

    /// Handle `setBreakpoints` for the source `key`. Return the response body.
    ///
    /// # Errors
    ///
    /// Return an error text if the request is over a limit.
    pub fn set(&mut self, key: &BString, lines: &[i64]) -> Result<Value, &'static str> {
        if lines.len() > MAX_BREAKPOINTS_PER_SOURCE {
            return Err("Too many breakpoints");
        }
        if !self.breakpoints.contains_key(key) && self.breakpoints.len() >= MAX_SOURCES {
            return Err("Too many sources with breakpoints");
        }
        let source = object([("path", Value::string(key.clone()))]);
        let list = self.breakpoints.entry(key.clone()).or_default();
        list.clear();
        let mut out = Vec::with_capacity(lines.len());
        if let Some(spans) = self.spans.get(key) {
            for &line in lines {
                let corrected = Self::calibrate(spans, line);
                let mut item = object([
                    ("source", source.clone()),
                    ("verified", Value::Bool(corrected > 0)),
                    ("line", Value::Int(line)),
                ]);
                if corrected > 0 {
                    let id = self.next_id;
                    self.next_id = self.next_id.saturating_add(1);
                    list.push(SourceBreakpoint {
                        id,
                        line: corrected,
                        valid: true,
                    });
                    if let Value::Object(map) = &mut item {
                        map.insert("id".into(), Value::Int(id));
                        map.insert("line".into(), Value::Int(corrected));
                    }
                }
                out.push(item);
            }
        } else {
            self.pending.insert(key.clone());
            for &line in lines {
                let id = self.next_id;
                self.next_id = self.next_id.saturating_add(1);
                list.push(SourceBreakpoint {
                    id,
                    line,
                    valid: true,
                });
                out.push(object([
                    ("id", Value::Int(id)),
                    ("line", Value::Int(line)),
                    ("source", source.clone()),
                    ("verified", Value::Bool(false)),
                ]));
            }
        }
        Ok(object([("breakpoints", Value::Array(out))]))
    }

    /// `SourceFileLoaded`: record the spans of a parsed source. Return the bodies of
    /// `breakpoint` events (`reason: "changed"`) for its pending breakpoints.
    pub fn source_loaded(&mut self, key: &BString, spans: &[CommandSpan]) -> Vec<Value> {
        if self.spans.contains_key(key) || self.spans.len() >= MAX_LOADED_SOURCES {
            return Vec::new();
        }
        self.spans.insert(key.clone(), spans.to_vec());
        if !self.pending.remove(key) {
            return Vec::new();
        }
        let Some(list) = self.breakpoints.get_mut(key) else {
            return Vec::new();
        };
        let mut events = Vec::with_capacity(list.len());
        for breakpoint in list.iter_mut() {
            let corrected = Self::calibrate(spans, breakpoint.line);
            let verified = corrected > 0;
            if verified {
                breakpoint.line = corrected;
            } else {
                breakpoint.valid = false;
            }
            events.push(object([
                (
                    "breakpoint",
                    object([
                        ("id", Value::Int(breakpoint.id)),
                        ("line", Value::Int(breakpoint.line)),
                        ("source", object([("path", Value::string(key.clone()))])),
                        ("verified", Value::Bool(verified)),
                    ]),
                ),
                ("reason", Value::string("changed")),
            ]));
        }
        events
    }

    /// Return whether `key` has any breakpoint.
    #[must_use]
    pub fn has_breakpoints(&self, key: &[u8]) -> bool {
        self.breakpoints
            .get(key)
            .is_some_and(|list| !list.is_empty())
    }

    /// Return the ids of the valid breakpoints at `line` of `key`.
    #[must_use]
    pub fn hits(&self, key: &[u8], line: u32) -> Vec<i64> {
        self.breakpoints.get(key).map_or_else(Vec::new, |list| {
            list.iter()
                .filter(|breakpoint| breakpoint.valid && breakpoint.line == i64::from(line))
                .map(|breakpoint| breakpoint.id)
                .collect()
        })
    }

    /// Return whether any source has a breakpoint.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.breakpoints.values().all(Vec::is_empty)
    }

    /// Remove all breakpoints (`ClearAll`). Parsed spans stay.
    pub fn clear(&mut self) {
        self.breakpoints.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(list: &[(u32, u32)]) -> Vec<CommandSpan> {
        list.iter()
            .map(|&(start, end)| CommandSpan { start, end })
            .collect()
    }

    #[test]
    fn calibration_follows_cmake() {
        // Commands at 2, 4..5, and 7..10.
        let table = spans(&[(2, 2), (4, 5), (7, 10)]);
        let cases = [
            (1, 2),  // before the first command
            (2, 2),  // on a command
            (3, 4),  // between commands
            (5, 7),  // inside a multi-line command: the next start
            (7, 7),  // start of the last command
            (8, 0),  // inside the last command: not verified
            (9, 0),  // inside the last command: not verified
            (10, 7), // end line of the last command
            (11, 7), // after the last command
            (99, 7),
        ];
        for (line, expected) in cases {
            assert_eq!(
                BreakpointManager::calibrate(&table, line),
                expected,
                "line {line}"
            );
        }
        assert_eq!(BreakpointManager::calibrate(&[], 3), 0);
    }

    #[test]
    fn pending_breakpoints_keep_lines_and_ids_until_load() {
        let mut manager = BreakpointManager::default();
        let key = BString::from("/s/a.cmake");
        let body = manager.set(&key, &[1, 8, 50]).unwrap();
        let text = String::from_utf8(rcmake_json::write_compact(&body).to_vec()).unwrap();
        assert_eq!(
            text,
            r#"{"breakpoints":[{"id":0,"line":1,"source":{"path":"/s/a.cmake"},"verified":false},{"id":1,"line":8,"source":{"path":"/s/a.cmake"},"verified":false},{"id":2,"line":50,"source":{"path":"/s/a.cmake"},"verified":false}]}"#
        );
        // A pending breakpoint keeps the requested line until its source loads.
        assert_eq!(manager.hits(&key, 1), vec![0]);
        let events = manager.source_loaded(&key, &spans(&[(2, 2), (7, 10)]));
        let text: Vec<String> = events
            .iter()
            .map(|event| String::from_utf8(rcmake_json::write_compact(event).to_vec()).unwrap())
            .collect();
        assert_eq!(
            text,
            [
                r#"{"breakpoint":{"id":0,"line":2,"source":{"path":"/s/a.cmake"},"verified":true},"reason":"changed"}"#,
                r#"{"breakpoint":{"id":1,"line":8,"source":{"path":"/s/a.cmake"},"verified":false},"reason":"changed"}"#,
                r#"{"breakpoint":{"id":2,"line":7,"source":{"path":"/s/a.cmake"},"verified":true},"reason":"changed"}"#,
            ]
        );
        assert_eq!(manager.hits(&key, 2), vec![0]);
        assert_eq!(manager.hits(&key, 7), vec![2]);
        assert!(manager.hits(&key, 8).is_empty());
        // A second load of the same source changes nothing.
        assert!(manager.source_loaded(&key, &spans(&[(1, 1)])).is_empty());
    }

    #[test]
    fn loaded_source_verifies_immediately() {
        let mut manager = BreakpointManager::default();
        let key = BString::from("/s/b.cmake");
        assert!(manager.source_loaded(&key, &spans(&[(3, 3)])).is_empty());
        let body = manager.set(&key, &[1, 3]).unwrap();
        let text = String::from_utf8(rcmake_json::write_compact(&body).to_vec()).unwrap();
        assert_eq!(
            text,
            r#"{"breakpoints":[{"id":0,"line":3,"source":{"path":"/s/b.cmake"},"verified":true},{"id":1,"line":3,"source":{"path":"/s/b.cmake"},"verified":true}]}"#
        );
        assert_eq!(manager.hits(&key, 3), vec![0, 1]);
    }

    #[test]
    fn limits_are_enforced() {
        let mut manager = BreakpointManager::default();
        let lines = vec![1; MAX_BREAKPOINTS_PER_SOURCE + 1];
        assert!(manager.set(&"/a".into(), &lines).is_err());
        for index in 0..MAX_SOURCES {
            manager.set(&format!("/f{index}").into(), &[1]).unwrap();
        }
        assert!(manager.set(&"/one-more".into(), &[1]).is_err());
        assert!(manager.set(&"/f0".into(), &[2]).is_ok());
    }

    #[test]
    fn collapse_is_lexical() {
        assert_eq!(collapse(b"/a/./b/../c//d"), "/a/c/d");
        assert_eq!(collapse(b"/../a"), "/a");
        assert_eq!(collapse(b"a/../../b"), "../b");
        assert_eq!(collapse(b"/a/b/c:3:EVAL"), "/a/b/c:3:EVAL");
    }
}
