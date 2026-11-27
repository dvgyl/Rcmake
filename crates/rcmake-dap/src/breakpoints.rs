//! Source breakpoints (`cmDebuggerBreakpointManager`) and the source key function.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::BString;
use rcmake_eval::debug::CommandSpan;
use rcmake_host::{Follow, Host};
use rcmake_json::Value;

use crate::protocol::object;

/// Most breakpoints in one `setBreakpoints` request.
pub const MAX_BREAKPOINTS_PER_SOURCE: usize = 4096;

/// Most sources with breakpoints or pending breakpoints.
pub const MAX_SOURCES: usize = 1024;

/// Most parsed sources whose command spans the adapter keeps.
pub const MAX_LOADED_SOURCES: usize = 1 << 16;

/// Longest `source.path` of a `setBreakpoints` request.
pub const MAX_SOURCE_PATH_BYTES: usize = 4096;

/// Most bytes of one `setBreakpoints` response.
pub const MAX_RESPONSE_BYTES: usize = crate::budget::MAX_RESPONSE_BYTES;

/// Most symbolic links that one source key resolves (`MAX_SYMBOLIC_LINKS`).
const MAX_SYMBOLIC_LINKS: usize = 32;

/// Bytes of one response row other than its source path.
const ROW_BYTES: usize = 96;

/// Check the limits of a `setBreakpoints` request before the adapter resolves its path.
///
/// # Errors
///
/// Return an error text if the request is over a limit.
pub fn check_request(path: &[u8], lines: &[i64]) -> Result<(), &'static str> {
    if path.len() > MAX_SOURCE_PATH_BYTES {
        return Err("Source path too long");
    }
    if lines.len() > MAX_BREAKPOINTS_PER_SOURCE {
        return Err("Too many breakpoints");
    }
    Ok(())
}

/// The one key for a source path, used for breakpoint requests, parsed sources, and
/// command lookups (`cmSystemTools::ToNormalizedPathOnDisk`).
///
/// The function expands a leading `~` or `~/` from `HOME`, changes `\` to `/`, and
/// makes a relative path absolute from the working directory. It removes empty and `.`
/// components. A `..` component removes the component before it, but on Unix a
/// symbolic link before a `..` resolves first (`LogicalPath`). On a file system that
/// ignores case (macOS and Windows), each existing component takes its spelling on
/// disk. Other symbolic links stay, so a logical path stays logical.
#[must_use]
pub fn source_key(host: &dyn Host, path: &[u8]) -> BString {
    let mut resolver = Resolver {
        host,
        path: expand_tilde(host, path),
        links: 0,
    };
    resolver.path.iter_mut().for_each(|byte| {
        if *byte == b'\\' {
            *byte = b'/';
        }
    });
    resolver.resolve();
    BString::from(resolver.path)
}

/// `ResolveTildePath` for `~` and `~/...`. A `~user` prefix stays.
fn expand_tilde(host: &dyn Host, path: &[u8]) -> Vec<u8> {
    let end = path
        .iter()
        .position(|&byte| byte == b'/' || byte == b'\\')
        .unwrap_or(path.len());
    if path.first() != Some(&b'~') || end != 1 {
        return path.to_vec();
    }
    let home = cfg!(windows)
        .then(|| host.env_get(b"USERPROFILE"))
        .flatten()
        .or_else(|| host.env_get(b"HOME"));
    let Some(home) = home else {
        return path.to_vec();
    };
    let mut out = home.to_vec();
    out.extend_from_slice(path.get(end..).unwrap_or_default());
    out
}

/// The end of the root of an absolute path (the index of its root `/`), or `None` for
/// a relative path.
fn root_slash(path: &[u8]) -> Option<usize> {
    match path {
        [drive, b':', b'/', ..] if cfg!(windows) && drive.is_ascii_alphabetic() => Some(2),
        [b'/', ..] => Some(0),
        _ => None,
    }
}

/// What to do after one component.
enum Step {
    /// Continue at this slash index.
    Continue(usize),
    /// Resolve the whole path again.
    Restart,
}

/// `cm::PathResolver::Impl` with the `LogicalPath` policy (the `CasePath` policy on
/// Windows), for a file system seen through a [`Host`].
struct Resolver<'a> {
    host: &'a dyn Host,
    path: Vec<u8>,
    links: usize,
}

impl Resolver<'_> {
    fn resolve(&mut self) {
        'restart: loop {
            let Some(root) = root_slash(&self.path) else {
                self.make_absolute();
                continue;
            };
            let mut slash = root;
            while self.path.len() > root + 1 && slash < self.path.len() {
                match self.component(root, slash) {
                    Step::Continue(next) => slash = next,
                    Step::Restart => continue 'restart,
                }
            }
            return;
        }
    }

    /// `ResolveRelativePath`: put the working directory before the path.
    fn make_absolute(&mut self) {
        let mut cwd = rcmake_host::path::from_native(&self.host.initial_cwd()).to_vec();
        cwd.iter_mut().for_each(|byte| {
            if *byte == b'\\' {
                *byte = b'/';
            }
        });
        if root_slash(&cwd).is_none() {
            cwd.insert(0, b'/');
        }
        if cwd.last() != Some(&b'/') {
            cwd.push(b'/');
        }
        cwd.append(&mut self.path);
        self.path = cwd;
    }

    /// `ResolveComponent`: handle the component after the `/` at `slash`.
    fn component(&mut self, root: usize, slash: usize) -> Step {
        let start = slash.saturating_add(1);
        let next = self
            .path
            .get(start..)
            .and_then(|rest| rest.iter().position(|&byte| byte == b'/'))
            .map_or(self.path.len(), |offset| start + offset);
        let name = self.path.get(start..next).unwrap_or_default();
        let dots = name == b"..";
        if name.is_empty() || name == b"." || (dots && slash == root) {
            if slash == root {
                self.erase(start, next.saturating_add(1));
            } else {
                self.erase(slash, next);
            }
            return Step::Continue(slash);
        }
        if dots {
            let previous = self
                .path
                .get(..slash)
                .and_then(|head| head.iter().rposition(|&byte| byte == b'/'))
                .unwrap_or(root);
            if cfg!(unix)
                && let Some(target) = self.read_link(slash)
            {
                return self.follow(previous, slash, target);
            }
            self.erase(previous.saturating_add(1), slash.saturating_add(4));
            return Step::Continue(previous);
        }
        Step::Continue(self.actual_case(slash, next))
    }

    /// Remove `path[from..to]`, with `to` clamped to the end.
    fn erase(&mut self, from: usize, to: usize) {
        let to = to.min(self.path.len());
        if from < to {
            self.path.drain(from..to);
        }
    }

    /// The target of the symbolic link `path[..end]`, if it is one. After
    /// [`MAX_SYMBOLIC_LINKS`] links, no path is a link.
    fn read_link(&mut self, end: usize) -> Option<Vec<u8>> {
        let path = rcmake_host::path::to_native(self.path.get(..end)?);
        let target = self.host.read_link(&path).ok()?;
        self.links = self.links.saturating_add(1);
        if self.links >= MAX_SYMBOLIC_LINKS {
            return None;
        }
        let mut target = rcmake_host::path::from_native(&target).to_vec();
        target.iter_mut().for_each(|byte| {
            if *byte == b'\\' {
                *byte = b'/';
            }
        });
        (!target.is_empty()).then_some(target)
    }

    /// `ResolveSymlink`: put `target` in place of the link `path[slash + 1..next]`.
    fn follow(&mut self, slash: usize, next: usize, target: Vec<u8>) -> Step {
        if root_slash(&target).is_none() {
            let start = slash.saturating_add(1).min(next);
            self.path.splice(start..next, target);
            return Step::Continue(slash);
        }
        self.path.splice(..next, target);
        Step::Restart
    }

    /// Give the existing component `path[slash + 1..next]` its spelling on disk if the
    /// file system ignores case. Return the slash after the component.
    fn actual_case(&mut self, slash: usize, next: usize) -> usize {
        if !cfg!(any(target_os = "macos", target_os = "ios", windows)) {
            return next;
        }
        let (Some(dir), Some(full), Some(name)) = (
            self.path.get(..slash.max(1)),
            self.path.get(..next),
            self.path.get(slash.saturating_add(1)..next),
        ) else {
            return next;
        };
        if self
            .host
            .metadata(&rcmake_host::path::to_native(full), Follow::No)
            .is_err()
        {
            return next;
        }
        let Ok(entries) = self.host.read_dir(&rcmake_host::path::to_native(dir)) else {
            return next;
        };
        if entries.iter().any(|entry| entry.name.as_slice() == name) {
            return next;
        }
        let Some(spelling) = entries
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            .map(|entry| entry.name.to_vec())
        else {
            return next;
        };
        let length = spelling.len();
        self.path.splice(slash.saturating_add(1)..next, spelling);
        slash.saturating_add(1).saturating_add(length)
    }
}

/// An upper bound of the JSON text length of the string `bytes`.
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
        let row = crate::budget::bytes_len(key, MAX_RESPONSE_BYTES)
            .map_or(usize::MAX, |path| path.saturating_add(ROW_BYTES));
        if lines.len().saturating_mul(row) > MAX_RESPONSE_BYTES {
            return Err("Breakpoint response too large");
        }
        if lines.is_empty() {
            self.breakpoints.remove(key);
            self.pending.remove(key);
            return Ok(object([("breakpoints", Value::Array(Vec::new()))]));
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
            if list.is_empty() {
                self.breakpoints.remove(key);
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
    ///
    /// As in CMake, a parse without commands records no command table, so a later parse
    /// of the same source can record one. The calibration of pending breakpoints records
    /// an empty table for such a parse (`ListFileFunctionLines[path]`).
    pub fn source_loaded(&mut self, key: &BString, spans: &[CommandSpan]) -> Vec<Value> {
        if self.spans.contains_key(key) || self.spans.len() >= MAX_LOADED_SOURCES {
            return Vec::new();
        }
        if !spans.is_empty() {
            self.spans.insert(key.clone(), spans.to_vec());
        }
        if !self.pending.remove(key) {
            return Vec::new();
        }
        let Some(list) = self.breakpoints.get_mut(key) else {
            return Vec::new();
        };
        if spans.is_empty() && !list.is_empty() {
            self.spans.insert(key.clone(), Vec::new());
        }
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
        self.breakpoints.is_empty()
    }

    /// Remove all breakpoints (`ClearAll`). Parsed spans stay.
    pub fn clear(&mut self) {
        self.breakpoints.clear();
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn text(value: &Value) -> String {
        String::from_utf8(rcmake_json::write_compact(value).to_vec()).unwrap()
    }

    fn spans(list: &[(u32, u32)]) -> Vec<CommandSpan> {
        list.iter()
            .map(|&(start, end)| CommandSpan { start, end })
            .collect()
    }

    #[test]
    fn response_size_counts_invalid_utf8_escapes() {
        // A lone invalid byte is six bytes of JSON text (`\ufffd`). At three bytes for
        // each, 500 rows of this key would fit in the response budget.
        let key = BString::from(vec![0xff_u8; 4000]);
        let mut manager = BreakpointManager::default();
        assert_eq!(
            manager.set(&key, &[1; 500]),
            Err("Breakpoint response too large")
        );
        assert!(manager.set(&key, &[1; 300]).is_ok());
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
    fn cleared_sources_free_the_source_limit() {
        let mut manager = BreakpointManager::default();
        for index in 0..MAX_SOURCES * 2 {
            let key = BString::from(format!("/f{index}"));
            manager.set(&key, &[1]).unwrap();
            let body = manager.set(&key, &[]).unwrap();
            assert_eq!(text(&body), r#"{"breakpoints":[]}"#);
        }
        assert!(manager.is_empty());
        assert!(manager.pending.is_empty());
        manager.source_loaded(&"/loaded".into(), &spans(&[(5, 8)]));
        // A loaded source whose rows are all unverified keeps no entry.
        manager.set(&"/loaded".into(), &[6]).unwrap();
        assert!(manager.is_empty());
    }

    #[test]
    fn empty_parse_leaves_the_table_absent() {
        let mut manager = BreakpointManager::default();
        let key = BString::from("/s/empty.cmake");
        assert!(manager.source_loaded(&key, &[]).is_empty());
        let body = manager.set(&key, &[2]).unwrap();
        assert!(text(&body).contains(r#""verified":false"#));
        let events = manager.source_loaded(&key, &spans(&[(2, 2)]));
        assert_eq!(
            events.iter().map(text).collect::<Vec<_>>(),
            [
                r#"{"breakpoint":{"id":0,"line":2,"source":{"path":"/s/empty.cmake"},"verified":true},"reason":"changed"}"#
            ]
        );
        assert_eq!(manager.hits(&key, 2), vec![0]);
    }

    #[test]
    fn pending_events_on_an_empty_parse_record_an_empty_table() {
        let mut manager = BreakpointManager::default();
        let key = BString::from("/s/empty.cmake");
        manager.set(&key, &[2]).unwrap();
        let events = manager.source_loaded(&key, &[]);
        assert_eq!(
            events.iter().map(text).collect::<Vec<_>>(),
            [
                r#"{"breakpoint":{"id":0,"line":2,"source":{"path":"/s/empty.cmake"},"verified":false},"reason":"changed"}"#
            ]
        );
        assert!(manager.source_loaded(&key, &spans(&[(2, 2)])).is_empty());
        assert!(manager.hits(&key, 2).is_empty());
    }

    #[test]
    fn oversize_requests_fail_before_work() {
        let long = vec![b'a'; MAX_SOURCE_PATH_BYTES + 1];
        assert_eq!(check_request(&long, &[1]), Err("Source path too long"));
        let lines = vec![1; MAX_BREAKPOINTS_PER_SOURCE + 1];
        assert_eq!(check_request(b"/a", &lines), Err("Too many breakpoints"));
        let mut key = b"/".to_vec();
        key.resize(MAX_SOURCE_PATH_BYTES, b'\x01');
        assert!(check_request(&key, &[1; MAX_BREAKPOINTS_PER_SOURCE]).is_ok());
        let mut manager = BreakpointManager::default();
        assert_eq!(
            manager.set(&key.into(), &[1; MAX_BREAKPOINTS_PER_SOURCE]),
            Err("Breakpoint response too large")
        );
        assert!(manager.is_empty());
    }

    fn host() -> rcmake_host::PureHost {
        let host = rcmake_host::PureHost::new("/w/cwd".into(), "/bin/cmake".into());
        host.seed_file(Path::new("/w/cwd/a.cmake"), b"").unwrap();
        host.seed_file(Path::new("/w/real/sub/b.cmake"), b"")
            .unwrap();
        host.seed_file(Path::new("/home/u/c.cmake"), b"").unwrap();
        host.env_set(b"HOME", Some(b"/home/u"));
        host
    }

    #[test]
    fn keys_follow_the_logical_path_rules() {
        let host = host();
        let key = |path: &str| source_key(&host, path.as_bytes()).to_string();
        assert_eq!(key("/a/./b/../c//d"), "/a/c/d");
        assert_eq!(key("/../a"), "/a");
        assert_eq!(key("a.cmake"), "/w/cwd/a.cmake");
        assert_eq!(key("../real/x/../sub/b.cmake"), "/w/real/sub/b.cmake");
        assert_eq!(key(""), "/w/cwd");
        assert_eq!(key("~/c.cmake"), "/home/u/c.cmake");
        assert_eq!(key("~"), "/home/u");
        assert_eq!(key("~other/c.cmake"), "/w/cwd/~other/c.cmake");
        assert_eq!(key("\\w\\cwd\\a.cmake"), "/w/cwd/a.cmake");
        assert_eq!(key("/a/b/c:3:EVAL"), "/a/b/c:3:EVAL");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_before_a_parent_step_resolves_first() {
        let host = host();
        host.symlink(Path::new("/w/real/sub"), Path::new("/w/abs"))
            .unwrap();
        host.symlink(Path::new("real/sub"), Path::new("/w/rel"))
            .unwrap();
        let key = |path: &str| source_key(&host, path.as_bytes()).to_string();
        assert_eq!(key("/w/abs/../sub/b.cmake"), "/w/real/sub/b.cmake");
        assert_eq!(key("/w/rel/../sub/b.cmake"), "/w/real/sub/b.cmake");
        assert_eq!(key("/w/abs/b.cmake"), "/w/abs/b.cmake");
        host.symlink(Path::new("loop"), Path::new("/w/loop"))
            .unwrap();
        assert_eq!(key("/w/loop/../x"), "/w/x");
    }

    #[test]
    fn a_case_sensitive_file_system_keeps_the_spelling() {
        let host = host();
        let key = |path: &str| source_key(&host, path.as_bytes()).to_string();
        assert_eq!(key("/w/CWD/a.cmake"), "/w/CWD/a.cmake");
        assert_eq!(key("/w/cwd/A.cmake"), "/w/cwd/A.cmake");
    }
}
