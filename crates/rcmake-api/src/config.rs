use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::Limits;
use rcmake_diag::{DiagOptions, Diagnostic};
use rcmake_eval::{CacheDefine, CancelToken, LedgerLevel, Services};
use rcmake_host::{
    Host, HostError,
    no_process::{DeniedCommand, NoProcessHost},
    overlay::OverlayHost,
    path,
};
use rcmake_model::GeneratorKind;

use crate::{CompletionState, ConfigureResult, OverlayDiff};

/// Policy for subprocess requests during configure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessPolicy {
    /// Permit subprocesses where the host and write mode support them.
    #[default]
    Allow,
    /// Reject subprocesses and record the requests.
    Deny,
}

/// Destination of writes made through the session host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WriteMode {
    /// Write through the supplied host.
    #[default]
    Direct,
    /// Keep changes in memory. Subprocesses are denied in this mode.
    Overlay,
}

/// An error at the configure API boundary.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// A request field has an invalid value.
    #[error("Invalid {field}. {message}")]
    InvalidConfig {
        /// Field that caused the error.
        field: &'static str,
        /// Description of the required value.
        message: String,
    },
    /// The caller cancelled the evaluation.
    #[error("Configure was cancelled.")]
    Cancelled,
    /// The evaluator exhausted its command budget.
    #[error("Configure reached its limit of {limit} commands.")]
    StepBudgetExceeded {
        /// Configured command limit.
        limit: u64,
    },
    /// A subprocess was denied and a partial result was unavailable.
    #[error("Configure could not continue after a subprocess request was denied.")]
    DeniedProcess {
        /// Subprocess requests retained by the host.
        commands: Vec<DeniedCommand>,
    },
    /// Evaluation failed.
    #[error("Configure failed with exit code {exit_code}.")]
    ConfigureFailed {
        /// Exit code from the evaluator.
        exit_code: i32,
        /// Diagnostics emitted during evaluation.
        diagnostics: Vec<Diagnostic>,
    },
    /// Resolution or generation failed after evaluation.
    #[error("Generation failed. {message}")]
    GenerateFailed {
        /// Resolver or generator error.
        message: String,
        /// Diagnostics emitted during evaluation and generation.
        diagnostics: Vec<Diagnostic>,
    },
    /// A host operation failed.
    #[error(transparent)]
    Io(#[from] HostError),
}

/// Settings for one project configure.
#[derive(Clone)]
pub struct ConfigureRequest {
    /// Directory that contains the top-level CMakeLists.txt.
    pub source_dir: PathBuf,
    /// Directory that receives configure and generation output.
    pub build_dir: PathBuf,
    /// Generator selected by the caller. An absent value permits automatic selection.
    pub generator: Option<GeneratorKind>,
    /// Cache definitions in application order.
    pub defines: Vec<CacheDefine>,
    /// Directory that contains the CMake Modules and Templates directories.
    pub cmake_root: PathBuf,
    /// Host supplied by the caller.
    pub host: Arc<dyn Host>,
    /// Policy for subprocess requests.
    pub process_policy: ProcessPolicy,
    /// Destination of writes.
    pub write_mode: WriteMode,
    /// Evaluator services and their default settings.
    pub services: Services,
    /// Build an evaluated workspace index.
    pub collect_index: bool,
    /// Cancellation token that overrides the token in services, if supplied.
    pub cancel: Option<CancelToken>,
    /// Command budget that overrides the budget in services, if supplied.
    pub step_budget: Option<u64>,
    /// Evaluator safety limits.
    pub limits: Limits,
    /// Initial diagnostic settings.
    pub diag: DiagOptions,
}

impl ConfigureRequest {
    /// Create a request with direct writes and the default evaluator settings.
    pub fn new(
        source_dir: impl Into<PathBuf>,
        build_dir: impl Into<PathBuf>,
        cmake_root: impl Into<PathBuf>,
        host: Arc<dyn Host>,
    ) -> Self {
        Self {
            source_dir: source_dir.into(),
            build_dir: build_dir.into(),
            generator: None,
            defines: Vec::new(),
            cmake_root: cmake_root.into(),
            host,
            process_policy: ProcessPolicy::default(),
            write_mode: WriteMode::default(),
            services: Services::default(),
            collect_index: false,
            cancel: None,
            step_budget: None,
            limits: Limits::default(),
            diag: DiagOptions::default(),
        }
    }

    /// Select a generator.
    #[must_use]
    pub fn generator(mut self, generator: GeneratorKind) -> Self {
        self.generator = Some(generator);
        self
    }

    /// Append a cache definition.
    #[must_use]
    pub fn define(mut self, definition: CacheDefine) -> Self {
        self.defines.push(definition);
        self
    }

    /// Set the subprocess policy.
    #[must_use]
    pub fn process_policy(mut self, policy: ProcessPolicy) -> Self {
        self.process_policy = policy;
        self
    }

    /// Set the write mode.
    #[must_use]
    pub fn write_mode(mut self, mode: WriteMode) -> Self {
        self.write_mode = mode;
        self
    }

    /// Set the evaluator services.
    #[must_use]
    pub fn services(mut self, services: Services) -> Self {
        self.services = services;
        self
    }

    /// Select whether to build an evaluated index.
    #[must_use]
    pub fn collect_index(mut self, collect: bool) -> Self {
        self.collect_index = collect;
        self
    }

    /// Set the cancellation token.
    #[must_use]
    pub fn cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Set the command budget.
    #[must_use]
    pub fn step_budget(mut self, limit: u64) -> Self {
        self.step_budget = Some(limit);
        self
    }

    /// Set the evaluator safety limits.
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Set the initial diagnostic options.
    #[must_use]
    pub fn diag(mut self, diag: DiagOptions) -> Self {
        self.diag = diag;
        self
    }

    /// Validate the request before any filesystem operation.
    ///
    /// Relative paths resolve against the host's initial working directory.
    ///
    /// # Errors
    ///
    /// Return an error for an invalid path or cache name, a cancelled session,
    /// or a zero command budget.
    pub fn validate(&self) -> Result<(), ApiError> {
        self.validate_at(&self.host.initial_cwd())
    }

    fn validate_at(&self, cwd: &Path) -> Result<(), ApiError> {
        for (field, value) in [
            ("source_dir", &self.source_dir),
            ("build_dir", &self.build_dir),
            ("cmake_root", &self.cmake_root),
        ] {
            if value.as_os_str().is_empty() || path::from_native(value).contains(&0) {
                return Err(ApiError::InvalidConfig {
                    field,
                    message: "Supply a nonempty path without null bytes.".into(),
                });
            }
            if !cwd.join(value).is_absolute() {
                return Err(ApiError::InvalidConfig {
                    field,
                    message: "Supply an absolute path or an absolute host working directory."
                        .into(),
                });
            }
        }
        for entry in &self.defines {
            if entry.name.is_empty()
                || entry
                    .name
                    .iter()
                    .any(|byte| matches!(*byte, 0 | b'\n' | b'\r' | b'"'))
            {
                return Err(ApiError::InvalidConfig {
                    field: "defines",
                    message: format!(
                        "Cannot write cache name {:?}. Use a nonempty name without null bytes, line breaks, or double quotes.",
                        entry.name
                    ),
                });
            }
        }
        if self
            .cancel
            .as_ref()
            .or(self.services.cancel.as_ref())
            .is_some_and(CancelToken::is_cancelled)
        {
            return Err(ApiError::Cancelled);
        }
        if self.step_budget.or(self.services.step_budget) == Some(0) {
            return Err(ApiError::StepBudgetExceeded { limit: 0 });
        }
        Ok(())
    }

    /// Apply the request's host policies and merge its evaluator settings.
    ///
    /// An overlay denies subprocesses because it cannot apply their writes in
    /// memory. A subprocess request in this mode makes the result partial.
    ///
    /// # Errors
    ///
    /// Return any error from [Self::validate].
    pub fn prepare(&self) -> Result<PreparedConfigure, ApiError> {
        let cwd = self.host.initial_cwd();
        self.validate_at(&cwd)?;
        let absolute = |path: &Path| cwd.join(path);
        let overlay = (self.write_mode == WriteMode::Overlay)
            .then(|| Arc::new(OverlayHost::from_shared(Arc::clone(&self.host))));
        let mut host: Arc<dyn Host> = match &overlay {
            Some(overlay) => Arc::clone(overlay) as Arc<dyn Host>,
            None => Arc::clone(&self.host),
        };
        let denied = if self.process_policy == ProcessPolicy::Deny || overlay.is_some() {
            let adapter = Arc::new(NoProcessHost::from_shared(host));
            host = Arc::clone(&adapter) as Arc<dyn Host>;
            Some(adapter)
        } else {
            None
        };
        let mut services = self.services.clone();
        services.cancel = self.cancel.clone().or(services.cancel);
        services.step_budget = self.step_budget.or(services.step_budget);
        if self.collect_index && matches!(services.provenance, LedgerLevel::Off) {
            services.provenance = LedgerLevel::Full { per_name_cap: 16 };
        }
        let source_dir = absolute(&self.source_dir);
        let build_dir = absolute(&self.build_dir);
        let cmake_root = absolute(&self.cmake_root);
        Ok(PreparedConfigure {
            source_dir,
            build_dir,
            cmake_root,
            cwd,
            generator: self.generator,
            defines: self.defines.clone(),
            limits: self.limits.clone(),
            diag: self.diag.clone(),
            services: Arc::new(services),
            host,
            base: Arc::clone(&self.host),
            overlay,
            denied,
            collect_index: self.collect_index,
        })
    }
}

/// Host policies and resolved paths for one configure session.
pub struct PreparedConfigure {
    /// Absolute source directory.
    pub source_dir: PathBuf,
    /// Absolute build directory.
    pub build_dir: PathBuf,
    /// Absolute root of the CMake module library.
    pub cmake_root: PathBuf,
    /// Initial working directory used to resolve relative paths.
    pub cwd: PathBuf,
    /// Generator selected by the caller, if specified.
    pub generator: Option<GeneratorKind>,
    /// Cache definitions in application order.
    pub defines: Vec<CacheDefine>,
    /// Evaluator safety limits.
    pub limits: Limits,
    /// Initial diagnostic settings.
    pub diag: DiagOptions,
    /// Combined evaluator settings for this session.
    pub services: Arc<Services>,
    host: Arc<dyn Host>,
    base: Arc<dyn Host>,
    overlay: Option<Arc<OverlayHost<dyn Host>>>,
    denied: Option<Arc<NoProcessHost<dyn Host>>>,
    collect_index: bool,
}

impl PreparedConfigure {
    /// Get the host to use for evaluation, resolution, and generation.
    #[must_use]
    pub fn host(&self) -> Arc<dyn Host> {
        Arc::clone(&self.host)
    }

    /// Get the retained subprocess requests denied during this session.
    #[must_use]
    pub fn denied_processes(&self) -> Vec<DeniedCommand> {
        self.denied
            .as_ref()
            .map_or_else(Vec::new, |host| host.denied_commands())
    }

    /// Attach the host's observations and build the requested index.
    ///
    /// Call this after the evaluator and generator have stopped using the host.
    /// An evaluator cancellation should return [ApiError::Cancelled] before this
    /// step if the caller requires a complete snapshot.
    #[must_use]
    pub fn finish(self, mut result: ConfigureResult) -> ConfigureResult {
        if let Some(adapter) = self.denied {
            let log = adapter.take_denied();
            if !log.commands.is_empty() || log.omitted_entries != 0 {
                if matches!(result.completion, CompletionState::Complete) {
                    result.completion = CompletionState::Partial {
                        denied_processes: Vec::new(),
                        omitted_denials: 0,
                        omitted_denial_bytes: 0,
                        cancelled: false,
                    };
                }
                if let CompletionState::Partial {
                    denied_processes,
                    omitted_denials,
                    omitted_denial_bytes,
                    ..
                } = &mut result.completion
                {
                    denied_processes.extend(log.commands);
                    *omitted_denials = omitted_denials.saturating_add(log.omitted_entries);
                    *omitted_denial_bytes = omitted_denial_bytes.saturating_add(log.omitted_bytes);
                }
            }
        }
        if let Some(overlay) = self.overlay {
            result.overlay_diff =
                Some(OverlayDiff::from_host(overlay.as_ref(), self.base.as_ref()));
        }
        if self.collect_index {
            result.build_index();
        }
        result
    }
}

#[cfg(test)]
mod tests;
