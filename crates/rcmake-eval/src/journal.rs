//! Recording and verification for configure replay.

#![forbid(unsafe_code)]

mod region;
mod replay;
mod trace;

pub use region::{
    CacheValue, CommandClass, EntryContext, Journal, JournalEffect, OpaqueReason, OutputStream,
    PropertyEdit, PropertyOrigin, RegionKey, RegionKind, RegionRecord, RegionState,
};
pub use replay::{
    ConfigureSnapshot, ReplayApplyError, ReplayMiss, VerifiedConfigure, VerifiedRegion,
    verify_configure, verify_region,
};
pub use trace::{
    JournalInput, PropertyKey, PropertyOwner, StateKey, StateLookup, TargetKey, VariableScope,
    policy_digest,
};
