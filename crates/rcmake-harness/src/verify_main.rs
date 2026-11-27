//! Isolated companion binary for explicit `rcmake verify --oracle` comparisons.
//!
//! Only this companion binary (outside the core `rcmake` engine) invokes an
//! explicit upstream CMake oracle executable and compares its outputs against
//! `rcmake` running in compatibility mode.

#![forbid(unsafe_code)]

mod verify;

use std::path::Path;
use std::process::ExitCode;

use rcmake_base::BString;
use rcmake_host::{NativeHost, path};

#[allow(
    clippy::disallowed_methods,
    reason = "Entry point initializes NativeHost and converts process arguments."
)]
fn main() -> ExitCode {
    let args: Vec<BString> = std::env::args_os()
        .skip(1)
        .map(|arg| path::from_native(Path::new(&arg)))
        .collect();
    let host = match NativeHost::new() {
        Ok(host) => host,
        Err(err) => {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                format!("rcmake-verify: failed to initialize host: {err}\n").as_bytes(),
            );
            return ExitCode::from(2);
        }
    };
    let code = verify::run(&host, &args);
    ExitCode::from(u8::try_from(code).unwrap_or(2))
}
