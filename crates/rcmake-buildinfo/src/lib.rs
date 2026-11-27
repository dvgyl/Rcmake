//! Bounded readers and semantic comparators for CMake build-tree artifacts.
//!
//! This crate reads File API reply documents, `CMakeCache.txt`, `compile_commands.json`,
//! and native `rcmake` metadata through [`rcmake_host::Host`] with strict size, depth,
//! and containment checks.

#![forbid(unsafe_code)]
