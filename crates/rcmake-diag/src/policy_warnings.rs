// SPDX-License-Identifier: BSD-3-Clause
// Derived from CMake (Copyright Kitware, Inc. and Contributors);
// see third_party/licences/CMake-BSD-3-Clause.txt.

//! Runtime policy warning summaries (`cmPolicies::GetPolicyWarning`).
//!
//! Recorded from CMake 4.5.1 runtime output: `cmake_policy(GET_WARNING <CMP> <var>)`
//! for each policy in a `cmake -P` script. These texts differ from the
//! `--help-policy` titles in [`crate::POLICIES`] for many policies.

/// `(policy number, summary)` pairs in ascending policy order.
pub(crate) static WARNING_SUMMARIES: &[(u16, &str)] = &[
    (0, "A minimum required CMake version must be specified."),
    (1, "CMAKE_BACKWARDS_COMPATIBILITY should no longer be used."),
    (2, "Logical target names must be globally unique."),
    (
        3,
        "Libraries linked via full path no longer produce linker search paths.",
    ),
    (
        4,
        "Libraries linked may not have leading or trailing whitespace.",
    ),
    (
        5,
        "Preprocessor definition values are now escaped automatically.",
    ),
    (
        6,
        "Installing MACOSX_BUNDLE targets requires a BUNDLE DESTINATION.",
    ),
    (7, "list command no longer ignores empty elements."),
    (
        8,
        "Libraries linked by full-path must have a valid library file name.",
    ),
    (
        9,
        "FILE GLOB_RECURSE calls should not follow symlinks by default.",
    ),
    (10, "Bad variable reference syntax is an error."),
    (
        11,
        "Included scripts do automatic cmake_policy PUSH and POP.",
    ),
    (12, "if() recognizes numbers and boolean constants."),
    (13, "Duplicate binary directories are not allowed."),
    (14, "Input directories must have CMakeLists.txt."),
    (
        15,
        "link_directories() treats paths relative to the source dir.",
    ),
    (
        16,
        "target_link_libraries() reports error if its only argument is not a target.",
    ),
    (
        17,
        "Prefer files from the CMake module directory when including from there.",
    ),
    (18, "Ignore CMAKE_SHARED_LIBRARY_<Lang>_FLAGS variable."),
    (
        19,
        "Do not re-expand variables in include and link information.",
    ),
    (
        20,
        "Automatically link Qt executables to qtmain target on Windows.",
    ),
    (
        21,
        "Fatal error on relative paths in INCLUDE_DIRECTORIES target property.",
    ),
    (22, "INTERFACE_LINK_LIBRARIES defines the link interface."),
    (
        23,
        "Plain and keyword target_link_libraries signatures cannot be mixed.",
    ),
    (24, "Disallow include export result."),
    (25, "Compiler id for Apple Clang is now AppleClang."),
    (26, "Disallow use of the LOCATION target property."),
    (
        27,
        "Conditionally linked imported targets with missing include directories.",
    ),
    (
        28,
        "Double colon in target name means ALIAS or IMPORTED target.",
    ),
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
        37,
        "Target names should not be reserved and should match a validity pattern.",
    ),
    (38, "Targets may not link directly to themselves."),
    (39, "Utility targets may not have link dependencies."),
    (
        40,
        "The target in the TARGET signature of add_custom_command() must exist and must be defined in the current directory.",
    ),
    (41, "Error on relative include with generator expression."),
    (42, "MACOSX_RPATH is enabled by default."),
    (43, "Ignore COMPILE_DEFINITIONS_<Config> properties."),
    (
        44,
        "Case sensitive <LANG>_COMPILER_ID generator expressions.",
    ),
    (45, "Error on non-existent target in get_target_property."),
    (46, "Error on non-existent dependency in add_dependencies."),
    (47, "Use QCC compiler id for the qcc drivers on QNX."),
    (48, "project() command manages VERSION variables."),
    (49, "Do not expand variables in target source entries."),
    (50, "Disallow add_custom_command SOURCE signatures."),
    (51, "List TARGET_OBJECTS in SOURCES target property."),
    (
        52,
        "Reject source and build dirs in installed INTERFACE_INCLUDE_DIRECTORIES.",
    ),
    (
        53,
        "Simplify variable reference and escape sequence evaluation.",
    ),
    (
        54,
        "Only interpret if() arguments as variables or keywords when unquoted.",
    ),
    (55, "Strict checking for break() command."),
    (
        56,
        "Honor link flags in try_compile() source-file signature.",
    ),
    (57, "Support new IN_LIST if() operator."),
    (
        58,
        "Ninja requires custom command byproducts to be explicit.",
    ),
    (
        59,
        "Do not treat DEFINITIONS as a built-in directory property.",
    ),
    (
        60,
        "Link libraries by full path even in implicit directories.",
    ),
    (
        61,
        "CTest does not by default tell make to ignore errors (-i).",
    ),
    (62, "Disallow install() of export() result."),
    (63, "Honor visibility properties for all target types."),
    (64, "Support new TEST if() operator."),
    (
        65,
        "Do not add flags to export symbols from executables without the ENABLE_EXPORTS target property.",
    ),
    (
        66,
        "Honor per-config flags in try_compile() source-file signature.",
    ),
    (
        67,
        "Honor language standard in try_compile() source-file signature.",
    ),
    (68, "RPATH settings on macOS do not affect install_name."),
    (69, "INTERPROCEDURAL_OPTIMIZATION is enforced when enabled."),
    (70, "Define file(GENERATE) behavior for relative paths."),
    (71, "Let AUTOMOC and AUTOUIC process GENERATED files."),
    (72, "FindOpenGL prefers GLVND by default when available."),
    (73, "Do not produce legacy _LIB_DEPENDS cache entries."),
    (74, "find_package uses <PackageName>_ROOT variables."),
    (
        75,
        "Include file check macros honor CMAKE_REQUIRED_LIBRARIES.",
    ),
    (
        76,
        "target_sources() command converts relative paths to absolute.",
    ),
    (77, "option() honors normal variables."),
    (78, "UseSWIG generates standard target names."),
    (
        79,
        "target_link_libraries allows use with targets in other directories.",
    ),
    (80, "BundleUtilities cannot be included at configure time."),
    (
        81,
        "Relative paths not allowed in LINK_DIRECTORIES target property.",
    ),
    (
        82,
        "Install rules from add_subdirectory() are interleaved with those in caller.",
    ),
    (83, "Add PIE options when linking executable."),
    (84, "The FindQt module does not exist for find_package()."),
    (85, "$<IN_LIST:...> handles empty list items."),
    (86, "UseSWIG honors SWIG_MODULE_NAME via -module flag."),
    (
        87,
        "Install CODE|SCRIPT allow the use of generator expressions.",
    ),
    (
        88,
        "FindBISON runs bison in CMAKE_CURRENT_BINARY_DIR when executing.",
    ),
    (
        89,
        "Compiler id for IBM Clang-based XL compilers is now XLClang.",
    ),
    (
        90,
        "export(PACKAGE) does not populate package registry by default.",
    ),
    (
        91,
        "MSVC runtime library flags are selected by an abstraction.",
    ),
    (
        92,
        "MSVC warning flags are not in CMAKE_<LANG>_FLAGS by default.",
    ),
    (93, "FindBoost reports Boost_VERSION in x.y.z format."),
    (
        94,
        "FindPython3,  FindPython2 and FindPython use LOCATION for lookup strategy.",
    ),
    (
        95,
        "RPATH entries are properly escaped in the intermediary CMake install script.",
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
    (
        99,
        "Link properties are transitive over private dependencies of static libraries.",
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
    (106, "The Documentation module is removed."),
    (107, "An ALIAS target cannot overwrite another target."),
    (108, "A target cannot link to itself through an alias."),
    (
        109,
        "find_program() requires permission to execute but not to read.",
    ),
    (
        110,
        "add_test() supports arbitrary characters in test names.",
    ),
    (
        111,
        "An imported target missing its location property fails during generation.",
    ),
    (
        112,
        "Target file component generator expressions do not add target dependencies.",
    ),
    (
        113,
        "Makefile generators do not repeat custom commands from target dependencies.",
    ),
    (114, "ExternalProject step targets fully adopt their steps."),
    (115, "Source file extensions must be explicit."),
    (
        116,
        "Ninja generators transform DEPFILEs from add_custom_command().",
    ),
    (
        117,
        "MSVC RTTI flag /GR is not added to CMAKE_CXX_FLAGS by default.",
    ),
    (
        118,
        "GENERATED sources may be used across directories without manual marking.",
    ),
    (
        119,
        "LANGUAGE source file property explicitly compiles as specified language.",
    ),
    (120, "The WriteCompilerDetectionHeader module is removed."),
    (
        121,
        "The list() command now validates parsing of index arguments.",
    ),
    (
        122,
        "UseSWIG use standard library name conventions for csharp language.",
    ),
    (
        123,
        "ARMClang cpu/arch compile and link flags must be set explicitly.",
    ),
    (
        124,
        "foreach() loop variables are only available in the loop scope.",
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
        129,
        "Compiler id for MCST LCC compilers is now LCC, not GNU.",
    ),
    (130, "while() diagnoses condition evaluation errors."),
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
        136,
        "Watcom runtime library flags are selected by an abstraction.",
    ),
    (
        137,
        "try_compile() passes platform variables in project mode",
    ),
    (138, "CheckIPOSupported uses flags from calling project."),
    (
        139,
        "The if() command supports path comparisons using PATH_EQUAL operator.",
    ),
    (140, "The return() command checks its arguments."),
    (
        141,
        "MSVC debug information format flags are selected by an abstraction.",
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
    (145, "The Dart and FindDart modules are removed."),
    (146, "The FindCUDA module is removed."),
    (
        147,
        "Visual Studio generators build custom commands in parallel.",
    ),
    (
        148,
        "The FindPythonInterp and FindPythonLibs modules are removed.",
    ),
    (
        149,
        "Visual Studio generators select latest Windows SDK by default.",
    ),
    (
        150,
        "ExternalProject_Add and FetchContent_Declare commands treat relative GIT_REPOSITORY paths as being relative to the parent project's remote.",
    ),
    (
        151,
        "AUTOMOC include directory is a system include directory by default.",
    ),
    (
        152,
        "file(REAL_PATH) resolves symlinks before collapsing ../ components.",
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
    (
        156,
        "De-duplicate libraries on link lines based on linker capabilities.",
    ),
    (157, "Swift compilation mode selected by an abstraction."),
    (
        158,
        "add_test() honors CMAKE_CROSSCOMPILING_EMULATOR only when cross-compiling.",
    ),
    (159, "file(STRINGS) with REGEX updates CMAKE_MATCH_<n>."),
    (
        160,
        "More read-only target properties now error when trying to set them.",
    ),
    (161, "CPACK_PRODUCTBUILD_DOMAINS defaults to true."),
    (
        162,
        "Visual Studio generators add UseDebugLibraries indicators by default.",
    ),
    (
        163,
        "The GENERATED source file property is now visible in all directories.",
    ),
    (
        164,
        "add_library() rejects SHARED libraries when not supported by the platform.",
    ),
    (
        165,
        "enable_language() must not be called before project().",
    ),
    (
        166,
        "TARGET_PROPERTY evaluates link properties transitively over private dependencies of static libraries.",
    ),
    (167, "The FindBoost module is removed."),
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
    (173, "The CMakeFindFrameworks module is removed."),
    (
        174,
        "cmake_parse_arguments(PARSE_ARGV) defines a variable for an empty string after a single-value keyword.",
    ),
    (175, "add_custom_command() rejects invalid arguments."),
    (176, "execute_process() ENCODING is UTF-8 by default."),
    (177, "install() DESTINATION paths are normalized."),
    (178, "Test command lines preserve empty arguments."),
    (
        179,
        "De-duplication of static libraries on link lines keeps first occurrence.",
    ),
    (
        180,
        "project() always sets <PROJECT-NAME>_* as normal variables.",
    ),
    (
        181,
        "Link command-line fragment variables are parsed and re-quoted.",
    ),
    (182, "Create shared library archives by default on AIX."),
    (183, "add_feature_info() supports full Condition Syntax."),
    (
        184,
        "MSVC runtime check flags are selected by an abstraction.",
    ),
    (
        185,
        "FindRuby no longer provides upper-case RUBY_* variables.",
    ),
    (
        186,
        "Regular expressions match ^ at most once in repeated searches.",
    ),
    (
        187,
        "Include source file without an extension after the same name with an extension.",
    ),
    (188, "The FindGCCXML module is removed."),
    (
        189,
        "TARGET_PROPERTY evaluates LINK_LIBRARIES properties transitively.",
    ),
    (
        190,
        "FindPython enforce consistency in cross-compiling mode.",
    ),
    (191, "The FindCABLE module is removed."),
    (
        192,
        "GNUInstallDirs uses absolute SYSCONFDIR, LOCALSTATEDIR, and RUNSTATEDIR in special prefixes.",
    ),
    (
        193,
        "GNUInstallDirs caches CMAKE_INSTALL_* with leading 'usr/' for install prefix '/'.",
    ),
    (194, "MSVC is not an assembler for language ASM."),
    (
        195,
        "Swift modules in build trees use the Swift module directory structure.",
    ),
    (196, "The CMakeDetermineVSServicePack module is removed."),
    (
        197,
        "MSVC link -machine: flag is not in CMAKE_*_LINKER_FLAGS.",
    ),
    (
        198,
        "CMAKE_PARENT_LIST_FILE is not defined in CMakeLists.txt.",
    ),
    (
        199,
        "$<CONFIG:cfgs> does not match mapped configurations that are not selected.",
    ),
    (
        200,
        "Location and configuration selection for imported targets is more consistent.",
    ),
    (
        201,
        "Python::NumPy does not depend on Python::Development.Module.",
    ),
    (
        202,
        "PDB file names always include their target's per-config POSTFIX.",
    ),
    (
        203,
        "_WINDLL is defined for shared libraries targeting the MSVC ABI.",
    ),
    (
        204,
        "A character set is always defined when targeting the MSVC ABI.",
    ),
    (
        205,
        "file(CREATE_LINK) with COPY_ON_ERROR copies directory content.",
    ),
    (
        206,
        "The CPack Archive Generator defaults to UID 0 and GID 0.",
    ),
    (
        207,
        "file(GET_RUNTIME_DEPENDENCIES) normalizes paths before matching.",
    ),
    (208, "export(EXPORT) does not allow empty arguments."),
    (
        209,
        "Verify interface header sets checks executables without exports.",
    ),
    (
        210,
        "CMAKE_<LANG>_LINK_FLAGS adds link flags to all target types.",
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
        214,
        "Honor CMAKE_EXE_LINKER_FLAGS for Swift executable targets.",
    ),
    (
        215,
        "Ninja generators emit Swift modules separately from compilation.",
    ),
    (216, "Swift targets have a default project name."),
    (217, "The MACROS directory property does not exist anymore."),
    (
        218,
        "The CMAKE_WARN_DEPRECATED and CMAKE_ERROR_DEPRECATED variables are ignored.",
    ),
    (219, "Macro invocations preserve backslashes in arguments."),
    (
        220,
        "Languages enabled in subdirectories propagate to the top-level directory.",
    ),
    (
        221,
        "cmake_host_system_information() DISTRIB_* queries read the host os-release.",
    ),
    (
        222,
        "The if() command supports path prefix tests using PATH_IS_PREFIX operator.",
    ),
    (223, "An empty path is not a prefix of any path."),
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
