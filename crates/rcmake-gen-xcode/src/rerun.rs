//! Regeneration script for the ZERO_CHECK target.

use std::{fmt::Write as _, path::Path};

use rcmake_gen_common::{GenError, ide::rerun_inputs};
use rcmake_host::{Host, path::to_native};
use rcmake_resolve::ConfigureMetadata;

use crate::{phases::quote, write_file};

pub(crate) const SCRIPT: &str = "CMakeScripts/ReRunCMake.sh";

pub(crate) fn write(
    host: &dyn Host,
    bin: &Path,
    source: &str,
    metadata: &ConfigureMetadata,
    cmake: &str,
) -> Result<(), GenError> {
    let stamp = bin.join("CMakeFiles/cmake.check_cache");
    let stamp = quote(&stamp.to_string_lossy());
    let mut script = "set -eu\n".to_owned();
    let _ = writeln!(script, "stamp={stamp}\nregen=0");
    let inputs = rerun_inputs(metadata);
    if inputs.is_empty() {
        let _ = writeln!(
            script,
            "set -- {}",
            quote(&format!("{source}/CMakeLists.txt"))
        );
    } else {
        script.push_str("set --");
        for input in inputs {
            let _ = write!(script, " {}", quote(&to_native(&input).to_string_lossy()));
        }
        script.push('\n');
    }
    script.push_str("for input do\n  if [ -f \"$input\" ] && [ \"$input\" -nt \"$stamp\" ]; then regen=1; fi\ndone\n");
    // Establish the first timestamp after generation, even when ZERO_CHECK has not run.
    let _ = writeln!(
        script,
        "if [ \"$regen\" = 1 ]; then\n  {} -S {} -B {}\nfi",
        quote(cmake),
        quote(source),
        quote(&bin.to_string_lossy())
    );
    write_file(host, &bin.join(SCRIPT), &script)?;
    host.create_dir_all(&bin.join("CMakeFiles"))
        .and_then(|_| {
            host.write(
                &bin.join("CMakeFiles/cmake.check_cache"),
                b"",
                rcmake_host::WriteMode::Truncate,
            )
        })
        .map_err(|error| GenError::Invalid(error.to_string().into()))
}
