use std::io;
use std::process::ExitCode;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use rcmake_host::native::NativeHost;
use rcmake_lsp::{
    RequestId, RpcMessage, Server,
    eval_provider::{EvalSemanticProvider, RefreshStatus},
};

enum Event {
    Message(RpcMessage),
    ParseError(RpcMessage),
    FrameError,
    InputError(io::Error),
    EndOfInput,
    SnapshotReady,
}

const EDIT_DEBOUNCE: Duration = Duration::from_millis(250);
const DISK_POLL_INTERVAL: Duration = Duration::from_secs(1);

struct Worker {
    provider: Arc<EvalSemanticProvider>,
    refresh: mpsc::SyncSender<()>,
}

impl Worker {
    fn start(
        provider: Arc<EvalSemanticProvider>,
        events: mpsc::SyncSender<Event>,
    ) -> io::Result<Self> {
        let (refresh, work) = mpsc::sync_channel(1);
        let evaluator = Arc::clone(&provider);
        thread::Builder::new()
            .name("rcmake-lsp-eval".to_owned())
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                loop {
                    match work.recv_timeout(DISK_POLL_INTERVAL) {
                        Ok(()) => {}
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            evaluator.snapshots().check_disk_inputs();
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                    loop {
                        match work.recv_timeout(EDIT_DEBOUNCE) {
                            Ok(()) => continue,
                            Err(mpsc::RecvTimeoutError::Timeout) => break,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    let result = evaluator.snapshots().refresh_once();
                    if matches!(
                        result,
                        Ok(RefreshStatus::Busy | RefreshStatus::UpToDate)
                            | Err(rcmake_api::ApiError::Cancelled)
                    ) {
                        continue;
                    }
                    if events.send(Event::SnapshotReady).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { provider, refresh })
    }

    fn wake(&self) {
        let _ = self.refresh.try_send(());
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.provider.snapshots().cancel();
    }
}

pub(super) fn serve() -> io::Result<ExitCode> {
    let host = Arc::new(NativeHost::new().map_err(io::Error::other)?);
    let mut server = Server::new(host.as_ref());
    let mut worker: Option<Worker> = None;
    let (sender, events) = mpsc::sync_channel(8);
    let input = sender.clone();
    thread::Builder::new()
        .name("rcmake-lsp-input".to_owned())
        .spawn(move || {
            let mut stream = io::stdin().lock();
            loop {
                let event = match rcmake_lsp::rpc::read_frame(&mut stream) {
                    Ok(Some(body)) => match RpcMessage::decode(&body) {
                        Ok(message) => Event::Message(message),
                        Err(error) => Event::ParseError(error),
                    },
                    Ok(None) => Event::EndOfInput,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
                        ) =>
                    {
                        Event::FrameError
                    }
                    Err(error) => Event::InputError(error),
                };
                let done = !matches!(event, Event::Message(_) | Event::ParseError(_));
                if input.send(event).is_err() || done {
                    break;
                }
            }
        })?;

    let mut output = io::stdout().lock();
    while let Ok(event) = events.recv() {
        match event {
            Event::Message(message) => {
                let initialized = matches!(&message, RpcMessage::Call { method, id: None, .. }
                    if method == "initialized");
                let (initialize, refresh, disk_change) = match &message {
                    RpcMessage::Call {
                        method,
                        id: Some(_),
                        ..
                    } => (method == "initialize", false, false),
                    RpcMessage::Call {
                        method, id: None, ..
                    } => {
                        let disk_change = method == "workspace/didChangeWatchedFiles";
                        (
                            false,
                            disk_change
                                || matches!(
                                    method.as_str(),
                                    "textDocument/didOpen"
                                        | "textDocument/didChange"
                                        | "textDocument/didSave"
                                        | "textDocument/didClose"
                                ),
                            disk_change,
                        )
                    }
                    _ => (false, false, false),
                };
                if disk_change && let Some(worker) = &worker {
                    worker.provider.snapshots().invalidate();
                }
                for response in server.handle(message) {
                    super::send(&mut output, response)?;
                }
                if initialize
                    && worker.is_none()
                    && let Some(provider) = server.attach_workspace_evaluator(host.clone())
                {
                    worker = Worker::start(provider, sender.clone()).ok();
                    if let Some(worker) = &worker {
                        worker.wake();
                    }
                }
                if initialized && let Some(worker) = &worker {
                    for request in
                        server.reconcile_disk_watches(worker.provider.snapshots().disk_paths())
                    {
                        super::send(&mut output, request)?;
                    }
                }
                if refresh && let Some(worker) = &worker {
                    worker.wake();
                }
                if let Some(code) = server.exit_code() {
                    return Ok(if code == 0 {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    });
                }
            }
            Event::SnapshotReady => {
                if let Some(worker) = &worker {
                    for request in
                        server.reconcile_disk_watches(worker.provider.snapshots().disk_paths())
                    {
                        super::send(&mut output, request)?;
                    }
                }
                for response in server.refresh_open_diagnostics() {
                    super::send(&mut output, response)?;
                }
            }
            Event::ParseError(error) => super::send(&mut output, error)?,
            Event::FrameError => {
                super::send(
                    &mut output,
                    RpcMessage::error(RequestId::Null, -32700, "Invalid Content-Length frame."),
                )?;
                return Ok(ExitCode::FAILURE);
            }
            Event::InputError(error) => return Err(error),
            Event::EndOfInput => break,
        }
    }
    Ok(ExitCode::SUCCESS)
}
