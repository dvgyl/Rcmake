//! Adversarial fuzzing and stress tests for the Rcmake parsers, engines, and CLI.
//!
//! Every campaign uses a deterministic seeded generator, so a failure reproduces
//! from its campaign name and case number. Each campaign runs on a worker thread
//! with the production stack size (64 MiB) and a wall-clock timeout. A campaign
//! fails when a case panics, when a case breaks an invariant, or when the campaign
//! does not finish in time.
//!
//! Set `RCMAKE_FUZZ_SCALE` to a number from 1 to 100 to multiply the random case
//! counts (default 1).

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::indexing_slicing
)]

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use rcmake_base::{BString, Limits};
use rcmake_diag::source::{SourceKind, SourceMap};
use rcmake_diag::{DiagOptions, DiagState};
use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{Follow, Host, NativeHost, PureHost, ReadRange, Stream, WriteMode};
use rcmake_json::{ReadOptions, Value};
use rcmake_presets::{EnvSnapshot, PresetKind};

/// Stack size of the production `rcmake-main` worker thread.
const STACK_BYTES: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Deterministic generator and campaign runner
// ---------------------------------------------------------------------------

/// SplitMix64 generator: small, fast, and fully deterministic.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x5DEE_CE66_D1CE_4E5B)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Return a value in `0..n`. `n` must not be zero.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next_u64() % u64::try_from(n.max(1)).unwrap_or(1)).unwrap_or(0)
    }

    /// Return `true` with a probability of `percent` / 100.
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T: ?Sized>(&mut self, items: &[&'a T]) -> &'a T {
        items[self.below(items.len())]
    }

    fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = self.below(max_len + 1);
        (0..len).map(|_| self.next_u64().to_le_bytes()[0]).collect()
    }
}

/// Multiplier for random case counts (`RCMAKE_FUZZ_SCALE`, default 1).
fn scale() -> usize {
    NativeHost::new()
        .ok()
        .and_then(|native| native.env_get(b"RCMAKE_FUZZ_SCALE"))
        .and_then(|v| v.to_string().trim().parse::<usize>().ok())
        .map_or(1, |n| n.clamp(1, 100))
}

/// Apply 1 to 8 random byte-level mutations to `seed`.
fn mutate(rng: &mut Rng, seed: &[u8], dict: &[&[u8]]) -> Vec<u8> {
    let mut out = seed.to_vec();
    for _ in 0..=rng.below(8) {
        let len = out.len();
        match rng.below(7) {
            0 if len > 0 => {
                let i = rng.below(len);
                out[i] ^= 1 << rng.below(8);
            }
            1 => {
                let i = rng.below(len + 1);
                out.insert(i, rng.next_u64().to_le_bytes()[0]);
            }
            2 if len > 0 => {
                let a = rng.below(len);
                let b = (a + rng.below(16) + 1).min(len);
                out.drain(a..b);
            }
            3 if len > 0 => {
                let a = rng.below(len);
                let b = (a + rng.below(32) + 1).min(len);
                let chunk = out[a..b].to_vec();
                let at = rng.below(out.len() + 1);
                out.splice(at..at, chunk);
            }
            4 if !dict.is_empty() => {
                let token = rng.pick(dict);
                let at = rng.below(len + 1);
                out.splice(at..at, token.iter().copied());
            }
            5 if len > 0 => out.truncate(rng.below(len)),
            _ => {
                let token = if dict.is_empty() {
                    b"\0".as_slice()
                } else {
                    rng.pick(dict)
                };
                out.extend_from_slice(token);
            }
        }
    }
    out
}

/// Short printable preview of an input, for failure reports.
fn preview(input: &[u8]) -> String {
    let text = String::from_utf8_lossy(input);
    let mut out: String = text
        .chars()
        .take(160)
        .flat_map(char::escape_debug)
        .collect();
    if text.chars().count() > 160 {
        let _ = write!(out, "... ({} bytes)", input.len());
    }
    out
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned())
}

/// One fuzzing campaign: a named set of cases with collected failures.
struct Campaign {
    name: &'static str,
    cases: usize,
    failures: Vec<String>,
    clock: NativeHost,
    slowest: (Duration, String),
}

impl Campaign {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            cases: 0,
            failures: Vec::new(),
            clock: NativeHost::new().expect("native host clock"),
            slowest: (Duration::ZERO, String::new()),
        }
    }

    /// Run one case. A panic or a failed invariant (a panic inside `f`) is recorded.
    fn case(&mut self, input: &[u8], f: impl FnOnce()) {
        self.cases += 1;
        let start = self.clock.monotonic();
        let outcome = catch_unwind(AssertUnwindSafe(f));
        let spent = self.clock.monotonic().saturating_sub(start);
        if spent > self.slowest.0 {
            self.slowest = (spent, preview(input));
        }
        if let Err(payload) = outcome {
            self.failures.push(format!(
                "case #{}: {}\n    input: {}",
                self.cases,
                panic_message(payload.as_ref()),
                preview(input)
            ));
        }
    }

    fn finish(self) {
        eprintln!(
            "[adversarial_fuzz] {}: {} cases, {} failures, slowest {:?} ({})",
            self.name,
            self.cases,
            self.failures.len(),
            self.slowest.0,
            self.slowest.1.chars().take(60).collect::<String>()
        );
        assert!(
            self.failures.is_empty(),
            "{}: {} of {} cases failed:\n{}",
            self.name,
            self.failures.len(),
            self.cases,
            self.failures.join("\n")
        );
    }
}

/// Run `body` on a 64 MiB worker thread and fail if it does not finish in `timeout`.
fn guarded(
    name: &'static str,
    timeout: Duration,
    body: impl FnOnce(&mut Campaign) + Send + 'static,
) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name(name.to_owned())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let mut campaign = Campaign::new(name);
            body(&mut campaign);
            let _ = tx.send(());
            campaign
        })
        .expect("spawn fuzz worker");
    match rx.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => match handle.join() {
            Ok(campaign) => campaign.finish(),
            Err(payload) => panic!(
                "{name}: worker panicked: {}",
                panic_message(payload.as_ref())
            ),
        },
        Err(RecvTimeoutError::Timeout) => {
            panic!("{name}: campaign did not finish in {timeout:?} (possible hang)")
        }
    }
}

// ---------------------------------------------------------------------------
// Shared drivers
// ---------------------------------------------------------------------------

fn argv(args: &[&str]) -> Vec<BString> {
    args.iter().copied().map(BString::from).collect()
}

#[path = "support/project_modules.rs"]
mod project_modules;

/// Result of one in-memory CLI run.
struct CliRun {
    code: i32,
    stderr: BString,
}

/// Run the CLI against a fresh in-memory [`PureHost`] seeded with `files`.
fn cli(files: &[(&str, &[u8])], args: Vec<BString>) -> (CliRun, Arc<PureHost>) {
    let host = Arc::new(PureHost::default());
    project_modules::seed(&host).expect("seed project modules");
    for (path, bytes) in files {
        host.seed_file(Path::new(path), bytes).expect("seed file");
    }
    let code = rcmake_cli::run(&args, host.clone());
    let stderr = host.stream_bytes(Stream::Stderr);
    (CliRun { code, stderr }, host)
}

/// Check the common CLI invariants: a known exit code, and a diagnostic on failure.
fn assert_cli_sane(run: &CliRun, allowed: &[i32]) {
    assert!(
        allowed.contains(&run.code),
        "unexpected exit code {} (stderr: {})",
        run.code,
        preview(run.stderr.as_slice())
    );
}

fn run_script(script: &[u8]) -> CliRun {
    cli(
        &[("/work/fuzz.cmake", script)],
        argv(&["cmake", "-P", "fuzz.cmake"]),
    )
    .0
}

/// Run a `-P` script and require exit code 0 or 1, with a `CMake Error` on failure.
fn assert_script_sane(script: &[u8]) -> CliRun {
    let run = run_script(script);
    assert_cli_sane(&run, &[0, 1]);
    if run.code == 1 {
        assert!(!run.stderr.is_empty(), "script failed without a diagnostic");
    }
    run
}

/// Run a script through an [`Interp`] with explicit `limits` on `host`.
fn interp_script(host: Arc<dyn Host>, dir: &Path, script: &[u8], limits: Limits) -> i32 {
    let path = dir.join("fuzz.cmake");
    host.write(&path, script, WriteMode::Truncate)
        .expect("write script");
    let cfg = InterpConfig {
        host: host.clone(),
        mode: RunMode::Script,
        cmake_root: PathBuf::from("/nonexistent/share/cmake-4.5"),
        cmake_command: PathBuf::from("/usr/local/bin/cmake"),
        cwd: dir.to_path_buf(),
        limits,
        diag: DiagOptions {
            initial_state: DiagState::default(),
            sarif: false,
        },
        defines: Vec::new(),
        script_args: argv(&["cmake", "-P", "fuzz.cmake"]),
    };
    let mut interp = Interp::new(cfg, rcmake_commands::register_all);
    interp.run_script(&path).exit_code
}

// ---------------------------------------------------------------------------
// rcmake-syntax
// ---------------------------------------------------------------------------

const SYNTAX_DICT: &[&[u8]] = &[
    b"(",
    b")",
    b"[[",
    b"]]",
    b"[=[",
    b"]=]",
    b"[==[",
    b"]==]",
    b"\"",
    b"\\",
    b"\\\n",
    b"#",
    b"#[[",
    b"#[=[",
    b"${",
    b"}",
    b"$ENV{",
    b"$CACHE{",
    b"@",
    b";",
    b"\n",
    b"\r",
    b"\r\n",
    b"\0",
    b"\xff",
    b"\xef\xbb\xbf",
    b"\xfe\xff",
    b" ",
    b"\t",
    b"if",
    b"elseif",
    b"else",
    b"endif",
    b"foreach",
    b"endforeach",
    b"while",
    b"endwhile",
    b"function",
    b"endfunction",
    b"macro",
    b"endmacro",
    b"block",
    b"endblock",
    b"set",
    b"message",
    b"x",
    b"_a1",
    b"\\;",
    b"\\\"",
    b"\\t",
    b"$<",
    b">",
];

const SYNTAX_SEEDS: &[&[u8]] = &[
    b"cmake_minimum_required(VERSION 3.20)\nproject(x C)\n",
    b"set(a \"quoted ${b} \\\" text\" [[bracket ]] ]=] text]] unquoted;list)\n",
    b"if(a AND (b OR NOT c))\n  message(STATUS yes)\nelseif(d)\nelse()\nendif()\n",
    b"function(f a b)\n  foreach(i IN LISTS ARGN)\n    message(${i})\n  endforeach()\nendfunction()\n",
    b"#[==[ bracket\ncomment ]==] while(x)\nendwhile()\n",
    b"macro(m)\nblock(SCOPE_FOR VARIABLES)\nendblock()\nendmacro()\n",
    b"set(x [=[\nline1\nline2]=])\nmessage(\"a\\\nb\")\n",
];

fn syntax_check(src: &[u8]) {
    let limits = Limits::default();
    let mut map = SourceMap::default();
    let id = map
        .add(
            BString::from("fuzz.cmake"),
            SourceKind::File,
            Arc::from(src),
        )
        .expect("source id");
    for kind in [SourceKind::File, SourceKind::VirtualEval] {
        if let Ok(list) = rcmake_syntax::parse_list_file(src, id, kind, &limits) {
            for cmd in list.commands.iter() {
                assert!(cmd.line >= 1, "command line number is zero");
                assert!(cmd.close_line >= cmd.line, "close line before open line");
                assert!(
                    cmd.span.start <= cmd.span.end && cmd.span.end <= src.len(),
                    "command span {:?} is outside the {}-byte input",
                    cmd.span,
                    src.len()
                );
                for arg in &cmd.args {
                    assert!(
                        arg.span.start <= arg.span.end && arg.span.end <= src.len(),
                        "argument span {:?} is outside the {}-byte input",
                        arg.span,
                        src.len()
                    );
                }
            }
        }
    }
}

#[test]
fn syntax_random_binary_and_token_soup() {
    let n = 1000 * scale();
    guarded("syntax/random", Duration::from_secs(300), move |c| {
        let mut rng = Rng::new(1);
        for _ in 0..n {
            let input = rng.bytes(512);
            c.case(&input, || syntax_check(&input));
        }
        for _ in 0..n {
            let mut input = Vec::new();
            for _ in 0..rng.below(64) {
                input.extend_from_slice(rng.pick(SYNTAX_DICT));
            }
            c.case(&input, || syntax_check(&input));
        }
    });
}

#[test]
fn syntax_mutated_seeds() {
    let n = 800 * scale();
    guarded("syntax/mutated", Duration::from_secs(300), move |c| {
        let mut rng = Rng::new(2);
        for i in 0..n {
            let input = mutate(&mut rng, SYNTAX_SEEDS[i % SYNTAX_SEEDS.len()], SYNTAX_DICT);
            c.case(&input, || syntax_check(&input));
        }
    });
}

fn repeat(unit: &str, n: usize) -> Vec<u8> {
    unit.repeat(n).into_bytes()
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn syntax_pathological_corpus() {
    guarded("syntax/pathological", Duration::from_secs(300), |c| {
        let mut corpus: Vec<Vec<u8>> = vec![
            repeat("[", 200_000),
            repeat("[[", 100_000),
            repeat("]]", 100_000),
            concat(&[b"set(x [", &repeat("=", 100_000), b"[ unclosed"]),
            concat(&[
                b"set(x [",
                &repeat("=", 50_000),
                b"[a]",
                &repeat("=", 49_999),
                b"])",
            ]),
            concat(&[b"set(x \"", &repeat("a", 1 << 20)]),
            concat(&[b"f(", &repeat("(", 100_000)]),
            concat(&[b"f(", &repeat("(", 100_000), &repeat(")", 100_001), b"\n"]),
            repeat("if(x)\n", 50_000),
            repeat("endif()\n", 50_000),
            repeat("function(f)\n", 20_000),
            concat(&[
                &repeat("while(1)\n", 20_000),
                &repeat("endwhile()\n", 20_000),
            ]),
            repeat("#[[", 50_000),
            b"#[==[ unclosed bracket comment".to_vec(),
            repeat("a()\n", 200_000),
            repeat("${", 100_000),
            concat(&[b"message(\"", &repeat("\\", 100_001), b"\")"]),
            concat(&[b"message(", &repeat("\\;", 100_000), b")"]),
            repeat("\r", 100_000),
            repeat("\\\r\n", 50_000),
            b"message(\0)\n".to_vec(),
            b"\0\0\0\0".to_vec(),
            b"set(a \0 b)\nset(\"\0\" [[\0]])\n".to_vec(),
            b"\xff\xfe(\n".to_vec(),
            b"\xff\xfem\0e\0s\0s\0a\0g\0e\0(\0)\0".to_vec(),
            b"\xfe\xff\0m\0(\0)".to_vec(),
            b"\0\0\xfe\xff".to_vec(),
            b"\xff\xfe\0\0".to_vec(),
            b"\xef\xbb\xbf\xef\xbb\xbfmessage()\n".to_vec(),
            b"message(\"a\rb\"\r)\r\n".to_vec(),
            b"message(\"\\\n\\\\\n\\\\\\\n\")".to_vec(),
            b"set(x [[\n]])\nset(y [=[]]]=])".to_vec(),
            b"\xc0\x80(\xed\xa0\x80)\xf4\x90\x80\x80".to_vec(),
            repeat("(", 1),
            repeat(")", 1),
            Vec::new(),
        ];
        for len in [1, 2, 3, 255, 256, 4095, 4096, 65_535, 65_536] {
            corpus.push(concat(&[b"x(", &repeat("a", len), b")"]));
        }
        for input in &corpus {
            c.case(input, || syntax_check(input));
        }
    });
}

// ---------------------------------------------------------------------------
// rcmake-expr through if() / while() / string(REGEX)
// ---------------------------------------------------------------------------

const IF_OPERANDS: &[&str] = &[
    "a",
    "b",
    "x",
    "y",
    "NUM",
    "\"a\"",
    "\"\"",
    "TRUE",
    "FALSE",
    "ON",
    "OFF",
    "0",
    "1",
    "-1",
    "1.2.3",
    "NOTFOUND",
    "x-NOTFOUND",
    "${a}",
    "${b}",
    "${undefined}",
    "\"${y}\"",
    "list",
    "/",
    "/work",
    "/work/fuzz.cmake",
    "IGNORE",
    "Y",
    "N",
    "2147483648",
    "99999999999999999999",
    "0x10",
    "1e3",
    ".*",
    "\"(a*)*b\"",
    "\"[\"",
    "\"\\\\\"",
    "\"\\n\"",
    "\"\\0\"",
    "self",
    "ENV{HOME}",
    "$ENV{PATH}",
    "$CACHE{X}",
    "CMAKE_VERSION",
    "\"a;b;c\"",
];
const IF_UNARY: &[&str] = &[
    "NOT",
    "DEFINED",
    "EXISTS",
    "COMMAND",
    "TARGET",
    "POLICY",
    "TEST",
    "IS_DIRECTORY",
    "IS_SYMLINK",
    "IS_ABSOLUTE",
    "IS_READABLE",
    "IS_WRITABLE",
    "IS_EXECUTABLE",
];
const IF_BINARY: &[&str] = &[
    "AND",
    "OR",
    "EQUAL",
    "LESS",
    "LESS_EQUAL",
    "GREATER",
    "GREATER_EQUAL",
    "STREQUAL",
    "STRLESS",
    "STRGREATER",
    "STRLESS_EQUAL",
    "STRGREATER_EQUAL",
    "VERSION_EQUAL",
    "VERSION_LESS",
    "VERSION_GREATER",
    "VERSION_LESS_EQUAL",
    "VERSION_GREATER_EQUAL",
    "MATCHES",
    "IN_LIST",
    "PATH_EQUAL",
    "IS_NEWER_THAN",
];

fn gen_condition(rng: &mut Rng, depth: usize, out: &mut Vec<String>) {
    let choice = if depth >= 6 { 0 } else { rng.below(5) };
    match choice {
        1 => {
            out.push(rng.pick(IF_UNARY).to_owned());
            gen_condition(rng, depth + 1, out);
        }
        2 | 3 => {
            gen_condition(rng, depth + 1, out);
            out.push(rng.pick(IF_BINARY).to_owned());
            gen_condition(rng, depth + 1, out);
        }
        4 => {
            out.push("(".to_owned());
            gen_condition(rng, depth + 1, out);
            out.push(")".to_owned());
        }
        _ => out.push(rng.pick(IF_OPERANDS).to_owned()),
    }
}

const IF_PRELUDE: &str = "set(a b)\nset(b a)\nset(self self)\nset(x 1)\nset(y x)\n\
set(list \"a;b;c\")\nset(NUM 42)\nset(e \"\")\n";

fn if_script(condition: &str) -> Vec<u8> {
    format!(
        "{IF_PRELUDE}if({condition})\n  message(STATUS T)\nelse()\n  message(STATUS F)\nendif()\n\
         while({condition})\n  break()\nendwhile()\n"
    )
    .into_bytes()
}

#[test]
fn expr_random_conditions() {
    let n = 600 * scale();
    guarded("expr/random-if", Duration::from_secs(600), move |c| {
        let mut rng = Rng::new(3);
        for _ in 0..n {
            let mut tokens = Vec::new();
            gen_condition(&mut rng, 0, &mut tokens);
            // Damage some conditions on purpose: drop, duplicate, or add stray tokens.
            if rng.chance(15) && !tokens.is_empty() {
                let i = rng.below(tokens.len());
                tokens.remove(i);
            }
            if rng.chance(10) {
                let i = rng.below(tokens.len() + 1);
                tokens.insert(i, rng.pick(&["(", ")", "\"(\"", "NOT", "AND"]).to_owned());
            }
            let script = if_script(&tokens.join(" "));
            c.case(&script, || {
                assert_script_sane(&script);
            });
        }
    });
}

#[test]
fn expr_pathological_conditions() {
    guarded("expr/pathological", Duration::from_secs(600), |c| {
        let mut conditions = vec![
            format!("{}TRUE{}", "(".repeat(20_000), ")".repeat(20_000)),
            format!("{}TRUE{}", "(".repeat(257), ")".repeat(257)),
            format!("{}TRUE", "NOT ".repeat(100_000)),
            format!("{}TRUE", "TRUE AND ".repeat(100_000)),
            format!("{}FALSE", "FALSE OR ".repeat(100_000)),
            format!("{}TRUE", "( NOT ".repeat(1_000)),
            "a".to_owned(),
            "self".to_owned(),
            "a STREQUAL b".to_owned(),
            "a EQUAL b".to_owned(),
            "${a} MATCHES ${b}".to_owned(),
            "self IN_LIST self".to_owned(),
            "DEFINED".to_owned(),
            "NOT".to_owned(),
            "AND".to_owned(),
            "( )".to_owned(),
            "\"\" MATCHES \"\"".to_owned(),
            "1 EQUAL 99999999999999999999999999999".to_owned(),
            "1.2.3.4.5.6.7.8.9.10 VERSION_LESS 99999999999999999999.1".to_owned(),
        ];
        let long_a = "a".repeat(64);
        for regex in [
            "^(a+)+$",
            "(a|aa)*c",
            "(.*)*x",
            "((a*)*)*b",
            "(a*)*(b*)*c",
            "^(a|a?)+$",
            "((((((((((((((((((((a))))))))))))))))))))",
            "[",
            "(",
            ")",
            "*",
            "+a",
            "a**",
            "\\\\",
            "[[:alpha:]",
            "[z-a]",
            "a{3}",
            "(?:a)",
            "^$^$",
            "(()|())*x",
        ] {
            conditions.push(format!("\"{long_a}!\" MATCHES \"{regex}\""));
        }
        for condition in &conditions {
            let script = if_script(condition);
            c.case(&script, || {
                assert_script_sane(&script);
            });
        }
        // Regex commands with patterns that match the empty string or backtrack heavily.
        for script in [
            "string(REGEX MATCHALL \"\" \"abc\" out)\n",
            "string(REGEX MATCHALL \"a*\" \"baaab\" out)\n",
            "string(REGEX REPLACE \"a*\" \"-\" out \"baaab\")\n",
            "string(REGEX REPLACE \"^\" \"-\" out \"abc\")\n",
            "string(REGEX REPLACE \"(a*)*b\" \"\\\\1\\\\9\" out \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\")\n",
            "string(REGEX MATCH \"(a|aa)*c\" out \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\")\n",
            "list(FILTER x INCLUDE REGEX \"((((\")\n",
            "set(l a;b;c)\nlist(TRANSFORM l REPLACE \"\" \"x\")\n",
        ] {
            c.case(script.as_bytes(), || {
                assert_script_sane(script.as_bytes());
            });
        }
    });
}

// ---------------------------------------------------------------------------
// rcmake-genex
// ---------------------------------------------------------------------------

const GENEX_DICT: &[&str] = &[
    "$<",
    ">",
    ":",
    ",",
    ";",
    "BOOL",
    "IF",
    "AND",
    "OR",
    "NOT",
    "1",
    "0",
    "STREQUAL",
    "EQUAL",
    "CONFIG",
    "TARGET_PROPERTY",
    "TARGET_GENEX_EVAL",
    "GENEX_EVAL",
    "TARGET_FILE",
    "TARGET_EXISTS",
    "LINK_ONLY",
    "BUILD_INTERFACE",
    "INSTALL_INTERFACE",
    "JOIN",
    "LIST",
    "REMOVE_DUPLICATES",
    "FILTER",
    "PATH",
    "LOWER_CASE",
    "ANGLE-R",
    "COMMA",
    "SEMICOLON",
    "a",
    "b",
    "INTERFACE_COMPILE_DEFINITIONS",
    "INTERFACE_LINK_LIBRARIES",
    "NAME",
    "TYPE",
    "IN_LIST",
    "VERSION_LESS",
    "COMPILE_LANGUAGE",
    "C",
    "CXX",
    "$",
    "<",
    "\\",
    "\"",
    " ",
    "x",
    "Debug",
    "Release",
    "TARGET_OBJECTS",
    "SHELL_PATH",
    "OUTPUT_CONFIG",
    "COMMAND_CONFIG",
];

fn gen_genex(rng: &mut Rng, depth: usize, out: &mut String) {
    if depth > 8 || rng.chance(25) {
        out.push_str(rng.pick(GENEX_DICT));
        return;
    }
    out.push_str("$<");
    out.push_str(rng.pick(GENEX_DICT));
    if rng.chance(80) {
        out.push(':');
        for i in 0..=rng.below(3) {
            if i > 0 {
                out.push(if rng.chance(80) { ',' } else { ';' });
            }
            gen_genex(rng, depth + 1, out);
        }
    }
    if rng.chance(92) {
        out.push('>');
    }
}

fn genex_check(input: &[u8]) {
    let _ = rcmake_genex::find_genex(input);
    let stripped = rcmake_genex::genex_strip(input);
    assert!(
        stripped.len() <= input.len(),
        "genex_strip grew the input from {} to {} bytes",
        input.len(),
        stripped.len()
    );
    let _ = rcmake_genex::rewrite_install_prefix(input, b"/prefix");
    if let Ok(ast) = rcmake_genex::parse(input) {
        for mode in [
            rcmake_genex::PreprocessMode::Build,
            rcmake_genex::PreprocessMode::Install,
        ] {
            let _ = rcmake_genex::strip(&ast, mode);
        }
    }
}

#[test]
fn genex_parse_random_and_nested() {
    let n = 1500 * scale();
    guarded("genex/parse", Duration::from_secs(300), move |c| {
        let mut rng = Rng::new(4);
        let dict: Vec<&[u8]> = GENEX_DICT.iter().map(|s| s.as_bytes()).collect();
        for i in 0..n {
            let mut text = String::new();
            gen_genex(&mut rng, 0, &mut text);
            let input = if i % 3 == 0 {
                mutate(&mut rng, text.as_bytes(), &dict)
            } else {
                text.into_bytes()
            };
            c.case(&input, || genex_check(&input));
        }
        for _ in 0..n / 3 {
            let input = rng.bytes(256);
            c.case(&input, || genex_check(&input));
        }
        let max = rcmake_genex::MAX_NESTING;
        let mut corpus = Vec::new();
        for depth in [max - 1, max, max + 1, 100_000] {
            corpus.push(format!("{}x{}", "$<1:".repeat(depth), ">".repeat(depth)).into_bytes());
            corpus.push("$<".repeat(depth).into_bytes());
            corpus.push(format!("{}x", "$<IF:".repeat(depth)).into_bytes());
        }
        corpus.extend([
            b"$<".to_vec(),
            b"$<>".to_vec(),
            b"$<:>".to_vec(),
            b"$<,,,,>".to_vec(),
            b"$<::::::>".to_vec(),
            b"$<$<$<$<".to_vec(),
            b">>>>>>>>".to_vec(),
            b"$<\0:\0>".to_vec(),
            b"$<\xff\xfe:\xc0\x80>".to_vec(),
            repeat("$<0:", 1)
                .into_iter()
                .chain(repeat(",", 100_000))
                .collect(),
        ]);
        for input in &corpus {
            c.case(input, || genex_check(input));
        }
    });
}

fn configure_with(content_genex: &str) -> (CliRun, Arc<PureHost>) {
    let lists = format!(
        "cmake_minimum_required(VERSION 3.20)\nproject(fuzz NONE)\n\
         add_library(a INTERFACE)\nadd_library(b INTERFACE)\n\
         set_property(TARGET a PROPERTY INTERFACE_COMPILE_DEFINITIONS \
         \"$<TARGET_PROPERTY:b,INTERFACE_COMPILE_DEFINITIONS>\")\n\
         set_property(TARGET b PROPERTY INTERFACE_COMPILE_DEFINITIONS \
         \"$<TARGET_PROPERTY:a,INTERFACE_COMPILE_DEFINITIONS>\")\n\
         set_property(TARGET a PROPERTY P \"$<GENEX_EVAL:$<TARGET_PROPERTY:P>>\")\n\
         target_link_libraries(a INTERFACE b)\ntarget_link_libraries(b INTERFACE a)\n\
         file(GENERATE OUTPUT out.txt CONTENT [==[{content_genex}]==] TARGET a)\n"
    );
    cli(
        &[("/work/CMakeLists.txt", lists.as_bytes())],
        argv(&["cmake", "-S", ".", "-B", "out", "-G", "Ninja"]),
    )
}

#[test]
fn genex_configure_cycles_and_random() {
    let n = 120 * scale();
    guarded("genex/configure", Duration::from_secs(600), move |c| {
        let (baseline, host) = configure_with("$<1:baseline>");
        assert_cli_sane(&baseline, &[0]);
        let generated = host
            .read(Path::new("/work/out/out.txt"), ReadRange::All { limit: 64 })
            .expect("Read generated baseline.");
        assert_eq!(generated, b"baseline");
        let mut corpus = vec![
            "$<TARGET_PROPERTY:a,INTERFACE_COMPILE_DEFINITIONS>".to_owned(),
            "$<TARGET_PROPERTY:a,INTERFACE_LINK_LIBRARIES>".to_owned(),
            "$<TARGET_GENEX_EVAL:a,$<TARGET_PROPERTY:a,P>>".to_owned(),
            "$<TARGET_PROPERTY:P>".to_owned(),
            "$<GENEX_EVAL:$<GENEX_EVAL:$<GENEX_EVAL:$<TARGET_PROPERTY:P>>>>".to_owned(),
            "$<TARGET_PROPERTY:missing,NAME>".to_owned(),
            "$<TARGET_FILE:a>".to_owned(),
            format!("{}x{}", "$<1:".repeat(999), ">".repeat(999)),
            format!("{}x{}", "$<1:".repeat(5_000), ">".repeat(5_000)),
            "$<".to_owned(),
            "$<JOIN:$<TARGET_PROPERTY:a,INTERFACE_COMPILE_DEFINITIONS>,$<COMMA>>".to_owned(),
        ];
        let mut rng = Rng::new(5);
        for _ in 0..n {
            let mut text = String::new();
            gen_genex(&mut rng, 0, &mut text);
            corpus.push(text);
        }
        for genex in &corpus {
            c.case(genex.as_bytes(), || {
                let (run, _) = configure_with(genex);
                assert_cli_sane(&run, &[0, 1]);
            });
        }
    });
}

// ---------------------------------------------------------------------------
// rcmake-presets
// ---------------------------------------------------------------------------

const PRESET_SEED: &str = r#"{
  "version": 10,
  "cmakeMinimumRequired": {"major": 3, "minor": 21, "patch": 0},
  "configurePresets": [
    {"name": "base", "hidden": true, "generator": "Ninja", "binaryDir": "${sourceDir}/out/${presetName}",
     "cacheVariables": {"A": "$env{A}", "B": {"type": "BOOL", "value": "ON"}, "C": "$penv{HOME}"},
     "environment": {"A": "x$env{B}", "B": "$penv{PATH}", "C": null},
     "condition": {"type": "equals", "lhs": "${hostSystemName}", "rhs": "PureOS"}},
    {"name": "p0", "inherits": ["base"], "displayName": "P0", "vendor": {"x": [1, 2, {"y": null}]}}
  ],
  "buildPresets": [{"name": "b0", "configurePreset": "p0", "jobs": 2, "targets": ["all"]}],
  "testPresets": [{"name": "t0", "configurePreset": "p0", "output": {"verbosity": "verbose"},
     "filter": {"include": {"name": "x.*"}}, "execution": {"repeat": {"mode": "until-pass", "count": 3}}}],
  "packagePresets": [{"name": "k0", "configurePreset": "p0", "generators": ["TGZ"]}],
  "workflowPresets": [{"name": "w0", "steps": [{"type": "configure", "name": "p0"},
     {"type": "build", "name": "b0"}]}]
}"#;

/// Load presets from `files` and resolve every defined preset of every kind.
fn presets_check(files: &[(&str, &[u8])], env: &[(&str, &str)]) {
    let host = PureHost::default();
    for (path, bytes) in files {
        host.seed_file(Path::new(path), bytes)
            .expect("seed presets");
    }
    let Ok(presets) = rcmake_presets::load(Path::new("/work"), &host) else {
        return;
    };
    let snapshot = EnvSnapshot {
        vars: env
            .iter()
            .map(|(k, v)| (BString::from(*k), BString::from(*v)))
            .collect::<BTreeMap<_, _>>(),
    };
    for kind in [
        PresetKind::Configure,
        PresetKind::Build,
        PresetKind::Test,
        PresetKind::Package,
        PresetKind::Workflow,
    ] {
        let names: Vec<BString> = presets.list_defined(kind).map(|s| s.name.clone()).collect();
        for name in names {
            let _ = rcmake_presets::resolve(&presets, kind, name.as_slice(), &snapshot);
        }
    }
}

const JSON_DICT: &[&[u8]] = &[
    b"{",
    b"}",
    b"[",
    b"]",
    b":",
    b",",
    b"\"",
    b"null",
    b"true",
    b"false",
    b"0",
    b"-0",
    b"1e999",
    b"-1e999",
    b"99999999999999999999999",
    b"\\u0000",
    b"\\uD800",
    b"\\uDC00",
    b"\\x",
    b"\\",
    b"\xff",
    b"\0",
    b"//",
    b"/*",
    b"*/",
    b"\"inherits\"",
    b"\"name\"",
    b"\"$env{A}\"",
    b"\"$penv{A}\"",
    b"\"${sourceDir}\"",
    b"\"version\"",
    b"\"include\"",
];

#[test]
fn presets_mutated_documents() {
    let n = 500 * scale();
    guarded("presets/mutated", Duration::from_secs(600), move |c| {
        let control = PureHost::default();
        control
            .seed_file(Path::new("/work/CMakePresets.json"), PRESET_SEED.as_bytes())
            .expect("Seed baseline presets.");
        let presets =
            rcmake_presets::load(Path::new("/work"), &control).expect("Load baseline presets.");
        let preset = rcmake_presets::resolve(
            &presets,
            PresetKind::Configure,
            b"p0",
            &EnvSnapshot {
                vars: BTreeMap::new(),
            },
        )
        .expect("Resolve baseline preset.");
        assert_eq!(preset.binary_dir, Some("/work/out/p0".into()));
        let mut rng = Rng::new(6);
        for _ in 0..n {
            let doc = mutate(&mut rng, PRESET_SEED.as_bytes(), JSON_DICT);
            c.case(&doc, || {
                presets_check(&[("/work/CMakePresets.json", &doc)], &[("PATH", "/bin")]);
            });
        }
    });
}

fn preset_doc(configure: &str) -> String {
    format!("{{\"version\": 10, \"configurePresets\": [{configure}]}}")
}

#[test]
fn presets_cycles_and_bombs() {
    guarded("presets/pathological", Duration::from_secs(300), |c| {
        let chain: Vec<String> = (0..3000)
            .map(|i| format!("{{\"name\": \"c{i}\", \"inherits\": \"c{}\"}}", i + 1))
            .chain(std::iter::once("{\"name\": \"c3000\"}".to_owned()))
            .collect();
        // Each variable doubles the previous one: naive expansion is 2^48 bytes.
        let bomb_env: Vec<String> = (0..48)
            .map(|i| format!("\"X{i}\": \"$env{{X{}}}$env{{X{}}}\"", i + 1, i + 1))
            .chain(std::iter::once("\"X48\": \"boom\"".to_owned()))
            .collect();
        let bomb = format!(
            "{{\"name\": \"bomb\", \"generator\": \"Ninja\", \"binaryDir\": \"/b\", \
             \"environment\": {{{}}}, \"cacheVariables\": {{\"V\": \"$env{{X0}}\"}}}}",
            bomb_env.join(", ")
        );
        let docs = vec![
            preset_doc("{\"name\": \"self\", \"inherits\": \"self\"}"),
            preset_doc(
                "{\"name\": \"a\", \"inherits\": \"b\"}, {\"name\": \"b\", \"inherits\": [\"c\"]}, \
                 {\"name\": \"c\", \"inherits\": [\"a\"]}",
            ),
            preset_doc("{\"name\": \"a\", \"inherits\": [\"missing\", \"a\", 7, null]}"),
            preset_doc(&chain.join(",")),
            preset_doc(&bomb),
            preset_doc(
                "{\"name\": \"cyc\", \"binaryDir\": \"/b\", \"generator\": \"Ninja\", \
                 \"environment\": {\"X\": \"$env{Y}\", \"Y\": \"$env{X}\"}, \
                 \"cacheVariables\": {\"V\": \"$env{X}\"}}",
            ),
            preset_doc(
                "{\"name\": \"pe\", \"binaryDir\": \"/b\", \"generator\": \"Ninja\", \
                 \"environment\": {\"P\": \"$penv{P}$penv{P}$penv{P}\"}, \
                 \"cacheVariables\": {\"V\": \"$env{P}\"}}",
            ),
            preset_doc(&format!(
                "{{\"name\": \"long\", \"binaryDir\": \"/b\", \"generator\": \"Ninja\", \
                 \"cacheVariables\": {{\"V\": \"{}\"}}}}",
                "${sourceDir}$env{HOME}$vendor{x}${unknownMacro}".repeat(20_000)
            )),
            format!(
                "{{\"version\": 10, \"vendor\": {}0{}}}",
                "[".repeat(100_000),
                "]".repeat(100_000)
            ),
            format!(
                "{{\"version\": 10, \"vendor\": {}}}",
                "{\"a\":".repeat(100_000)
            ),
            "{\"version\": 1e400}".to_owned(),
            "{\"version\": -1}".to_owned(),
            "{\"version\": 99999999999999999999}".to_owned(),
            "{\"version\": \"10\"}".to_owned(),
            "{\"version\": 10, \"configurePresets\": {}}".to_owned(),
            "{\"version\": 10, \"configurePresets\": [null, 1, \"x\", [], {}]}".to_owned(),
            "[]".to_owned(),
            String::new(),
            "\u{feff}{\"version\": 10}".to_owned(),
        ];
        let parent_env = [
            ("P", "$penv{P}"),
            ("HOME", "/home/$env{HOME}"),
            ("PATH", "/bin"),
        ];
        for doc in &docs {
            c.case(doc.as_bytes(), || {
                presets_check(&[("/work/CMakePresets.json", doc.as_bytes())], &parent_env);
            });
        }
        // Include cycles: self include, two-file cycle, and a long include chain.
        let include_self = b"{\"version\": 10, \"include\": [\"CMakePresets.json\"]}".to_vec();
        let include_a = b"{\"version\": 10, \"include\": [\"a.json\"]}".to_vec();
        let include_back = b"{\"version\": 10, \"include\": [\"CMakePresets.json\"]}".to_vec();
        c.case(&include_self, || {
            presets_check(&[("/work/CMakePresets.json", &include_self)], &[]);
        });
        c.case(&include_a, || {
            presets_check(
                &[
                    ("/work/CMakePresets.json", &include_a),
                    ("/work/a.json", &include_back),
                ],
                &[],
            );
        });
        let chain_files: Vec<(String, Vec<u8>)> = (0..500)
            .map(|i| {
                let name = if i == 0 {
                    "/work/CMakePresets.json".to_owned()
                } else {
                    format!("/work/i{i}.json")
                };
                let body = format!("{{\"version\": 10, \"include\": [\"i{}.json\"]}}", i + 1);
                (name, body.into_bytes())
            })
            .collect();
        let chain_refs: Vec<(&str, &[u8])> = chain_files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        c.case(b"include chain of 500 files", || {
            presets_check(&chain_refs, &[])
        });
    });
}

// ---------------------------------------------------------------------------
// rcmake-json and rcmake-instrument
// ---------------------------------------------------------------------------

fn gen_json(rng: &mut Rng, depth: usize, out: &mut String) {
    let choice = if depth > 6 {
        rng.below(6)
    } else {
        rng.below(8)
    };
    match choice {
        0 => out.push_str("null"),
        1 => out.push_str(rng.pick(&["true", "false"])),
        2 => out.push_str(rng.pick(&[
            "0",
            "-0",
            "1",
            "-1",
            "9223372036854775807",
            "-9223372036854775808",
            "18446744073709551615",
            "18446744073709551616",
            "1e308",
            "1e309",
            "-1e309",
            "4.9e-324",
            "1e-400",
            "0.1",
            "123456789012345678901234567890",
            "1E+2",
            "-0.0",
        ])),
        3 => out.push_str(rng.pick(&[
            "\"\"",
            "\"a\"",
            "\"\\u0000\"",
            "\"\\uD83D\\uDE00\"",
            "\"\\uD800\"",
            "\"\\uDC00\"",
            "\"\\uD800\\u0041\"",
            "\"\\\\\"",
            "\"\\\"\"",
            "\"\\/\"",
            "\"\\b\\f\\n\\r\\t\"",
            "\"\u{1F600}\"",
            "\"\\u00e9\"",
        ])),
        4 | 5 => out.push_str(rng.pick(&["\"key\"", "\"$env{A}\"", "\"x\""])),
        6 => {
            out.push('[');
            for i in 0..rng.below(4) {
                if i > 0 {
                    out.push(',');
                }
                gen_json(rng, depth + 1, out);
            }
            out.push(']');
        }
        _ => {
            out.push('{');
            for i in 0..rng.below(4) {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "\"k{}\":", rng.below(3));
                gen_json(rng, depth + 1, out);
            }
            out.push('}');
        }
    }
}

/// Parse with both option sets. On success, check that compact output parses back
/// to the same value (a write/read round trip).
fn json_check(input: &[u8]) {
    for options in [
        ReadOptions::default(),
        ReadOptions {
            allow_comments: false,
            dup_keys_last_wins: false,
        },
    ] {
        if let Ok(value) = rcmake_json::parse(input, &options) {
            let compact = rcmake_json::write_compact(&value);
            let styled = rcmake_json::write_styled(&value, "  ");
            let again = rcmake_json::parse(compact.as_slice(), &ReadOptions::default())
                .unwrap_or_else(|e| panic!("compact output does not parse: {e}"));
            assert!(
                json_equivalent(&value, &again),
                "round trip changed the value: {value:?} -> {again:?}"
            );
            let styled_again = rcmake_json::parse(styled.as_slice(), &ReadOptions::default())
                .unwrap_or_else(|e| panic!("styled output does not parse: {e}"));
            assert!(
                json_equivalent(&value, &styled_again),
                "styled round trip changed the value"
            );
        }
    }
    let _ = rcmake_instrument::query::parse_query(input);
}

/// Compare two values by their compact text form. This treats `Int(1)` and
/// `UInt(1)` as equal, the same as `jsoncpp` value equality.
fn json_equivalent(a: &Value, b: &Value) -> bool {
    a == b || rcmake_json::write_compact(a) == rcmake_json::write_compact(b)
}

#[test]
fn json_random_and_structured() {
    let n = 1000 * scale();
    guarded("json/random", Duration::from_secs(300), move |c| {
        let mut rng = Rng::new(7);
        for _ in 0..n {
            let input = rng.bytes(256);
            c.case(&input, || json_check(&input));
        }
        for i in 0..n {
            let mut text = String::new();
            gen_json(&mut rng, 0, &mut text);
            let input = if i % 2 == 0 {
                mutate(&mut rng, text.as_bytes(), JSON_DICT)
            } else {
                text.into_bytes()
            };
            c.case(&input, || json_check(&input));
        }
    });
}

const QUERY_SEED: &[u8] =
    br#"{"version": 1, "hooks": ["postGenerate", "preBuild", "postCMakeBuild"],
  "options": ["staticSystemInformation", "dynamicSystemInformation", "cdashSubmit", "trace"],
  "callbacks": ["/bin/true"], "customContent": {"x": {"type": "string", "content": "y"}}}"#;

#[test]
fn json_and_instrument_pathological() {
    let n = 500 * scale();
    guarded("json/pathological", Duration::from_secs(300), move |c| {
        let mut corpus: Vec<Vec<u8>> = vec![
            repeat("[", 1_000_000),
            format!("{}0{}", "[".repeat(1000), "]".repeat(1000)).into_bytes(),
            format!("{}0{}", "[".repeat(1001), "]".repeat(1001)).into_bytes(),
            repeat("{\"a\":", 200_000),
            format!("{}null{}", "{\"a\":".repeat(999), "}".repeat(999)).into_bytes(),
            repeat("9", 100_000),
            format!("-{}", "9".repeat(100_000)).into_bytes(),
            format!("0.{}e-99999999999", "1".repeat(10_000)).into_bytes(),
            b"1e99999999999999999999".to_vec(),
            b"\"\\uD800\"".to_vec(),
            b"\"\\uDC00\\uD800\"".to_vec(),
            b"\"\\uD800\\uD800\"".to_vec(),
            b"\"\\u12\"".to_vec(),
            b"\"\\uZZZZ\"".to_vec(),
            b"\"\\x41\"".to_vec(),
            b"\"\xff\xfe\xc0\x80\xed\xa0\x80\"".to_vec(),
            b"\"a\0b\"".to_vec(),
            b"/* unclosed comment".to_vec(),
            b"// only a comment".to_vec(),
            b"{\"a\":1,\"a\":2}".to_vec(),
            b"{\"a\":1,}".to_vec(),
            b"[1,]".to_vec(),
            b"\xef\xbb\xbf\xef\xbb\xbf{}".to_vec(),
            b"{} trailing".to_vec(),
            repeat("\"", 100_001),
            Vec::new(),
        ];
        let mut rng = Rng::new(8);
        for _ in 0..n {
            corpus.push(mutate(&mut rng, QUERY_SEED, JSON_DICT));
        }
        for input in &corpus {
            c.case(input, || json_check(input));
        }
    });
}

// ---------------------------------------------------------------------------
// CLI: -P scripts, -E tools, argv fuzzing, and configure stress
// ---------------------------------------------------------------------------

#[test]
fn script_recursion_and_expansion_caps() {
    guarded("cli/script-caps", Duration::from_secs(600), |c| {
        let scripts: Vec<(Vec<u8>, &[i32])> = vec![
            (b"function(f)\n  f()\nendfunction()\nf()\n".to_vec(), &[1]),
            (b"macro(m)\n  m()\nendmacro()\nm()\n".to_vec(), &[1]),
            (b"include(${CMAKE_CURRENT_LIST_FILE})\n".to_vec(), &[1]),
            (
                b"function(f)\n  cmake_language(EVAL CODE \"f()\")\nendfunction()\nf()\n".to_vec(),
                &[1],
            ),
            (
                b"function(f)\n  cmake_language(CALL f)\nendfunction()\nf()\n".to_vec(),
                &[1],
            ),
            (
                b"function(cb v a)\n  set(${v} x)\nendfunction()\nvariable_watch(w cb)\nset(w 1)\n"
                    .to_vec(),
                &[0, 1],
            ),
            (
                b"set(CMAKE_MAXIMUM_RECURSION_DEPTH 999999999)\nfunction(f n)\n  math(EXPR n \"${n}+1\")\n  \
                  if(n LESS 1500)\n    f(${n})\n  endif()\nendfunction()\nf(0)\n"
                    .to_vec(),
                &[0, 1],
            ),
            (
                b"set(a x)\nforeach(i RANGE 20)\n  set(a \"${a}${a}\")\nendforeach()\n\
                  string(LENGTH \"${a}\" n)\nmessage(STATUS ${n})\n"
                    .to_vec(),
                &[0],
            ),
            (
                format!("set(v0 x)\n{}message(\"${{v2000}}\")\n", (1..=2000)
                    .map(|i| format!("set(v{i} \"${{v{}}}\")\n", i - 1))
                    .collect::<String>())
                .into_bytes(),
                &[0],
            ),
            (
                format!("set(a a)\nmessage(\"{}a{}\")\n", "${".repeat(10_000), "}".repeat(10_000))
                    .into_bytes(),
                &[0, 1],
            ),
            (
                format!("message(\"{}\")\n", "${a}".repeat(200_000)).into_bytes(),
                &[0],
            ),
            (b"set(a \"${a\")\n".to_vec(), &[1]),
            (b"set(a \"$ENV{\")\nset(b \"$CACHE{}\")\nset(c \"@a@\")\n".to_vec(), &[0, 1]),
            (b"string(REPEAT x -1 out)\n".to_vec(), &[1]),
            (b"math(EXPR x \"1/0\")\nmath(EXPR y \"-9223372036854775808/-1\")\n".to_vec(), &[1]),
            (b"math(EXPR x \"((((((((((((((((((((1))))))))))))))))))))\")\n".to_vec(), &[0]),
            (format!("math(EXPR x \"{}1{}\")\n", "(".repeat(10_000), ")".repeat(10_000)).into_bytes(), &[0, 1]),
            (b"string(JSON x GET \"[[[[[[[[[[]]]]]]]]]]\" 0 0 0 0 0 0 0 0 0 0 0)\n".to_vec(), &[1]),
            (
                format!("string(JSON x GET \"{}\" 0)\n", "[".repeat(100_000)).into_bytes(),
                &[1],
            ),
            // An undefined list gives NOTFOUND and exit code 0, the same as CMake 4.5.1.
            (b"list(GET x 99999999999999999999 y)\nlist(SUBLIST x -5 -5 y)\n".to_vec(), &[0]),
            (b"cmake_policy(SET CMP0121 NEW)\nset(x a;b)\nlist(GET x 99999999999999999999 y)\n".to_vec(), &[1]),
            (b"foreach(i RANGE 5 0 -1)\nendforeach()\nforeach(i RANGE 0 5 0)\nendforeach()\n".to_vec(), &[0, 1]),
            (b"cmake_language(EXIT 300)\n".to_vec(), &[0, 1, 44, 255, 300]),
            (b"message(FATAL_ERROR \"\0\")\n".to_vec(), &[1]),
            (b"file(WRITE \"\0\" x)\nfile(READ \"/work/../../../../etc/passwd\" x)\n".to_vec(), &[0, 1]),
        ];
        for (script, allowed) in &scripts {
            c.case(script, || {
                let run = run_script(script);
                assert_cli_sane(&run, allowed);
            });
        }
    });
}

/// Doubling a value past `max_value_bytes` must stop with an error, not exhaust memory.
#[test]
fn script_value_size_limits() {
    guarded("cli/value-limits", Duration::from_secs(300), |c| {
        let limits = Limits {
            max_value_bytes: 1 << 20,
            ..Limits::default()
        };
        for script in [
            "set(a x)\nforeach(i RANGE 64)\n  set(a \"${a}${a}\")\nendforeach()\n",
            "set(a x)\nforeach(i RANGE 64)\n  string(APPEND a \"${a}\")\nendforeach()\n",
            "set(a x)\nforeach(i RANGE 64)\n  list(APPEND a ${a})\nendforeach()\n",
            "string(REPEAT abc 1000000000 out)\n",
            "set(a x)\nforeach(i RANGE 64)\n  string(CONCAT a \"${a}\" \"${a}\")\nendforeach()\n",
        ] {
            c.case(script.as_bytes(), || {
                let host = Arc::new(PureHost::default());
                let code =
                    interp_script(host, Path::new("/work"), script.as_bytes(), limits.clone());
                assert_eq!(code, 1, "value growth past the limit did not fail");
            });
        }
    });
}

/// Unbounded loops must stop at the configured wall-clock budget.
#[test]
fn script_wall_budget_stops_infinite_loops() {
    guarded("cli/wall-budget", Duration::from_secs(120), |c| {
        let limits = Limits {
            wall_budget: Some(Duration::from_secs(1)),
            ..Limits::default()
        };
        let host = Arc::new(NativeHost::new().expect("native host"));
        let dir = std::env::temp_dir().join(format!("rcmake-fuzz-budget-{}", std::process::id()));
        host.create_dir_all(&dir).expect("budget dir");
        assert_eq!(
            interp_script(
                host.clone(),
                &dir,
                b"message(STATUS baseline)",
                limits.clone()
            ),
            0
        );
        for script in [
            "set(i 0)\nwhile(TRUE)\n  math(EXPR i \"${i}+1\")\nendwhile()\n",
            "function(f)\n  while(1)\n    set(x y)\n  endwhile()\nendfunction()\nf()\n",
            "foreach(i RANGE 2000000000)\n  set(x ${i})\nendforeach()\n",
        ] {
            c.case(script.as_bytes(), || {
                let code = interp_script(host.clone(), &dir, script.as_bytes(), limits.clone());
                assert_eq!(code, 1, "the wall-clock budget did not stop the loop");
            });
        }
        let _ = host.remove_dir_all(&dir);
    });
}

#[test]
fn script_wall_budget_empty_loop_body() {
    guarded("cli/wall-budget-empty", Duration::from_secs(30), |c| {
        let limits = Limits {
            wall_budget: Some(Duration::from_secs(1)),
            ..Limits::default()
        };
        let host = Arc::new(NativeHost::new().expect("native host"));
        let dir = std::env::temp_dir().join(format!("rcmake-fuzz-empty-{}", std::process::id()));
        host.create_dir_all(&dir).expect("budget dir");
        assert_eq!(
            interp_script(
                host.clone(),
                &dir,
                b"message(STATUS baseline)",
                limits.clone()
            ),
            0
        );
        for script in [
            "while(TRUE)\nendwhile()\n",
            "foreach(i RANGE 2000000000)\nendforeach()\n",
        ] {
            c.case(script.as_bytes(), || {
                let code = interp_script(host.clone(), &dir, script.as_bytes(), limits.clone());
                assert_eq!(code, 1, "the wall-clock budget did not stop the empty loop");
            });
        }
        let _ = host.remove_dir_all(&dir);
    });
}

/// Build one `ustar` header block plus padded content.
fn tar_entry(name: &[u8], kind: u8, link: &[u8], data: &[u8]) -> Vec<u8> {
    let mut header = [0u8; 512];
    let put = |header: &mut [u8; 512], at: usize, bytes: &[u8], max: usize| {
        for (i, b) in bytes.iter().take(max).enumerate() {
            header[at + i] = *b;
        }
    };
    put(&mut header, 0, name, 100);
    put(&mut header, 100, b"0000644\0", 8);
    put(&mut header, 108, b"0000000\0", 8);
    put(&mut header, 116, b"0000000\0", 8);
    put(
        &mut header,
        124,
        format!("{:011o}\0", data.len()).as_bytes(),
        12,
    );
    put(&mut header, 136, b"00000000000\0", 12);
    put(&mut header, 148, b"        ", 8);
    header[156] = kind;
    put(&mut header, 157, link, 100);
    put(&mut header, 257, b"ustar\0", 6);
    put(&mut header, 263, b"00", 2);
    let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
    put(&mut header, 148, format!("{sum:06o}\0 ").as_bytes(), 8);
    let mut out = header.to_vec();
    out.extend_from_slice(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out
}

fn tar(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut out = entries.concat();
    out.extend_from_slice(&[0u8; 1024]);
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Build a one-entry stored (uncompressed) zip archive.
fn zip_one(name: &[u8], data: &[u8]) -> Vec<u8> {
    let crc = crc32(data).to_le_bytes();
    let size = u32::try_from(data.len()).unwrap_or(0).to_le_bytes();
    let name_len = u16::try_from(name.len()).unwrap_or(0).to_le_bytes();
    let mut out = Vec::new();
    out.extend_from_slice(b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0");
    out.extend_from_slice(&crc);
    out.extend_from_slice(&size);
    out.extend_from_slice(&size);
    out.extend_from_slice(&name_len);
    out.extend_from_slice(b"\0\0");
    out.extend_from_slice(name);
    out.extend_from_slice(data);
    let central = out.len();
    out.extend_from_slice(b"PK\x01\x02\x14\0\x14\0\0\0\0\0\0\0\0\0");
    out.extend_from_slice(&crc);
    out.extend_from_slice(&size);
    out.extend_from_slice(&size);
    out.extend_from_slice(&name_len);
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(name);
    let central_len = u32::try_from(out.len() - central)
        .unwrap_or(0)
        .to_le_bytes();
    let central_at = u32::try_from(central).unwrap_or(0).to_le_bytes();
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0\x01\0\x01\0");
    out.extend_from_slice(&central_len);
    out.extend_from_slice(&central_at);
    out.extend_from_slice(b"\0\0");
    out
}

fn exists(host: &PureHost, path: &str) -> bool {
    host.metadata(Path::new(path), Follow::No).is_ok()
}

/// Archive entries must never land outside the extraction directory.
#[test]
fn tool_e_archive_slip() {
    guarded("cli/archive-slip", Duration::from_secs(120), |c| {
        let archives: Vec<(&str, Vec<u8>)> = vec![
            (
                "dotdot.tar",
                tar(&[tar_entry(b"../../escape.txt", b'0', b"", b"x")]),
            ),
            (
                "deep.tar",
                tar(&[tar_entry(b"a/../../../escape.txt", b'0', b"", b"x")]),
            ),
            (
                "abs.tar",
                tar(&[tar_entry(b"/escape.txt", b'0', b"", b"x")]),
            ),
            (
                "passwd.tar",
                tar(&[tar_entry(b"../../../../../../etc/passwd", b'0', b"", b"x")]),
            ),
            (
                "symlink.tar",
                tar(&[
                    tar_entry(b"lnk", b'2', b"../..", b""),
                    tar_entry(b"lnk/escape.txt", b'0', b"", b"x"),
                ]),
            ),
            (
                "abslink.tar",
                tar(&[
                    tar_entry(b"lnk", b'2', b"/", b""),
                    tar_entry(b"lnk/escape.txt", b'0', b"", b"x"),
                ]),
            ),
            (
                "hardlink.tar",
                tar(&[tar_entry(b"h", b'1', b"../../escape.txt", b"")]),
            ),
            ("dotdot.zip", zip_one(b"../../escape.txt", b"x")),
            ("abs.zip", zip_one(b"/escape.txt", b"x")),
            (
                "nul.tar",
                tar(&[tar_entry(b"a\0/../../escape.txt", b'0', b"", b"x")]),
            ),
            (
                "truncated.tar",
                tar_entry(b"t.txt", b'0', b"", &[b'x'; 2048])[..700].to_vec(),
            ),
        ];
        for (name, bytes) in &archives {
            c.case(name.as_bytes(), || {
                let archive = format!("/work/{name}");
                let host = Arc::new(PureHost::new(
                    PathBuf::from("/work/a/b/dest"),
                    PathBuf::from("/usr/local/bin/cmake"),
                ));
                host.seed_file(Path::new(&archive), bytes)
                    .expect("seed archive");
                host.create_dir_all(Path::new("/work/a/b/dest"))
                    .expect("dest");
                let code =
                    rcmake_cli::run(&argv(&["cmake", "-E", "tar", "xf", &archive]), host.clone());
                assert!(code == 0 || code == 1, "unexpected exit code {code}");
                for outside in [
                    "/escape.txt",
                    "/etc/passwd",
                    "/work/escape.txt",
                    "/work/a/escape.txt",
                    "/work/a/b/escape.txt",
                ] {
                    assert!(
                        !exists(&host, outside),
                        "{name} wrote outside the destination: {outside}"
                    );
                }
            });
        }
    });
}

const E_COMMANDS: &[&str] = &[
    "capabilities",
    "cat",
    "chdir",
    "compare_files",
    "copy",
    "copy_directory",
    "copy_directory_if_different",
    "copy_if_different",
    "copy_if_newer",
    "create_symlink",
    "create_hardlink",
    "echo",
    "echo_append",
    "env",
    "environment",
    "make_directory",
    "md5sum",
    "sha1sum",
    "sha224sum",
    "sha256sum",
    "sha384sum",
    "sha512sum",
    "remove",
    "remove_directory",
    "rename",
    "rm",
    "sleep",
    "tar",
    "time",
    "touch",
    "touch_nocreate",
    "true",
    "false",
    "cmake_echo_color",
    "read_symlink",
    "",
    "\0",
    "nonexistent",
];
const E_ARGS: &[&str] = &[
    "",
    "a",
    "/work/a",
    "/work/dir",
    "dir",
    "../../etc/passwd",
    "/etc/passwd",
    "\0",
    "a\0b",
    "-",
    "--",
    "-r",
    "-f",
    "-rf",
    "cf",
    "xf",
    "tf",
    "cfz",
    "cvjf",
    "--format=zip",
    "--format=7zip",
    "--format=bogus",
    "--files-from=list",
    "--mtime=not-a-date",
    "--zstd",
    "--touch",
    "evil.tar",
    "out.tar",
    "\u{fffd}",
    "*",
    "/",
    "a/b/c/d/e",
    "1e9",
    "-1",
    "99999999999999999999",
    "--ignore-eol",
    "--switch=",
    "--red",
    "--bold",
    "NAME=value",
    "--unset=PATH",
    "--modify",
    "PATH=set:x",
];

#[test]
fn tool_e_argv_fuzz() {
    let n = 800 * scale();
    guarded("cli/tool-e", Duration::from_secs(600), move |c| {
        let mut rng = Rng::new(9);
        let evil = tar(&[tar_entry(b"../../escape.txt", b'0', b"", b"x")]);
        let long = "z".repeat(100_000);
        for _ in 0..n {
            let mut args = vec!["cmake".to_owned(), "-E".to_owned()];
            args.push(rng.pick(E_COMMANDS).to_owned());
            for _ in 0..rng.below(5) {
                args.push(if rng.chance(2) {
                    long.clone()
                } else {
                    rng.pick(E_ARGS).to_owned()
                });
            }
            let label = args.join(" ");
            c.case(label.as_bytes(), || {
                let (run, host) = cli(
                    &[
                        ("/work/a", b"content"),
                        ("/work/dir/x", b"x"),
                        ("/work/evil.tar", &evil),
                        ("/work/list", b"a\ndir\n"),
                    ],
                    args.iter().map(|a| BString::from(a.as_str())).collect(),
                );
                assert!(
                    (0..=255).contains(&run.code),
                    "exit code {} out of range",
                    run.code
                );
                // Only the archive entry `../../escape.txt` can create this path. Other
                // commands can create `/etc/passwd` on purpose, for example make_directory.
                assert!(
                    !exists(&host, "/escape.txt"),
                    "an archive entry escaped the directory"
                );
            });
        }
    });
}

const CLI_FLAGS: &[&str] = &[
    "-S",
    "-B",
    ".",
    "out",
    "-G",
    "Ninja",
    "Unix Makefiles",
    "Bogus Generator",
    "-D",
    "-DX=1",
    "-DX:BOOL=ON",
    "-D=",
    "-DX:BADTYPE=1",
    "-U",
    "*",
    "-C",
    "init.cmake",
    "-P",
    "fuzz.cmake",
    "-E",
    "echo",
    "--preset",
    "p0",
    "--preset=missing",
    "--list-presets",
    "--list-presets=all",
    "--build",
    "--target",
    "all",
    "--install",
    "--fresh",
    "-W",
    "-Wdev",
    "-Werror=dev",
    "-Wno-error=deprecated",
    "--trace",
    "--trace-expand",
    "--trace-format=json-v1",
    "--trace-source=x",
    "--log-level=TRACE",
    "--log-level=bogus",
    "--log-context",
    "--sarif-output=s.json",
    "--graphviz=g.dot",
    "-L",
    "-LAH",
    "-N",
    "--version",
    "--help",
    "--help-command",
    "if",
    "--help-policy",
    "CMP0999",
    "--workflow",
    "--debug-output",
    "-j",
    "0",
    "-1",
    "--parallel",
    "99999999999999999999",
    "-A",
    "x64",
    "-T",
    "v143",
    "--toolchain",
    "/nonexistent.cmake",
    "--install-prefix",
    "\0",
    "a\0b",
    "-",
    "--",
    "--warn-uninitialized",
    "--check-system-vars",
    "--compile-no-warning-as-error",
    "--profiling-format=google-trace",
    "--profiling-output=p.json",
    "--find-package",
    "-DNAME=x",
    "-DMODE=EXIST",
    "--loglevel=x",
];
const CTEST_FLAGS: &[&str] = &[
    "--test-dir",
    "out",
    "-R",
    "(",
    "-E",
    "[",
    "-L",
    "x",
    "-j",
    "-1",
    "0",
    "999999",
    "--repeat",
    "until-pass:999999",
    "until-fail:0",
    "bogus",
    "--timeout",
    "-1",
    "--schedule-random",
    "--output-on-failure",
    "--stop-time",
    "25:99",
    "-N",
    "-V",
    "-VV",
    "--show-only=json-v1",
    "-I",
    "1,0,-1",
    "--preset",
    "t0",
    "-C",
    "Debug",
    "--no-tests=error",
    "-S",
    "dash.cmake",
    "-D",
    "Experimental",
    "--test-load",
    "nan",
    "\0",
];
const CPACK_FLAGS: &[&str] = &[
    "-G",
    "TGZ",
    "ZIP",
    "Bogus",
    "--config",
    "missing.cmake",
    "-D",
    "X=1",
    "-C",
    "Debug",
    "-B",
    "out",
    "--preset",
    "k0",
    "--list-presets",
    "-P",
    "pkg",
    "-R",
    "1.0",
    "--vendor",
    "\0",
];

#[test]
fn cli_argv_fuzz() {
    let n = 600 * scale();
    guarded("cli/argv", Duration::from_secs(900), move |c| {
        let mut rng = Rng::new(10);
        let lists = b"cmake_minimum_required(VERSION 3.20)\nproject(x NONE)\n\
add_library(i INTERFACE)\nenable_testing()\nadd_test(NAME t COMMAND x)\n";
        let script = b"message(STATUS fuzz)\n";
        for i in 0..n {
            let (tool, pool): (&str, &[&str]) = match i % 4 {
                1 => ("ctest", CTEST_FLAGS),
                2 => ("cpack", CPACK_FLAGS),
                _ => ("cmake", CLI_FLAGS),
            };
            let mut args = vec![tool.to_owned()];
            for _ in 0..rng.below(7) {
                args.push(rng.pick(pool).to_owned());
            }
            let label = args.join(" ");
            c.case(label.as_bytes(), || {
                let (run, _) = cli(
                    &[
                        ("/work/CMakeLists.txt", lists),
                        ("/work/fuzz.cmake", script),
                        ("/work/init.cmake", b"set(X 1 CACHE STRING \"\")\n"),
                        ("/work/dash.cmake", script),
                        ("/work/CMakePresets.json", PRESET_SEED.as_bytes()),
                    ],
                    args.iter().map(|a| BString::from(a.as_str())).collect(),
                );
                assert!(
                    (0..=255).contains(&run.code),
                    "exit code {} out of range",
                    run.code
                );
            });
        }
    });
}

/// Files to seed into a [`PureHost`]: absolute path and contents.
type SeedFiles = Vec<(String, Vec<u8>)>;

#[test]
fn configure_stress_cycles_and_recursion() {
    guarded("cli/configure-stress", Duration::from_secs(900), |c| {
        let n = 500;
        let mut ring = String::from("cmake_minimum_required(VERSION 3.20)\nproject(ring NONE)\n");
        for i in 0..n {
            let _ = writeln!(
                ring,
                "add_library(t{i} INTERFACE)\n\
                 target_compile_definitions(t{i} INTERFACE D{i})\n\
                 target_link_libraries(t{i} INTERFACE t{})",
                (i + 1) % n
            );
        }
        ring.push_str(
            "file(GENERATE OUTPUT defs.txt CONTENT \
             \"$<TARGET_PROPERTY:t0,INTERFACE_COMPILE_DEFINITIONS>\")\n\
             install(TARGETS t0 t1 EXPORT ring)\ninstall(EXPORT ring DESTINATION lib/cmake)\n\
             export(EXPORT ring FILE ring.cmake)\n",
        );
        let cases: Vec<(&str, SeedFiles)> = vec![
            ("interface ring of 500 targets", vec![("/work/CMakeLists.txt".to_owned(), ring.into_bytes())]),
            (
                "add_subdirectory self recursion",
                vec![(
                    "/work/CMakeLists.txt".to_owned(),
                    b"cmake_minimum_required(VERSION 3.20)\nproject(x NONE)\n\
                      if(NOT DEFINED depth)\n  set(depth 0)\nendif()\n\
                      math(EXPR depth \"${depth}+1\")\n\
                      add_subdirectory(${CMAKE_CURRENT_SOURCE_DIR} b${depth})\n"
                        .to_vec(),
                )],
            ),
            (
                "include self recursion",
                vec![(
                    "/work/CMakeLists.txt".to_owned(),
                    b"cmake_minimum_required(VERSION 3.20)\nproject(x NONE)\n\
                      include(${CMAKE_CURRENT_LIST_FILE})\n"
                        .to_vec(),
                )],
            ),
            (
                "duplicate and self-linked targets",
                vec![(
                    "/work/CMakeLists.txt".to_owned(),
                    b"cmake_minimum_required(VERSION 3.20)\nproject(x NONE)\n\
                      add_library(a INTERFACE)\nadd_library(a INTERFACE)\n\
                      target_link_libraries(a INTERFACE a a::b \"$<LINK_ONLY:a>\")\n\
                      add_library(a::b ALIAS a)\nadd_library(c ALIAS c)\n\
                      set_property(TARGET a PROPERTY INTERFACE_LINK_LIBRARIES \"$<TARGET_PROPERTY:a,INTERFACE_LINK_LIBRARIES>\")\n\
                      file(GENERATE OUTPUT o.txt CONTENT \"$<TARGET_PROPERTY:a,INTERFACE_LINK_LIBRARIES>\")\n"
                        .to_vec(),
                )],
            ),
            (
                "NUL bytes and invalid UTF-8 in listfile",
                vec![(
                    "/work/CMakeLists.txt".to_owned(),
                    b"cmake_minimum_required(VERSION 3.20)\nproject(\xff\xfe NONE)\n\
                      add_library(\"\0\" INTERFACE)\nset(\xc0\x80 \0)\n"
                        .to_vec(),
                )],
            ),
            (
                "missing CMakeLists.txt",
                vec![("/work/other.txt".to_owned(), b"x".to_vec())],
            ),
        ];
        for (label, files) in &cases {
            c.case(label.as_bytes(), || {
                let refs: Vec<(&str, &[u8])> = files
                    .iter()
                    .map(|(p, b)| (p.as_str(), b.as_slice()))
                    .collect();
                let (run, _) = cli(
                    &refs,
                    argv(&["cmake", "-S", ".", "-B", "out", "-G", "Ninja"]),
                );
                assert_cli_sane(&run, &[0, 1]);
            });
        }
    });
}
