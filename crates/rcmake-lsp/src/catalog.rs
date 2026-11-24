pub(crate) const COMMANDS: &[(&str, &str)] = &[
    (
        "add_compile_definitions",
        "Add preprocessor definitions for targets in this directory.",
    ),
    (
        "add_compile_options",
        "Add compiler options for targets in this directory.",
    ),
    (
        "add_custom_command",
        "Add a command that generates files or runs at a target build step.",
    ),
    (
        "add_custom_target",
        "Create a target that can run commands without an output file.",
    ),
    (
        "add_definitions",
        "Add preprocessor definitions to compilation.",
    ),
    (
        "add_dependencies",
        "Set build order dependencies between targets.",
    ),
    (
        "add_executable",
        "Create an executable target from source files.",
    ),
    (
        "add_library",
        "Create a library target from source files or declare an imported library.",
    ),
    (
        "add_link_options",
        "Add linker options for targets in this directory.",
    ),
    (
        "add_subdirectory",
        "Process the CMakeLists.txt file in a subdirectory.",
    ),
    ("add_test", "Register a test for CTest."),
    (
        "aux_source_directory",
        "Collect source file names from a directory.",
    ),
    (
        "block",
        "Start a block with its own variable or policy scope.",
    ),
    ("break", "Exit a loop."),
    ("build_command", "Get the command line to build a project."),
    (
        "cmake_file_api",
        "Request File API queries for this project.",
    ),
    (
        "cmake_host_system_information",
        "Get information about the host system.",
    ),
    (
        "cmake_language",
        "Call a command by name, evaluate code, or defer a call.",
    ),
    (
        "cmake_minimum_required",
        "Set the minimum CMake version and initialize policy settings.",
    ),
    (
        "cmake_parse_arguments",
        "Parse options and keyword arguments passed to a function or macro.",
    ),
    ("cmake_path", "Read and modify path syntax."),
    (
        "cmake_pkg_config",
        "Read pkg-config metadata and create targets from it.",
    ),
    ("cmake_policy", "Get or set CMake policy behavior."),
    (
        "configure_file",
        "Generate a file from an input file and current variables.",
    ),
    ("continue", "Start the next loop iteration."),
    (
        "create_test_sourcelist",
        "Create source files for a test driver.",
    ),
    (
        "define_property",
        "Declare a property and its documentation.",
    ),
    ("else", "Start the alternative branch of an if block."),
    ("elseif", "Start another conditional branch."),
    ("enable_language", "Enable compiler support for a language."),
    (
        "enable_testing",
        "Enable test generation for this directory and its subdirectories.",
    ),
    ("endblock", "End a block."),
    ("endforeach", "End a foreach loop."),
    ("endfunction", "End a function definition."),
    ("endif", "End an if block."),
    ("endmacro", "End a macro definition."),
    ("endwhile", "End a while loop."),
    (
        "execute_process",
        "Run one or more processes during configuration.",
    ),
    (
        "export",
        "Generate target import information for use from another build.",
    ),
    ("file", "Read, write, transfer, or inspect files."),
    ("find_file", "Search for a file and store its path."),
    ("find_library", "Search for a library and store its path."),
    (
        "find_package",
        "Find a package and load its CMake configuration.",
    ),
    (
        "find_path",
        "Search for a directory that contains a specified file.",
    ),
    ("find_program", "Search for a program and store its path."),
    ("foreach", "Run a block for each item in a list or range."),
    ("function", "Define a command with its own variable scope."),
    ("get_cmake_property", "Get a global CMake property."),
    ("get_directory_property", "Get a directory property."),
    ("get_filename_component", "Get a component of a path."),
    ("get_property", "Get a property from the specified scope."),
    ("get_source_file_property", "Get a source file property."),
    ("get_target_property", "Get a target property."),
    ("get_test_property", "Get a test property."),
    ("if", "Run a block when a condition is true."),
    (
        "include",
        "Read and run commands from a CMake file or module.",
    ),
    ("include_directories", "Add header search directories."),
    (
        "include_guard",
        "Prevent the current file from running again in the specified scope.",
    ),
    (
        "include_regular_expression",
        "Set the pattern that selects scanned include directives.",
    ),
    ("install", "Add rules to install targets or files."),
    ("link_directories", "Add linker search directories."),
    (
        "link_libraries",
        "Add libraries to targets created after this command.",
    ),
    ("list", "Read or modify a semicolon-separated list."),
    ("load_cache", "Read values from another project's cache."),
    (
        "macro",
        "Define a command that runs in its caller's variable scope.",
    ),
    (
        "mark_as_advanced",
        "Control whether the CMake GUI shows a cache entry by default.",
    ),
    ("math", "Evaluate an integer expression."),
    ("message", "Print a message or issue a diagnostic."),
    ("option", "Declare a Boolean option."),
    ("project", "Name the project and enable its languages."),
    (
        "remove_definitions",
        "Remove preprocessor definitions from compilation.",
    ),
    ("return", "Exit the current file, directory, or function."),
    (
        "separate_arguments",
        "Split a command line into its arguments.",
    ),
    (
        "set",
        "Set a variable, cache entry, or environment variable.",
    ),
    (
        "set_directory_properties",
        "Set properties for the current directory.",
    ),
    ("set_property", "Set a property in the specified scope."),
    (
        "set_source_files_properties",
        "Set properties on source files.",
    ),
    ("set_target_properties", "Set properties on targets."),
    ("set_tests_properties", "Set properties on tests."),
    ("site_name", "Get the host name."),
    ("source_group", "Group source files in a generated project."),
    ("string", "Read, compare, or transform strings."),
    (
        "target_compile_definitions",
        "Set preprocessor definitions and usage requirements for a target.",
    ),
    (
        "target_compile_features",
        "Set compiler features required by a target.",
    ),
    (
        "target_compile_options",
        "Set compiler options and usage requirements for a target.",
    ),
    (
        "target_include_directories",
        "Set header search directories and usage requirements for a target.",
    ),
    (
        "target_link_directories",
        "Set linker search directories for a target.",
    ),
    (
        "target_link_libraries",
        "Set link dependencies and usage requirements for a target.",
    ),
    (
        "target_link_options",
        "Set linker options and usage requirements for a target.",
    ),
    (
        "target_precompile_headers",
        "Set the headers that the compiler precompiles for a target.",
    ),
    (
        "target_sources",
        "Add source files or file sets to a target.",
    ),
    ("try_compile", "Check whether a test project compiles."),
    ("try_run", "Compile and run a test program."),
    (
        "unset",
        "Remove a variable, cache entry, or environment variable.",
    ),
    (
        "variable_watch",
        "Run a callback when a variable is read or changed.",
    ),
    ("while", "Repeat a block while a condition is true."),
];

pub(crate) const VARIABLES: &[(&str, &str)] = &[
    ("CMAKE_BINARY_DIR", "Path to the top-level build directory."),
    (
        "CMAKE_BUILD_TYPE",
        "Configuration for a single-configuration generator.",
    ),
    ("CMAKE_COMMAND", "Path to the CMake executable."),
    (
        "CMAKE_CONFIGURATION_TYPES",
        "Available configurations for a multi-configuration generator.",
    ),
    ("CMAKE_CXX_COMPILER", "C++ compiler used by the project."),
    (
        "CMAKE_CXX_STANDARD",
        "Default C++ standard for newly created targets.",
    ),
    ("CMAKE_C_COMPILER", "C compiler used by the project."),
    (
        "CMAKE_C_STANDARD",
        "Default C standard for newly created targets.",
    ),
    (
        "CMAKE_CURRENT_BINARY_DIR",
        "Build directory for the current source directory.",
    ),
    ("CMAKE_CURRENT_FUNCTION", "Name of the current function."),
    (
        "CMAKE_CURRENT_LIST_DIR",
        "Directory of the listfile that is currently running.",
    ),
    (
        "CMAKE_CURRENT_LIST_FILE",
        "Path to the listfile that is currently running.",
    ),
    (
        "CMAKE_CURRENT_SOURCE_DIR",
        "Source directory that CMake is processing.",
    ),
    (
        "CMAKE_GENERATOR",
        "Name of the selected build system generator.",
    ),
    (
        "CMAKE_INSTALL_PREFIX",
        "Base directory for relative install destinations.",
    ),
    (
        "CMAKE_MODULE_PATH",
        "Extra directories to search for CMake modules.",
    ),
    (
        "CMAKE_PREFIX_PATH",
        "Extra installation prefixes to search when finding packages and files.",
    ),
    (
        "CMAKE_SOURCE_DIR",
        "Path to the top-level source directory.",
    ),
    ("CMAKE_SYSTEM_NAME", "Name of the target operating system."),
    (
        "CMAKE_TOOLCHAIN_FILE",
        "File that describes the compiler and target platform.",
    ),
    ("CMAKE_VERSION", "Version of the CMake implementation."),
    (
        "PROJECT_BINARY_DIR",
        "Build directory for the current project.",
    ),
    ("PROJECT_NAME", "Name of the current project."),
    (
        "PROJECT_SOURCE_DIR",
        "Source directory for the current project.",
    ),
    ("PROJECT_VERSION", "Version of the current project."),
];

pub(crate) const KEYWORDS: &[&str] = &[
    "ALIAS",
    "AND",
    "CACHE",
    "COMMAND",
    "COMPONENT",
    "CONFIG",
    "DEFINED",
    "DESTINATION",
    "EXCLUDE_FROM_ALL",
    "EXISTS",
    "FALSE",
    "FORCE",
    "GENERATED",
    "GLOBAL",
    "IMPORTED",
    "INTERFACE",
    "LISTS",
    "MODULE",
    "NAMES",
    "NOT",
    "OFF",
    "ON",
    "OPTIONAL",
    "OR",
    "PARENT_SCOPE",
    "PRIVATE",
    "PUBLIC",
    "QUIET",
    "RANGE",
    "REQUIRED",
    "SHARED",
    "STATIC",
    "STREQUAL",
    "TARGET",
    "TRUE",
    "VERSION",
];
