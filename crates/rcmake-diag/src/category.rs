//! CMake 4.4+/4.5 unified diagnostic categories (`cmake-diagnostics(7)`) and state stack.

/// Diagnostic categories defined in CMake 4.5.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(usize)]
pub enum DiagCategory {
    /// `CMD_AUTHOR` (default `WARN`).
    Author = 0,
    /// `CMD_DEPRECATED` (parent `CMD_AUTHOR`, default `WARN`).
    Deprecated = 1,
    /// `CMD_EXPERIMENTAL` (parent `CMD_AUTHOR`, default `WARN`).
    Experimental = 2,
    /// `CMD_POLICY` (parent `CMD_AUTHOR`, default `WARN`).
    Policy = 3,
    /// `CMD_STRICT` (parent `CMD_AUTHOR`, default `IGNORE`).
    Strict = 4,
    /// `CMD_NON_TARGET_DIRECTIVE` (parent `CMD_STRICT`, default `IGNORE`).
    NonTargetDirective = 5,
    /// `CMD_INSTALL_ABSOLUTE_DESTINATION` (parent `CMD_STRICT`, default `IGNORE`).
    InstallAbsoluteDestination = 6,
    /// `CMD_UNINITIALIZED` (no parent, default `IGNORE`).
    Uninitialized = 7,
    /// `CMD_UNUSED_CLI` (no parent, default `IGNORE`).
    UnusedCli = 8,
}

impl DiagCategory {
    /// All categories in canonical order.
    pub const ALL: [Self; 9] = [
        Self::Author,
        Self::Deprecated,
        Self::Experimental,
        Self::Policy,
        Self::Strict,
        Self::NonTargetDirective,
        Self::InstallAbsoluteDestination,
        Self::Uninitialized,
        Self::UnusedCli,
    ];

    /// Parse a CMake `CMD_*` category name.
    #[must_use]
    pub fn parse(name: &[u8]) -> Option<Self> {
        match name {
            b"CMD_AUTHOR" => Some(Self::Author),
            b"CMD_DEPRECATED" => Some(Self::Deprecated),
            b"CMD_EXPERIMENTAL" => Some(Self::Experimental),
            b"CMD_POLICY" => Some(Self::Policy),
            b"CMD_STRICT" => Some(Self::Strict),
            b"CMD_NON_TARGET_DIRECTIVE" => Some(Self::NonTargetDirective),
            b"CMD_INSTALL_ABSOLUTE_DESTINATION" => Some(Self::InstallAbsoluteDestination),
            b"CMD_UNINITIALIZED" => Some(Self::Uninitialized),
            b"CMD_UNUSED_CLI" => Some(Self::UnusedCli),
            _ => None,
        }
    }

    /// Official `CMD_*` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Author => "CMD_AUTHOR",
            Self::Deprecated => "CMD_DEPRECATED",
            Self::Experimental => "CMD_EXPERIMENTAL",
            Self::Policy => "CMD_POLICY",
            Self::Strict => "CMD_STRICT",
            Self::NonTargetDirective => "CMD_NON_TARGET_DIRECTIVE",
            Self::InstallAbsoluteDestination => "CMD_INSTALL_ABSOLUTE_DESTINATION",
            Self::Uninitialized => "CMD_UNINITIALIZED",
            Self::UnusedCli => "CMD_UNUSED_CLI",
        }
    }

    /// SARIF `ruleId` string for this category.
    #[must_use]
    pub const fn sarif_rule_id(self) -> &'static str {
        match self {
            Self::Author => "CMake.Author",
            Self::Deprecated => "CMake.Deprecated",
            Self::Experimental => "CMake.Experimental",
            Self::Policy => "CMake.Policy",
            Self::Strict => "CMake.Strict",
            Self::NonTargetDirective => "CMake.NonTargetDirective",
            Self::InstallAbsoluteDestination => "CMake.InstallAbsoluteDestination",
            Self::Uninitialized => "CMake.Uninitialized",
            Self::UnusedCli => "CMake.UnusedCli",
        }
    }

    /// Parent category in CMake 4.5.1's diagnostic hierarchy, if any.
    #[must_use]
    pub const fn parent(self) -> Option<Self> {
        match self {
            Self::Deprecated | Self::Experimental | Self::Policy | Self::Strict => {
                Some(Self::Author)
            }
            Self::NonTargetDirective | Self::InstallAbsoluteDestination => Some(Self::Strict),
            Self::Author | Self::Uninitialized | Self::UnusedCli => None,
        }
    }

    /// Built-in default action for this category.
    #[must_use]
    pub const fn default_action(self) -> DiagAction {
        match self {
            Self::Author | Self::Deprecated | Self::Experimental | Self::Policy => DiagAction::Warn,
            Self::Strict
            | Self::NonTargetDirective
            | Self::InstallAbsoluteDestination
            | Self::Uninitialized
            | Self::UnusedCli => DiagAction::Ignore,
        }
    }

    /// Returns `true` if `self` is equal to `ancestor` or descends from it.
    #[must_use]
    pub fn is_within(self, ancestor: Self) -> bool {
        let mut cur = Some(self);
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = c.parent();
        }
        false
    }
}

/// Severity action configured for a [`DiagCategory`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagAction {
    /// Suppress diagnostic.
    Ignore = 0,
    /// Emit warning.
    Warn = 1,
    /// Emit error and continue processing (`SEND_ERROR`).
    SendError = 2,
    /// Emit error and stop immediately (`FATAL_ERROR`).
    FatalError = 3,
}

impl DiagAction {
    /// Parse a `cmake_diagnostic()` action keyword.
    #[must_use]
    pub fn parse(name: &[u8]) -> Option<Self> {
        match name {
            b"IGNORE" => Some(Self::Ignore),
            b"WARN" => Some(Self::Warn),
            b"SEND_ERROR" => Some(Self::SendError),
            b"FATAL_ERROR" => Some(Self::FatalError),
            _ => None,
        }
    }

    /// Official CMake action keyword.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "IGNORE",
            Self::Warn => "WARN",
            Self::SendError => "SEND_ERROR",
            Self::FatalError => "FATAL_ERROR",
        }
    }
}

/// Active diagnostic action table for one scope frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagState {
    actions: [DiagAction; 9],
}

impl Default for DiagState {
    fn default() -> Self {
        let mut actions = [DiagAction::Ignore; 9];
        for cat in DiagCategory::ALL {
            if let Some(slot) = actions.get_mut(cat as usize) {
                *slot = cat.default_action();
            }
        }
        Self { actions }
    }
}

impl DiagState {
    /// Get the active action for `cat`.
    #[must_use]
    pub fn get(&self, cat: DiagCategory) -> DiagAction {
        self.actions
            .get(cat as usize)
            .copied()
            .unwrap_or(DiagAction::Ignore)
    }

    /// Set `cat` (and optionally its descendants) to `action`.
    pub fn set(&mut self, cat: DiagCategory, action: DiagAction, recurse: bool) {
        if recurse {
            for c in DiagCategory::ALL {
                if c.is_within(cat)
                    && let Some(slot) = self.actions.get_mut(c as usize)
                {
                    *slot = action;
                }
            }
        } else if let Some(slot) = self.actions.get_mut(cat as usize) {
            *slot = action;
        }
    }

    /// Promote `cat` (and optionally its descendants) to at least `action`.
    pub fn promote(&mut self, cat: DiagCategory, action: DiagAction, recurse: bool) {
        for c in DiagCategory::ALL {
            if (c == cat || (recurse && c.is_within(cat)))
                && let Some(slot) = self.actions.get_mut(c as usize)
                && *slot < action
            {
                *slot = action;
            }
        }
    }

    /// Demote `cat` (and optionally its descendants) to at most `action`.
    pub fn demote(&mut self, cat: DiagCategory, action: DiagAction, recurse: bool) {
        for c in DiagCategory::ALL {
            if (c == cat || (recurse && c.is_within(cat)))
                && let Some(slot) = self.actions.get_mut(c as usize)
                && *slot > action
            {
                *slot = action;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promote_and_demote_recurse_matches_cmake() {
        let mut s = DiagState::default();
        assert_eq!(s.get(DiagCategory::Strict), DiagAction::Ignore);
        assert_eq!(s.get(DiagCategory::NonTargetDirective), DiagAction::Ignore);

        s.promote(DiagCategory::Strict, DiagAction::Warn, true);
        assert_eq!(s.get(DiagCategory::Strict), DiagAction::Warn);
        assert_eq!(s.get(DiagCategory::NonTargetDirective), DiagAction::Warn);
        assert_eq!(
            s.get(DiagCategory::InstallAbsoluteDestination),
            DiagAction::Warn
        );

        s.demote(DiagCategory::Strict, DiagAction::Ignore, false);
        assert_eq!(s.get(DiagCategory::Strict), DiagAction::Ignore);
        assert_eq!(s.get(DiagCategory::NonTargetDirective), DiagAction::Warn);
    }
}
