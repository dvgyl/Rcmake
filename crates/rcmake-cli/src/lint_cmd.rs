//! Native `rcmake lint` subcommand.

#![forbid(unsafe_code)]

use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Host, Stream};

/// Execute `rcmake lint`.
pub(crate) fn run(_argv0: Option<&BString>, _args: &[BString], host: Arc<dyn Host>) -> i32 {
    host.write_out(Stream::Stderr, b"rcmake lint is not yet implemented.\n");
    2
}
