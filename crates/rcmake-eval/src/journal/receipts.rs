// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeSet;
use std::sync::Mutex;

use super::JournalDigest;

pub(super) const MAX_RECEIPTS: usize = 64;

#[derive(Default)]
struct Receipts {
    previous: BTreeSet<JournalDigest>,
    current: BTreeSet<JournalDigest>,
}

/// Bounded confirmations for inputs that a configure service verifies directly.
///
/// A receipt confirms a previous observation. The service must independently
/// verify every input and determine the expected result before reuse.
#[derive(Default)]
pub struct ReplayReceipts(Mutex<Receipts>);

impl ReplayReceipts {
    /// Check for a matching confirmation from the previous successful configure.
    #[must_use]
    pub fn contains(&self, digest: JournalDigest) -> bool {
        self.0
            .lock()
            .is_ok_and(|receipts| receipts.previous.contains(&digest))
    }

    /// Retain a confirmation that the current configure verified.
    pub fn record(&self, digest: JournalDigest) {
        if let Ok(mut receipts) = self.0.lock()
            && receipts.current.len() < MAX_RECEIPTS
        {
            receipts.current.insert(digest);
        }
    }

    pub(super) fn restore(&self, previous: &[JournalDigest]) {
        if let Ok(mut receipts) = self.0.lock() {
            receipts.previous.clear();
            receipts.current.clear();
            if previous.len() <= MAX_RECEIPTS {
                receipts.previous.extend(previous);
            }
        }
    }

    pub(super) fn saved(&self) -> Vec<JournalDigest> {
        self.0.lock().map_or_else(
            |_| Vec::new(),
            |receipts| receipts.current.iter().copied().collect(),
        )
    }
}
