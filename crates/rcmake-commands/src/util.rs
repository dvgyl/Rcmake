//! Argument and error helpers shared by the command families.

use rcmake_base::BString;
use rcmake_eval::CmdError;
use rcmake_expr::ExpandedArg;
use rcmake_model::ModelError;

/// Expanded value bytes of one argument.
pub(crate) fn bytes(arg: &ExpandedArg) -> &[u8] {
    arg.value.as_slice()
}

/// Expanded value bytes of every argument, in order.
pub(crate) fn values(args: &[ExpandedArg]) -> Vec<&[u8]> {
    args.iter().map(bytes).collect()
}

/// Concatenate byte slices into one owned string.
pub(crate) fn cat(parts: &[&[u8]]) -> BString {
    BString::from(parts.concat())
}

/// Report a project model error as the command's status message.
pub(crate) fn model_error(error: ModelError) -> CmdError {
    CmdError::status(error.to_string())
}
