#![allow(clippy::disallowed_methods)]

use core::time::Duration;
use std::fs::File;
use std::io::{self, ErrorKind, PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use rustix::fs::OFlags;
use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, kill_process, kill_process_group, waitid,
};

use super::{io_at, open_output};
use crate::error::{HostError, HostResult, from_errno};
use crate::{
    GracefulTimeout, Input, Output, ProcessResult, RunOutput, RunRequest, Stream, TimeoutSignal,
    WriteMode,
};

enum Destination {
    Inherit,
    Null,
    Fd(OwnedFd),
}

impl Destination {
    fn stdio(&self) -> io::Result<Stdio> {
        Ok(match self {
            Self::Inherit => Stdio::inherit(),
            Self::Null => Stdio::null(),
            Self::Fd(fd) => Stdio::from(fd.try_clone()?),
        })
    }
}

struct Capture {
    pipe: PipeReader,
    stream: Stream,
}

fn nonblocking(fd: impl AsFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(&fd)?;
    rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK)?;
    Ok(())
}

fn destination(
    output: &Output,
    stream: Stream,
    capture: &mut Vec<Capture>,
) -> HostResult<Destination> {
    Ok(match output {
        Output::Capture => {
            let (reader, writer) = io_at_process(io::pipe())?;
            io_at_process(nonblocking(&reader))?;
            capture.push(Capture {
                pipe: reader,
                stream,
            });
            Destination::Fd(writer.into())
        }
        Output::File { path, append } => {
            let mode = if *append {
                WriteMode::Append
            } else {
                WriteMode::Truncate
            };
            Destination::Fd(open_output(path, mode)?.into())
        }
        Output::Inherit => Destination::Inherit,
        Output::Null => Destination::Null,
        Output::SharedWithStdout => return Err(from_errno(Errno::INVAL, None)),
    })
}

struct Feed<'a> {
    pipe: PipeWriter,
    bytes: &'a [u8],
}

fn process_input(input: &Input) -> HostResult<(Stdio, Option<Feed<'_>>)> {
    match input {
        Input::Inherit => Ok((Stdio::inherit(), None)),
        Input::Null => Ok((Stdio::null(), None)),
        Input::File(path) => Ok((io_at(File::open(path), path)?.into(), None)),
        Input::Bytes(bytes) => {
            let (reader, writer) = io_at_process(io::pipe())?;
            io_at_process(nonblocking(&writer))?;
            Ok((
                reader.into(),
                Some(Feed {
                    pipe: writer,
                    bytes,
                }),
            ))
        }
    }
}

fn io_at_process<T>(result: io::Result<T>) -> HostResult<T> {
    result.map_err(|err| HostError::from_io(err, None))
}

struct Stage {
    child: Option<Child>,
    result: Option<ProcessResult>,
}

struct Pipeline {
    stages: Vec<Stage>,
    grouped: bool,
}

fn child_status(child: &mut Child, grouped: bool) -> io::Result<Option<ProcessResult>> {
    if grouped {
        // Keep the leader's PID reserved until we finish or signal its group.
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        let status = waitid(WaitId::Pid(Pid::from_child(child)), options)?;
        Ok(status.and_then(|status| {
            status
                .exit_status()
                .map(ProcessResult::Exited)
                .or_else(|| status.terminating_signal().map(ProcessResult::Signaled))
        }))
    } else {
        Ok(child.try_wait()?.and_then(|status| {
            status
                .code()
                .map(ProcessResult::Exited)
                .or_else(|| status.signal().map(ProcessResult::Signaled))
        }))
    }
}

impl Pipeline {
    fn collect_status(&mut self) -> io::Result<bool> {
        let mut done = true;
        for stage in &mut self.stages {
            if stage.result.is_none()
                && let Some(child) = &mut stage.child
            {
                match child_status(child, self.grouped) {
                    Ok(status) => stage.result = status,
                    Err(err) if err.kind() == ErrorKind::Interrupted => {}
                    Err(err) => return Err(err),
                }
                done &= stage.result.is_some();
            }
        }
        Ok(done)
    }

    fn reap(&mut self) {
        for stage in &mut self.stages {
            if let Some(mut child) = stage.child.take() {
                let _ = child.wait();
            }
        }
    }

    fn signal(&self, signal: TimeoutSignal) {
        let signal = match signal {
            TimeoutSignal::Interrupt => Signal::INT,
            TimeoutSignal::Quit => Signal::QUIT,
            TimeoutSignal::Terminate => Signal::TERM,
            TimeoutSignal::User1 => Signal::USR1,
            TimeoutSignal::User2 => Signal::USR2,
        };
        for stage in &self.stages {
            if stage.result.is_none()
                && let Some(child) = &stage.child
            {
                let _ = kill_process(Pid::from_child(child), signal);
            }
        }
    }

    fn stop(&mut self) {
        for stage in &mut self.stages {
            if let Some(child) = &mut stage.child {
                if self.grouped {
                    let group = Pid::from_child(child);
                    let _ = kill_process_group(group, Signal::KILL);
                }
                if stage.result.is_none() {
                    let _ = child.kill();
                }
            }
        }
        self.reap();
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_pipeline(
    req: &RunRequest,
    input: Stdio,
    stdout: Destination,
    stderr: Option<Destination>,
    pipeline: &mut Pipeline,
    start: Instant,
) -> HostResult<()> {
    let mut next_input = Some(input);
    let mut specs = req.pipeline.iter().peekable();
    while let Some(spec) = specs.next() {
        if req
            .timeout
            .is_some_and(|timeout| start.elapsed() >= timeout)
        {
            pipeline.stages.push(Stage {
                child: None,
                result: Some(ProcessResult::TimedOut),
            });
            continue;
        }
        let stage_in = next_input
            .take()
            .ok_or_else(|| from_errno(Errno::INVAL, None))?;
        let stage_out = if specs.peek().is_some() {
            let (reader, writer) = io_at_process(io::pipe())?;
            next_input = Some(reader.into());
            writer.into()
        } else {
            io_at_process(stdout.stdio())?
        };
        let stage_err = io_at_process(stderr.as_ref().unwrap_or(&stdout).stdio())?;
        let program = crate::path::to_native(spec.program.as_slice());
        let mut command = Command::new(&program);
        command
            .args(
                spec.args
                    .iter()
                    .map(|arg| crate::path::to_native(arg.as_slice())),
            )
            .env_clear()
            .envs(req.env.iter().map(|(k, v)| {
                (
                    crate::path::to_native(k.as_slice()),
                    crate::path::to_native(v.as_slice()),
                )
            }))
            .current_dir(&req.cwd)
            .stdin(stage_in)
            .stdout(stage_out)
            .stderr(stage_err);
        if pipeline.grouped {
            // Each stage leads its own group. A fast first stage can exit before the next spawn.
            command.process_group(0);
        }
        let stage = match command.spawn() {
            Ok(child) => Stage {
                child: Some(child),
                result: None,
            },
            Err(err) => Stage {
                child: None,
                result: Some(ProcessResult::SpawnFailed(HostError::from_io(
                    err,
                    Some(program),
                ))),
            },
        };
        pipeline.stages.push(stage);
    }
    Ok(())
}

fn pump_input(feed: &mut Option<Feed<'_>>) -> io::Result<bool> {
    let Some(input) = feed else {
        return Ok(false);
    };
    if !input.bytes.is_empty() {
        match input.pipe.write(input.bytes) {
            Ok(n) => input.bytes = input.bytes.get(n..).unwrap_or_default(),
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                return Ok(false);
            }
            Err(err) if err.kind() == ErrorKind::BrokenPipe => input.bytes = &[],
            Err(err) => return Err(err),
        }
    }
    if input.bytes.is_empty() {
        *feed = None;
    }
    Ok(true)
}

fn pump_output(
    captures: &mut Vec<Capture>,
    output: &mut RunOutput,
    tee: bool,
    output_limit: usize,
    on_output: &mut dyn FnMut(Stream, &[u8]),
) -> io::Result<bool> {
    let mut busy = false;
    let mut buffer = [0u8; 8192];
    let mut error = None;
    captures.retain_mut(|capture| match capture.pipe.read(&mut buffer) {
        Ok(0) => false,
        Ok(n) => {
            let bytes = buffer.get(..n).unwrap_or_default();
            if output
                .stdout
                .len()
                .saturating_add(output.stderr.len())
                .saturating_add(n)
                > output_limit
            {
                error = Some(io::Error::other("Process output exceeds the size limit"));
                return false;
            }
            match capture.stream {
                Stream::Stdout => output.stdout.extend_from_slice(bytes),
                Stream::Stderr => output.stderr.extend_from_slice(bytes),
            }
            if tee {
                on_output(capture.stream, bytes);
            }
            busy = true;
            true
        }
        Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => true,
        Err(err) => {
            error = Some(err);
            false
        }
    });
    error.map_or(Ok(busy), Err)
}

pub(super) fn run_pipeline(
    req: &RunRequest,
    mut graceful: Option<GracefulTimeout>,
    output_limit: usize,
    on_output: &mut dyn FnMut(Stream, &[u8]),
) -> HostResult<RunOutput> {
    let start = Instant::now();
    let (input, mut feed) = process_input(&req.stdin)?;
    let mut captures = Vec::new();
    let mut stdout = destination(&req.stdout, Stream::Stdout, &mut captures)?;
    let stderr = if req.stderr == Output::SharedWithStdout {
        if matches!(stdout, Destination::Inherit) {
            stdout = Destination::Fd(io_at_process(io::stdout().as_fd().try_clone_to_owned())?);
        }
        None
    } else {
        Some(destination(&req.stderr, Stream::Stderr, &mut captures)?)
    };
    let mut pipeline = Pipeline {
        stages: Vec::new(),
        grouped: req.timeout.is_some(),
    };
    spawn_pipeline(req, input, stdout, stderr, &mut pipeline, start)?;
    let mut output = RunOutput::default();
    let mut time_limit = req.timeout;
    let mut timed_out = false;
    loop {
        let done = io_at_process(pipeline.collect_status())?;
        let read = io_at_process(pump_output(
            &mut captures,
            &mut output,
            req.tee,
            output_limit,
            on_output,
        ))?;
        if done && captures.is_empty() {
            pipeline.reap();
            break;
        }
        if time_limit.is_some_and(|timeout| start.elapsed() >= timeout) {
            if let Some(timeout) = graceful.take().filter(|_| !done) {
                pipeline.signal(timeout.signal);
                timed_out = true;
                time_limit = Some(start.elapsed().saturating_add(timeout.grace));
                continue;
            }
            if done
                && let Some(last) = pipeline
                    .stages
                    .iter_mut()
                    .rfind(|stage| stage.child.is_some())
            {
                last.result = Some(ProcessResult::TimedOut);
            }
            pipeline.stop();
            break;
        }
        let wrote = io_at_process(pump_input(&mut feed))?;
        if !read && !wrote {
            let pause = Duration::from_millis(2);
            let remaining =
                time_limit.map_or(pause, |timeout| timeout.saturating_sub(start.elapsed()));
            std::thread::sleep(pause.min(remaining));
        }
    }
    output.results = std::mem::take(&mut pipeline.stages)
        .into_iter()
        .map(|stage| {
            if timed_out {
                ProcessResult::TimedOut
            } else {
                stage.result.unwrap_or(ProcessResult::TimedOut)
            }
        })
        .collect();
    Ok(output)
}
