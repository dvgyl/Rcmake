//! Build-system generator selection (`-G`, `CMAKE_GENERATOR`).

/// A build-system generator that Rcmake implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GeneratorKind {
    /// `Ninja`.
    Ninja,
    /// `Ninja Multi-Config`.
    NinjaMultiConfig,
    /// `Unix Makefiles`.
    UnixMakefiles,
}

impl GeneratorKind {
    /// All generators, in `cmake --help` order.
    pub const ALL: [Self; 3] = [Self::Ninja, Self::NinjaMultiConfig, Self::UnixMakefiles];

    /// Generator name as given to `-G` and stored in `CMAKE_GENERATOR`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ninja => "Ninja",
            Self::NinjaMultiConfig => "Ninja Multi-Config",
            Self::UnixMakefiles => "Unix Makefiles",
        }
    }

    /// `true` for a multi-configuration generator (`GENERATOR_IS_MULTI_CONFIG`).
    #[must_use]
    pub const fn multi_config(self) -> bool {
        matches!(self, Self::NinjaMultiConfig)
    }

    /// The `Modules/` file that finds the build tool (`CMAKE_MAKE_PROGRAM`).
    #[must_use]
    pub const fn find_make_module(self) -> &'static str {
        match self {
            Self::Ninja | Self::NinjaMultiConfig => "CMakeNinjaFindMake",
            Self::UnixMakefiles => "CMakeUnixFindMake",
        }
    }

    /// Generator with the exact name `name` (case-sensitive, as in CMake).
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.name().as_bytes() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::GeneratorKind;

    #[test]
    fn names_round_trip() {
        for kind in GeneratorKind::ALL {
            assert_eq!(GeneratorKind::from_name(kind.name().as_bytes()), Some(kind));
        }
        assert_eq!(GeneratorKind::from_name(b"ninja"), None);
        assert!(GeneratorKind::NinjaMultiConfig.multi_config());
        assert_eq!(
            GeneratorKind::UnixMakefiles.find_make_module(),
            "CMakeUnixFindMake"
        );
    }
}
