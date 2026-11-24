use std::path::Path;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{FileKind, Host, HostError, path};
use rcmake_instrument::{
    Hook, InstrumentError, InstrumentOption, Query, Snippet, SnippetRole, hooks, query,
    snippet::{self, SnippetDetails, SystemSample},
};

use crate::emit_cli_error;

pub(crate) enum CommandExit {
    Code(i32),
    Signal(i32),
}

impl CommandExit {
    pub(crate) fn code(&self) -> i32 {
        match *self {
            Self::Code(code) => code,
            Self::Signal(signal) => 128_i32.saturating_add(signal),
        }
    }
}

pub(crate) struct InstrumentationStep {
    start: Duration,
    time_start: u64,
    pub(crate) before: Option<SystemSample>,
}

impl InstrumentationStep {
    pub(crate) fn start(host: &dyn Host, sample: bool) -> Self {
        let before = sample.then(|| SystemSample::collect(host));
        let time = host.now();
        Self {
            start: host.monotonic(),
            time_start: u64::try_from(time.secs)
                .unwrap_or_default()
                .saturating_mul(1000)
                .saturating_add(u64::from(time.nanos / 1_000_000)),
            before,
        }
    }

    pub(crate) fn record(
        self,
        host: &dyn Host,
        build_dir: &Path,
        argv: &[BString],
        role: SnippetRole,
        exit: &CommandExit,
    ) -> Result<(), InstrumentError> {
        let duration = host.monotonic().saturating_sub(self.start);
        let (result, interrupt_signal) = match *exit {
            CommandExit::Code(code) => (Some(i64::from(code)), None),
            CommandExit::Signal(signal) => (None, Some(signal)),
        };
        let snippet = Snippet {
            role,
            command: instrumentation_command(argv),
            working_dir: path::from_native(&host.initial_cwd()),
            result,
            time_start: self.time_start,
            duration: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            target: None,
            outputs: Vec::new(),
            source: None,
            language: None,
            test_name: None,
            config: None,
            stdout: None,
            stderr: None,
        };
        let data = build_dir.join(".cmake/instrumentation/v1/data");
        if self.before.is_none() && interrupt_signal.is_none() {
            snippet::write_snippet(host, &data, &snippet)?;
        } else {
            let cmake_content = if role == SnippetRole::Configure {
                None
            } else {
                instrumentation_content(host, &data)?
            };
            let details = SnippetDetails {
                interrupt_signal,
                dynamic: self
                    .before
                    .map(|before| (before, SystemSample::collect(host))),
                cmake_content,
                ..SnippetDetails::default()
            };
            snippet::write_snippet_with_details(host, &data, &snippet, &details)?;
        }
        Ok(())
    }
}

fn instrumentation_command(argv: &[BString]) -> BString {
    let mut command = BString::default();
    for (index, arg) in argv.iter().enumerate() {
        if index > 0 {
            command.push(b' ');
        }
        let quote = arg.is_empty()
            || arg
                .iter()
                .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\\'));
        if quote {
            command.push(b'"');
        }
        for &byte in arg.iter() {
            if quote && matches!(byte, b'"' | b'\\') {
                command.push(b'\\');
            }
            command.push(byte);
        }
        if quote {
            command.push(b'"');
        }
    }
    command
}

fn instrumentation_content(host: &dyn Host, data: &Path) -> Result<Option<BString>, HostError> {
    let entries = match host.read_dir(&data.join("content")) {
        Ok(entries) => entries,
        Err(err) if err.kind == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok(entries
        .into_iter()
        .filter(|entry| matches!(entry.kind, FileKind::File | FileKind::Symlink))
        .filter_map(|entry| {
            (entry.name.starts_with(b"cmake-") && entry.name.ends_with(b".json"))
                .then_some(entry.name)
        })
        .max_by(|left, right| {
            left.strip_suffix(b".json")
                .cmp(&right.strip_suffix(b".json"))
        })
        .map(|name| path::from_native(&Path::new("content").join(path::to_native(&name)))))
}

pub(crate) fn sample_instrumentation(queries: &[Query]) -> bool {
    queries.iter().any(|query| {
        query
            .options
            .contains(&InstrumentOption::DynamicSystemInformation)
    })
}

pub(crate) fn instrument_lifecycle(
    host: &dyn Host,
    build_dir: &Path,
    argv: &[BString],
    role: SnippetRole,
    post_hook: Hook,
    run: impl FnOnce() -> CommandExit,
) -> i32 {
    let queries = match query::read_queries(host, build_dir) {
        Ok(queries) => queries,
        Err(err) => {
            emit_cli_error(host, err.to_string());
            return 1;
        }
    };
    if queries.is_empty() {
        return run().code();
    }
    if role == SnippetRole::CMakeBuild
        && let Err(err) = hooks::run_hook(host, build_dir, Hook::PreCMakeBuild)
    {
        emit_cli_error(host, err.to_string());
        return 1;
    }
    let step = InstrumentationStep::start(host, sample_instrumentation(&queries));
    let injected = rcmake_instrument::injected_interrupt(host);
    let (exit, code) = match (injected, role) {
        (Some(signal), SnippetRole::CMakeInstall) => (CommandExit::Signal(signal), 1),
        (Some(signal), SnippetRole::CMakeBuild) => (CommandExit::Signal(signal), run().code()),
        _ => {
            let exit = run();
            let code = exit.code();
            (exit, code)
        }
    };
    if let Err(err) = step
        .record(host, build_dir, argv, role, &exit)
        .and_then(|()| {
            if matches!(exit, CommandExit::Signal(_)) {
                return Ok(());
            }
            hooks::run_hook(host, build_dir, post_hook)
        })
    {
        emit_cli_error(host, err.to_string());
        return if code == 0 { 1 } else { code };
    }
    code
}
