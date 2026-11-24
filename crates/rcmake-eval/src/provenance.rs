//! Bounded histories of variable writes.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, VecDeque};

use rcmake_base::BString;
use rcmake_diag::Backtrace;

use crate::services::LedgerLevel;

const MAX_EVENTS: usize = 2_000_000;
const PREVIEW_BYTES: usize = 256;

/// A change to a normal or cache variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarOp {
    /// Set a normal variable.
    Set,
    /// Unset a normal variable.
    Unset,
    /// Set a cache entry.
    CacheSet,
    /// Remove a cache entry.
    CacheUnset,
    /// Set or unset a normal variable in the parent scope.
    ParentScope,
}

/// A bounded preview of a variable value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValuePreview {
    /// First 256 bytes of the value, or all bytes of a shorter value.
    pub prefix: BString,
    /// Length of the full value in bytes.
    pub len: usize,
    /// FNV-1a hash of the full value.
    pub hash: u64,
}

impl ValuePreview {
    fn new(value: &[u8]) -> Self {
        let hash = value.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        });
        Self {
            prefix: value.get(..PREVIEW_BYTES).unwrap_or(value).into(),
            len: value.len(),
            hash,
        }
    }
}

/// A recorded variable write.
#[derive(Clone, Debug)]
pub struct VarEvent {
    /// Execution sequence number supplied by the evaluator.
    pub seq: u64,
    /// Operation that changed the variable.
    pub op: VarOp,
    /// Depth of the scope in which the write occurred.
    pub scope_depth: u32,
    /// New value, or `None` for an unset operation.
    pub value_preview: Option<ValuePreview>,
    /// Call stack at the write.
    pub backtrace: Backtrace,
}

/// Variable histories for a single evaluation session.
#[derive(Clone, Debug, Default)]
pub struct VarLedger {
    level: LedgerLevel,
    events: BTreeMap<BString, VecDeque<VarEvent>>,
    retained: usize,
    max_events: usize,
    incomplete: bool,
    total_limit_reached: bool,
}

impl VarLedger {
    /// Start recording with the session's default total event limit.
    #[must_use]
    pub fn new(level: LedgerLevel) -> Self {
        Self::with_event_limit(level, MAX_EVENTS)
    }

    /// Start recording with a lower total limit on retained events.
    #[must_use]
    pub fn with_event_limit(level: LedgerLevel, limit: usize) -> Self {
        Self {
            level,
            max_events: limit.min(MAX_EVENTS),
            ..Self::default()
        }
    }

    /// Record one write. Call this method in execution order.
    pub fn record(
        &mut self,
        name: &[u8],
        seq: u64,
        op: VarOp,
        scope_depth: u32,
        value: Option<&[u8]>,
        backtrace: &Backtrace,
    ) {
        if self.total_limit_reached {
            return;
        }
        let cap = match &self.level {
            LedgerLevel::Off => return,
            LedgerLevel::Names(names) if names.contains(name) => self.max_events,
            LedgerLevel::Names(_) => return,
            LedgerLevel::Full { per_name_cap } => *per_name_cap as usize,
        };
        if cap == 0 {
            self.incomplete = true;
            return;
        }

        let history = self.events.get_mut(name);
        let replace = matches!(self.level, LedgerLevel::Full { .. })
            && history.as_ref().is_some_and(|events| events.len() == cap);
        if replace {
            self.incomplete = true;
        } else if self.retained == self.max_events {
            self.incomplete = true;
            self.total_limit_reached = true;
            return;
        } else {
            self.retained += 1;
        }

        let event = VarEvent {
            seq,
            op,
            scope_depth,
            value_preview: value.map(ValuePreview::new),
            backtrace: backtrace.clone(),
        };
        if let Some(history) = history {
            if replace {
                history.pop_front();
            }
            history.push_back(event);
        } else {
            self.events.insert(name.into(), VecDeque::from([event]));
        }
    }

    /// Iterate over one variable's retained events, oldest first.
    pub fn history(&self, name: &[u8]) -> impl DoubleEndedIterator<Item = &VarEvent> {
        self.events.get(name).into_iter().flat_map(VecDeque::iter)
    }

    /// Iterate over the recorded names in byte order.
    pub fn names(&self) -> impl ExactSizeIterator<Item = &BString> {
        self.events.keys()
    }

    /// Return whether an event was discarded because a limit was reached.
    #[must_use]
    pub fn is_incomplete(&self) -> bool {
        self.incomplete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ledger: &mut VarLedger, name: &[u8], seq: u64) {
        ledger.record(
            name,
            seq,
            VarOp::Set,
            0,
            Some(b"value"),
            &Backtrace::default(),
        );
    }

    #[test]
    fn retains_the_latest_events_for_each_name() {
        let mut ledger = VarLedger::new(LedgerLevel::Full { per_name_cap: 2 });
        set(&mut ledger, b"Z", 1);
        set(&mut ledger, b"A", 2);
        set(&mut ledger, b"Z", 3);
        set(&mut ledger, b"Z", 4);
        assert_eq!(
            ledger
                .names()
                .map(|name| name.as_slice())
                .collect::<Vec<_>>(),
            [b"A".as_slice(), b"Z".as_slice()]
        );
        assert_eq!(
            ledger
                .history(b"Z")
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [3, 4]
        );
        assert_eq!(ledger.history(b"A").next().map(|event| event.seq), Some(2));
        assert!(ledger.is_incomplete());
    }

    #[test]
    fn filters_names_and_stops_at_the_total_limit() {
        let names = [BString::from("Z")].into_iter().collect();
        let mut ledger = VarLedger::with_event_limit(LedgerLevel::Names(names), 2);
        set(&mut ledger, b"A", 1);
        set(&mut ledger, b"Z", 2);
        set(&mut ledger, b"Z", 3);
        assert!(!ledger.is_incomplete());
        set(&mut ledger, b"Z", 4);
        assert_eq!(
            ledger
                .history(b"Z")
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(ledger.history(b"A").next().is_none());
        assert!(ledger.is_incomplete());
    }

    #[test]
    fn off_and_zero_capacity_keep_no_history() {
        let mut off = VarLedger::default();
        set(&mut off, b"A", 1);
        assert_eq!(off.names().len(), 0);
        assert!(!off.is_incomplete());

        let mut zero = VarLedger::new(LedgerLevel::Full { per_name_cap: 0 });
        set(&mut zero, b"A", 1);
        assert_eq!(zero.names().len(), 0);
        assert!(zero.is_incomplete());
    }

    #[test]
    fn keeps_raw_value_prefix_and_distinguishes_empty_from_unset() {
        let mut ledger = VarLedger::new(LedgerLevel::Full { per_name_cap: 4 });
        let mut bytes = vec![0xff; PREVIEW_BYTES];
        bytes.push(1);
        let backtrace = Backtrace::default();
        ledger.record(b"A", 1, VarOp::Set, 3, Some(&bytes), &backtrace);
        bytes.pop();
        bytes.push(2);
        ledger.record(b"A", 2, VarOp::Set, 3, Some(&bytes), &backtrace);
        ledger.record(b"A", 3, VarOp::CacheSet, 3, Some(b""), &backtrace);
        ledger.record(b"A", 4, VarOp::ParentScope, 3, None, &backtrace);

        let events: Vec<_> = ledger.history(b"A").collect();
        let previews: Vec<_> = events
            .iter()
            .filter_map(|event| event.value_preview.as_ref())
            .collect();
        let [first, second, empty] = previews.as_slice() else {
            assert_eq!(previews.len(), 3);
            return;
        };
        assert_eq!(first.prefix.as_slice(), &[0xff; PREVIEW_BYTES]);
        assert_eq!(first.prefix, second.prefix);
        assert_eq!(first.len, PREVIEW_BYTES + 1);
        assert_ne!(first.hash, second.hash);
        assert!(empty.prefix.is_empty());
        assert_eq!(events.last().map(|event| event.scope_depth), Some(3));
        assert!(
            events
                .last()
                .is_some_and(|event| event.value_preview.is_none())
        );
    }
}
