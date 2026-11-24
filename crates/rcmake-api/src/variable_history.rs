use std::collections::BTreeSet;
use std::ops::Deref;

use rcmake_base::BString;
use rcmake_eval::{LedgerLevel, VarLedger, VarWrite};

const MAX_TRUNCATED_NAMES: usize = 65_536;

/// Coverage of one variable's write history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VariableHistory {
    /// The snapshot does not contain a ledger.
    Unavailable,
    /// The recording policy excluded this name.
    Excluded,
    /// The ledger retained all recorded writes for this name, possibly zero.
    Complete,
    /// The ledger discarded one or more writes for this name.
    Truncated,
    /// The ledger lost writes and their names are unknown.
    Unknown,
}

impl VariableHistory {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Excluded => "excluded",
            Self::Complete => "complete",
            Self::Truncated => "truncated",
            Self::Unknown => "unknown",
        }
    }
}

/// Variable ledger and coverage information from one evaluation.
#[derive(Clone, Debug, Default)]
pub struct VarLedgerSnapshot {
    ledger: VarLedger,
    level: LedgerLevel,
    truncated: BTreeSet<BString>,
    session_truncated: Option<bool>,
    unknown_names: bool,
}

impl VarLedgerSnapshot {
    /// Start recording with the evaluator's default total event limit.
    #[must_use]
    pub fn new(level: LedgerLevel) -> Self {
        Self::from_ledger(VarLedger::new(level.clone()), level)
    }

    /// Start recording with a lower total event limit.
    #[must_use]
    pub fn with_event_limit(level: LedgerLevel, limit: usize) -> Self {
        Self::from_ledger(VarLedger::with_event_limit(level.clone(), limit), level)
    }

    /// Attach a ledger that the evaluator recorded independently.
    ///
    /// Supply its original recording policy. When the ledger reports loss,
    /// the snapshot cannot determine which names lost writes.
    #[must_use]
    pub fn from_ledger(ledger: VarLedger, level: LedgerLevel) -> Self {
        let unknown_names = ledger.is_incomplete();
        let session_truncated = (!unknown_names).then_some(false);
        Self {
            ledger,
            level,
            truncated: BTreeSet::new(),
            session_truncated,
            unknown_names,
        }
    }

    /// Record one write and its retention status.
    ///
    /// Supply a unique sequence number in increasing order.
    pub fn record(&mut self, name: &[u8], write: VarWrite<'_>) {
        if !self.selected(name) {
            return;
        }
        let sequence = write.seq;
        self.ledger.record(name, write);
        let retained = self.ledger.history(name).next_back().map(|event| event.seq);
        if retained != Some(sequence) {
            if self.ledger.dropped(name) == 0 {
                if self.truncated.len() < MAX_TRUNCATED_NAMES {
                    self.truncated.insert(name.into());
                } else if !self.truncated.contains(name) {
                    self.unknown_names = true;
                    self.session_truncated = Some(true);
                }
            }
            if !matches!(self.level, LedgerLevel::Full { per_name_cap: 0 }) {
                self.session_truncated = Some(true);
            }
        }
    }

    /// Report coverage for one name.
    #[must_use]
    pub fn coverage(&self, name: &[u8]) -> VariableHistory {
        if !self.selected(name) {
            VariableHistory::Excluded
        } else if self.ledger.dropped(name) != 0 || self.truncated.contains(name) {
            VariableHistory::Truncated
        } else if self.unknown_names {
            VariableHistory::Unknown
        } else {
            VariableHistory::Complete
        }
    }

    /// Report whether the session discarded a write outside its per-name retention.
    ///
    /// An absent value means an independently recorded ledger reported a loss
    /// whose cause is unavailable.
    #[must_use]
    pub fn session_truncated(&self) -> Option<bool> {
        self.session_truncated
    }

    fn selected(&self, name: &[u8]) -> bool {
        match &self.level {
            LedgerLevel::Off => false,
            LedgerLevel::Names(names) => names.contains(name),
            LedgerLevel::Full { .. } => true,
        }
    }
}

impl Deref for VarLedgerSnapshot {
    type Target = VarLedger;

    fn deref(&self) -> &Self::Target {
        &self.ledger
    }
}
