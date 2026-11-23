//! Typed arena ids and the backtrace-tagged value wrapper [`BT`].

use rcmake_diag::Backtrace;

macro_rules! arena_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            /// Position of the item in its arena.
            #[must_use]
            pub const fn index(self) -> usize {
                self.0 as usize
            }

            /// Id of the item at `index`. Returns `None` when `index` does not fit in the
            /// id type.
            pub(crate) fn from_index(index: usize) -> Option<Self> {
                u32::try_from(index).ok().map(Self)
            }
        }
    };
}

arena_id!(
    /// Id of a [`crate::Directory`] in [`crate::ProjectModel`].
    DirId
);
arena_id!(
    /// Id of a [`crate::Target`] in [`crate::ProjectModel`].
    TargetId
);
arena_id!(
    /// Id of a [`crate::CustomCommand`] in [`crate::ProjectModel`].
    CommandId
);
arena_id!(
    /// Id of a [`crate::TestEntry`] in [`crate::ProjectModel`].
    TestId
);
arena_id!(
    /// Id of a [`crate::FileSet`] in [`crate::ProjectModel`].
    FileSetId
);

impl DirId {
    /// The top-level directory. Every [`crate::ProjectModel`] has it.
    pub const ROOT: Self = Self(0);
}

/// A value with the [`Backtrace`] of the command that produced it (`BT<T>` in CMake).
///
/// File-API replies and diagnostics read the backtrace, so usage requirements keep it.
#[derive(Clone, Debug, Default)]
pub struct BT<T> {
    /// The value.
    pub value: T,
    /// Where the value came from.
    pub backtrace: Backtrace,
}

impl<T> BT<T> {
    /// Tag `value` with `backtrace`.
    #[must_use]
    pub const fn new(value: T, backtrace: Backtrace) -> Self {
        Self { value, backtrace }
    }
}

impl<T: PartialEq> PartialEq for BT<T> {
    /// Compare values only. Backtraces are provenance, not identity.
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
