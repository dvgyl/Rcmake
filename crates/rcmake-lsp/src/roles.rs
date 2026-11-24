use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variable {
    Normal,
    Cache,
    Environment,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Other,
    Target,
    TargetDeclaration,
    CommandDeclaration,
    Variable(Variable),
    Policy,
    Path,
}

pub(crate) type Keyword = (&'static str, &'static str);

const SCOPE: [Keyword; 3] = [
    ("PRIVATE", "Use these items to build this target."),
    (
        "PUBLIC",
        "Use these items to build this target and targets that depend on it.",
    ),
    (
        "INTERFACE",
        "Use these items to build targets that depend on this target.",
    ),
];
const LINK_CONFIGURATION: [Keyword; 3] = [
    ("debug", "Link the next item only in debug configurations."),
    (
        "optimized",
        "Link the next item in configurations other than debug.",
    ),
    ("general", "Link the next item in all configurations."),
];
const LINK_SCOPE: &[Keyword] = &[
    SCOPE[0],
    SCOPE[1],
    SCOPE[2],
    ("LINK_PRIVATE", "Use these libraries to link this target."),
    (
        "LINK_PUBLIC",
        "Use these libraries to link this target and its dependents.",
    ),
    (
        "LINK_INTERFACE_LIBRARIES",
        "Set the libraries to link into dependents.",
    ),
    LINK_CONFIGURATION[0],
    LINK_CONFIGURATION[1],
    LINK_CONFIGURATION[2],
];
const BEFORE_SCOPE: Keyword = ("BEFORE", "Put these items before existing entries.");
const SYSTEM: Keyword = (
    "SYSTEM",
    "Mark the directories as system include directories.",
);
const ALIAS: Keyword = ("ALIAS", "Give an existing target another name.");
const IMPORTED: Keyword = (
    "IMPORTED",
    "Refer to an artifact built outside this project.",
);
const GLOBAL: Keyword = (
    "GLOBAL",
    "Make the imported target visible in all directories.",
);
const TARGET: Keyword = ("TARGET", "Select a build target.");
const SOURCES: Keyword = ("SOURCES", "Start the list of source files.");
const PROPERTIES: Keyword = ("PROPERTIES", "Start the list of property names and values.");
const PROPERTY: Keyword = ("PROPERTY", "Start the property name.");
const INSTALL_TARGET_OPTIONS: &[Keyword] = &[
    ("EXPORT", "Associate these targets with an export set."),
    ("FILE", "Set the export file name."),
    ("NAMESPACE", "Add a prefix to the exported target names."),
    ("RUNTIME", "Select executable or runtime artifacts."),
    ("LIBRARY", "Select shared library artifacts."),
    ("ARCHIVE", "Select archive artifacts."),
    ("OBJECTS", "Select object files."),
    ("FRAMEWORK", "Select framework artifacts."),
    ("BUNDLE", "Select application bundle artifacts."),
    ("PUBLIC_HEADER", "Select public headers."),
    ("PRIVATE_HEADER", "Select private headers."),
    ("RESOURCE", "Select resource files."),
    ("FILE_SET", "Select a named file set."),
    ("CXX_MODULES_BMI", "Select compiled C++ module interfaces."),
    ("INCLUDES", "Add include directories to exported targets."),
    ("DESTINATION", "Set the installation path."),
    ("PERMISSIONS", "Set installation permissions."),
    ("CONFIGURATIONS", "Select build configurations."),
    ("COMPONENT", "Select the installation component."),
    (
        "NAMELINK_COMPONENT",
        "Select the component for the library link.",
    ),
    ("OPTIONAL", "Continue if an artifact is absent."),
    (
        "EXCLUDE_FROM_ALL",
        "Exclude this rule from a full installation.",
    ),
    ("NAMELINK_ONLY", "Install only the library link."),
    ("NAMELINK_SKIP", "Skip the library link."),
    (
        "EXPORT_LINK_INTERFACE_LIBRARIES",
        "Export legacy link interface properties.",
    ),
];
const TARGET_KINDS: &[Keyword] = &[
    ("STATIC", "Create a static library."),
    ("SHARED", "Create a shared library."),
    ("MODULE", "Create a library for dynamic loading."),
    (
        "OBJECT",
        "Compile sources without linking or archiving the object files.",
    ),
    (
        "INTERFACE",
        "Create a library that supplies usage requirements.",
    ),
    ("UNKNOWN", "Use an imported library of an unknown type."),
    ALIAS,
    IMPORTED,
];
const EXECUTABLE_FLAGS: &[Keyword] = &[
    (
        "WIN32",
        "Create a Windows application with a graphical interface.",
    ),
    ("MACOSX_BUNDLE", "Create a macOS application bundle."),
    (
        "EXCLUDE_FROM_ALL",
        "Exclude this target from the default build.",
    ),
    ALIAS,
    IMPORTED,
];
const SOURCE_FLAGS: &[Keyword] = &[
    SCOPE[0],
    SCOPE[1],
    SCOPE[2],
    ("FILE_SET", "Add files to a named file set."),
    ("TYPE", "Set the file set type."),
    ("BASE_DIRS", "Start the list of file set base directories."),
    ("FILES", "Start the list of files in the file set."),
];
const INCLUDE_FLAGS: &[Keyword] = &[
    ("OPTIONAL", "Continue if the file is not available."),
    (
        "RESULT_VARIABLE",
        "Store the resolved file name or NOTFOUND in a variable.",
    ),
    ("NO_POLICY_SCOPE", "Use the policy scope of the caller."),
    (
        "NO_DIAGNOSTIC_SCOPE",
        "Use the diagnostic scope of the caller.",
    ),
];

fn arg(command: &CommandInvocation, index: usize) -> &[u8] {
    command
        .args
        .get(index)
        .map_or(b"", |arg| arg.text.as_slice())
}

pub(crate) fn declaration_variable(command: &CommandInvocation) -> Variable {
    let name = arg(command, 0);
    if name.starts_with(b"ENV{") && name.ends_with(b"}") {
        Variable::Environment
    } else if (name.starts_with(b"CACHE{") && name.ends_with(b"}"))
        || command.lower == "option"
        || (command.lower == "set" && old_cache_syntax(command))
    {
        Variable::Cache
    } else {
        Variable::Normal
    }
}

fn old_cache_syntax(command: &CommandInvocation) -> bool {
    let len = command.args.len();
    let force = usize::from(len > 0 && arg(command, len - 1) == b"FORCE");
    len >= 5 + force && arg(command, len - 3 - force) == b"CACHE"
}

pub(crate) struct Arguments<'a> {
    command: &'a CommandInvocation,
    marker: Option<usize>,
}

impl<'a> Arguments<'a> {
    pub(crate) fn new(command: &'a CommandInvocation) -> Self {
        let marker = match command.lower.as_slice() {
            b"set_target_properties" => {
                command.args.iter().position(|arg| arg.text == "PROPERTIES")
            }
            b"set_property" | b"get_property" => {
                command.args.iter().position(|arg| arg.text == "PROPERTY")
            }
            b"export" | b"install" if arg(command, 0) == b"TARGETS" => command
                .args
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, arg)| {
                    INSTALL_TARGET_OPTIONS
                        .iter()
                        .any(|(name, _)| arg.text.as_slice() == name.as_bytes())
                })
                .map(|(index, _)| index),
            _ => None,
        };
        Self { command, marker }
    }

    fn before_marker(&self, index: usize) -> bool {
        self.marker.is_none_or(|marker| index <= marker)
    }

    pub(crate) fn role(&self, index: usize) -> Role {
        let command = self.command;
        let name = command.lower.as_slice();
        match (name, index) {
            (b"function" | b"macro", 0) => Role::CommandDeclaration,
            (b"set" | b"option" | b"unset", 0) => Role::Variable(declaration_variable(command)),
            (b"include" | b"add_subdirectory", 0) => Role::Path,
            (b"cmake_policy", 1) if matches!(arg(command, 0), b"SET" | b"GET") => Role::Policy,
            (b"add_executable" | b"add_library" | b"add_custom_target", 0) => {
                Role::TargetDeclaration
            }
            (b"add_library" | b"add_executable", 2) if arg(command, 1) == b"ALIAS" => Role::Target,
            (
                b"target_compile_definitions"
                | b"target_compile_features"
                | b"target_compile_options"
                | b"target_include_directories"
                | b"target_link_directories"
                | b"target_link_options"
                | b"target_precompile_headers"
                | b"target_sources",
                0,
            )
            | (b"target_link_libraries" | b"add_dependencies" | b"link_libraries", _)
            | (b"get_target_property", 1) => Role::Target,
            (b"build_command", 2..) if arg(command, index - 1) == b"TARGET" => Role::Target,
            (b"set_target_properties", _) if self.before_marker(index) => Role::Target,
            (b"set_property" | b"get_property", _) => {
                let scope = usize::from(name == b"get_property");
                if index > scope && arg(command, scope) == b"TARGET" && self.before_marker(index) {
                    Role::Target
                } else {
                    Role::Other
                }
            }
            (b"if" | b"elseif" | b"while", 1..)
                if command
                    .args
                    .get(index - 1)
                    .is_some_and(|arg| arg.kind == ArgKind::Unquoted && arg.text == "TARGET") =>
            {
                Role::Target
            }
            (b"target_precompile_headers", 2) if arg(command, 1) == b"REUSE_FROM" => Role::Target,
            (b"add_custom_command", 1) if arg(command, 0) == b"TARGET" => Role::Target,
            (b"install" | b"export", 1..)
                if arg(command, 0) == b"TARGETS" && self.before_marker(index) =>
            {
                Role::Target
            }
            _ => Role::Other,
        }
    }

    pub(crate) fn keywords(&self, index: usize) -> &'static [Keyword] {
        let command = self.command;
        use std::slice::from_ref;
        match (command.lower.as_slice(), index) {
            (b"target_link_libraries", 1..) => LINK_SCOPE,
            (b"link_libraries", _) => &LINK_CONFIGURATION,
            (b"target_compile_definitions" | b"target_compile_features", 1..) => &SCOPE,
            (b"target_precompile_headers", 1) => &[
                (
                    "REUSE_FROM",
                    "Reuse the precompiled header from another target.",
                ),
                SCOPE[0],
                SCOPE[1],
                SCOPE[2],
            ],
            (b"target_precompile_headers", 2..) if arg(command, 1) != b"REUSE_FROM" => &SCOPE,
            (b"target_compile_options" | b"target_link_options", 1) => {
                &[BEFORE_SCOPE, SCOPE[0], SCOPE[1], SCOPE[2]]
            }
            (b"target_link_directories", 1) => &[
                BEFORE_SCOPE,
                ("AFTER", "Put these directories after existing entries."),
                SCOPE[0],
                SCOPE[1],
                SCOPE[2],
            ],
            (b"target_include_directories", 1) => &[
                SYSTEM,
                BEFORE_SCOPE,
                ("AFTER", "Put these directories after existing entries."),
                SCOPE[0],
                SCOPE[1],
                SCOPE[2],
            ],
            (
                b"target_compile_options"
                | b"target_include_directories"
                | b"target_link_directories"
                | b"target_link_options",
                1..,
            ) => &SCOPE,
            (b"target_sources", 1..) => SOURCE_FLAGS,
            (b"add_library", 1) => TARGET_KINDS,
            (b"add_library" | b"add_executable", 2..) if arg(command, index - 1) == b"IMPORTED" => {
                from_ref(&GLOBAL)
            }
            (b"add_library" | b"add_executable", 2) if arg(command, 1) != b"ALIAS" => {
                from_ref(&IMPORTED)
            }
            (b"add_executable", 1) => EXECUTABLE_FLAGS,
            (b"add_custom_target", 1..) => &[
                ("ALL", "Include this target in the default build."),
                SOURCES,
            ],
            (b"add_custom_command", 0) => {
                &[TARGET, ("OUTPUT", "Start the list of generated files.")]
            }
            (b"set_target_properties", _) if self.before_marker(index) => from_ref(&PROPERTIES),
            (b"set_property", 0) | (b"get_property", 1) => from_ref(&TARGET),
            (b"set_property", 1..) if self.before_marker(index) => &[
                PROPERTY,
                ("APPEND", "Append values to the property as a list."),
                ("APPEND_STRING", "Append values to the property as text."),
            ],
            (b"get_property", 2..) if self.before_marker(index) => from_ref(&PROPERTY),
            (b"install" | b"export", 0) => &[("TARGETS", "Select targets to install or export.")],
            (b"install" | b"export", 1..)
                if arg(command, 0) == b"TARGETS" && self.before_marker(index) =>
            {
                INSTALL_TARGET_OPTIONS
            }
            (b"build_command", 1..) if index % 2 != 0 => from_ref(&TARGET),
            (b"include", 1..) if arg(command, index - 1) != b"RESULT_VARIABLE" => INCLUDE_FLAGS,
            (b"add_subdirectory", 1..) => &[
                (
                    "EXCLUDE_FROM_ALL",
                    "Exclude targets in the subdirectory from the default build.",
                ),
                SYSTEM,
            ],
            (b"if" | b"elseif" | b"while", _) if self.role(index) != Role::Target => &[
                TARGET,
                ("NOT", "Invert the condition."),
                ("AND", "Require both conditions to be true."),
                ("OR", "Require at least one condition to be true."),
                ("EXISTS", "Check if a path exists."),
                ("DEFINED", "Check if a variable is defined."),
                ("STREQUAL", "Compare two strings for equality."),
            ],
            (b"message", 0) => &[
                ("STATUS", "Print a status message."),
                ("WARNING", "Print a warning."),
                ("AUTHOR_WARNING", "Print a warning for the project author."),
                ("SEND_ERROR", "Print an error and skip generation."),
                ("FATAL_ERROR", "Print an error and stop processing."),
                ("NOTICE", "Print an important message."),
                ("VERBOSE", "Print a message with additional detail."),
                ("DEBUG", "Print a debug message."),
                ("TRACE", "Print a trace message."),
            ],
            (b"cmake_policy", 0) => &[
                ("SET", "Set the behavior of a policy."),
                ("GET", "Store the behavior of a policy in a variable."),
                ("PUSH", "Start a new policy scope."),
                ("POP", "Restore the previous policy scope."),
                ("VERSION", "Set policies for a CMake version."),
            ],
            (b"cmake_policy", 2) if arg(command, 0) == b"SET" => &[
                ("NEW", "Use the behavior that the policy introduced."),
                ("OLD", "Use the behavior that preceded the policy."),
            ],
            (b"project", 1..) => &[
                ("VERSION", "Set the project version."),
                ("LANGUAGES", "Start the list of languages to enable."),
                ("DESCRIPTION", "Set a short description of the project."),
                ("HOMEPAGE_URL", "Set the project website URL."),
            ],
            (b"fetchcontent_declare", 1..) => &[
                ("URL", "Set the archive download address."),
                ("URL_HASH", "Set the expected archive hash."),
                ("GIT_REPOSITORY", "Set the Git repository address."),
                ("GIT_TAG", "Select a Git revision."),
                (
                    "SOURCE_SUBDIR",
                    "Set the subdirectory to add to this project.",
                ),
                (
                    "OVERRIDE_FIND_PACKAGE",
                    "Redirect find_package() calls to this dependency.",
                ),
                ("FIND_PACKAGE_ARGS", "Set the arguments for find_package()."),
            ],
            _ => &[],
        }
    }

    pub(crate) fn keyword(&self, index: usize, arg: &Argument) -> Option<Keyword> {
        if arg.kind == ArgKind::Bracket
            || (arg.kind == ArgKind::Quoted
                && matches!(self.command.lower.as_slice(), b"if" | b"elseif" | b"while"))
        {
            return None;
        }
        self.keywords(index)
            .iter()
            .copied()
            .find(|(name, _)| arg.text.as_slice() == name.as_bytes())
    }
}
