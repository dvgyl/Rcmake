//! Probe admission, key inputs, and outcomes.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_store::{Digest, Domain, KeyHasher, StoreMode};
use serde::{Deserialize, Serialize};

use super::template::{PathBindings, Templated};

/// A reason to execute the original probe without consulting the cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BypassReason {
    /// The probe cache is disabled.
    Disabled,
    /// The caller retains the scratch directory.
    DebugTryCompile,
    /// The caller records a trace.
    Trace,
    /// The caller records profiling events.
    Profiling,
    /// The probe configures a user project.
    ProjectSignature,
    /// The caller supplies a persistent binary directory.
    ExplicitBinaryDir,
    /// The toolchain or invocation lacks a deterministic execution contract.
    UnverifiedExecution,
}

/// Whether this call can use the probe cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProbeEligibility {
    /// The call can use the cache if its input validation succeeds.
    Eligible,
    /// Run the probe at the original call site.
    Bypass(BypassReason),
}

/// Conditions that the evaluator establishes before planning a probe.
pub(crate) struct Admission {
    /// The effective probe store mode.
    pub(crate) store_mode: StoreMode,
    /// The caller retains scratch files.
    pub(crate) debug_try_compile: bool,
    /// Tracing is active.
    pub(crate) trace: bool,
    /// Profiling is active.
    pub(crate) profiling: bool,
    /// The call uses the sources signature.
    pub(crate) sources_signature: bool,
    /// The build uses private, disposable scratch space.
    pub(crate) private_scratch: bool,
    /// The caller can validate all inputs and reproduce all visible effects.
    pub(crate) verified_execution: bool,
}

impl Admission {
    /// Check the call before accessing the store.
    pub(crate) fn eligibility(&self) -> ProbeEligibility {
        let reason = if self.store_mode == StoreMode::Off {
            BypassReason::Disabled
        } else if self.debug_try_compile {
            BypassReason::DebugTryCompile
        } else if self.trace {
            BypassReason::Trace
        } else if self.profiling {
            BypassReason::Profiling
        } else if !self.sources_signature {
            BypassReason::ProjectSignature
        } else if !self.private_scratch {
            BypassReason::ExplicitBinaryDir
        } else if !self.verified_execution {
            BypassReason::UnverifiedExecution
        } else {
            return ProbeEligibility::Eligible;
        };
        ProbeEligibility::Bypass(reason)
    }
}

/// Identity of the Rcmake executable and its build options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RcmakeBuildId(pub(crate) Digest);

/// Absolute path identity required to replay a probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReuseScope {
    /// Use the same path and target bindings on a hit.
    ExactBindings,
    /// The caller verified that moving the probe preserves all inputs and effects.
    ///
    /// This includes diagnostic text and any paths or metadata in artifact bytes.
    VerifiedRelocation,
}

/// Complete inputs that the caller knows before running a probe.
///
/// Represent ordered arguments in execution order. Include materialized source names
/// and bytes in `files`. The seed must cover effective policies and the build
/// configuration. Keep external reads in the journal.
#[derive(Clone, Debug)]
pub(crate) struct ProbeRecipe {
    /// Admission result for this invocation. This field does not change the key.
    pub(crate) eligibility: ProbeEligibility,
    /// Rcmake build identity.
    pub(crate) rcmake: RcmakeBuildId,
    /// Content identity of the CMake data used by the probe.
    pub(crate) cmake_root: Digest,
    /// Host identity required by the execution contract.
    pub(crate) host: Digest,
    /// Generator, toolset, platform, and instance identity.
    pub(crate) generator: Digest,
    /// Canonical project and source files by relative name.
    pub(crate) files: BTreeMap<BString, Templated>,
    /// Effective compiler and toolchain binary digests by role.
    pub(crate) tools: BTreeMap<BString, Digest>,
    /// Ordered compiler and linker flags.
    pub(crate) flags: Vec<BString>,
    /// Ordered preprocessor definitions.
    pub(crate) definitions: Vec<BString>,
    /// Ordered link libraries.
    pub(crate) link_libraries: Vec<BString>,
    /// Effective compiler target triple.
    pub(crate) target: BString,
    /// Effective sysroot identity, including its path when visible to tools.
    pub(crate) sysroot: BString,
    /// Canonical seed, enabled languages, and effective command options.
    pub(crate) seed: Digest,
    /// Canonical imported target and alias definitions.
    pub(crate) imports: Digest,
    /// Environment fingerprint from the complete inherited environment.
    pub(crate) environment: Digest,
    /// Path and artifact relocation contract.
    pub(crate) scope: ReuseScope,
    /// Bindings for this invocation.
    pub(crate) bindings: PathBindings,
}

/// Hash the complete inherited environment, including locale and empty values.
///
/// Pass the effective environment in order. This preserves the precedence of
/// duplicate names and case variants on hosts that support them.
pub(crate) fn environment_fingerprint(environment: &[(BString, BString)]) -> Digest {
    let mut key = KeyHasher::new(Domain::ProbeV1);
    key.field(0, b"environment");
    for (name, value) in environment {
        key.field(1, name);
        key.field(2, value);
    }
    key.finish()
}

/// Identify the generated project and source files for configure history.
pub(crate) fn project_recipe_digest(files: &BTreeMap<BString, Templated>) -> Digest {
    let mut key = KeyHasher::new(Domain::ProbeV1);
    key.field(0, b"project-recipe");
    for (name, contents) in files {
        key.field(1, name);
        key.field(2, contents.digest().as_bytes());
    }
    key.finish()
}

/// Hash the recipe with explicit field boundaries and stable iteration order.
pub(crate) fn compute_probe_key(recipe: &ProbeRecipe) -> Digest {
    let mut key = KeyHasher::new(Domain::ProbeV1);
    key.field(1, &1_u16.to_le_bytes());
    key.field(2, recipe.rcmake.0.as_bytes());
    key.field(3, recipe.cmake_root.as_bytes());
    key.field(4, recipe.host.as_bytes());
    key.field(5, recipe.generator.as_bytes());
    key.field(6, project_recipe_digest(&recipe.files).as_bytes());
    for (role, tool) in &recipe.tools {
        key.field(8, role);
        key.field(9, tool.as_bytes());
    }
    for flag in &recipe.flags {
        key.field(10, flag);
    }
    for definition in &recipe.definitions {
        key.field(11, definition);
    }
    for library in &recipe.link_libraries {
        key.field(12, library);
    }
    key.field(13, &recipe.target);
    key.field(14, &recipe.sysroot);
    key.field(15, recipe.seed.as_bytes());
    key.field(16, recipe.imports.as_bytes());
    key.field(17, recipe.environment.as_bytes());
    match recipe.scope {
        ReuseScope::ExactBindings => key.field(18, recipe.bindings.digest().as_bytes()),
        ReuseScope::VerifiedRelocation => key.field(19, &[]),
    }
    key.finish()
}

/// Output stream used by a captured diagnostic.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum DiagnosticStream {
    /// The standard output stream.
    Stdout,
    /// The standard error stream.
    Stderr,
}

/// A rendered diagnostic in the order in which the probe emitted it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ProbeDiagnostic<T> {
    /// The original output stream.
    pub(crate) stream: DiagnosticStream,
    /// Diagnostic bytes or their template.
    pub(crate) message: T,
}

/// An artifact that the cache can materialize for the current call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ArtifactRef {
    /// Content digest in the blob store.
    pub(crate) blob: Digest,
    /// Artifact location relative to the scratch directory.
    pub(crate) relative_path: Templated,
    /// Permission bits, including the executable bit.
    pub(crate) mode: u32,
}

/// Cached compile outcome. The run phase of `try_run` executes at the call site.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ProbeOutcome {
    /// Whether the compile and optional link succeeded.
    pub(crate) compiled: bool,
    /// Build exit status for the configure log.
    pub(crate) exit_code: i32,
    /// Exact captured compile output with path bindings.
    pub(crate) output: Templated,
    /// Optional executable or archive.
    pub(crate) artifact: Option<ArtifactRef>,
    /// Ordered rendered diagnostics that the caller can replay.
    pub(crate) diagnostics: Vec<ProbeDiagnostic<Templated>>,
}
