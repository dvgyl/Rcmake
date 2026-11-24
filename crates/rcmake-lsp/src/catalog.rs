use crate::generated_catalog;

pub(crate) const DOCS: &str = "https://cmake.org/cmake/help/v4.5";
pub(crate) const DEPRECATED: usize = 2;
pub(crate) const READONLY: usize = 4;

pub(crate) struct Entry(
    pub(crate) &'static str,
    pub(crate) &'static str,
    pub(crate) &'static str,
    pub(crate) &'static str,
    pub(crate) usize,
);

#[derive(Clone, Copy)]
pub(crate) enum Topic {
    Command,
    Variable,
    Policy,
    TargetProperty,
    Module,
}

impl Topic {
    pub(crate) fn entries(self) -> &'static [Entry] {
        match self {
            Self::Command => generated_catalog::COMMANDS,
            Self::Variable => generated_catalog::VARIABLES,
            Self::Policy => generated_catalog::POLICIES,
            Self::TargetProperty => generated_catalog::TARGET_PROPERTIES,
            Self::Module => generated_catalog::MODULES,
        }
    }

    pub(crate) fn lookup(self, name: &str) -> Option<&'static Entry> {
        let entries = self.entries();
        entries.get(entries.binary_search_by_key(&name, |entry| entry.0).ok()?)
    }

    pub(crate) fn link(self, entry: &Entry) -> String {
        let category = match self {
            Self::Command => "command",
            Self::Variable => "variable",
            Self::Policy => "policy",
            Self::TargetProperty => "prop_tgt",
            Self::Module => "module",
        };
        let name = if entry.3.is_empty() { entry.0 } else { entry.3 };
        format!("{DOCS}/{category}/{name}.html")
    }
}

pub(crate) fn command(name: &str) -> Option<&'static Entry> {
    Topic::Command.lookup(&name.to_ascii_lowercase())
}

pub(crate) fn code_span(text: &str) -> String {
    let text = text.replace(['\r', '\n'], " ");
    let fence = "`".repeat(backticks(&text) + 1);
    if text.starts_with('`')
        || text.ends_with('`')
        || (text.starts_with(' ') && text.ends_with(' '))
    {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

pub(crate) fn code_block(text: &str) -> String {
    let fence = "`".repeat((backticks(text) + 1).max(3));
    format!("{fence}cmake\n{text}\n{fence}")
}

fn backticks(text: &str) -> usize {
    text.split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default()
}

pub(crate) fn escape_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if "\\`*[]<>|&".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

pub(crate) const MODULE_COMMANDS: &[(&str, &str, &str, &str)] = &[
    (
        "FetchContent_Declare",
        "FetchContent",
        "FetchContent_Declare(<name> <content-options>...)",
        "Record how to obtain a dependency at configure time.",
    ),
    (
        "FetchContent_MakeAvailable",
        "FetchContent",
        "FetchContent_MakeAvailable(<name>...)",
        "Obtain the declared dependencies and make them available to the project.",
    ),
    (
        "FetchContent_GetProperties",
        "FetchContent",
        "FetchContent_GetProperties(<name> [SOURCE_DIR <var>] [BINARY_DIR <var>] [POPULATED <var>])",
        "Read the source directory, binary directory, or population status of a dependency.",
    ),
    (
        "FetchContent_Populate",
        "FetchContent",
        "FetchContent_Populate(<name> <content-options>...)",
        "Obtain the source files for a dependency.",
    ),
    (
        "ExternalProject_Add",
        "ExternalProject",
        "ExternalProject_Add(<name> <option>...)",
        "Add a target that drives the build of an external project.",
    ),
];

pub(crate) fn signature_override(name: &str) -> Option<&'static str> {
    match name {
        "target_link_libraries" => Some(
            "target_link_libraries(<target> <PRIVATE|PUBLIC|INTERFACE> <item>... [...])\n\
             target_link_libraries(<target> <item>...)",
        ),
        "target_compile_options" => Some(
            "target_compile_options(<target> [BEFORE] <INTERFACE|PUBLIC|PRIVATE> <option>... [...])",
        ),
        "target_include_directories" => Some(
            "target_include_directories(<target> [SYSTEM] [AFTER|BEFORE] <INTERFACE|PUBLIC|PRIVATE> <dir>... [...])",
        ),
        "target_compile_definitions" => Some(
            "target_compile_definitions(<target> <INTERFACE|PUBLIC|PRIVATE> <definition>... [...])",
        ),
        "target_compile_features" => {
            Some("target_compile_features(<target> <INTERFACE|PUBLIC|PRIVATE> <feature>... [...])")
        }
        "target_link_options" => Some(
            "target_link_options(<target> [BEFORE] <INTERFACE|PUBLIC|PRIVATE> <option>... [...])",
        ),
        "target_link_directories" => Some(
            "target_link_directories(<target> [BEFORE|AFTER] <INTERFACE|PUBLIC|PRIVATE> <dir>... [...])",
        ),
        "target_precompile_headers" => Some(
            "target_precompile_headers(<target> <INTERFACE|PUBLIC|PRIVATE> <header>... [...])\n\
             target_precompile_headers(<target> REUSE_FROM <other-target>)",
        ),
        "target_sources" => {
            Some("target_sources(<target> <INTERFACE|PUBLIC|PRIVATE> <source>... [...])")
        }
        "add_library" => Some("add_library(<name> [<type>] [EXCLUDE_FROM_ALL] [<source>...])"),
        "add_executable" => Some("add_executable(<name> [<option>...] [<source>...])"),
        "add_subdirectory" => {
            Some("add_subdirectory(<source-dir> [<binary-dir>] [EXCLUDE_FROM_ALL] [SYSTEM])")
        }
        "include" => Some(
            "include(<file|module> [OPTIONAL] [RESULT_VARIABLE <var>] [NO_POLICY_SCOPE] [NO_DIAGNOSTIC_SCOPE])",
        ),
        "add_dependencies" => Some("add_dependencies(<target> [<target-dependency>...])"),
        "set" => Some("set(<variable> <value>... [PARENT_SCOPE])"),
        "option" => Some("option(<variable> \"<help>\" [<value>])"),
        _ => None,
    }
}
