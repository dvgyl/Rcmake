//! Recording and verification for configure replay.

#![forbid(unsafe_code)]

mod activate;
pub(crate) use activate::ReplayObservations;
pub(crate) use leaf::REPLAY_LINK_CHECK;
mod frame;
mod leaf;
pub(crate) use leaf::shape::valid_expansion;
mod path_bytes;
mod receipts;
mod region;
mod replay;
mod trace;

pub use activate::ReplayHost;
pub(crate) use activate::ReplaySession;
pub use receipts::ReplayReceipts;

pub use region::{
    CacheValue, EntryContext, Journal, JournalEffect, OpaqueOperation, OpaqueReason, PropertyEdit,
    PropertyOrigin, RegionKey, RegionKind, RegionRecord, RegionState,
};
pub use replay::{
    ConfigureSnapshot, ReplayApplyError, ReplayCodecError, ReplayMiss, ReplayMissReason,
    VerifiedConfigure, VerifiedRegion, verify_configure, verify_region,
};
pub use trace::{
    JournalDigest, JournalFileStat, JournalInput, JournalStamp, PropertyKey, PropertyOwner,
    StateKey, StateLookup, TargetKey, VariableScope, directory_digest, file_digest, policy_digest,
};
