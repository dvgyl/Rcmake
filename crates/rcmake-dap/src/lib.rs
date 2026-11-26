//! CMake 4.5.1 Debug Adapter Protocol (`--debugger`) server for `rcmake`.

#![forbid(unsafe_code)]

/// Whether this platform has a native debugger pipe transport.
pub const TRANSPORT_SUPPORTED: bool = cfg!(unix);
