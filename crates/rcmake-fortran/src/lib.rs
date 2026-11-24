//! Fortran source scanning and build dependency collation with no I/O.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod collate;
mod scan;
mod source;

pub use collate::{
    CollateError, Collation, DependInfo, DependSource, ModuleMap, ObjectScan, collate_make_depend,
    collate_ninja_dyndep,
};
pub use scan::{FortranSourceInfo, IncludeResolver, ScanError, ScanInput, scan};
pub use source::{FortranCompiler, ModuleCase, SourceForm};
