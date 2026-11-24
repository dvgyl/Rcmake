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

/// A recorded property write.
#[derive(Clone, Debug)]
pub struct PropEvent {
    /// Execution sequence number supplied by the evaluator.
    pub seq: u64,
    /// Operation that changed the property.
    pub op: PropOp,
    /// Call stack at the write.
    pub origin: Backtrace,
}

/// Recent property writes in one evaluation session.
#[derive(Clone, Debug, Default)]
pub struct PropLedger {
    /// Histories in the order in which their first events arrived.
    pub(crate) events: IndexMap<(PropScope, BString), Vec<PropEvent>>,
    /// Whether the ledger discarded an event when it reached a limit.
    pub(crate) incomplete: bool,
}

impl PropLedger {
    /// Maximum number of events retained for each property on one owner.
    pub const PER_PROPERTY_CAP: usize = 16;

    /// Record a successful property write. Call this method in execution order.
    pub fn record(&mut self, scope: &PropScope, name: &[u8], event: PropEvent) {
        let history = self.events.entry((scope.clone(), name.into())).or_default();
        if history.len() == Self::PER_PROPERTY_CAP {
            history.remove(0);
            self.incomplete = true;
        }
        history.push(event);
    }

    /// Get the retained writes to a property, oldest first.
    #[must_use]
    pub fn history(&self, scope: &PropScope, name: &[u8]) -> &[PropEvent] {
        self.events
            .get(&PropertyKey(scope, name))
            .map_or(&[], Vec::as_slice)
    }

    /// Iterate over the histories in the order of their first writes.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&PropScope, &BString, &[PropEvent])> {
        self.events
            .iter()
            .map(|((scope, name), events)| (scope, name, events.as_slice()))
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
        PropEvent {
            seq,
            op,
            origin: Backtrace::default(),
        }
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
