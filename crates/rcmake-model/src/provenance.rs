//! Source-language mappings and property write histories.

#![forbid(unsafe_code)]

use indexmap::{Equivalent, IndexMap, IndexSet};
use rcmake_base::BString;
use rcmake_diag::Backtrace;

use crate::{AppendMode, PropScope};

/// Languages enabled in a project and their source extension mappings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LanguageRegistry {
    /// Languages in the order in which they were first enabled.
    pub enabled: Vec<BString>,
    /// Exact extension bytes without a leading dot, mapped to a language.
    pub by_extension: IndexMap<BString, BString>,
    /// Union of the ignored extensions reported by enabled languages.
    pub ignored: IndexSet<BString>,
}

impl LanguageRegistry {
    /// Record a language after its toolchain has been enabled.
    pub fn register_language(&mut self, language: &[u8]) {
        if !language.is_empty()
            && language != b"NONE"
            && !self.enabled.iter().any(|enabled| enabled == language)
        {
            self.enabled.push(language.into());
        }
    }

    /// Load the source and ignored extension lists for a language.
    ///
    /// A later mapping for an extension replaces its earlier mapping. The lists
    /// contain individual extensions without a leading dot. Case is significant.
    pub fn set_extensions(
        &mut self,
        language: &[u8],
        source: impl IntoIterator<Item = impl AsRef<[u8]>>,
        ignored: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) {
        for extension in source {
            let extension = extension.as_ref();
            if !extension.is_empty() {
                self.by_extension.insert(extension.into(), language.into());
            }
        }
        for extension in ignored {
            let extension = extension.as_ref();
            if !extension.is_empty() {
                self.ignored.insert(extension.into());
            }
        }
    }

    /// Get the language for an extension, with an optional leading dot.
    #[must_use]
    pub fn language_for_extension(&self, extension: &[u8]) -> Option<&BString> {
        self.by_extension
            .get(extension.strip_prefix(b".").unwrap_or(extension))
    }

    /// Return whether an extension is ignored after source mappings take precedence.
    #[must_use]
    pub fn is_ignored(&self, extension: &[u8]) -> bool {
        let extension = extension.strip_prefix(b".").unwrap_or(extension);
        !self.by_extension.contains_key(extension) && self.ignored.contains(extension)
    }
}

/// Operation that changed a property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropOp {
    /// Set a property value.
    Set,
    /// Append a list item or raw string to a property value.
    Append(AppendMode),
    /// Unset a property.
    Unset,
}

const PREVIEW_BYTES: usize = 256;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_EVENTS: usize = 32_768;
const HISTORY_BYTES: usize = 192;
const EVENT_BYTES: usize = 2 * (std::mem::size_of::<PropEvent>() + std::mem::size_of::<usize>());

/// Bounded preview of one property value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropValuePreview {
    prefix: BString,
    len: usize,
    hash: u64,
}

impl PropValuePreview {
    /// Capture up to 256 bytes and hash the full value.
    #[must_use]
    pub fn new(value: &[u8]) -> Self {
        let hash = value.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        });
        Self {
            prefix: value.get(..PREVIEW_BYTES).unwrap_or(value).into(),
            len: value.len(),
            hash,
        }
    }

    /// First bytes of the value.
    #[must_use]
    pub fn prefix(&self) -> &[u8] {
        &self.prefix
    }

    /// Length of the full value in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the full value is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// FNV-1a hash of the full value.
    #[must_use]
    pub fn hash(&self) -> u64 {
        self.hash
    }
}

/// A recorded property write.
#[derive(Clone, Debug)]
pub struct PropEvent {
    /// Execution sequence number supplied by the evaluator.
    pub seq: u64,
    /// Operation that changed the property.
    pub op: PropOp,
    /// Value before the write, or absence.
    pub old_value: Option<PropValuePreview>,
    /// Value after the write, or absence.
    pub new_value: Option<PropValuePreview>,
    /// Call stack at the write.
    pub origin: Backtrace,
}

impl PropEvent {
    /// Capture a successful write and its old and new values.
    #[must_use]
    pub fn new(
        seq: u64,
        op: PropOp,
        old: Option<&[u8]>,
        new: Option<&[u8]>,
        origin: &Backtrace,
    ) -> Self {
        Self {
            seq,
            op,
            old_value: old.map(PropValuePreview::new),
            new_value: new.map(PropValuePreview::new),
            origin: origin.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct History {
    events: Vec<PropEvent>,
    costs: Vec<usize>,
    dropped: u32,
}

/// Recent property writes in one evaluation session.
#[derive(Clone, Debug, Default)]
pub struct PropLedger {
    events: IndexMap<(PropScope, BString), History>,
    incomplete: bool,
    retained: usize,
    bytes: usize,
}

impl PropLedger {
    /// Maximum number of events retained for each property on one owner.
    pub const PER_PROPERTY_CAP: usize = 16;

    /// Record a successful property write. Call this method in execution order.
    pub fn record(&mut self, scope: &PropScope, name: &[u8], event: PropEvent) {
        let key = PropertyKey(scope, name);
        if !self.events.contains_key(&key) {
            let owner_bytes = match scope {
                PropScope::Source { path, .. } => path.len(),
                PropScope::Test { name, .. } | PropScope::Cache(name) => name.len(),
                PropScope::Install { file } => file.len(),
                _ => 0,
            };
            let key_bytes = HISTORY_BYTES
                .saturating_add(name.len())
                .saturating_add(owner_bytes);
            if self.events.len() >= MAX_EVENTS || self.bytes.saturating_add(key_bytes) > MAX_BYTES {
                self.incomplete = true;
                return;
            }
            self.bytes += key_bytes;
            self.events.insert(
                (scope.clone(), name.into()),
                History {
                    events: Vec::with_capacity(1),
                    costs: Vec::with_capacity(1),
                    dropped: 0,
                },
            );
        }
        let Some(history) = self.events.get_mut(&key) else {
            return;
        };
        let event_bytes = event
            .origin
            .frames()
            .fold(EVENT_BYTES, |bytes, frame| {
                bytes
                    .saturating_add(96)
                    .saturating_add(frame.file.len())
                    .saturating_add(frame.name.as_ref().map_or(0, |name| name.len()))
            })
            .saturating_add(
                event
                    .old_value
                    .as_ref()
                    .map_or(0, |value| value.prefix.len()),
            )
            .saturating_add(
                event
                    .new_value
                    .as_ref()
                    .map_or(0, |value| value.prefix.len()),
            );
        if event_bytes > MAX_BYTES {
            self.incomplete = true;
            history.dropped = history.dropped.saturating_add(1);
            return;
        }
        while history.events.len() >= Self::PER_PROPERTY_CAP
            || self.retained >= MAX_EVENTS
            || self.bytes.saturating_add(event_bytes) > MAX_BYTES
        {
            self.incomplete = true;
            history.dropped = history.dropped.saturating_add(1);
            if history.events.is_empty() {
                history.events.shrink_to_fit();
                history.costs.shrink_to_fit();
                return;
            }
            history.events.remove(0);
            self.bytes -= history.costs.remove(0);
            self.retained -= 1;
        }
        if history.events.capacity() > 2 * (history.events.len() + 1) {
            history.events.shrink_to_fit();
            history.costs.shrink_to_fit();
        }
        self.retained += 1;
        self.bytes += event_bytes;
        history.events.push(event);
        history.costs.push(event_bytes);
    }

    /// Get the retained writes to a property, oldest first.
    #[must_use]
    pub fn history(&self, scope: &PropScope, name: &[u8]) -> &[PropEvent] {
        self.events
            .get(&PropertyKey(scope, name))
            .map_or(&[], |history| history.events.as_slice())
    }

    /// Get the number of discarded writes for this recorded property.
    #[must_use]
    pub fn dropped(&self, scope: &PropScope, name: &[u8]) -> u32 {
        self.events
            .get(&PropertyKey(scope, name))
            .map_or(0, |history| history.dropped)
    }

    /// Iterate over the histories in the order of their first writes.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&PropScope, &BString, &[PropEvent])> {
        self.events
            .iter()
            .map(|((scope, name), history)| (scope, name, history.events.as_slice()))
    }

    /// Return whether a history reached its limit and discarded an event.
    #[must_use]
    pub fn is_incomplete(&self) -> bool {
        self.incomplete
    }
}

#[derive(Hash)]
struct PropertyKey<'a>(&'a PropScope, &'a [u8]);

impl Equivalent<(PropScope, BString)> for PropertyKey<'_> {
    fn equivalent(&self, (scope, name): &(PropScope, BString)) -> bool {
        self.0 == scope && self.1 == name.as_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DirId;

    #[test]
    fn keeps_enable_order_and_uses_the_last_extension_mapping() {
        let mut registry = LanguageRegistry::default();
        registry.register_language(b"NONE");
        registry.register_language(b"CXX");
        registry.set_extensions(b"CXX", ["C", "cc", "shared"], ["h", "shared"]);
        registry.register_language(b"C");
        registry.set_extensions(b"C", ["c", "shared"], ["h", "obj"]);
        registry.register_language(b"CXX");

        assert_eq!(registry.enabled, [BString::from("CXX"), BString::from("C")]);
        assert_eq!(
            registry.language_for_extension(b".shared"),
            Some(&BString::from("C"))
        );
        assert_eq!(
            registry.language_for_extension(b"C"),
            Some(&BString::from("CXX"))
        );
        assert_eq!(
            registry.language_for_extension(b"c"),
            Some(&BString::from("C"))
        );
        assert!(registry.language_for_extension(b"CC").is_none());
        assert!(!registry.is_ignored(b"shared"));
        assert!(registry.is_ignored(b".h"));
        assert!(registry.is_ignored(b"obj"));
        assert!(!registry.is_ignored(b"unknown"));
    }

    #[test]
    fn accepts_a_language_with_no_source_extensions() {
        let mut registry = LanguageRegistry::default();
        registry.register_language(b"ASM");
        registry.set_extensions(b"ASM", std::iter::empty::<&str>(), ["", "obj"]);
        assert_eq!(registry.enabled, [BString::from("ASM")]);
        assert!(registry.by_extension.is_empty());
        assert!(!registry.is_ignored(b""));
    }

    fn event(seq: u64, op: PropOp) -> PropEvent {
        PropEvent::new(seq, op, Some(b"old"), Some(b"new"), &Backtrace::default())
    }

    #[test]
    fn caps_each_history_and_keeps_owners_separate() {
        let mut ledger = PropLedger::default();
        let global = PropScope::Global;
        let dir = PropScope::Directory(DirId::ROOT);
        let name = b"\xffFLAGS";
        ledger.record(&global, name, event(0, PropOp::Set));
        ledger.record(&dir, name, event(1, PropOp::Set));
        let cap = PropLedger::PER_PROPERTY_CAP as u64;
        for seq in 2..=cap {
            ledger.record(&global, name, event(seq, PropOp::Append(AppendMode::List)));
        }
        assert!(!ledger.is_incomplete());
        ledger.record(&global, name, event(cap + 1, PropOp::Unset));

        let history = ledger.history(&global, name);
        assert_eq!(history.len(), PropLedger::PER_PROPERTY_CAP);
        assert_eq!(ledger.dropped(&global, name), 1);
        assert_eq!(history.first().map(|event| event.seq), Some(2));
        assert_eq!(history.last().map(|event| event.op), Some(PropOp::Unset));
        assert_eq!(
            ledger.history(&dir, name).first().map(|event| event.seq),
            Some(1)
        );
        assert!(ledger.history(&global, b"OTHER").is_empty());
        assert_eq!(
            ledger.iter().map(|(scope, _, _)| scope).collect::<Vec<_>>(),
            [&global, &dir]
        );
        assert!(ledger.is_incomplete());
    }
}
