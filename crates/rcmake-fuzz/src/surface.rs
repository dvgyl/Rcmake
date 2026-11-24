//! The engine surfaces that the harness fuzzes (ADR-016 section 5.2).

/// One fuzzed engine surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    /// `rcmake-syntax`: parse a listfile.
    Syntax,
    /// `rcmake-expr`: `${}` expansion and `if()` conditions.
    Expr,
    /// `rcmake-regex`: compile and search.
    Regex,
    /// `rcmake-genex`: parse and evaluate generator expressions.
    Genex,
    /// `rcmake-json`: read and write.
    Json,
    /// `rcmake-presets`: load a preset file graph from an in-memory host.
    Presets,
    /// `rcmake-cps`: read a `.cps` document.
    Cps,
    /// `rcmake-archive`: list and extract to an in-memory host.
    Archive,
    /// `rcmake-base`: lexical `cmake_path` and list operations.
    Path,
}

impl Surface {
    /// All surfaces, in ADR-016 order.
    pub const ALL: [Self; 9] = [
        Self::Syntax,
        Self::Expr,
        Self::Regex,
        Self::Genex,
        Self::Json,
        Self::Presets,
        Self::Cps,
        Self::Archive,
        Self::Path,
    ];

    /// Command-line name of the surface.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Expr => "expr",
            Self::Regex => "regex",
            Self::Genex => "genex",
            Self::Json => "json",
            Self::Presets => "presets",
            Self::Cps => "cps",
            Self::Archive => "archive",
            Self::Path => "path",
        }
    }

    /// Surface with the command-line name `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|surface| surface.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::Surface;

    #[test]
    fn names_round_trip() {
        for surface in Surface::ALL {
            assert_eq!(Surface::from_name(surface.name()), Some(surface));
        }
        assert_eq!(Surface::from_name("SYNTAX"), None);
    }
}
