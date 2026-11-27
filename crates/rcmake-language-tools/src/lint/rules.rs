//! Rule-specific tables and pure checkers for CMake static lint rules.

#![forbid(unsafe_code)]

use core::ops::Range;

use rcmake_base::{BString, version::Version};
use rcmake_diag::policy::PolicyId;
use rcmake_syntax::{ArgKind, CommandInvocation};

/// Metadata for a documented obsolete or removed CMake command.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ObsoleteCommandInfo {
    pub(crate) name: &'static str,
    pub(crate) deprecated_in: (u16, u16),
    /// `Some((4, 0))` for commands removed in CMake 4.0+, `None` if deprecated only.
    pub(crate) removed_in: Option<(u16, u16)>,
    pub(crate) suggestion: &'static str,
}

const OBSOLETE_COMMANDS: &[ObsoleteCommandInfo] = &[
    ObsoleteCommandInfo {
        name: "build_name",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Use CMAKE_SYSTEM_NAME and compiler identification variables instead.",
    },
    ObsoleteCommandInfo {
        name: "exec_program",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Replace exec_program() with execute_process().",
    },
    ObsoleteCommandInfo {
        name: "export_library_dependencies",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Replace export_library_dependencies() with install(EXPORT) or export().",
    },
    ObsoleteCommandInfo {
        name: "install_files",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Replace install_files() with install(FILES ...).",
    },
    ObsoleteCommandInfo {
        name: "install_programs",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Replace install_programs() with install(PROGRAMS ...).",
    },
    ObsoleteCommandInfo {
        name: "install_targets",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Replace install_targets() with install(TARGETS ...).",
    },
    ObsoleteCommandInfo {
        name: "load_command",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Remove load_command(); dynamic CMake command plugins are no longer supported.",
    },
    ObsoleteCommandInfo {
        name: "make_directory",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Replace make_directory() with file(MAKE_DIRECTORY ...).",
    },
    ObsoleteCommandInfo {
        name: "output_required_files",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Remove output_required_files(); it is no longer supported.",
    },
    ObsoleteCommandInfo {
        name: "qt_wrap_cpp",
        deprecated_in: (3, 14),
        removed_in: None,
        suggestion: "Use CMAKE_AUTOMOC or the Qt module's qt_wrap_cpp/qt6_wrap_cpp macros instead.",
    },
    ObsoleteCommandInfo {
        name: "qt_wrap_ui",
        deprecated_in: (3, 14),
        removed_in: None,
        suggestion: "Use CMAKE_AUTOUIC or the Qt module's qt_wrap_ui/qt6_wrap_ui macros instead.",
    },
    ObsoleteCommandInfo {
        name: "remove",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Replace remove() with list(REMOVE_ITEM ...).",
    },
    ObsoleteCommandInfo {
        name: "subdir_depends",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Remove subdir_depends(); express dependencies on targets directly.",
    },
    ObsoleteCommandInfo {
        name: "subdirs",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Replace subdirs() with add_subdirectory().",
    },
    ObsoleteCommandInfo {
        name: "use_mangled_mesa",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Remove use_mangled_mesa(); it is no longer supported.",
    },
    ObsoleteCommandInfo {
        name: "utility_source",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Remove utility_source(); use add_custom_command() or ExternalProject instead.",
    },
    ObsoleteCommandInfo {
        name: "variable_requires",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Replace variable_requires() with explicit if() checks.",
    },
    ObsoleteCommandInfo {
        name: "write_file",
        deprecated_in: (3, 0),
        removed_in: Some((4, 0)),
        suggestion: "Replace write_file() with file(WRITE ...) or file(APPEND ...).",
    },
];

/// Look up an obsolete/removed CMake command by lowercase name.
#[must_use]
pub(crate) fn lookup_obsolete_command(lower: &[u8]) -> Option<&'static ObsoleteCommandInfo> {
    let name = core::str::from_utf8(lower).ok()?;
    OBSOLETE_COMMANDS
        .binary_search_by_key(&name, |info| info.name)
        .ok()
        .and_then(|idx| OBSOLETE_COMMANDS.get(idx))
}

/// Parsed `min[...max]` version specification from `cmake_minimum_required` / `cmake_policy`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedVersionRange {
    pub(crate) min: Version,
    pub(crate) max: Option<Version>,
    pub(crate) span: Range<usize>,
}

impl ParsedVersionRange {
    /// Effective policy version (`max` if supplied, otherwise `min`).
    #[must_use]
    pub(crate) fn effective_policy_version(&self) -> &Version {
        self.max.as_ref().unwrap_or(&self.min)
    }
}

/// Parse the `VERSION min[...max]` operand of `cmake_minimum_required` or `cmake_policy`.
#[must_use]
pub(crate) fn parse_version_arg(cmd: &CommandInvocation) -> Option<ParsedVersionRange> {
    let ver_kw_idx = cmd
        .args
        .iter()
        .position(|a| a.kind != ArgKind::Bracket && a.text.as_slice() == b"VERSION")?;
    let val_arg = cmd.args.get(ver_kw_idx + 1)?;
    let raw = val_arg.text.as_slice();
    if let Some(dots_pos) = raw.windows(3).position(|w| w == b"...") {
        let min_bytes = raw.get(..dots_pos)?;
        let max_bytes = raw.get(dots_pos + 3..)?;
        let min = Version::parse_strict(min_bytes)?;
        let max = Version::parse_strict(max_bytes)?;
        Some(ParsedVersionRange {
            min,
            max: Some(max),
            span: val_arg.span.clone(),
        })
    } else {
        let min = Version::parse_strict(raw)?;
        Some(ParsedVersionRange {
            min,
            max: None,
            span: val_arg.span.clone(),
        })
    }
}

/// Details of a `cmake_policy(SET CMPnnnn OLD)` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OldPolicyMatch {
    pub(crate) id: PolicyId,
    pub(crate) span: Range<usize>,
    pub(crate) removed_old: bool,
    pub(crate) introduced: (u16, u16, u16),
}

/// Inspect `cmake_policy(SET CMPnnnn OLD)` and return its pinned policy details.
#[must_use]
pub(crate) fn check_old_policy_call(cmd: &CommandInvocation) -> Option<OldPolicyMatch> {
    if cmd.lower.as_slice() != b"cmake_policy" || cmd.args.len() < 3 {
        return None;
    }
    let sub = cmd.args.first()?;
    let pol_arg = cmd.args.get(1)?;
    let mode_arg = cmd.args.get(2)?;
    if sub.text.as_slice() != b"SET" || mode_arg.text.as_slice() != b"OLD" {
        return None;
    }
    let id = PolicyId::parse(pol_arg.text.as_slice())?;
    let info = id.info()?;
    Some(OldPolicyMatch {
        id,
        span: pol_arg.span.clone(),
        removed_old: info.removed_old,
        introduced: info.introduced,
    })
}

/// Parse a `file(GLOB|GLOB_RECURSE <var> ...)` command signature matching CMake 4.5.1.
#[derive(Clone, Debug)]
pub(crate) struct GlobSignature {
    pub(crate) output_var: BString,
    pub(crate) has_configure_depends: bool,
    pub(crate) is_recurse: bool,
}

#[must_use]
pub(crate) fn parse_file_glob(cmd: &CommandInvocation) -> Option<GlobSignature> {
    if cmd.lower.as_slice() != b"file" || cmd.args.len() < 3 {
        return None;
    }
    let mode = cmd.args.first()?.text.as_slice();
    let is_recurse = match mode {
        b"GLOB" => false,
        b"GLOB_RECURSE" => true,
        _ => return None,
    };
    let out_arg = cmd.args.get(1)?;
    if out_arg.text.is_empty() || out_arg.text.as_slice().contains(&b'$') {
        return None;
    }

    let mut has_configure_depends = false;
    let mut saw_expression_before_cd = false;
    let mut saw_any_expression = false;
    let mut i = 2usize;
    while i < cmd.args.len() {
        let Some(arg) = cmd.args.get(i) else {
            break;
        };
        match arg.text.as_slice() {
            b"LIST_DIRECTORIES" | b"RELATIVE" => {
                i += 2;
            }
            b"FOLLOW_SYMLINKS" if is_recurse => {
                i += 1;
            }
            b"CONFIGURE_DEPENDS" => {
                if saw_any_expression {
                    saw_expression_before_cd = true;
                }
                has_configure_depends = true;
                i += 1;
            }
            _ => {
                saw_any_expression = true;
                i += 1;
            }
        }
    }

    Some(GlobSignature {
        output_var: out_arg.text.clone(),
        has_configure_depends: has_configure_depends && !saw_expression_before_cd,
        is_recurse,
    })
}

/// Classify `add_definitions(...)` arguments into preprocessor definitions (`-D`/`/D`)
/// versus general compiler flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AddDefinitionsClassification {
    pub(crate) has_definitions: bool,
    pub(crate) has_other_flags: bool,
}

#[must_use]
pub(crate) fn classify_add_definitions(cmd: &CommandInvocation) -> AddDefinitionsClassification {
    let mut has_definitions = false;
    let mut has_other_flags = false;
    for arg in &cmd.args {
        let bytes = arg.text.as_slice();
        if bytes.starts_with(b"-D") || bytes.starts_with(b"/D") {
            has_definitions = true;
        } else if !bytes.is_empty() {
            has_other_flags = true;
        }
    }
    AddDefinitionsClassification {
        has_definitions,
        has_other_flags,
    }
}

/// Return `true` if `name` is a well-known CMake built-in, platform, generator,
/// project, or standard module variable that should not be flagged as undefined
/// unless explicitly `unset()` in scope.
#[must_use]
pub(crate) fn is_builtin_or_external_var(name: &[u8]) -> bool {
    if name.starts_with(b"CMAKE_")
        || name.starts_with(b"CTEST_")
        || name.starts_with(b"CPACK_")
        || name.starts_with(b"FETCHCONTENT_")
        || name.starts_with(b"PROJECT_")
        || name.starts_with(b"ARGV")
    {
        return true;
    }
    matches!(
        name,
        b"ARGC"
            | b"ARGN"
            | b"BUILD_SHARED_LIBS"
            | b"BUILD_TESTING"
            | b"WIN32"
            | b"APPLE"
            | b"UNIX"
            | b"LINUX"
            | b"BSD"
            | b"CYGWIN"
            | b"MINGW"
            | b"MSYS"
            | b"MSVC"
            | b"MSVC_IDE"
            | b"MSVC_VERSION"
            | b"MSVC_TOOLSET_VERSION"
            | b"XCODE"
            | b"XCODE_VERSION"
            | b"IOS"
            | b"ANDROID"
            | b"EMSCRIPTEN"
            | b"BORLAND"
            | b"WATCOM"
            | b"GHSMULTI"
            | b"CYGWIN_VERSION"
            | b"ENV"
            | b"CACHE"
            | b"TRUE"
            | b"FALSE"
            | b"ON"
            | b"OFF"
            | b"YES"
            | b"NO"
            | b"EXECUTABLE_OUTPUT_PATH"
            | b"LIBRARY_OUTPUT_PATH"
    )
}
