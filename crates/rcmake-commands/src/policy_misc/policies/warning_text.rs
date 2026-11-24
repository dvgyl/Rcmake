use rcmake_base::BString;
use rcmake_diag::PolicyId;

pub(super) fn body(id: PolicyId) -> BString {
    let summary = SUMMARY_OVERRIDES
        .binary_search_by_key(&id.number(), |&(number, _)| number)
        .ok()
        .and_then(|index| SUMMARY_OVERRIDES.get(index))
        .map(|&(_, summary)| summary.to_owned())
        .or_else(|| id.info().map(|info| info.summary.replace("``", "")))
        .unwrap_or_default();
    format!(
        "Policy {id} is not set: {summary}  Run \"cmake --help-policy {id}\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning."
    ).into()
}

// The reference binary uses different summaries from its help pages for these policies.
const SUMMARY_OVERRIDES: &[(u16, &str)] = &[
    (
        11,
        "Included scripts do automatic cmake_policy PUSH and POP.",
    ),
    (
        16,
        "target_link_libraries() reports error if its only argument is not a target.",
    ),
    (
        21,
        "Fatal error on relative paths in INCLUDE_DIRECTORIES target property.",
    ),
    (
        23,
        "Plain and keyword target_link_libraries signatures cannot be mixed.",
    ),
    (26, "Disallow use of the LOCATION target property."),
    (29, "The subdir_depends command should not be called."),
    (30, "The use_mangled_mesa command should not be called."),
    (31, "The load_command command should not be called."),
    (
        32,
        "The output_required_files command should not be called.",
    ),
    (
        33,
        "The export_library_dependencies command should not be called.",
    ),
    (34, "The utility_source command should not be called."),
    (35, "The variable_requires command should not be called."),
    (36, "The build_name command should not be called."),
    (
        40,
        "The target in the TARGET signature of add_custom_command() must exist and must be defined in the current directory.",
    ),
    (43, "Ignore COMPILE_DEFINITIONS_<Config> properties."),
    (
        44,
        "Case sensitive <LANG>_COMPILER_ID generator expressions.",
    ),
    (48, "project() command manages VERSION variables."),
    (
        52,
        "Reject source and build dirs in installed INTERFACE_INCLUDE_DIRECTORIES.",
    ),
    (55, "Strict checking for break() command."),
    (57, "Support new IN_LIST if() operator."),
    (64, "Support new TEST if() operator."),
    (
        65,
        "Do not add flags to export symbols from executables without the ENABLE_EXPORTS target property.",
    ),
    (71, "Let AUTOMOC and AUTOUIC process GENERATED files."),
    (74, "find_package uses <PackageName>_ROOT variables."),
    (
        76,
        "target_sources() command converts relative paths to absolute.",
    ),
    (
        79,
        "target_link_libraries allows use with targets in other directories.",
    ),
    (
        82,
        "Install rules from add_subdirectory() are interleaved with those in caller.",
    ),
    (83, "Add PIE options when linking executable."),
    (
        87,
        "Install CODE|SCRIPT allow the use of generator expressions.",
    ),
    (
        88,
        "FindBISON runs bison in CMAKE_CURRENT_BINARY_DIR when executing.",
    ),
    (
        94,
        "FindPython3,  FindPython2 and FindPython use LOCATION for lookup strategy.",
    ),
    (
        96,
        "project() preserves leading zeros in version components.",
    ),
    (
        97,
        "ExternalProject_Add with GIT_SUBMODULES \"\" initializes no submodules.",
    ),
    (
        98,
        "FindFLEX runs flex in CMAKE_CURRENT_BINARY_DIR when executing.",
    ),
    (100, "Let AUTOMOC and AUTOUIC process .hh files."),
    (
        101,
        "target_compile_options honors BEFORE keyword in all scopes.",
    ),
    (
        102,
        "mark_as_advanced() does nothing if a cache entry does not exist.",
    ),
    (
        103,
        "Multiple export() with same FILE without APPEND is not allowed.",
    ),
    (
        104,
        "CMAKE_CUDA_ARCHITECTURES now detected for NVCC, empty CUDA_ARCHITECTURES not allowed.",
    ),
    (105, "Device link step uses the link options."),
    (107, "An ALIAS target cannot overwrite another target."),
    (108, "A target cannot link to itself through an alias."),
    (
        113,
        "Makefile generators do not repeat custom commands from target dependencies.",
    ),
    (
        116,
        "Ninja generators transform DEPFILEs from add_custom_command().",
    ),
    (
        117,
        "MSVC RTTI flag /GR is not added to CMAKE_CXX_FLAGS by default.",
    ),
    (
        119,
        "LANGUAGE source file property explicitly compiles as specified language.",
    ),
    (
        121,
        "The list() command now validates parsing of index arguments.",
    ),
    (
        122,
        "UseSWIG use standard library name conventions for csharp language.",
    ),
    (
        125,
        "find_(path|file|library|program) have consistent behavior for cache variables.",
    ),
    (
        126,
        "set(CACHE) does not remove a normal variable of the same name.",
    ),
    (
        127,
        "cmake_dependent_option() supports full Condition Syntax.",
    ),
    (
        128,
        "Selection of language standard and extension flags improved.",
    ),
    (
        131,
        "LINK_LIBRARIES supports the LINK_ONLY generator expression.",
    ),
    (
        132,
        "Do not set compiler environment variables on first run",
    ),
    (
        133,
        "The CPack module disables SLA by default in the CPack DragNDrop Generator.",
    ),
    (
        134,
        "Fallback to \"HOST\" Windows registry view when \"TARGET\" view is not usable.",
    ),
    (
        135,
        "ExternalProject and FetchContent ignore timestamps in archives by default for the URL download method",
    ),
    (
        137,
        "try_compile() passes platform variables in project mode",
    ),
    (
        139,
        "The if() command supports path comparisons using PATH_EQUAL operator.",
    ),
    (
        142,
        "The Xcode generator does not append per-config suffixes to library search paths.",
    ),
    (143, "Global property USE_FOLDERS treated as ON by default"),
    (
        144,
        "find_package uses upper-case <PACKAGENAME>_ROOT variables.",
    ),
    (
        147,
        "Visual Studio generators build custom commands in parallel.",
    ),
    (
        149,
        "Visual Studio generators select latest Windows SDK by default.",
    ),
    (
        150,
        "ExternalProject_Add and FetchContent_Declare commands treat relative GIT_REPOSITORY paths as being relative to the parent project's remote.",
    ),
    (153, "The exec_program command should not be called."),
    (
        154,
        "Generated files are private by default in targets using file sets.",
    ),
    (
        155,
        "C++ sources in targets with at least C++20 are scanned for imports when supported.",
    ),
    (157, "Swift compilation mode selected by an abstraction."),
    (
        158,
        "add_test() honors CMAKE_CROSSCOMPILING_EMULATOR only when cross-compiling.",
    ),
    (161, "CPACK_PRODUCTBUILD_DOMAINS defaults to true."),
    (
        162,
        "Visual Studio generators add UseDebugLibraries indicators by default.",
    ),
    (
        164,
        "add_library() rejects SHARED libraries when not supported by the platform.",
    ),
    (
        166,
        "TARGET_PROPERTY evaluates link properties transitively over private dependencies of static libraries.",
    ),
    (
        168,
        "FetchContent implements steps directly instead of through a sub-build.",
    ),
    (
        169,
        "FetchContent_Populate(depName) single-argument signature is deprecated.",
    ),
    (
        170,
        "FETCHCONTENT_FULLY_DISCONNECTED requirements are enforced.",
    ),
    (171, "'codegen' is a reserved target name."),
    (
        172,
        "The CPack module enables per-machine installation by default in the CPack WIX Generator.",
    ),
    (
        174,
        "cmake_parse_arguments(PARSE_ARGV) defines a variable for an empty string after a single-value keyword.",
    ),
    (
        181,
        "Link command-line fragment variables are parsed and re-quoted.",
    ),
    (183, "add_feature_info() supports full Condition Syntax."),
    (
        184,
        "MSVC runtime check flags are selected by an abstraction.",
    ),
    (
        190,
        "FindPython enforce consistency in cross-compiling mode.",
    ),
    (
        192,
        "GNUInstallDirs uses absolute SYSCONFDIR, LOCALSTATEDIR, and RUNSTATEDIR in special prefixes.",
    ),
    (
        193,
        "GNUInstallDirs caches CMAKE_INSTALL_* with leading 'usr/' for install prefix '/'.",
    ),
    (
        199,
        "$<CONFIG:cfgs> does not match mapped configurations that are not selected.",
    ),
    (
        211,
        "A file may belong to at most one non-HEADERS file set in a target.",
    ),
    (
        212,
        "add_custom_command DEPENDS does not strip .exe suffixes.",
    ),
    (
        213,
        "file(ARCHIVE_{CREATE,EXTRACT}) encode archive paths as UTF-8 by default.",
    ),
    (
        215,
        "Ninja generators emit Swift modules separately from compilation.",
    ),
    (
        218,
        "The CMAKE_WARN_DEPRECATED and CMAKE_ERROR_DEPRECATED variables are ignored.",
    ),
    (
        221,
        "cmake_host_system_information() DISTRIB_* queries read the host os-release.",
    ),
    (
        222,
        "The if() command supports path prefix tests using PATH_IS_PREFIX operator.",
    ),
    (
        224,
        "Test fixtures use AROUND_EACH_REPEAT mode by default with ctest --repeat.",
    ),
    (
        225,
        "ExternalProject sets CMAKE_INSTALL_PREFIX to <INSTALL_DIR> in the default configure command.",
    ),
    (226, "Respect Swift_MODULE_NAME with Xcode Generator"),
];
