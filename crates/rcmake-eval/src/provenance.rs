//! Bounded histories of variable writes.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, VecDeque};

use rcmake_base::BString;
use rcmake_diag::Backtrace;
use rcmake_model::DirId;

use crate::services::LedgerLevel;

const MAX_EVENTS: usize = 32_768;
const MAX_HISTORY: usize = 64;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const HISTORY_BYTES: usize = 192;
const EVENT_BYTES: usize = 2 * std::mem::size_of::<(VarEvent, usize)>();
const PREVIEW_BYTES: usize = 256;

impl crate::Interp {
    /// Return the names, declaration sites and parameters of user-defined commands.
    pub fn callable_definitions(
        &self,
    ) -> impl Iterator<Item = (crate::UserCommandKind, &BString, &[u8], u32, &[BString])> {
        self.scripted.iter().filter_map(|(key, cmd)| {
            let crate::Command::User(cmd) = cmd else {
                return None;
            };
            // Renamed previous definitions have a separate internal lookup key.
            (key.eq_ignore_ascii_case(&cmd.name)).then_some((
                cmd.kind,
                &cmd.name,
                cmd.body.file.as_ref(),
                cmd.line,
                cmd.params.as_slice(),
            ))
        })
    }
}

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
        Self {
            prefix: value.get(..PREVIEW_BYTES).unwrap_or(value).into(),
            len: value.len(),
            hash: value_hash(value),
        }
    }

    /// Check whether these recorded bytes and hash match a full value.
    #[must_use]
    pub fn matches(&self, value: &[u8]) -> bool {
        self.len == value.len()
            && value.starts_with(&self.prefix)
            && (self.len == self.prefix.len() || self.hash == value_hash(value))
    }
}

fn value_hash(value: &[u8]) -> u64 {
    value.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
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
    /// Stable identity of the scope for this session.
    pub scope_id: u64,
    /// Directory whose variable scope changed, when the write is directory-scoped.
    pub binding_dir: Option<DirId>,
    /// Value before the write, or absence.
    pub old_value: Option<ValuePreview>,
    /// Value after the write, or absence.
    pub new_value: Option<ValuePreview>,
    /// Call stack at the write.
    pub backtrace: Backtrace,
}

/// Values and position captured by the evaluator at a variable write.
pub struct VarWrite<'a> {
    /// Execution sequence number.
    pub seq: u64,
    /// Operation that changed the variable.
    pub op: VarOp,
    /// Depth of the destination scope.
    pub scope_depth: u32,
    /// Identity of the destination scope, unique for the session.
    pub scope_id: u64,
    /// Value before the write, or absence.
    pub old_value: Option<&'a [u8]>,
    /// Value after the write, or absence.
    pub new_value: Option<&'a [u8]>,
    /// Call stack at the write.
    pub backtrace: &'a Backtrace,
}

#[derive(Clone, Debug, Default)]
struct History {
    events: VecDeque<(VarEvent, usize)>,
    dropped: u32,
}

/// Variable histories for a single evaluation session.
#[derive(Clone, Debug, Default)]
pub struct VarLedger {
    level: LedgerLevel,
    events: BTreeMap<BString, History>,
    retained: usize,
    max_events: usize,
    incomplete: bool,
    bytes: usize,
}

impl VarLedger {
    /// Start recording with the session's default total event limit.
    #[must_use]
    pub fn new(level: LedgerLevel) -> Self {
        Self::with_event_limit(level, MAX_EVENTS)
    }

    /// Start recording with a lower total limit on retained events.
    #[must_use]
    pub fn with_event_limit(mut level: LedgerLevel, limit: usize) -> Self {
        let mut bytes = 0usize;
        let mut incomplete = false;
        if let LedgerLevel::Names(names) = &mut level {
            names.retain(|name| {
                let cost = HISTORY_BYTES.saturating_add(name.len());
                if bytes.saturating_add(cost) > MAX_BYTES / 4 {
                    incomplete = true;
                    return false;
                }
                bytes += cost;
                true
            });
        }
        Self {
            level,
            max_events: limit.min(MAX_EVENTS),
            bytes,
            incomplete,
            ..Self::default()
        }
    }

    /// Record one write. Pass the values observed at the mutation point.
    pub fn record(&mut self, name: &[u8], write: VarWrite<'_>) {
        self.record_at(name, write, None);
    }

    pub(crate) fn record_at(
        &mut self,
        name: &[u8],
        write: VarWrite<'_>,
        binding_dir: Option<DirId>,
    ) {
        let cap = match &self.level {
            LedgerLevel::Off => return,
            LedgerLevel::Names(names) if names.contains(name) => MAX_HISTORY,
            LedgerLevel::Names(_) => return,
            LedgerLevel::Full { per_name_cap } => (*per_name_cap as usize).min(MAX_HISTORY),
        };
        if cap == 0 || self.max_events == 0 {
            self.incomplete = true;
            return;
        }
        let event_bytes = write
            .backtrace
            .frames()
            .fold(EVENT_BYTES, |bytes, frame| {
                bytes
                    .saturating_add(96)
                    .saturating_add(frame.file.len())
                    .saturating_add(frame.name.as_ref().map_or(0, |name| name.len()))
            })
            .saturating_add(
                write
                    .old_value
                    .map_or(0, |value| value.len().min(PREVIEW_BYTES)),
            )
            .saturating_add(
                write
                    .new_value
                    .map_or(0, |value| value.len().min(PREVIEW_BYTES)),
            );

        if !self.events.contains_key(name) {
            let key_bytes = HISTORY_BYTES.saturating_add(name.len());
            if self.events.len() >= MAX_EVENTS || self.bytes.saturating_add(key_bytes) > MAX_BYTES {
                self.incomplete = true;
                return;
            }
            self.bytes += key_bytes;
            self.events.insert(
                name.into(),
                History {
                    events: VecDeque::with_capacity(1),
                    dropped: 0,
                },
            );
        }
        let Some(history) = self.events.get_mut(name) else {
            return;
        };
        if event_bytes > MAX_BYTES {
            history.dropped = history.dropped.saturating_add(1);
            self.incomplete = true;
            return;
        }
        while history.events.len() >= cap
            || self.retained >= self.max_events
            || self.bytes.saturating_add(event_bytes) > MAX_BYTES
        {
            history.dropped = history.dropped.saturating_add(1);
            self.incomplete = true;
            let Some((_, bytes)) = history.events.pop_front() else {
                history.events.shrink_to_fit();
                return;
            };
            self.retained -= 1;
            self.bytes -= bytes;
        }
        if history.events.capacity() > 2 * (history.events.len() + 1) {
            history.events.shrink_to_fit();
        }
        self.retained += 1;
        self.bytes += event_bytes;
        history.events.push_back((
            VarEvent {
                seq: write.seq,
                op: write.op,
                scope_depth: write.scope_depth,
                scope_id: write.scope_id,
                binding_dir,
                old_value: write.old_value.map(ValuePreview::new),
                new_value: write.new_value.map(ValuePreview::new),
                backtrace: write.backtrace.clone(),
            },
            event_bytes,
        ));
    }

    /// Iterate over one variable's retained events, oldest first.
    pub fn history(&self, name: &[u8]) -> impl DoubleEndedIterator<Item = &VarEvent> {
        self.events
            .get(name)
            .into_iter()
            .flat_map(|history| history.events.iter().map(|(event, _)| event))
    }

    /// Get the number of discarded writes for a recorded variable name.
    #[must_use]
    pub fn dropped(&self, name: &[u8]) -> u32 {
        self.events.get(name).map_or(0, |history| history.dropped)
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
            VarWrite {
                seq,
                op: VarOp::Set,
                scope_depth: 0,
                scope_id: 0,
                old_value: None,
                new_value: Some(b"value"),
                backtrace: &Backtrace::default(),
            },
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
    fn filters_names_and_keeps_recording_at_the_total_limit() {
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
            [3, 4]
        );
        assert!(ledger.history(b"A").next().is_none());
        assert_eq!(ledger.dropped(b"Z"), 1);
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
        let mut record = |seq, op, old_value: Option<&[u8]>, new_value: Option<&[u8]>| {
            ledger.record(
                b"A",
                VarWrite {
                    seq,
                    op,
                    scope_depth: 3,
                    scope_id: 11,
                    old_value,
                    new_value,
                    backtrace: &backtrace,
                },
            );
        };
        record(1, VarOp::Set, None, Some(&bytes));
        bytes.pop();
        bytes.push(2);
        record(2, VarOp::Set, Some(b"old"), Some(&bytes));
        record(3, VarOp::CacheSet, None, Some(b""));
        record(4, VarOp::ParentScope, Some(b""), None);

        let events: Vec<_> = ledger.history(b"A").collect();
        let previews: Vec<_> = events
            .iter()
            .filter_map(|event| event.new_value.as_ref())
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
        assert_eq!(events.last().map(|event| event.scope_id), Some(11));
        assert_eq!(
            events
                .get(1)
                .and_then(|event| event.old_value.as_ref())
                .map(|value| value.prefix.as_slice()),
            Some(b"old".as_slice())
        );
        assert!(events.last().is_some_and(|event| event.new_value.is_none()));
    }
}
