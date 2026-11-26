//! Recording and verification for configure replay.

#![forbid(unsafe_code)]

#[cfg(feature = "replay")]
mod activate;
#[cfg(feature = "replay")]
mod codec;
#[cfg(feature = "replay")]
mod frame;
#[cfg(feature = "replay")]
mod inputs;
#[cfg(feature = "replay")]
mod leaf;
#[cfg(feature = "replay")]
mod receipts;
#[cfg(feature = "replay")]
mod region;
#[cfg(feature = "replay")]
mod replay;
#[cfg(feature = "replay")]
mod setup;
#[cfg(feature = "replay")]
mod trace;

#[cfg(feature = "replay")]
pub(crate) use activate::ReplayState;

#[cfg(feature = "replay")]
pub use activate::ReplayHost;
#[cfg(feature = "replay")]
pub use frame::{EncodeError, EncodeJson, JsonSink, ObjectWriter};
#[cfg(feature = "replay")]
pub use inputs::{ReplaySetupInputs, resolve_setup_file};
#[cfg(feature = "replay")]
pub use receipts::ReplayReceipts;

#[cfg(feature = "replay")]
pub use region::{
    CacheValue, EntryContext, Journal, JournalEffect, OpaqueOperation, OpaqueReason, PropertyEdit,
    PropertyOrigin, RegionKey, RegionKind, RegionRecord, RegionState,
};
#[cfg(feature = "replay")]
pub use replay::{
    ConfigureSnapshot, ReplayApplyError, ReplayCodecError, ReplayMiss, ReplayMissReason,
    VerifiedConfigure, VerifiedRegion, verify_configure, verify_region,
};
#[cfg(feature = "replay")]
pub use trace::{
    JournalDigest, JournalFileStat, JournalInput, JournalStamp, PropertyKey, PropertyOwner,
    StateKey, StateLookup, TargetKey, VariableScope, directory_digest, file_digest, policy_digest,
};

#[cfg(not(feature = "replay"))]
mod disabled {
    use rcmake_expr::ExpandedArg;
    use rcmake_model::DirId;
    use rcmake_syntax::Argument;

    use crate::code::{Code, SourceText};
    use crate::interp::BodyMode;
    use crate::{CmdResult, Cx, Interp, Registry};

    impl Cx<'_> {
        /// Start a setup span. No-op when the `replay` feature is disabled.
        #[inline(always)]
        pub fn begin_replay_setup(&mut self) {}

        /// End a setup span. No-op when the `replay` feature is disabled.
        #[inline(always)]
        pub fn end_replay_setup(&mut self, _ok: bool) {}

        /// End the setup span of `project()`. No-op when the `replay` feature is
        /// disabled.
        #[inline(always)]
        pub fn end_replay_project(&mut self, _ok: bool) {}

        /// Note a system or compiler determination. No-op when the `replay`
        /// feature is disabled.
        #[inline(always)]
        pub fn note_replay_determination(&mut self) {}
    }

    pub(crate) struct ForeachSaved;

    impl Registry {
        /// Mark the standard implementations that have a configure replay contract.
        /// No-op when the `replay` feature is disabled.
        #[inline(always)]
        pub fn mark_replay_commands(&mut self) {}
    }

    impl Interp {
        #[inline(always)]
        pub(crate) fn note_replay_source(&mut self, _abs: &[u8], _text: &SourceText) {}

        #[inline(always)]
        pub(crate) fn run_listfile_body(
            &mut self,
            code: &Code,
            _abs: &[u8],
            _directory: bool,
        ) -> CmdResult {
            self.run_code(code, 0, code.len(), BodyMode::File)
        }

        #[inline(always)]
        pub(crate) fn observe_replay_variable(&self, _name: &[u8]) {}

        #[inline(always)]
        pub(crate) fn observe_replay_variable_access(&self, _name: &[u8]) {}

        #[inline(always)]
        pub(crate) fn observe_replay_env(&mut self, _name: &[u8]) {}

        #[inline(always)]
        pub(crate) fn observe_replay_condition(&mut self, _args: &[ExpandedArg]) {}

        #[inline(always)]
        pub(crate) fn mark_replay_opaque(&mut self) {}

        #[inline(always)]
        pub(crate) fn note_replay_var_write(&mut self, _name: &[u8]) {}

        #[inline(always)]
        pub(crate) fn note_replay_cache_write(&mut self, _name: &[u8]) {}

        #[inline(always)]
        pub(crate) fn note_replay_defer(
            &mut self,
            _dir: DirId,
            _command: &[u8],
            _args: &[Argument],
        ) {
        }

        #[inline(always)]
        pub(crate) fn note_replay_expansion(
            &mut self,
            _lower: &[u8],
            _raw_args: &[Argument],
            _args: &[ExpandedArg],
        ) {
        }

        #[inline(always)]
        pub(crate) fn note_replay_executed_listfile(&mut self, _file: &[u8]) {}

        #[inline(always)]
        pub(crate) fn capture_replay_foreach_var(&self, _name: Option<&[u8]>) -> ForeachSaved {
            ForeachSaved
        }

        #[inline(always)]
        pub(crate) fn finish_replay_foreach_var(
            &mut self,
            _name: Option<&[u8]>,
            _before: ForeachSaved,
        ) {
        }
    }
}
