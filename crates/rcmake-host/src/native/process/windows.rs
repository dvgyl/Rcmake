#![allow(clippy::disallowed_methods)]

use std::fs::File;
use std::io::{self, ErrorKind, PipeReader, PipeWriter, Read, Write};
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use process_wrap::std::{ChildWrapper, CommandWrap, JobObject};

use super::super::{io_at, open_output};
use crate::{
    GracefulTimeout, HostError, HostResult, Input, Output, ProcessResult, RunOutput, RunRequest,
    Stream, WriteMode,
};

enum Destination {
    Inherit,
    Null,
    Handle(OwnedHandle),
}

impl Destination {
    fn stdio(&self) -> io::Result<Stdio> {
        Ok(match self {
            Self::Inherit => Stdio::inherit(),
            Self::Null => Stdio::null(),
            Self::Handle(handle) => handle.try_clone()?.into(),
        })
    }
}

type Capture = (Stream, PipeReader);
type Feed<'a> = (PipeWriter, &'a [u8]);

fn destination(
    output: &Output,
    stream: Stream,
    captures: &mut Vec<Capture>,
) -> HostResult<Destination> {
    Ok(match output {
        Output::Capture => {
            let (read, write) = process_io(io::pipe())?;
            captures.push((stream, read));
            Destination::Handle(write.into())
        }
        Output::File { path, append } => {
            let mode = if *append {
                WriteMode::Append
            } else {
                WriteMode::Truncate
            };
            Destination::Handle(open_output(path, mode)?.into())
        }
        Output::Inherit => Destination::Inherit,
        Output::Null => Destination::Null,
        Output::SharedWithStdout => return Err(crate::error::SyntheticError::InvalidInput.at(None)),
    })
}

fn input(input: &Input) -> HostResult<(Stdio, Option<Feed<'_>>)> {
    Ok(match input {
        Input::Inherit => (Stdio::inherit(), None),
        Input::Null => (Stdio::null(), None),
        Input::File(path) => (io_at(File::open(path), path)?.into(), None),
        Input::Bytes(bytes) => {
            let (read, write) = process_io(io::pipe())?;
            (read.into(), Some((write, bytes)))
        }
    })
}

fn process_io<T>(r: io::Result<T>) -> HostResult<T> {
    r.map_err(|error| HostError::from_io(error, None))
}

struct Stage {
    job: Option<Box<dyn ChildWrapper>>,
    result: Option<ProcessResult>,
}

#[derive(Default)]
struct Pipeline {
    stages: Vec<Stage>,
    finished: bool,
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if !self.finished {
            self.stop();
        }
    }
}

impl Pipeline {
    fn collect_status(&mut self) -> io::Result<bool> {
        for stage in &mut self.stages {
            if stage.result.is_none()
                && let Some(job) = &mut stage.job
                && let Some(status) = job.inner_mut().try_wait()?
            {
                stage.result = status.code().map(ProcessResult::Exited);
            }
        }
        Ok(self.stages.iter().all(|stage| stage.result.is_some()))
    }

    fn stop(&mut self) {
        for job in self
            .stages
            .iter_mut()
            .filter_map(|stage| stage.job.as_mut())
        {
            let _ = job.start_kill();
        }
        for job in self
            .stages
            .iter_mut()
            .filter_map(|stage| stage.job.as_mut())
        {
            let _ = job.inner_mut().wait();
        }
        self.finished = true;
    }

    fn spawn(
        &mut self,
        req: &RunRequest,
        input: Stdio,
        stdout: Destination,
        stderr: Option<Destination>,
        start: Instant,
    ) -> HostResult<()> {
        let mut next_input = input;
        let mut specs = req.pipeline.iter().peekable();
        while let Some(spec) = specs.next() {
            if req.timeout.is_some_and(|limit| start.elapsed() >= limit) {
                self.stages.push(Stage {
                    job: None,
                    result: Some(ProcessResult::TimedOut),
                });
                continue;
            }
            let (stage_output, following_input) = if specs.peek().is_some() {
                let (read, write) = process_io(io::pipe())?;
                (write.into(), read.into())
            } else {
                (process_io(stdout.stdio())?, Stdio::null())
            };
            let program = crate::path::to_native(&spec.program);
            let mut command = Command::new(&program);
            command
                .args(spec.args.iter().map(|arg| crate::path::to_native(arg)))
                .env_clear()
                .envs(req.env.iter().map(|(key, value)| {
                    (crate::path::to_native(key), crate::path::to_native(value))
                }))
                .current_dir(&req.cwd)
                .stdin(next_input)
                .stdout(stage_output)
                .stderr(process_io(stderr.as_ref().unwrap_or(&stdout).stdio())?);
            next_input = following_input;

            let mut command = CommandWrap::from(command);
            command.wrap(JobObject);
            let stage = match command.spawn() {
                Ok(job) => Stage {
                    job: Some(job),
                    result: None,
                },
                Err(err) => Stage {
                    job: None,
                    result: Some(ProcessResult::SpawnFailed(HostError::from_io(
                        err,
                        Some(program),
                    ))),
                },
            };
            self.stages.push(stage);
        }
        Ok(())
    }
}

enum Event {
    Output(Stream, Vec<u8>),
    CaptureFinished,
}

fn capture_output((stream, mut reader): Capture, sender: SyncSender<io::Result<Event>>) {
    let mut buffer = [0; 8192];
    loop {
        let event = match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(Ok(Event::CaptureFinished));
                return;
            }
            Ok(n) => buffer
                .get(..n)
                .map(|bytes| Event::Output(stream, bytes.to_vec()))
                .ok_or_else(|| io::Error::other("Invalid pipe read length")),
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                let _ = sender.send(Err(err));
                return;
            }
        };
        if sender.send(event).is_err() {
            return;
        }
    }
}

struct Supervisor<'a> {
    req: &'a RunRequest,
    start: Instant,
    output_limit: usize,
}

impl Supervisor<'_> {
    fn collect(
        self,
        mut pipeline: Pipeline,
        captures: Vec<Capture>,
        feed: Option<Feed<'_>>,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(8);
            let mut active_captures = captures.len();
            for capture in captures {
                let sender = tx.clone();
                process_io(
                    thread::Builder::new()
                        .spawn_scoped(scope, move || capture_output(capture, sender)),
                )?;
            }
            let feed = feed
                .map(|(mut pipe, bytes)| {
                    let sender = tx.clone();
                    thread::Builder::new().spawn_scoped(scope, move || {
                        if let Err(err) = pipe.write_all(bytes)
                            && err.kind() != ErrorKind::BrokenPipe
                        {
                            let _ = sender.send(Err(err));
                        }
                    })
                })
                .transpose()
                .map_err(|err| HostError::from_io(err, None));
            drop(tx);

            // Drop the job handles and the receiver inside the scope before it joins workers.
            let result = (|| {
                let feed = feed?;
                let mut output = RunOutput::default();
                let mut closed = false;
                loop {
                    let done = process_io(pipeline.collect_status())?;
                    if done && active_captures == 0 {
                        if feed.as_ref().is_some_and(|thread| !thread.is_finished()) {
                            pipeline.stop();
                        }
                        pipeline.finished = true;
                        break;
                    }
                    let pause = Duration::from_millis(2);
                    let remaining = self
                        .req
                        .timeout
                        .map(|limit| limit.saturating_sub(self.start.elapsed()));
                    if remaining == Some(Duration::ZERO) {
                        if done
                            && let Some(last) = pipeline
                                .stages
                                .iter_mut()
                                .rfind(|stage| stage.job.is_some())
                        {
                            last.result = Some(ProcessResult::TimedOut);
                        }
                        pipeline.stop();
                        break;
                    }
                    let pause = remaining.map_or(pause, |remaining| remaining.min(pause));
                    if closed {
                        thread::sleep(pause);
                        continue;
                    }
                    match rx.recv_timeout(pause) {
                        Ok(event) => {
                            let Event::Output(stream, bytes) = process_io(event)? else {
                                active_captures -= 1;
                                continue;
                            };
                            if output
                                .stdout
                                .len()
                                .saturating_add(output.stderr.len())
                                .saturating_add(bytes.len())
                                > self.output_limit
                            {
                                return process_io(Err(io::Error::other(
                                    "Process output exceeds the size limit",
                                )));
                            }
                            match stream {
                                Stream::Stdout => output.stdout.extend_from_slice(&bytes),
                                Stream::Stderr => output.stderr.extend_from_slice(&bytes),
                            }
                            if self.req.tee {
                                on_output(stream, &bytes);
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => closed = true,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                }
                output.results = pipeline
                    .stages
                    .iter_mut()
                    .map(|stage| stage.result.take().unwrap_or(ProcessResult::TimedOut))
                    .collect();
                Ok(output)
            })();
            drop(rx);
            drop(pipeline);
            result
        })
    }
}

pub(in crate::native) fn run_pipeline(
    req: &RunRequest,
    _graceful: Option<GracefulTimeout>,
    output_limit: usize,
    on_output: &mut dyn FnMut(Stream, &[u8]),
) -> HostResult<RunOutput> {
    let start = Instant::now();
    let (input, feed) = input(&req.stdin)?;
    let mut captures = Vec::new();
    let mut stdout = destination(&req.stdout, Stream::Stdout, &mut captures)?;
    let stderr = if req.stderr == Output::SharedWithStdout {
        if matches!(stdout, Destination::Inherit) {
            stdout =
                Destination::Handle(process_io(io::stdout().as_handle().try_clone_to_owned())?);
        }
        None
    } else {
        Some(destination(&req.stderr, Stream::Stderr, &mut captures)?)
    };
    let mut pipeline = Pipeline::default();
    pipeline.spawn(req, input, stdout, stderr, start)?;
    Supervisor {
        req,
        start,
        output_limit,
    }
    .collect(pipeline, captures, feed, on_output)
}
