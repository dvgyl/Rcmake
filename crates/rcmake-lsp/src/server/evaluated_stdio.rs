use std::path::PathBuf;
use std::sync::Arc;

use rcmake_api::ConfigureRequest;
use rcmake_host::{FileKind, Follow, Host, discover_cmake_root};

use crate::eval_provider::EvalSemanticProvider;

use super::{DynamicWatches, Provider, RpcMessage, Server, State};

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

const DISK_REGISTRATION_ID: &str = "rcmake-watch-disk-inputs";

pub(super) struct DiskWatches(DynamicWatches);

impl Default for DiskWatches {
    fn default() -> Self {
        Self(DynamicWatches::new(
            DISK_REGISTRATION_ID,
            "rcmake-disk-watch-",
        ))
    }
}

impl Server<'_> {
    /// Replace the separate disk input registration if its paths changed.
    pub fn reconcile_disk_watches(&mut self, paths: Vec<PathBuf>) -> Vec<RpcMessage> {
        if !self.watch_files || self.state != State::Running {
            return Vec::new();
        }
        self.disk_watches
            .0
            .reconcile(paths, self.relative_watch_patterns)
    }
}
