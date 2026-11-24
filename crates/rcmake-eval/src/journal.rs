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
    JournalDigest, JournalFileStat, JournalInput, JournalStamp, PropertyKey, PropertyOwner,
    StateKey, StateLookup, TargetKey, VariableScope, directory_digest, file_digest, policy_digest,
};
