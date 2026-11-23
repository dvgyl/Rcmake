//! Common Package Specification (CPS) support for Rcmake (ADR-013).
//!
//! CPS is a JSON format that describes an installed package: its components (libraries,
//! executables, interface targets), their usage requirements, and their per-configuration
//! files. Rcmake writes CPS **0.15.0** for `install(PACKAGE_INFO)` and
//! `export(PACKAGE_INFO)`, and `find_package()` reads CPS `>= 0.13 && < 0.16`.
//!
//! This crate holds the data model. The [`writer`], [`reader`], and [`naming`] modules
//! hold the file operations.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use rcmake_base::BString;

pub mod naming;
pub mod reader;
pub mod writer;

#[cfg(test)]
mod tests;

/// CPS schema version that Rcmake writes (`cps_version`).
pub const CPS_VERSION: &str = "0.15.0";

/// Error of a CPS operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CpsError {
    /// The operation is not implemented yet.
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
    /// The CPS document is not valid. The text is the CMake-style message.
    #[error("{0}")]
    Invalid(BString),
}

/// Value of the `type` key of a CPS component.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CpsComponentType {
    /// `archive`: a static library.
    Archive,
    /// `dylib`: a shared library.
    Dylib,
    /// `module`: a loadable module.
    Module,
    /// `executable`: a program.
    Executable,
    /// `interface`: usage requirements without a file.
    Interface,
    /// `symbolic`: a name without usage requirements.
    Symbolic,
    /// `jar`: a Java archive.
    Jar,
    /// A type that this CPS version does not define. The reader keeps the spelling.
    Unknown(BString),
}

impl CpsComponentType {
    /// The `type` keyword.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Archive => b"archive",
            Self::Dylib => b"dylib",
            Self::Module => b"module",
            Self::Executable => b"executable",
            Self::Interface => b"interface",
            Self::Symbolic => b"symbolic",
            Self::Jar => b"jar",
            Self::Unknown(name) => name,
        }
    }

    /// The type with keyword `name`.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Self {
        match name {
            b"archive" => Self::Archive,
            b"dylib" => Self::Dylib,
            b"module" => Self::Module,
            b"executable" => Self::Executable,
            b"interface" => Self::Interface,
            b"symbolic" => Self::Symbolic,
            b"jar" => Self::Jar,
            _ => Self::Unknown(name.into()),
        }
    }
}

/// Usage requirements that a component or a configuration of a component can set.
///
/// Maps are keyed by language (`c`, `cpp`, `fortran`, or `*` for all languages).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpsUsage {
    /// `location`: the main file (with `@prefix@`).
    pub location: Option<BString>,
    /// `link_location`: the file to link against, when it is not `location`.
    pub link_location: Option<BString>,
    /// `includes`: include directories by language.
    pub includes: BTreeMap<BString, Vec<BString>>,
    /// `definitions`: preprocessor definitions by language (`None` value means
    /// define without a value).
    pub definitions: BTreeMap<BString, BTreeMap<BString, Option<BString>>>,
    /// `compile_features`.
    pub compile_features: Vec<BString>,
    /// `compile_flags` by language.
    pub compile_flags: BTreeMap<BString, Vec<BString>>,
    /// `link_features`.
    pub link_features: Vec<BString>,
    /// `link_flags`.
    pub link_flags: Vec<BString>,
    /// `link_languages`.
    pub link_languages: Vec<BString>,
    /// `link_libraries`: plain library names or paths.
    pub link_libraries: Vec<BString>,
    /// `link_requires`: components needed only to link (`:comp` or `pkg:comp`).
    pub link_requires: Vec<BString>,
    /// `requires`: components whose usage requirements propagate.
    pub requires: Vec<BString>,
}

/// One component of a CPS package (`components.<name>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpsComponent {
    /// `type`.
    pub ty: CpsComponentType,
    /// Requirements for all configurations.
    pub usage: CpsUsage,
    /// `license`: SPDX license expression of this component.
    pub license: Option<BString>,
    /// `configurations`: requirements of one configuration, by configuration name.
    pub configurations: BTreeMap<BString, CpsUsage>,
}

/// One entry of the `requires` key of a package: a dependency on another package.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpsRequirement {
    /// `components`: required components of the dependency.
    pub components: Vec<BString>,
    /// `version`: minimum version of the dependency.
    pub version: Option<BString>,
    /// `hints`: directories to search for the dependency.
    pub hints: Vec<BString>,
}

/// A CPS package file (`<Pkg>.cps` or appendix `<Pkg>-<appendix>.cps`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpsPackage {
    /// `name`.
    pub name: BString,
    /// `cps_version`.
    pub cps_version: BString,
    /// `version`.
    pub version: Option<BString>,
    /// `compat_version`.
    pub compat_version: Option<BString>,
    /// `version_schema`.
    pub version_schema: Option<BString>,
    /// `description`.
    pub description: Option<BString>,
    /// `website`.
    pub website: Option<BString>,
    /// `license`: SPDX license expression of the package.
    pub license: Option<BString>,
    /// `default_license`: license of components without `license`.
    pub default_license: Option<BString>,
    /// `cps_path`: prefix of the package (`@prefix@/...`).
    pub cps_path: Option<BString>,
    /// `configurations`: configuration names in preference order.
    pub configurations: Vec<BString>,
    /// `default_components`.
    pub default_components: Vec<BString>,
    /// `requires`: dependencies on other packages, by package name.
    pub requires: BTreeMap<BString, CpsRequirement>,
    /// `components`, by component name.
    pub components: BTreeMap<BString, CpsComponent>,
}

/// A configuration supplement file (`<Pkg>[-<appendix>]@<config>.cps`).
///
/// It adds the files of one configuration to components of the root or appendix file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpsConfigSupplement {
    /// `name`: the package name.
    pub name: BString,
    /// `cps_version`.
    pub cps_version: BString,
    /// `configuration`: lowercase configuration name (`noconfig` when empty).
    pub configuration: BString,
    /// `components`: requirements of this configuration, by component name.
    pub components: BTreeMap<BString, CpsUsage>,
}

/// All CPS files that one `install(PACKAGE_INFO)` or `export(PACKAGE_INFO)` writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpsFileSet {
    /// File name of the root or appendix file.
    pub file_name: BString,
    /// Content of the root or appendix file.
    pub package: CpsPackage,
    /// Configuration supplements with their file names.
    pub supplements: Vec<(BString, CpsConfigSupplement)>,
}
