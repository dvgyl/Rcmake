use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_api::ConfigureRequest;
use rcmake_host::{FileKind, Follow, Host, discover_cmake_root};

use crate::eval_provider::EvalSemanticProvider;
use crate::json::object;
use crate::rpc::RequestId;
use crate::uri;
use rcmake_json::Value;

use super::{Provider, RpcMessage, Server, State};

impl Server<'_> {
    /// Attach an evaluator for an initialized workspace with a root listfile.
    ///
    /// Refresh this provider on a worker. The server can answer queries from
    /// the previous snapshot or use lexical analysis while configure runs.
    pub fn attach_workspace_evaluator(
        &mut self,
        host: Arc<dyn Host>,
    ) -> Option<Arc<EvalSemanticProvider>> {
        if self.state != State::AwaitInitialized || self.semantic.is_some() {
            return None;
        }
        let root = self.workspace_roots.iter().find(|root| {
            host.metadata(&root.path.join("CMakeLists.txt"), Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
        })?;
        let modules = discover_cmake_root(host.as_ref(), None).1?;
        let build = root.path.join(".rcmake-lsp");
        let request = ConfigureRequest::new(&root.path, build, modules, host);
        let provider = Arc::new(
            EvalSemanticProvider::new(request, rcmake_api::PreparedConfigure::evaluate)
                .with_explicit_refresh(),
        );
        self.semantic = Some(Provider::Shared(provider.clone()));
        Some(provider)
    }

    /// Get diagnostic notifications for open documents after evaluation.
    pub fn refresh_open_diagnostics(&self) -> impl Iterator<Item = RpcMessage> + '_ {
        self.documents
            .keys()
            .filter(|_| self.state == State::Running)
            .filter_map(|uri| self.refresh_diagnostics(uri))
    }
}

const WATCH_METHOD: &str = "workspace/didChangeWatchedFiles";
const DISK_REGISTRATION_ID: &str = "rcmake-watch-disk-inputs";

#[derive(Default)]
pub(super) struct DiskWatches {
    pub(super) relative: bool,
    paths: Vec<PathBuf>,
    request: u64,
}

impl Server<'_> {
    /// Replace the separate disk input registration if its paths changed.
    pub fn reconcile_disk_watches(&mut self, mut paths: Vec<PathBuf>) -> Vec<RpcMessage> {
        if !self.watch_files || self.state != State::Running {
            return Vec::new();
        }
        paths.sort();
        paths.dedup();
        let (paths, watchers): (Vec<_>, Vec<_>) = paths
            .into_iter()
            .filter_map(|path| {
                let pattern = glob_pattern(&path, self.disk_watches.relative)?;
                Some((
                    path,
                    object([("globPattern", pattern), ("kind", Value::Int(7))]),
                ))
            })
            .take(256)
            .unzip();
        let watches = &mut self.disk_watches;
        if paths == watches.paths {
            return Vec::new();
        }
        let mut messages = Vec::new();
        let mut request_id = || {
            watches.request += 1;
            RequestId::String(format!("rcmake-disk-watch-{}", watches.request))
        };
        if !watches.paths.is_empty() {
            messages.push(RpcMessage::request(
                request_id(),
                "client/unregisterCapability",
                object([(
                    "unregisterations",
                    Value::Array(vec![object([
                        ("id", Value::string(DISK_REGISTRATION_ID)),
                        ("method", Value::string(WATCH_METHOD)),
                    ])]),
                )]),
            ));
        }
        if !watchers.is_empty() {
            messages.push(RpcMessage::request(
                request_id(),
                "client/registerCapability",
                object([(
                    "registrations",
                    Value::Array(vec![object([
                        ("id", Value::string(DISK_REGISTRATION_ID)),
                        ("method", Value::string(WATCH_METHOD)),
                        (
                            "registerOptions",
                            object([("watchers", Value::Array(watchers))]),
                        ),
                    ])]),
                )]),
            ));
        }
        watches.paths = paths;
        messages
    }
}

fn glob_pattern(path: &Path, relative: bool) -> Option<Value> {
    let name = path.file_name()?.to_str()?;
    if name == "CMakeLists.txt" || name.ends_with(".cmake") || name == ".rcmake.toml" {
        return None;
    }
    let literal = |text: &str| !text.contains(['*', '?', '[', ']', '{', '}', '\\']);
    if relative {
        return literal(name).then(|| {
            object([
                (
                    "baseUri",
                    Value::string(uri::file(path.parent().unwrap_or(path))),
                ),
                ("pattern", Value::string(name)),
            ])
        });
    }
    let path = path.to_str()?.replace('\\', "/");
    literal(&path).then(|| Value::string(path))
}
