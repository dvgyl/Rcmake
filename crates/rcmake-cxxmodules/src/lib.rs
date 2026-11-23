//! C++20 named module support for Rcmake (ADR-012).
//!
//! The Ninja generator builds C++ modules in three steps:
//!
//! 1. A scan step runs the compiler's dependency scanner for each source. It writes a
//!    P1689r5 `.ddi` file that lists the modules the source provides and requires
//!    ([`p1689`]).
//! 2. A collate step (`cmake -E cmake_ninja_dyndep`) reads all `.ddi` files of a target
//!    and the target's `CXXDependInfo.json` ([`tdi`]). It resolves each import to a
//!    module file, writes a Ninja `dyndep` file ([`dyndep`]), one module map per source
//!    ([`modmap`]), and `CXXModules.json` for dependent targets ([`collate`]).
//! 3. The compile steps use the dyndep file to build in the correct order.
//!
//! This crate depends only on `rcmake-base` and `rcmake-json`. It does no file I/O: the
//! callers pass file contents and write the results.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use rcmake_base::BString;

pub mod collate;
pub mod dyndep;
pub mod modmap;
pub mod p1689;
pub mod tdi;

#[cfg(test)]
mod tests;

/// Error of a module operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModulesError {
    /// The operation is not implemented yet.
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
    /// An input is not valid, or the imports can not be resolved. The text is the
    /// CMake-style message.
    #[error("{0}")]
    Invalid(BString),
}

/// How a source names a required module (P1689r5 `lookup-method`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LookupMethod {
    /// `by-name`: `import name;`.
    #[default]
    ByName,
    /// `include-angle`: `import <header>;`.
    IncludeAngle,
    /// `include-quote`: `import "header";`.
    IncludeQuote,
}

impl LookupMethod {
    /// The `lookup-method` keyword.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ByName => "by-name",
            Self::IncludeAngle => "include-angle",
            Self::IncludeQuote => "include-quote",
        }
    }

    /// The method with keyword `name`.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"by-name" => Some(Self::ByName),
            b"include-angle" => Some(Self::IncludeAngle),
            b"include-quote" => Some(Self::IncludeQuote),
            _ => None,
        }
    }
}

/// One module that a source provides (P1689r5 `provides` entry).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleProvide {
    /// `logical-name`.
    pub logical_name: BString,
    /// `source-path`.
    pub source_path: Option<BString>,
    /// `compiled-module-path`.
    pub compiled_module_path: Option<BString>,
    /// `unique-on-source-path`.
    pub unique_on_source_path: bool,
    /// `is-interface` (default `true`).
    pub is_interface: bool,
}

/// One module that a source requires (P1689r5 `requires` entry).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleRequire {
    /// `logical-name`.
    pub logical_name: BString,
    /// `source-path`.
    pub source_path: Option<BString>,
    /// `compiled-module-path`.
    pub compiled_module_path: Option<BString>,
    /// `unique-on-source-path`.
    pub unique_on_source_path: bool,
    /// `lookup-method`.
    pub lookup_method: LookupMethod,
}

/// One rule (one scanned source) of a P1689r5 file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct P1689Rule {
    /// `primary-output`: the object file of the source.
    pub primary_output: Option<BString>,
    /// `outputs`: other outputs.
    pub outputs: Vec<BString>,
    /// `provides`.
    pub provides: Vec<ModuleProvide>,
    /// `requires`.
    pub requires: Vec<ModuleRequire>,
}

/// A P1689r5 `.ddi` file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct P1689File {
    /// `version` (`1`).
    pub version: u64,
    /// `revision` (`0`).
    pub revision: u64,
    /// `rules`.
    pub rules: Vec<P1689Rule>,
}

/// Compiler module map format (`--modmapfmt=` of `cmake_ninja_dyndep`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModmapFormat {
    /// LLVM Clang `@<file>.modmap` response file (`-fmodule-file=<name>=<bmi>`).
    Clang,
    /// GCC `-fmodule-mapper=<file>.modmap` mapper file.
    Gcc,
    /// MSVC `@<file>.modmap` response file (`/reference`, `/ifcOutput`).
    Msvc,
}

impl ModmapFormat {
    /// The `--modmapfmt=` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clang => "clang",
            Self::Gcc => "gcc",
            Self::Msvc => "msvc",
        }
    }

    /// The format with `--modmapfmt=` value `name`.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"clang" => Some(Self::Clang),
            b"gcc" => Some(Self::Gcc),
            b"msvc" => Some(Self::Msvc),
            _ => None,
        }
    }
}

/// Visibility of a module source in `CXXDependInfo.json`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ModuleVisibility {
    /// `PRIVATE` file set.
    #[default]
    Private,
    /// `PUBLIC` file set.
    Public,
    /// `INTERFACE` file set.
    Interface,
}

/// One `CXX_MODULES` source of a target (`cxx-modules.<object>` in
/// `CXXDependInfo.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleSourceInfo {
    /// `source`: absolute source path.
    pub source: BString,
    /// `name`: file set name.
    pub file_set: BString,
    /// `relative-directory`: source directory relative to its file set base.
    pub relative_directory: BString,
    /// `visibility`.
    pub visibility: ModuleVisibility,
    /// `bmi-only`: the source compiles only to a module file.
    pub bmi_only: bool,
    /// `destination`: install destination of the source.
    pub destination: Option<BString>,
}

/// One export of a target (`exports` in `CXXDependInfo.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleExportInfo {
    /// `export-name`: exported target name.
    pub export_name: BString,
    /// `namespace`.
    pub namespace: BString,
    /// `destination`: export file directory.
    pub destination: BString,
    /// `cxx-module-info-dir`: directory of the per-target module properties.
    pub cxx_module_info_dir: BString,
    /// `export-prefix`: build tree export directory.
    pub export_prefix: BString,
    /// `install`: `true` for `install(EXPORT)`, `false` for `export()`.
    pub install: bool,
}

/// BMI install rule of a target (`bmi-installation` in `CXXDependInfo.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BmiInstallInfo {
    /// `destination`.
    pub destination: BString,
    /// `component`.
    pub component: BString,
    /// `permissions`.
    pub permissions: BString,
    /// `exclude-from-all`.
    pub exclude_from_all: bool,
    /// `optional`.
    pub optional: bool,
    /// `message-level`.
    pub message_level: BString,
    /// `script-location`.
    pub script_location: BString,
}

/// Per-target collation input (`CXXDependInfo.json`, the "TDI").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetDependInfo {
    /// `compiler-id`.
    pub compiler_id: BString,
    /// `compiler-frontend-variant`.
    pub compiler_frontend_variant: BString,
    /// `compiler-simulate-id`.
    pub compiler_simulate_id: BString,
    /// `config`.
    pub config: BString,
    /// `language` (`CXX`).
    pub language: BString,
    /// `module-dir`: directory of the target's module files.
    pub module_dir: BString,
    /// `dir-cur-bld`.
    pub dir_cur_bld: BString,
    /// `dir-cur-src`.
    pub dir_cur_src: BString,
    /// `dir-top-bld`.
    pub dir_top_bld: BString,
    /// `dir-top-src`.
    pub dir_top_src: BString,
    /// `include-dirs`.
    pub include_dirs: Vec<BString>,
    /// `cxx-modules`: module sources by object path.
    pub cxx_modules: BTreeMap<BString, ModuleSourceInfo>,
    /// `exports`.
    pub exports: Vec<ModuleExportInfo>,
    /// `bmi-installation`.
    pub bmi_installation: Option<BmiInstallInfo>,
    /// `linked-target-dirs`: directories of linked targets' `CXXModules.json`.
    pub linked_target_dirs: Vec<BString>,
    /// `forward-modules-from-target-dirs`.
    pub forward_modules_from_target_dirs: Vec<BString>,
}

/// One module file of `CXXModules.json` (`modules.<logical-name>`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleBmi {
    /// `bmi`: module file path.
    pub bmi: BString,
    /// `is-private`: the module is not visible to dependent targets.
    pub is_private: bool,
}

/// One resolved import of `CXXModules.json` (`references.<logical-name>`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleReference {
    /// `path`: module file path.
    pub path: BString,
    /// `lookup-method`.
    pub lookup_method: LookupMethod,
}

/// The module table of a target that dependent targets read (`CXXModules.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CxxModulesJson {
    /// `modules`: modules of this target and forwarded targets, by logical name.
    pub modules: BTreeMap<BString, ModuleBmi>,
    /// `references`: resolved imports, by logical name.
    pub references: BTreeMap<BString, ModuleReference>,
    /// `usages`: transitive imports, by logical name.
    pub usages: BTreeMap<BString, Vec<BString>>,
}
