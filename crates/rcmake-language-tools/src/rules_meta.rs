//! Stable lint rule identifiers, severities, and rule metadata.

#![forbid(unsafe_code)]

use core::fmt;

use rcmake_diag::sarif::{SarifLevel, SarifRule};

/// Diagnostic severity level for lint findings and `.rcmake.toml` overrides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Informational or advisory note.
    Note,
    /// Actionable warning.
    Warning,
    /// Error that causes `rcmake lint` to exit with code 1.
    Error,
}

impl Severity {
    /// Parse a severity name from `.rcmake.toml` (`"note"`, `"warning"`, or `"error"`).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "note" => Some(Self::Note),
            "warning" => Some(Self::Warning),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// Wire and display name (`"note"`, `"warning"`, `"error"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// Convert to a SARIF 2.1.0 severity level.
    #[must_use]
    pub const fn to_sarif(self) -> SarifLevel {
        match self {
            Self::Note => SarifLevel::Note,
            Self::Warning => SarifLevel::Warning,
            Self::Error => SarifLevel::Error,
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable public identifier for one of the 8 built-in static lint rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuleId {
    /// Reference to a proven-absent variable or probable typo of a nearby binding.
    UndefinedVariable,
    /// `file(GLOB)` / `file(GLOB_RECURSE)` without `CONFIGURE_DEPENDS` flowing to a target.
    GlobWithoutConfigureDepends,
    /// Condition comparing `CMAKE_BUILD_TYPE` without a single-config generator guard.
    SingleConfigCondition,
    /// Unconditional assignment or `FORCE` cache overwrite of `CMAKE_BUILD_TYPE`.
    UnguardedBuildType,
    /// Directory-wide legacy commands (`include_directories`, `link_libraries`, etc.).
    DirectoryOptions,
    /// Obsolete or removed CMake commands and outdated `cmake_minimum_required` versions.
    ObsoleteCommand,
    /// Setting a CMake policy to `OLD` via `cmake_policy(SET CMPnnnn OLD)`.
    OldPolicy,
    /// Unquoted expanding operand inside `if()` / `elseif()` where empty or list values break arity.
    FragileIfExpansion,
}

/// Static metadata describing a lint rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleInfo {
    /// Rule identifier string.
    pub id: &'static str,
    /// Short human-readable summary for CLI and SARIF metadata.
    pub short_description: &'static str,
    /// Default reporting severity when not overridden in `.rcmake.toml`.
    pub default_severity: Severity,
}

/// Rule identifier emitted for malformed or unknown `# rcmake-lint:` comment directives.
pub const INVALID_DIRECTIVE_RULE: RuleInfo = RuleInfo {
    id: "invalid-lint-directive",
    short_description: "Unknown or malformed rcmake-lint directive.",
    default_severity: Severity::Warning,
};

/// Ordered catalog of all 8 configurable lint rules.
pub const ALL_RULES: [RuleId; 8] = [
    RuleId::UndefinedVariable,
    RuleId::GlobWithoutConfigureDepends,
    RuleId::SingleConfigCondition,
    RuleId::UnguardedBuildType,
    RuleId::DirectoryOptions,
    RuleId::ObsoleteCommand,
    RuleId::OldPolicy,
    RuleId::FragileIfExpansion,
];

impl RuleId {
    /// Parse a stable public rule ID string.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "undefined-variable" => Some(Self::UndefinedVariable),
            "glob-without-configure-depends" => Some(Self::GlobWithoutConfigureDepends),
            "single-config-condition" => Some(Self::SingleConfigCondition),
            "unguarded-build-type" => Some(Self::UnguardedBuildType),
            "directory-options" => Some(Self::DirectoryOptions),
            "obsolete-command" => Some(Self::ObsoleteCommand),
            "old-policy" => Some(Self::OldPolicy),
            "fragile-if-expansion" => Some(Self::FragileIfExpansion),
            _ => None,
        }
    }

    /// Stable kebab-case rule ID string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.info().id
    }

    /// Default severity level for this rule.
    #[must_use]
    pub const fn default_severity(self) -> Severity {
        self.info().default_severity
    }

    /// Static metadata descriptor for this rule.
    #[must_use]
    pub const fn info(self) -> RuleInfo {
        match self {
            Self::UndefinedVariable => RuleInfo {
                id: "undefined-variable",
                short_description: "Variable reference is unset or likely misspelled.",
                default_severity: Severity::Warning,
            },
            Self::GlobWithoutConfigureDepends => RuleInfo {
                id: "glob-without-configure-depends",
                short_description: "Target source glob omits CONFIGURE_DEPENDS.",
                default_severity: Severity::Warning,
            },
            Self::SingleConfigCondition => RuleInfo {
                id: "single-config-condition",
                short_description: "Condition inspects CMAKE_BUILD_TYPE without a single-config guard.",
                default_severity: Severity::Warning,
            },
            Self::UnguardedBuildType => RuleInfo {
                id: "unguarded-build-type",
                short_description: "Unconditional assignment overwrites or masks CMAKE_BUILD_TYPE.",
                default_severity: Severity::Warning,
            },
            Self::DirectoryOptions => RuleInfo {
                id: "directory-options",
                short_description: "Directory-wide legacy option command affects all targets in scope.",
                default_severity: Severity::Note,
            },
            Self::ObsoleteCommand => RuleInfo {
                id: "obsolete-command",
                short_description: "Obsolete command or unsupported CMake minimum/policy version.",
                default_severity: Severity::Warning,
            },
            Self::OldPolicy => RuleInfo {
                id: "old-policy",
                short_description: "Policy set to OLD behavior.",
                default_severity: Severity::Warning,
            },
            Self::FragileIfExpansion => RuleInfo {
                id: "fragile-if-expansion",
                short_description: "Unquoted variable expansion in if()/elseif() condition.",
                default_severity: Severity::Warning,
            },
        }
    }

    /// Convert to a SARIF 2.1.0 rule descriptor.
    #[must_use]
    pub const fn to_sarif(self) -> SarifRule<'static> {
        let info = self.info();
        SarifRule {
            id: info.id,
            short_description: info.short_description,
            default_level: info.default_severity.to_sarif(),
        }
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
