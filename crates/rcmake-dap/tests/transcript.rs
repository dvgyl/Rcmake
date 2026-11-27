//! Scripted client transcripts over the memory transport, with an interpreter on
//! `PureHost`. Each test covers one frame or stepping rule of CMake 4.5.1, or one
//! session lifecycle case.

// Test helpers outside `#[test]` functions fail the test on unexpected data.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rcmake_dap::transport::memory::{self, MemoryClient, MemoryOptions};
use rcmake_dap::{Config, DebugServer};
use rcmake_eval::{BuiltinFn, CmdError, CmdFlags, Flow, Interp, InterpConfig, Registry, RunMode};
use rcmake_host::PureHost;
use rcmake_json::Value;

const LIMIT: Duration = Duration::from_secs(20);

fn register(reg: &mut Registry) {
    let flags = CmdFlags { scriptable: true };
    let set: BuiltinFn = |cx, args| {
        let Some((name, rest)) = args.split_first() else {
            return Err(CmdError::status(
                "called with incorrect number of arguments",
            ));
        };
        let value: Vec<Vec<u8>> = rest.iter().map(|arg| arg.value.to_vec()).collect();
        cx.set_var(&name.value, value.join(&b';').into());
        Ok(Flow::Normal)
    };
    let language: BuiltinFn = |cx, args| match args.first().map(|arg| arg.value.as_slice()) {
        Some(b"EVAL") => {
            let code: Vec<u8> = args
                .iter()
                .skip(2)
                .flat_map(|arg| arg.value.to_vec())
                .collect();
            cx.eval_code(&code)
        }
        Some(b"CALL") => {
            let name = args.get(1).map(|arg| arg.value.clone()).unwrap_or_default();
            cx.call_command(&name, args.get(2..).unwrap_or_default().to_vec())
        }
        _ => Err(CmdError::status("unknown mode")),
    };
    let subdirectory: BuiltinFn = |cx, args| {
        let (Some(src), Some(bin)) = (args.first(), args.get(1)) else {
            return Err(CmdError::status("needs two paths"));
        };
        let src = PathBuf::from(String::from_utf8_lossy(&src.value).into_owned());
        let bin = PathBuf::from(String::from_utf8_lossy(&bin.value).into_owned());
        cx.run_subdirectory(&src, &bin, rcmake_eval::SubdirOptions::default())?;
        Ok(Flow::Normal)
    };
    let noop: BuiltinFn = |_, _| Ok(Flow::Normal);
    reg.add("set", set, flags);
    reg.add("cmake_language", language, flags);
    reg.add(
        "add_subdirectory",
        subdirectory,
        CmdFlags { scriptable: false },
    );
    reg.add("project", noop, CmdFlags { scriptable: false });
    reg.add("cmake_minimum_required", noop, flags);
}

fn host(files: &[(&str, &str)]) -> Arc<PureHost> {
    let host = Arc::new(PureHost::new(
        PathBuf::from("/w"),
        PathBuf::from("/bin/cmake"),
    ));
    for (path, text) in files {
        host.seed_file(Path::new(path), text.as_bytes()).unwrap();
    }
    host.seed_file(Path::new("/bin/cmake"), b"binary").unwrap();
    host
}

fn interp(host: &Arc<PureHost>, mode: RunMode) -> Interp {
    let config = InterpConfig::new(host.clone(), mode, "/share/cmake-4.5", "/bin/cmake", "/w");
    Interp::new(config, register)
}

/// What the evaluator thread runs.
#[derive(Clone, Copy)]
enum Run {
    Script(&'static str),
    Configure,
}

fn execute(host: &Arc<PureHost>, run: Run, server: &DebugServer) -> i32 {
    match run {
        Run::Script(path) => {
            let mut interp = interp(host, RunMode::Script);
            interp.set_debug_hook(Some(server.hook()));
            interp.run_script(Path::new(path)).exit_code
        }
        Run::Configure => {
            let mut interp = interp(host, RunMode::Configure);
            interp.set_debug_hook(Some(server.hook()));
            interp
                .run_configure(Path::new("/w/src"), Path::new("/w/build"), &[])
                .exit_code
        }
    }
}

struct Session {
    client: Client,
    done: JoinHandle<(i32, Vec<u8>)>,
}

fn config(host: &Arc<PureHost>, stdout: &Arc<Mutex<Vec<u8>>>) -> Config {
    let sink = Arc::clone(stdout);
    Config {
        host: host.clone(),
        stdout: Arc::new(move |bytes: &[u8]| sink.lock().unwrap().extend_from_slice(bytes)),
        log: None,
        handshake_limit: Some(LIMIT),
        exit_limit: Some(LIMIT),
    }
}

fn start_with(files: &[(&str, &str)], run: Run, options: MemoryOptions) -> Session {
    let host = host(files);
    let (listener, client) = memory::pair(options);
    let done = std::thread::spawn(move || {
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let server = DebugServer::start(Box::new(listener), config(&host, &stdout));
        let code = execute(&host, run, &server);
        server.finish(code);
        let out = stdout.lock().unwrap().clone();
        (code, out)
    });
    Session {
        client: Client::new(client),
        done,
    }
}

fn start(files: &[(&str, &str)], run: Run) -> Session {
    start_with(files, run, MemoryOptions::default())
}

/// Run `test` with a time limit, so that a deadlock fails instead of hanging.
fn bounded(test: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        test();
        let _ = tx.send(());
    });
    match rx.recv_timeout(LIMIT) {
        Ok(()) => handle.join().unwrap(),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            if let Err(panic) = handle.join() {
                std::panic::resume_unwind(panic);
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("test did not finish in time"),
    }
}

fn get<'a>(value: &'a Value, path: &[&str]) -> &'a Value {
    path.iter().fold(value, |value, key| {
        value
            .get(key)
            .unwrap_or_else(|| panic!("no {key} in {}", text(value)))
    })
}

fn text(value: &Value) -> String {
    String::from_utf8(rcmake_json::write_compact(value).to_vec()).unwrap()
}

fn string(value: &Value) -> String {
    value.as_str().unwrap().to_owned()
}

struct Client {
    inner: MemoryClient,
    seq: i64,
    pending: VecDeque<Value>,
    closed: bool,
    /// The evaluator is at a stop that the test saw.
    stopped: bool,
}

impl Client {
    fn new(inner: MemoryClient) -> Self {
        Self {
            inner,
            seq: 0,
            pending: VecDeque::new(),
            closed: false,
            stopped: false,
        }
    }

    fn send(&mut self, command: &str, arguments: &str) -> i64 {
        self.seq += 1;
        let body = if arguments.is_empty() {
            format!(
                r#"{{"seq":{},"type":"request","command":"{command}"}}"#,
                self.seq
            )
        } else {
            format!(
                r#"{{"seq":{},"type":"request","command":"{command}","arguments":{arguments}}}"#,
                self.seq
            )
        };
        self.inner.send(body.as_bytes()).unwrap();
        self.seq
    }

    fn read(&mut self) -> Option<Value> {
        if self.closed {
            return None;
        }
        match self.inner.recv() {
            Ok(Some(body)) => Some(rcmake_json::parse(&body, &Default::default()).unwrap()),
            _ => {
                self.closed = true;
                None
            }
        }
    }

    /// The first message (queued or new) that matches `predicate`.
    fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Option<Value> {
        if let Some(index) = self.pending.iter().position(&predicate) {
            return self.pending.remove(index);
        }
        while let Some(message) = self.read() {
            if predicate(&message) {
                return Some(message);
            }
            self.pending.push_back(message);
        }
        None
    }

    fn event(&mut self, name: &str) -> Value {
        self.until(|m| m.get("event").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("no {name} event"))
    }

    fn response(&mut self, seq: i64) -> Value {
        self.until(|m| {
            m.get("type").and_then(Value::as_str) == Some("response")
                && m.get("request_seq").and_then(Value::as_i64) == Some(seq)
        })
        .unwrap_or_else(|| panic!("no response to {seq}"))
    }

    fn request(&mut self, command: &str, arguments: &str) -> Value {
        let seq = self.send(command, arguments);
        self.response(seq)
    }

    fn handshake(&mut self, breakpoints: &[(&str, &[i64])], filters: &[&str]) -> Vec<Value> {
        let init = self.request(
            "initialize",
            r#"{"adapterID":"t","supportsVariableType":true}"#,
        );
        assert!(get(&init, &["success"]).as_bool().unwrap());
        self.event("initialized");
        let mut out = Vec::new();
        for (path, lines) in breakpoints {
            let lines: Vec<String> = lines.iter().map(|n| format!(r#"{{"line":{n}}}"#)).collect();
            out.push(self.request(
                "setBreakpoints",
                &format!(
                    r#"{{"source":{{"path":"{path}"}},"breakpoints":[{}]}}"#,
                    lines.join(",")
                ),
            ));
        }
        let filters: Vec<String> = filters.iter().map(|f| format!("\"{f}\"")).collect();
        self.request(
            "setExceptionBreakpoints",
            &format!(r#"{{"filters":[{}]}}"#, filters.join(",")),
        );
        self.request("configurationDone", "");
        self.event("thread");
        out
    }

    /// Wait for the next stop. Return its reason, or `None` at `terminated`.
    fn stop(&mut self) -> Option<Value> {
        let message = self.until(|m| {
            matches!(
                m.get("event").and_then(Value::as_str),
                Some("stopped" | "terminated")
            )
        })?;
        let stopped = message.get("event").and_then(Value::as_str) == Some("stopped");
        self.stopped = stopped;
        stopped.then(|| get(&message, &["body"]).clone())
    }

    fn reason(&mut self) -> String {
        let body = self.stop().expect("a stop");
        string(get(&body, &["reason"]))
    }

    /// The stack as `name(args) Line: n` strings, innermost first.
    fn stack(&mut self) -> Vec<String> {
        let response = self.request(
            "stackTrace",
            r#"{"threadId":1,"format":{"parameters":true,"parameterValues":true,"line":true}}"#,
        );
        get(&response, &["body", "stackFrames"])
            .as_array()
            .unwrap()
            .iter()
            .map(|frame| string(get(frame, &["name"])))
            .collect()
    }

    fn frame_ids(&mut self) -> Vec<i64> {
        let response = self.request("stackTrace", r#"{"threadId":1}"#);
        get(&response, &["body", "stackFrames"])
            .as_array()
            .unwrap()
            .iter()
            .map(|frame| get(frame, &["id"]).as_i64().unwrap())
            .collect()
    }

    fn evaluate(&mut self, frame: i64, expression: &str) -> String {
        let response = self.request(
            "evaluate",
            &format!(r#"{{"expression":"{expression}","frameId":{frame}}}"#),
        );
        string(get(&response, &["body", "result"]))
    }

    /// Rows `(name, value)` of a variables reference.
    fn rows(&mut self, reference: i64) -> Vec<(String, String, i64)> {
        let response = self.request(
            "variables",
            &format!(r#"{{"variablesReference":{reference}}}"#),
        );
        get(&response, &["body", "variables"])
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    string(get(row, &["name"])),
                    string(get(row, &["value"])),
                    get(row, &["variablesReference"]).as_i64().unwrap(),
                )
            })
            .collect()
    }

    fn locals(&mut self, frame: i64) -> Vec<(String, String, i64)> {
        let scopes = self.request("scopes", &format!(r#"{{"frameId":{frame}}}"#));
        let scope = &get(&scopes, &["body", "scopes"]).as_array().unwrap()[0];
        let root = self.rows(get(scope, &["variablesReference"]).as_i64().unwrap());
        let locals = root.iter().find(|row| row.0 == "Locals").unwrap().2;
        self.rows(locals)
    }

    fn action(&mut self, command: &str) {
        self.stopped = false;
        self.request(command, r#"{"threadId":1}"#);
    }

    /// Continue to the end and disconnect. Return the exit code event value.
    fn finish(&mut self) -> i64 {
        if self.stopped {
            self.action("continue");
        }
        while self.stop().is_some() {
            self.action("continue");
        }
        let seq = self.send("disconnect", "{}");
        self.response(seq);
        let exited = self.event("exited");
        get(&exited, &["body", "exitCode"]).as_i64().unwrap()
    }
}

fn wait(session: Session) -> (i32, String) {
    let (code, out) = session.done.join().unwrap();
    (code, String::from_utf8(out).unwrap())
}

#[test]
fn initialize_response_precedes_initialized_event() {
    bounded(|| {
        let mut session = start(
            &[("/w/main.cmake", "set(A 1)\n")],
            Run::Script("main.cmake"),
        );
        let client = &mut session.client;
        client.send("initialize", r#"{"adapterID":"t"}"#);
        let first = client.read().unwrap();
        assert_eq!(string(get(&first, &["type"])), "response");
        assert_eq!(
            string(get(&first, &["body", "cmakeVersion", "full"])),
            "4.5.1"
        );
        let filters: Vec<(String, bool)> = get(&first, &["body", "exceptionBreakpointFilters"])
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    string(get(f, &["filter"])),
                    get(f, &["default"]).as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            filters,
            [
                ("LOG".into(), false),
                ("WARNING".into(), false),
                ("MESSAGE".into(), false),
                ("INTERNAL_ERROR".into(), true),
                ("FATAL_ERROR".into(), true),
            ]
        );
        let second = client.read().unwrap();
        assert_eq!(string(get(&second, &["event"])), "initialized");
        client.request("configurationDone", "");
        let thread = client.event("thread");
        assert_eq!(string(get(&thread, &["body", "reason"])), "started");
        assert_eq!(client.finish(), 0);
        let (code, out) = wait(session);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "Waiting for debugger client to connect...\nDebugger client connected.\n"
        );
    });
}

#[test]
fn exit_events_are_ordered() {
    bounded(|| {
        let mut session = start(
            &[("/w/main.cmake", "set(A 1)\n")],
            Run::Script("main.cmake"),
        );
        session.client.handshake(&[], &[]);
        let names: Vec<String> = (0..3)
            .map(|_| {
                let message = session.client.read().unwrap();
                string(get(&message, &["event"]))
            })
            .collect();
        assert_eq!(names, ["thread", "exited", "terminated"]);
        let threads = session.client.request("threads", "");
        assert_eq!(text(get(&threads, &["body"])), r#"{"threads":[]}"#);
        session.client.request("disconnect", "{}");
        assert_eq!(wait(session).0, 0);
    });
}

#[test]
fn breakpoint_next_and_variables() {
    bounded(|| {
        let main = "set(X 1)\nset(Y 2)\nset(Z 3)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        let set = client.handshake(&[("/w/main.cmake", &[2])], &[]);
        assert_eq!(
            text(get(&set[0], &["body"])),
            r#"{"breakpoints":[{"id":0,"line":2,"source":{"path":"/w/main.cmake"},"verified":false}]}"#
        );
        let changed = client.event("breakpoint");
        assert_eq!(
            text(get(&changed, &["body"])),
            r#"{"breakpoint":{"id":0,"line":2,"source":{"path":"/w/main.cmake"},"verified":true},"reason":"changed"}"#
        );
        let stop = client.stop().unwrap();
        assert_eq!(
            text(&stop),
            r#"{"allThreadsStopped":true,"hitBreakpointIds":[0],"reason":"breakpoint","threadId":1}"#
        );
        assert_eq!(client.stack(), ["set(Y, 2) Line: 2"]);
        let frame = client.frame_ids()[0];
        let locals = client.locals(frame);
        assert!(locals.iter().any(|row| row.0 == "X" && row.1 == "1"));
        assert!(!locals.iter().any(|row| row.0 == "Y"));
        assert_eq!(client.evaluate(frame, "X"), "1");
        assert_eq!(client.evaluate(frame, "NOPE"), "");
        let unknown = client.request("stackTrace", r#"{"threadId":7}"#);
        assert_eq!(string(get(&unknown, &["message"])), "Unknown threadId '7'");
        client.action("next");
        assert_eq!(client.reason(), "step");
        assert_eq!(client.stack(), ["set(Z, 3) Line: 3"]);
        assert_eq!(client.finish(), 0);
        assert_eq!(wait(session).0, 0);
    });
}

#[test]
fn include_frames_and_empty_include() {
    bounded(|| {
        let main = "include(/w/inc.cmake)\ninclude(/w/empty.cmake)\nset(B 2)\n";
        let files = [
            ("/w/main.cmake", main),
            ("/w/inc.cmake", "set(I1 x)\nset(I2\n  y)\n"),
            ("/w/empty.cmake", "# nothing\n"),
        ];
        let mut session = start(&files, Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/main.cmake", &[1])], &[]);
        assert_eq!(client.reason(), "breakpoint");
        client.action("stepIn");
        assert_eq!(client.reason(), "step");
        // The first child command is at depth 2 with the caller beneath it, and no
        // line-0 file frame between them.
        assert_eq!(
            client.stack(),
            ["set(I1, x) Line: 1", "include(/w/inc.cmake) Line: 1"]
        );
        client.action("stepOut");
        assert_eq!(client.reason(), "step");
        assert_eq!(client.stack(), ["include(/w/empty.cmake) Line: 2"]);
        // An empty file has no command: step in reaches the next caller command.
        client.action("stepIn");
        assert_eq!(client.reason(), "step");
        assert_eq!(client.stack(), ["set(B, 2) Line: 3"]);
        assert_eq!(client.finish(), 0);
        wait(session);
    });
}

#[test]
fn function_and_macro_frames() {
    bounded(|| {
        let main = "function(f a)\n  set(L ${a})\nendfunction()\n\
                    macro(m x)\n  set(M ${x})\nendmacro()\n\
                    f(1)\nm(3)\nset(END 1)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/main.cmake", &[7])], &[]);
        assert_eq!(client.reason(), "breakpoint");
        // `next` over a call does not stop inside it.
        client.action("next");
        assert_eq!(client.reason(), "step");
        assert_eq!(client.stack(), ["m(3) Line: 8"]);
        // The macro body has the definition line and the substituted arguments.
        client.action("stepIn");
        assert_eq!(client.reason(), "step");
        assert_eq!(client.stack(), ["set(M, 3) Line: 5", "m(3) Line: 8"]);
        client.action("stepOut");
        assert_eq!(client.stack(), ["set(END, 1) Line: 9"]);
        assert_eq!(client.finish(), 0);
        wait(session);
    });
    bounded(|| {
        let main = "function(f a)\n  set(L ${a})\nendfunction()\nf(1)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/main.cmake", &[4])], &[]);
        client.stop();
        client.action("stepIn");
        assert_eq!(client.stack(), ["set(L, ${a}) Line: 2", "f(1) Line: 4"]);
        let ids = client.frame_ids();
        assert_eq!(client.evaluate(ids[0], "a"), "1");
        // Both frames of one directory see the live function scope, as in CMake.
        assert_eq!(client.evaluate(ids[1], "a"), "1");
        assert_eq!(client.finish(), 0);
        wait(session);
    });
}

#[test]
fn next_on_block_stops_inside_the_block() {
    bounded(|| {
        let main = "block()\n  set(V 1)\nendblock()\nset(Z 3)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/main.cmake", &[1])], &[]);
        client.stop().unwrap();
        assert_eq!(client.stack(), ["block() Line: 1"]);
        client.action("next");
        assert_eq!(client.reason(), "step");
        assert_eq!(client.stack(), ["set(V, 1) Line: 2"]);
        // A normal `endblock()` does not stop.
        client.action("next");
        assert_eq!(client.stack(), ["set(Z, 3) Line: 4"]);
        assert_eq!(client.finish(), 0);
        wait(session);
    });
}

#[test]
fn call_stops_twice_on_its_line() {
    bounded(|| {
        let main = "function(g x)\n  set(G ${x})\nendfunction()\ncmake_language(CALL g 7)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/main.cmake", &[4])], &[]);
        assert_eq!(client.reason(), "breakpoint");
        assert_eq!(client.stack(), ["cmake_language(CALL, g, 7) Line: 4"]);
        client.action("continue");
        assert_eq!(client.reason(), "breakpoint");
        assert_eq!(
            client.stack(),
            ["g(7) Line: 4", "cmake_language(CALL, g, 7) Line: 4"]
        );
        client.action("stepIn");
        assert_eq!(client.stack().len(), 3);
        assert_eq!(client.finish(), 0);
        wait(session);
    });
}

#[test]
fn eval_frames_name_the_virtual_source() {
    bounded(|| {
        let main = "cmake_language(EVAL CODE \"set(E1 1)\")\nset(AFTER 1)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/main.cmake", &[1])], &[]);
        client.stop();
        client.action("stepIn");
        assert_eq!(client.reason(), "step");
        let response = client.request("stackTrace", r#"{"threadId":1}"#);
        let frames = get(&response, &["body", "stackFrames"]).as_array().unwrap();
        assert_eq!(
            string(get(&frames[0], &["source", "path"])),
            "/w/main.cmake:1:EVAL"
        );
        assert_eq!(frames.len(), 2);
        assert_eq!(client.finish(), 0);
        wait(session);
    });
}

#[test]
fn parse_exception_shows_the_file_frame_but_eval_error_does_not() {
    bounded(|| {
        let files = [
            ("/w/main.cmake", "include(/w/bad.cmake)\n"),
            ("/w/bad.cmake", "set(A\n"),
        ];
        let mut session = start(&files, Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[], &["FATAL_ERROR"]);
        let stop = client.stop().unwrap();
        assert_eq!(string(get(&stop, &["reason"])), "exception");
        assert_eq!(string(get(&stop, &["description"])), "Pause on exception");
        assert!(string(get(&stop, &["text"])).starts_with("CMake Error at bad.cmake:1:"));
        assert_eq!(
            client.stack(),
            ["/w/bad.cmake() Line: 0", "include(/w/bad.cmake) Line: 1"]
        );
        let info = client.request("exceptionInfo", r#"{"threadId":1}"#);
        assert_eq!(string(get(&info, &["body", "exceptionId"])), "FATAL_ERROR");
        client.finish();
        wait(session);
    });
    bounded(|| {
        let main = "cmake_language(EVAL CODE \"set(A\")\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[], &["FATAL_ERROR"]);
        assert_eq!(client.reason(), "exception");
        assert_eq!(
            client.stack(),
            ["cmake_language(EVAL, CODE, set(A) Line: 1"]
        );
        client.action("continue");
        // `cmake_language` then reports `unknown error.`.
        let stop = client.stop().unwrap();
        assert!(string(get(&stop, &["text"])).contains("cmake_language unknown error."));
        assert_eq!(client.finish(), 1);
        assert_eq!(wait(session).0, 1);
    });
}

#[test]
fn warning_and_notice_filters_stop() {
    bounded(|| {
        let main = "message(WARNING \"careful\")\nmessage(NOTICE \"plain\")\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[], &["WARNING", "MESSAGE"]);
        let warning = client.stop().unwrap();
        assert_eq!(
            string(get(&warning, &["text"])),
            "CMake Warning at main.cmake:1 (message):\n  careful\n\n"
        );
        client.action("continue");
        let notice = client.stop().unwrap();
        assert_eq!(string(get(&notice, &["text"])), "plain");
        assert_eq!(client.finish(), 0);
        wait(session);
    });
}

#[test]
fn parent_and_child_directory_frames_see_their_own_variables() {
    bounded(|| {
        let files = [
            (
                "/w/src/CMakeLists.txt",
                "project(P)\nset(PV parent)\nadd_subdirectory(/w/src/sub /w/build/sub)\n",
            ),
            (
                "/w/src/sub/CMakeLists.txt",
                "set(CV child)\nset(PV shadow)\nset(LAST 1)\n",
            ),
        ];
        let mut session = start(&files, Run::Configure);
        let client = &mut session.client;
        client.handshake(&[("/w/src/sub/CMakeLists.txt", &[3])], &[]);
        assert_eq!(client.reason(), "breakpoint");
        let ids = client.frame_ids();
        assert_eq!(ids.len(), 2);
        assert_eq!(client.evaluate(ids[0], "PV"), "shadow");
        assert_eq!(client.evaluate(ids[0], "CV"), "child");
        assert_eq!(client.evaluate(ids[1], "PV"), "parent");
        assert_eq!(client.evaluate(ids[1], "CV"), "");
        let parent = client.locals(ids[1]);
        assert!(!parent.iter().any(|row| row.0 == "CV"));
        let child = client.locals(ids[0]);
        assert!(child.iter().any(|row| row.0 == "CV"));
        assert_eq!(client.finish(), 0);
        wait(session);
    });
}

#[test]
fn root_step_out_runs_to_the_end() {
    bounded(|| {
        let main = "set(A 1)\nset(B 2)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/main.cmake", &[1])], &[]);
        client.stop();
        client.action("stepOut");
        assert!(client.stop().is_none());
        client.request("disconnect", "{}");
        assert_eq!(wait(session).0, 0);
    });
}

#[test]
fn breakpoint_interrupts_next() {
    bounded(|| {
        let main = "function(f)\n  set(IN 1)\nendfunction()\nf()\nset(OUT 1)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/main.cmake", &[2, 4])], &[]);
        client.stop().unwrap();
        assert_eq!(client.stack(), ["f() Line: 4"]);
        client.action("next");
        let stop = client.stop().unwrap();
        assert_eq!(string(get(&stop, &["reason"])), "breakpoint");
        assert_eq!(client.stack(), ["set(IN, 1) Line: 2", "f() Line: 4"]);
        // The breakpoint cleared the step request: `continue` runs to the end.
        client.action("continue");
        assert!(client.stop().is_none());
        client.request("disconnect", "{}");
        wait(session);
    });
}

#[test]
fn pending_breakpoints_calibrate_on_load() {
    bounded(|| {
        let files = [
            ("/w/main.cmake", "include(/w/inc.cmake)\n"),
            ("/w/inc.cmake", "set(P1 1)\n\nset(P2\n  2\n  3)\n"),
        ];
        let mut session = start(&files, Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/inc.cmake", &[2, 4, 5, 9])], &[]);
        let changed: Vec<String> = (0..4)
            .map(|_| {
                let event = client.event("breakpoint");
                let point = get(&event, &["body", "breakpoint"]);
                format!(
                    "{}:{}:{}",
                    get(point, &["id"]).as_i64().unwrap(),
                    get(point, &["line"]).as_i64().unwrap(),
                    get(point, &["verified"]).as_bool().unwrap()
                )
            })
            .collect();
        assert_eq!(changed, ["0:3:true", "1:4:false", "2:3:true", "3:3:true"]);
        assert_eq!(client.reason(), "breakpoint");
        assert_eq!(client.stack()[0], "set(P2, 2, 3) Line: 3");
        assert_eq!(client.finish(), 0);
        wait(session);
    });
}

#[test]
fn disconnect_while_stopped_finishes_the_run() {
    bounded(|| {
        let main = "set(A 1)\nmessage(WARNING \"w\")\nset(B 2)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/main.cmake", &[1, 3])], &["WARNING"]);
        client.stop();
        let response = client.request("disconnect", "{}");
        assert!(get(&response, &["success"]).as_bool().unwrap());
        assert_eq!(wait(session).0, 0);
    });
}

#[test]
fn eof_before_configuration_done_runs_without_debugger() {
    bounded(|| {
        let main = "set(A 1)\nmessage(STATUS done)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        session.client.request("initialize", r#"{"adapterID":"t"}"#);
        session.client.inner.close();
        assert_eq!(wait(session).0, 0);
    });
}

#[test]
fn eof_while_stopped_resumes() {
    bounded(|| {
        let main = "set(A 1)\nset(B 2)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        session.client.handshake(&[("/w/main.cmake", &[1, 2])], &[]);
        session.client.stop();
        session.client.inner.close_write();
        assert_eq!(wait(session).0, 0);
    });
}

#[test]
fn session_error_ends_the_session() {
    bounded(|| {
        let main = "set(A 1)\nset(B 2)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        session.client.handshake(&[("/w/main.cmake", &[1])], &[]);
        session.client.stop();
        // `next` without `threadId` does not deserialize, as in `cppdap`.
        session.client.send("next", "");
        let (code, out) = wait(session);
        assert_eq!(code, 0);
        assert!(
            out.ends_with("[CMake Debugger] DAP session error: Failed to deserialize request\n")
        );
    });
}

#[test]
fn stale_queries_racing_with_resume_get_answers() {
    bounded(|| {
        let main = "set(A 1)\nset(B 2)\nset(C 3)\n";
        let mut session = start(&[("/w/main.cmake", main)], Run::Script("main.cmake"));
        let client = &mut session.client;
        client.handshake(&[("/w/main.cmake", &[1, 2, 3])], &[]);
        for _ in 0..3 {
            client.stop().unwrap();
            let frame = client.frame_ids()[0];
            // Queue queries and a resume without waiting. Every query gets a response.
            let mut seqs = Vec::new();
            for _ in 0..20 {
                seqs.push(client.send("scopes", &format!(r#"{{"frameId":{frame}}}"#)));
                seqs.push(client.send(
                    "evaluate",
                    &format!(r#"{{"expression":"A","frameId":{frame}}}"#),
                ));
                seqs.push(client.send("variables", r#"{"variablesReference":1}"#));
            }
            seqs.push(client.send("continue", r#"{"threadId":1}"#));
            for _ in 0..5 {
                seqs.push(client.send("variables", r#"{"variablesReference":1}"#));
            }
            for seq in seqs {
                let response = client.response(seq);
                assert!(get(&response, &["success"]).as_bool().unwrap());
            }
        }
        assert_eq!(client.finish(), 0);
        wait(session);
    });
}

#[test]
fn stalled_client_does_not_block_the_interpreter() {
    bounded(|| {
        // The client reads nothing. The pipe to it holds 64 bytes, and a blocked write
        // fails after 200 ms, which ends the session.
        let options = MemoryOptions {
            client_buffer: 64,
            write_timeout: Duration::from_millis(200),
            ..MemoryOptions::default()
        };
        let main = "set(A 1)\nset(B 2)\n";
        let session = start_with(
            &[("/w/main.cmake", main)],
            Run::Script("main.cmake"),
            options,
        );
        let client = &session.client.inner;
        for (seq, body) in [
            r#""command":"initialize","arguments":{"adapterID":"t"}"#,
            r#""command":"setBreakpoints","arguments":{"source":{"path":"/w/main.cmake"},"breakpoints":[{"line":1},{"line":2}]}"#,
            r#""command":"configurationDone""#,
        ]
        .iter()
        .enumerate()
        {
            client
                .send(format!(r#"{{"seq":{seq},"type":"request",{body}}}"#).as_bytes())
                .unwrap();
        }
        // The stalled write, not a malformed request, ends the session.
        let (code, out) = wait(session);
        assert_eq!(code, 0);
        assert!(!out.contains("session error"), "{out}");
    });
}

#[test]
fn two_invocations_in_one_process() {
    for _ in 0..2 {
        breakpoint_next_and_variables();
    }
}

#[test]
fn hook_moves_between_interpreters_of_one_invocation() {
    bounded(|| {
        // A `-C` script interpreter gets the hook, reports to the same session, and
        // drops: the session stays active for the main interpreter.
        let host = host(&[
            ("/w/pre.cmake", "set(P 1)\n"),
            ("/w/main.cmake", "set(M 1)\n"),
        ]);
        let (listener, client) = memory::pair(MemoryOptions::default());
        let mut client = Client::new(client);
        let worker = std::thread::spawn({
            let host = host.clone();
            move || {
                let stdout = Arc::new(Mutex::new(Vec::new()));
                let server = DebugServer::start(Box::new(listener), config(&host, &stdout));
                let mut main = interp(&host, RunMode::Script);
                main.set_debug_hook(Some(server.hook()));
                {
                    let mut preload = interp(&host, RunMode::Script);
                    preload.set_debug_hook(main.take_debug_hook());
                    assert!(!main.has_debugger());
                    preload.run_script(Path::new("pre.cmake"));
                    main.set_debug_hook(preload.take_debug_hook());
                }
                assert!(server.is_active());
                assert!(main.has_debugger() && main.has_command_observers());
                let code = main.run_script(Path::new("main.cmake")).exit_code;
                server.finish(code);
                code
            }
        });
        client.handshake(&[("/w/pre.cmake", &[1]), ("/w/main.cmake", &[1])], &[]);
        client.stop().unwrap();
        assert_eq!(client.stack(), ["set(P, 1) Line: 1"]);
        client.action("continue");
        assert_eq!(client.reason(), "breakpoint");
        assert_eq!(client.stack(), ["set(M, 1) Line: 1"]);
        assert_eq!(client.finish(), 0);
        assert_eq!(worker.join().unwrap(), 0);
    });
}

#[test]
fn inspection_does_not_mark_variables_read() {
    bounded(|| {
        // `X` is a cache entry that the script never reads; `Y` is set and read.
        let host = host(&[("/w/main.cmake", "set(Y 2)\nset(Z ${Y})\n")]);
        let unread_after = |interp: &mut Interp| -> Vec<String> {
            interp
                .unread_variables()
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .collect()
        };
        let prepare = |interp: &mut Interp| {
            interp.cx().cache().insert(
                "X".into(),
                rcmake_model::CacheEntry {
                    value: "1".into(),
                    ty: rcmake_model::CacheType::String,
                    help: Default::default(),
                    advanced: false,
                    props: Default::default(),
                },
            );
            interp.track_variable_reads(["X".into(), "Y".into()]);
        };
        let mut control = interp(&host, RunMode::Script);
        prepare(&mut control);
        control.run_script(Path::new("main.cmake"));
        let expected = unread_after(&mut control);
        assert_eq!(expected, ["X"]);
        let (listener, client) = memory::pair(MemoryOptions::default());
        let mut client = Client::new(client);
        let worker = std::thread::spawn({
            let host = host.clone();
            move || {
                let stdout = Arc::new(Mutex::new(Vec::new()));
                let server = DebugServer::start(Box::new(listener), config(&host, &stdout));
                let mut interp = interp(&host, RunMode::Script);
                prepare(&mut interp);
                interp.set_debug_hook(Some(server.hook()));
                let code = interp.run_script(Path::new("main.cmake")).exit_code;
                let unread = unread_after(&mut interp);
                server.finish(code);
                unread
            }
        });
        client.handshake(&[("/w/main.cmake", &[2])], &[]);
        client.stop().unwrap();
        let frame = client.frame_ids()[0];
        // `evaluate` and the scope tree read `X` without marking it read.
        assert_eq!(client.evaluate(frame, "X"), "1");
        assert!(client.locals(frame).iter().any(|row| row.0 == "Y"));
        let scopes = client.request("scopes", &format!(r#"{{"frameId":{frame}}}"#));
        let scope = &get(&scopes, &["body", "scopes"]).as_array().unwrap()[0];
        let root = client.rows(get(scope, &["variablesReference"]).as_i64().unwrap());
        let cache = root.iter().find(|row| row.0 == "CacheVariables").unwrap().2;
        assert!(
            client
                .rows(cache)
                .iter()
                .any(|row| row.0 == "X:STRING" && row.1 == "1")
        );
        client.finish();
        assert_eq!(worker.join().unwrap(), expected);
    });
}

#[test]
fn attached_debugger_disables_incremental_replay() {
    let host = host(&[("/w/src/CMakeLists.txt", "set(A 1)\n")]);
    let mut plain = interp(&host, RunMode::Configure);
    let (outcome, _) =
        plain.configure_incremental(Path::new("/w/src"), Path::new("/w/build"), &[], None);
    assert_eq!(outcome.exit_code, 0);
    assert!(!plain.has_debugger());

    let (listener, client) = memory::pair(MemoryOptions::default());
    let mut client = Client::new(client);
    let worker = std::thread::spawn({
        let host = host.clone();
        move || {
            let stdout = Arc::new(Mutex::new(Vec::new()));
            let server = DebugServer::start(Box::new(listener), config(&host, &stdout));
            let mut interp = interp(&host, RunMode::Configure);
            interp.set_debug_hook(Some(server.hook()));
            let (outcome, frame) =
                interp.configure_incremental(Path::new("/w/src"), Path::new("/w/build"), &[], None);
            server.finish(outcome.exit_code);
            (interp.has_command_observers(), frame.is_none())
        }
    });
    client.handshake(&[("/w/src/CMakeLists.txt", &[1])], &[]);
    // The breakpoint hits: the configure ran every command instead of a replay.
    assert_eq!(client.reason(), "breakpoint");
    client.finish();
    assert_eq!(worker.join().unwrap(), (true, true));
}
