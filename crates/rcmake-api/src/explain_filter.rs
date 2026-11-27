//! Typed entry filters for `rcmake explain <target> --flag|--define|--include|--dep`.

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_model::UsageField;
use rcmake_resolve::ChainKind;

use crate::ExplainEntry;

/// Typed selection of target entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExplainFilter {
    /// Exact value, compiler spelling, or flag token.
    Any(BString),
    /// Compile or link flag token from options or `CMAKE_*_FLAGS*` inputs.
    Flag(BString),
    /// Preprocessor definition `NAME`, or `NAME=VALUE` when `value` is set.
    Define {
        /// Definition name, compared case-sensitively.
        name: BString,
        /// Required value, when given.
        value: Option<BString>,
    },
    /// Include directory, compared after lexical normalization.
    Include(BString),
    /// Target that the explained target reaches through link or order-only edges.
    Dependency(BString),
}

impl ExplainFilter {
    /// Parse `NAME` or `NAME=VALUE` for [`ExplainFilter::Define`].
    #[must_use]
    pub fn define(text: &[u8]) -> Self {
        match text.iter().position(|&byte| byte == b'=') {
            Some(split) => Self::Define {
                name: text.get(..split).unwrap_or_default().into(),
                value: Some(text.get(split + 1..).unwrap_or_default().into()),
            },
            None => Self::Define {
                name: text.into(),
                value: None,
            },
        }
    }

    /// Stable lowercase name used in reports.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Any(_) => "any",
            Self::Flag(_) => "flag",
            Self::Define { .. } => "define",
            Self::Include(_) => "include",
            Self::Dependency(_) => "dep",
        }
    }

    /// Filter text as the user gave it.
    #[must_use]
    pub fn text(&self) -> BString {
        match self {
            Self::Any(value)
            | Self::Flag(value)
            | Self::Include(value)
            | Self::Dependency(value) => value.clone(),
            Self::Define { name, value: None } => name.clone(),
            Self::Define {
                name,
                value: Some(value),
            } => {
                let mut text = name.clone();
                text.push(b'=');
                text.extend_from_slice(value);
                text
            }
        }
    }

    pub(crate) fn matches(&self, entry: &ExplainEntry) -> bool {
        match self {
            Self::Any(filter) => entry.matches_filter(filter),
            Self::Flag(filter) => {
                entry.is_flag_string()
                    && (entry.value == *filter
                        || entry.spelling.as_ref() == Some(filter)
                        || entry.tokens().iter().any(|token| token == filter))
            }
            Self::Define { name, value } => {
                let wanted = |candidate: &[u8]| {
                    define_matches(
                        candidate,
                        name,
                        value.as_ref().map(|value| value.as_slice()),
                    )
                };
                if entry.kind == ChainKind::Usage(UsageField::CompileDefinitions) {
                    return wanted(&entry.value);
                }
                entry.is_flag_string()
                    && entry.tokens().iter().any(|token| {
                        token
                            .strip_prefix(b"-D")
                            .or_else(|| token.strip_prefix(b"/D"))
                            .is_some_and(wanted)
                    })
            }
            Self::Include(dir) => {
                let wanted = Lexical::new(dir).normalize();
                let same = |candidate: &[u8]| Lexical::new(candidate).normalize() == wanted;
                if entry.kind == ChainKind::Usage(UsageField::IncludeDirectories) {
                    return same(&entry.value);
                }
                if !entry.is_flag_string() {
                    return false;
                }
                let tokens = entry.tokens();
                tokens.iter().enumerate().any(|(index, token)| {
                    let attached = token
                        .strip_prefix(b"-I")
                        .or_else(|| token.strip_prefix(b"/I"))
                        .or_else(|| token.strip_prefix(b"-isystem"))
                        .filter(|rest| !rest.is_empty());
                    match attached {
                        Some(rest) => same(rest),
                        None if matches!(token.as_slice(), b"-I" | b"/I" | b"-isystem") => {
                            tokens.get(index + 1).is_some_and(|next| same(next))
                        }
                        None => false,
                    }
                })
            }
            Self::Dependency(_) => false,
        }
    }
}

fn define_matches(candidate: &[u8], name: &[u8], value: Option<&[u8]>) -> bool {
    let (candidate_name, candidate_value) = match candidate.iter().position(|&byte| byte == b'=') {
        Some(split) => (
            candidate.get(..split).unwrap_or_default(),
            Some(candidate.get(split + 1..).unwrap_or_default()),
        ),
        None => (candidate, None),
    };
    candidate_name == name && value.is_none_or(|value| candidate_value == Some(value))
}

#[cfg(test)]
mod tests {
    use rcmake_model::BT;

    use super::*;

    fn entry(field: UsageField, value: &str) -> ExplainEntry {
        ExplainEntry::new(
            ChainKind::Usage(field),
            &BT {
                value: value.into(),
                backtrace: rcmake_diag::Backtrace::default(),
            },
        )
    }

    #[test]
    fn define_matches_names_values_and_flag_spellings() {
        let any = ExplainFilter::define(b"FOO");
        let one = ExplainFilter::define(b"FOO=1");
        for value in ["FOO", "FOO=1"] {
            assert!(any.matches(&entry(UsageField::CompileDefinitions, value)));
        }
        assert!(!any.matches(&entry(UsageField::CompileDefinitions, "FOOBAR")));
        assert!(one.matches(&entry(UsageField::CompileDefinitions, "FOO=1")));
        assert!(!one.matches(&entry(UsageField::CompileDefinitions, "FOO")));
        for flags in ["-DFOO=1 -O2", "/DFOO", "-DFOO"] {
            assert!(
                any.matches(&entry(UsageField::CompileOptions, flags)),
                "{flags}"
            );
        }
        assert!(!any.matches(&entry(UsageField::CompileOptions, "-DFOOBAR")));
    }

    #[test]
    fn include_matches_after_normalization_and_in_flags() {
        let filter = ExplainFilter::Include("/src/inc".into());
        assert!(filter.matches(&entry(UsageField::IncludeDirectories, "/src/inc/../inc")));
        assert!(filter.matches(&entry(UsageField::CompileOptions, "-isystem /src/inc")));
        assert!(filter.matches(&entry(UsageField::CompileOptions, "-I/src/./inc")));
        assert!(!filter.matches(&entry(UsageField::IncludeDirectories, "/src/include")));
    }

    #[test]
    fn flag_matches_only_flag_strings() {
        let filter = ExplainFilter::Flag("--sysroot=/x".into());
        assert!(filter.matches(&entry(UsageField::CompileOptions, "-O2 --sysroot=/x")));
        assert!(!filter.matches(&entry(UsageField::CompileDefinitions, "--sysroot=/x")));
    }
}
