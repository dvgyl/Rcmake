//! Ninja C++ module pipeline (ADR-012): the `CXX_SCAN__<t>_`, `CXX_DYNDEP__<t>_`, and
//! `CXX_COMPILER__<t>_scanned_` rules and the `dyndep` bindings of compile edges.

use rcmake_gen_common::GenError;

/// Error for a build graph with module scan or collate steps.
pub(crate) const fn unsupported() -> GenError {
    GenError::Unimplemented("Ninja C++ module scanning")
}
