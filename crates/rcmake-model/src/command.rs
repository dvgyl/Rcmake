//! Custom commands (`add_custom_command()`, `add_custom_target()`).

use rcmake_base::BString;
use rcmake_diag::{Backtrace, PolicyMap};

use crate::{BT, DirId, TargetId};

/// `PRE_BUILD`, `PRE_LINK`, or `POST_BUILD`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetEvent {
    /// `PRE_BUILD`.
    PreBuild,
    /// `PRE_LINK`.
    PreLink,
    /// `POST_BUILD`.
    PostBuild,
}

/// What a custom command is attached to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CustomCommandKind {
    /// `add_custom_command(OUTPUT ...)`.
    Output,
    /// `add_custom_command(TARGET ... PRE_BUILD|PRE_LINK|POST_BUILD)`.
    TargetEvent {
        /// The target.
        target: TargetId,
        /// When the command runs.
        event: TargetEvent,
    },
    /// The commands of `add_custom_target()`.
    Utility(TargetId),
}

/// A custom command (`cmCustomCommand`).
#[derive(Clone, Debug)]
pub struct CustomCommand {
    /// Directory of the call.
    pub dir: DirId,
    /// Attachment.
    pub kind: CustomCommandKind,
    /// Backtrace of the call.
    pub backtrace: Backtrace,
    /// Policies at the call (`CMP0116` and others).
    pub policies: PolicyMap,
    /// `OUTPUT` files.
    pub outputs: Vec<BT<BString>>,
    /// `BYPRODUCTS` files.
    pub byproducts: Vec<BT<BString>>,
    /// `DEPENDS` items.
    pub depends: Vec<BT<BString>>,
    /// `MAIN_DEPENDENCY` file.
    pub main_dependency: Option<BString>,
    /// Command lines; each line is a list of arguments.
    pub command_lines: Vec<Vec<BString>>,
    /// `WORKING_DIRECTORY`.
    pub working_directory: Option<BString>,
    /// `COMMENT` (`None` when not given).
    pub comment: Option<BString>,
    /// `DEPFILE`.
    pub depfile: Option<BString>,
    /// `JOB_POOL`.
    pub job_pool: Option<BString>,
    /// `IMPLICIT_DEPENDS` pairs of language and file.
    pub implicit_depends: Vec<(BString, BString)>,
    /// `VERBATIM`.
    pub verbatim: bool,
    /// `USES_TERMINAL`.
    pub uses_terminal: bool,
    /// `COMMAND_EXPAND_LISTS`.
    pub command_expand_lists: bool,
    /// `JOB_SERVER_AWARE`.
    pub job_server_aware: bool,
    /// `CODEGEN`.
    pub codegen: bool,
}

impl CustomCommand {
    /// A custom command with no outputs, dependencies, or command lines.
    #[must_use]
    pub fn new(
        dir: DirId,
        kind: CustomCommandKind,
        backtrace: Backtrace,
        policies: PolicyMap,
    ) -> Self {
        Self {
            dir,
            kind,
            backtrace,
            policies,
            outputs: Vec::new(),
            byproducts: Vec::new(),
            depends: Vec::new(),
            main_dependency: None,
            command_lines: Vec::new(),
            working_directory: None,
            comment: None,
            depfile: None,
            job_pool: None,
            implicit_depends: Vec::new(),
            verbatim: false,
            uses_terminal: false,
            command_expand_lists: false,
            job_server_aware: false,
            codegen: false,
        }
    }
}
