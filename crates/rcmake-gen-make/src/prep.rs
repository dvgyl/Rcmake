//! Test and install preparation targets (`test_prep/all`, `test_prep/<test>`, and
//! `install_prep/<component>`) for Unix Makefiles (ADR-011).

use rcmake_base::BString;
use rcmake_gen_common::GenError;
use rcmake_resolve::GenerationResult;

/// Names of the preparation targets of `result`.
#[expect(dead_code, reason = "preparation targets are not generated yet")]
pub(crate) fn prep_targets(result: &GenerationResult) -> Result<Vec<BString>, GenError> {
    let _ = result;
    Err(GenError::Unimplemented(
        "test_prep and install_prep targets",
    ))
}
