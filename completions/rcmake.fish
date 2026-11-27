# SPDX-License-Identifier: MIT OR Apache-2.0
# Fish completion for rcmake.

function __rcmake_first_arg
    set -l tokens (commandline -opc)
    if test (count $tokens) -ge 2
        echo $tokens[2]
    end
end

function __rcmake_second_arg
    set -l tokens (commandline -opc)
    if test (count $tokens) -ge 3
        echo $tokens[3]
    end
end

function __rcmake_in_mode
    set -l mode (__rcmake_first_arg)
    test "$mode" = "$argv[1]"
end

function __rcmake_is_configure
    set -l mode (__rcmake_first_arg)
    not contains -- "$mode" explain diff-config fmt lint verify doctor cache ccmake ctest cpack lsp --build --install --open --workflow -E
end

function __rcmake_needs_subcommand
    set -l tokens (commandline -opc)
    test (count $tokens) -eq 1
end

function __rcmake_needs_second_token
    set -l tokens (commandline -opc)
    test (count $tokens) -eq 2; and test "$tokens[2]" = "$argv[1]"
end

function __rcmake_verify_before_sep
    set -l tokens (commandline -opc)
    test (count $tokens) -ge 2; and test "$tokens[2]" = "verify"; and not contains -- "--" $tokens
end

# Subcommand: explain
complete -c rcmake -n __rcmake_needs_subcommand -f -a explain -d "Explain targets, variables, find_package, or genex"
complete -c rcmake -n '__rcmake_needs_second_token explain' -f -a why-not -d "Explain why find_package failed for a package"
complete -c rcmake -n '__rcmake_in_mode explain' -s B -r -a '(__fish_complete_directories)' -d "Read CMakeCache.txt from build directory"
complete -c rcmake -n '__rcmake_in_mode explain' -l target -d "Look up a target or alias (or consumer target with --eval)"
complete -c rcmake -n '__rcmake_in_mode explain' -l var -d "Look up a variable"
complete -c rcmake -n '__rcmake_in_mode explain' -l config -x -a "Debug Release RelWithDebInfo MinSizeRel" -d "Select a target configuration"
complete -c rcmake -n '__rcmake_in_mode explain' -l filter -x -d "Filter by target value or compiler flag"
complete -c rcmake -n '__rcmake_in_mode explain' -l flag -x -d "Show where a compile or link flag comes from"
complete -c rcmake -n '__rcmake_in_mode explain' -l define -x -d "Show where a preprocessor definition comes from"
complete -c rcmake -n '__rcmake_in_mode explain' -l include -r -a '(__fish_complete_directories)' -d "Show where an include directory comes from"
complete -c rcmake -n '__rcmake_in_mode explain' -l dep -x -d "Show link and order-only paths to a dependency target"
complete -c rcmake -n '__rcmake_in_mode explain' -l find -x -d "Explain how find_package searched for a package"
complete -c rcmake -n '__rcmake_in_mode explain' -l call -x -d "Select Nth find_package call"
complete -c rcmake -n '__rcmake_in_mode explain' -l all-candidates -d "Show every tested path in find_package"
complete -c rcmake -n '__rcmake_in_mode explain' -l fresh -d "Re-evaluate find_package instead of reading saved trace"
complete -c rcmake -n '__rcmake_in_mode explain' -l eval -x -d "Evaluate a generator expression"
complete -c rcmake -n '__rcmake_in_mode explain' -l eval-property -x -d "Evaluate a target property"
complete -c rcmake -n '__rcmake_in_mode explain' -l lang -x -a "C CXX CUDA HIP Fortran OBJC OBJCXX Swift ASM" -d "Set $<COMPILE_LANGUAGE>"
complete -c rcmake -n '__rcmake_in_mode explain' -l link-lang -x -a "C CXX CUDA HIP Fortran OBJC OBJCXX Swift ASM" -d "Set $<LINK_LANGUAGE>"
complete -c rcmake -n '__rcmake_in_mode explain' -l directory -r -a '(__fish_complete_directories)' -d "Look up target names from source directory"
complete -c rcmake -n '__rcmake_in_mode explain' -l link -d "Follow $<LINK_ONLY:...> usage"
complete -c rcmake -n '__rcmake_in_mode explain' -l command-config -x -a "Debug Release RelWithDebInfo MinSizeRel" -d "Set $<COMMAND_CONFIG>"
complete -c rcmake -n '__rcmake_in_mode explain' -l output-config -x -a "Debug Release RelWithDebInfo MinSizeRel" -d "Set $<OUTPUT_CONFIG>"
complete -c rcmake -n '__rcmake_in_mode explain' -l no-steps -d "Show only the final evaluated result"
complete -c rcmake -n '__rcmake_in_mode explain' -l json -d "Write JSON report to stdout"
complete -c rcmake -n '__rcmake_in_mode explain' -l verbose -d "Show full values and call stacks"
complete -c rcmake -n '__rcmake_in_mode explain' -l color -x -a "auto always never" -d "Set terminal color mode"
complete -c rcmake -n '__rcmake_in_mode explain' -s h -l help -d "Show explain help"

# Subcommand: diff-config
complete -c rcmake -n __rcmake_needs_subcommand -f -a diff-config -d "Compare two configured build directories"
complete -c rcmake -n '__rcmake_in_mode diff-config' -l preset -x -d "Use binary directory of a configure preset"
complete -c rcmake -n '__rcmake_in_mode diff-config' -s S -r -a '(__fish_complete_directories)' -d "Read presets from source directory"
complete -c rcmake -n '__rcmake_in_mode diff-config' -l config -x -a "Debug Release RelWithDebInfo MinSizeRel" -d "Compare configurations (a or a:b)"
complete -c rcmake -n '__rcmake_in_mode diff-config' -l target -x -d "Compare only this target"
complete -c rcmake -n '__rcmake_in_mode diff-config' -l only -x -a "targets compile link toolchain packages cache" -d "Compare only these comma-separated sections"
complete -c rcmake -n '__rcmake_in_mode diff-config' -l cache -x -a "key all none" -d "Cache entries to compare"
complete -c rcmake -n '__rcmake_in_mode diff-config' -l no-normalize -d "Keep absolute paths without root normalization"
complete -c rcmake -n '__rcmake_in_mode diff-config' -l prepare -d "Create File API query files in each directory"
complete -c rcmake -n '__rcmake_in_mode diff-config' -l json -d "Write JSON report to stdout"
complete -c rcmake -n '__rcmake_in_mode diff-config' -l color -x -a "auto always never" -d "Set terminal color mode"
complete -c rcmake -n '__rcmake_in_mode diff-config' -s h -l help -d "Show diff-config help"

# Subcommand: fmt
complete -c rcmake -n __rcmake_needs_subcommand -f -a fmt -d "Losslessly format CMake listfiles"
complete -c rcmake -n '__rcmake_in_mode fmt' -l check -d "Report files that would change without writing"
complete -c rcmake -n '__rcmake_in_mode fmt' -l diff -d "Print unified diffs without writing"
complete -c rcmake -n '__rcmake_in_mode fmt' -l stdin -d "Read input from stdin and write formatted output to stdout"
complete -c rcmake -n '__rcmake_in_mode fmt' -l stdin-filepath -r -F -d "Resolve .rcmake.toml using this path for --stdin"
complete -c rcmake -n '__rcmake_in_mode fmt' -s h -l help -d "Show fmt help"

# Subcommand: lint
complete -c rcmake -n __rcmake_needs_subcommand -f -a lint -d "Run static lint rules on CMake listfiles"
complete -c rcmake -n '__rcmake_in_mode lint' -l sarif -d "Emit SARIF 2.1.0 JSON to stdout"
complete -c rcmake -n '__rcmake_in_mode lint' -l list-rules -d "List all built-in lint rules and default severities"
complete -c rcmake -n '__rcmake_in_mode lint' -l deny-warnings -d "Treat warning findings as failures (exit 1)"
complete -c rcmake -n '__rcmake_in_mode lint' -s h -l help -d "Show lint help"

# Subcommand: verify
complete -c rcmake -n __rcmake_needs_subcommand -f -a verify -d "Verify configure parity against CMake 4.5.1"
complete -c rcmake -n __rcmake_verify_before_sep -l oracle -r -F -d "Path to reference CMake 4.5.1 executable"
complete -c rcmake -n __rcmake_verify_before_sep -l timeout -x -d "Overall wall-clock timeout in seconds"
complete -c rcmake -n __rcmake_verify_before_sep -l json -d "Write JSON report to stdout"
complete -c rcmake -n __rcmake_verify_before_sep -s h -l help -d "Show verify help"

# Subcommand: doctor
complete -c rcmake -n __rcmake_needs_subcommand -f -a doctor -d "Inspect tools, probe cache, and build drift"
complete -c rcmake -n '__rcmake_in_mode doctor' -s B -r -a '(__fish_complete_directories)' -d "Check build directory drift"
complete -c rcmake -n '__rcmake_in_mode doctor' -s h -l help -d "Show doctor help"

# Subcommand: cache
complete -c rcmake -n __rcmake_needs_subcommand -f -a cache -d "Manage the compiler probe cache"
complete -c rcmake -n '__rcmake_needs_second_token cache' -f -a "status\t'Show cache size and entry counts' dir\t'Print probe cache directory' clear\t'Remove cache entries and blobs' export\t'Write probe entries to a portable archive' import\t'Read probe entries from a portable archive'"
complete -c rcmake -n '__rcmake_in_mode cache; and test (__rcmake_second_arg) = "import"' -l sha256 -x -d "Expected 64-hex-digit SHA-256 digest for trusted import"
complete -c rcmake -n '__rcmake_in_mode cache' -s h -l help -d "Show cache help"

# Subcommand: ccmake
complete -c rcmake -n __rcmake_needs_subcommand -f -a ccmake -d "Start the interactive cache editor TUI (Unix)"
complete -c rcmake -n '__rcmake_in_mode ccmake' -s S -r -a '(__fish_complete_directories)' -d "Specify source directory"
complete -c rcmake -n '__rcmake_in_mode ccmake' -s B -r -a '(__fish_complete_directories)' -d "Specify build directory"
complete -c rcmake -n '__rcmake_in_mode ccmake' -s C -r -F -d "Pre-load initial cache script"
complete -c rcmake -n '__rcmake_in_mode ccmake' -s D -x -d "Create or update a cache entry"
complete -c rcmake -n '__rcmake_in_mode ccmake' -s U -x -d "Remove matching cache entries"
complete -c rcmake -n '__rcmake_in_mode ccmake' -s G -x -a "'Ninja' 'Ninja Multi-Config' 'Unix Makefiles' 'Xcode'" -d "Specify build system generator"
complete -c rcmake -n '__rcmake_in_mode ccmake' -s T -x -d "Specify generator toolset"
complete -c rcmake -n '__rcmake_in_mode ccmake' -s A -x -d "Specify generator platform"
complete -c rcmake -n '__rcmake_in_mode ccmake' -l toolchain -r -F -d "Specify toolchain file"
complete -c rcmake -n '__rcmake_in_mode ccmake' -l install-prefix -r -a '(__fish_complete_directories)' -d "Specify CMAKE_INSTALL_PREFIX"
complete -c rcmake -n '__rcmake_in_mode ccmake' -l project-file -r -F -d "Specify alternate project file name"
complete -c rcmake -n '__rcmake_in_mode ccmake' -s h -s H -l help -d "Print ccmake usage"
complete -c rcmake -n '__rcmake_in_mode ccmake' -l version -f -a "json-v1" -d "Print ccmake version"

# Subcommand: ctest
complete -c rcmake -n __rcmake_needs_subcommand -f -a ctest -d "Run the bundled CTest driver"
complete -c rcmake -n '__rcmake_in_mode ctest' -l test-dir -r -a '(__fish_complete_directories)' -d "Directory containing tests"
complete -c rcmake -n '__rcmake_in_mode ctest' -s C -x -a "Debug Release RelWithDebInfo MinSizeRel" -d "Choose configuration to test"
complete -c rcmake -n '__rcmake_in_mode ctest' -s R -x -d "Include tests matching regex"
complete -c rcmake -n '__rcmake_in_mode ctest' -s E -x -d "Exclude tests matching regex"
complete -c rcmake -n '__rcmake_in_mode ctest' -s L -x -d "Include labels matching regex"
complete -c rcmake -n '__rcmake_in_mode ctest' -o LE -x -d "Exclude labels matching regex"
complete -c rcmake -n '__rcmake_in_mode ctest' -s j -l parallel -f -d "Run tests in parallel"
complete -c rcmake -n '__rcmake_in_mode ctest' -l output-on-failure -d "Output logs of failed tests"
complete -c rcmake -n '__rcmake_in_mode ctest' -l output-junit -r -F -d "Write JUnit XML report"
complete -c rcmake -n '__rcmake_in_mode ctest' -l preset -x -d "Specify a test preset"
complete -c rcmake -n '__rcmake_in_mode ctest' -l presets-file -r -F -d "Path to presets file"
complete -c rcmake -n '__rcmake_in_mode ctest' -l list-presets -f -a "defined" -d "List test presets"
complete -c rcmake -n '__rcmake_in_mode ctest' -l rerun-failed -d "Rerun previously failed tests"
complete -c rcmake -n '__rcmake_in_mode ctest' -l stop-on-failure -d "Stop on first failure"
complete -c rcmake -n '__rcmake_in_mode ctest' -l repeat -x -d "Repeat tests (mode:n)"
complete -c rcmake -n '__rcmake_in_mode ctest' -l timeout -x -d "Default test timeout in seconds"
complete -c rcmake -n '__rcmake_in_mode ctest' -s N -l show-only -f -a "human json-v1 json-v1-raw" -d "Show tests without executing"
complete -c rcmake -n '__rcmake_in_mode ctest' -s S -r -F -d "Run a CTest dashboard script"
complete -c rcmake -n '__rcmake_in_mode ctest' -s V -d "Enable verbose output"
complete -c rcmake -n '__rcmake_in_mode ctest' -s Q -d "Make CTest quiet"
complete -c rcmake -n '__rcmake_in_mode ctest' -s h -l help -d "Print CTest usage"

# Subcommand: cpack
complete -c rcmake -n __rcmake_needs_subcommand -f -a cpack -d "Run the bundled CPack driver"
complete -c rcmake -n '__rcmake_in_mode cpack' -s G -x -a "TGZ TXZ TZST TBZ2 TZ TAR STGZ ZIP ZIP_DEFLATE ZIP_STORE ZIP_BZ2 ZIP_LZMA ZIP_LZMA2 ZIP_ZSTD 7Z 7Z_STORE 7Z_DEFLATE 7Z_BZ2 7Z_LZMA 7Z_LZMA2 7Z_ZSTD 7Z_PPMD DEB RPM External Bundle DragNDrop productbuild NSIS NSIS64 WIX IFW" -d "Override CPACK_GENERATOR"
complete -c rcmake -n '__rcmake_in_mode cpack' -s C -x -a "Debug Release RelWithDebInfo MinSizeRel" -d "Select build configurations"
complete -c rcmake -n '__rcmake_in_mode cpack' -s D -x -d "Set a CPack variable"
complete -c rcmake -n '__rcmake_in_mode cpack' -l config -r -F -d "Select CPack config file"
complete -c rcmake -n '__rcmake_in_mode cpack' -s P -x -d "Set package name"
complete -c rcmake -n '__rcmake_in_mode cpack' -s R -x -d "Set package version"
complete -c rcmake -n '__rcmake_in_mode cpack' -s B -r -a '(__fish_complete_directories)' -d "Set package output directory"
complete -c rcmake -n '__rcmake_in_mode cpack' -l vendor -x -d "Set package vendor"
complete -c rcmake -n '__rcmake_in_mode cpack' -l preset -x -d "Use a package preset"
complete -c rcmake -n '__rcmake_in_mode cpack' -l presets-file -r -F -d "Select presets file"
complete -c rcmake -n '__rcmake_in_mode cpack' -l list-presets -f -a "defined" -d "List package presets"
complete -c rcmake -n '__rcmake_in_mode cpack' -s V -l verbose -d "Enable verbose output"
complete -c rcmake -n '__rcmake_in_mode cpack' -l debug -d "Enable debug output"
complete -c rcmake -n '__rcmake_in_mode cpack' -l trace -d "Trace CMake scripts"
complete -c rcmake -n '__rcmake_in_mode cpack' -l trace-expand -d "Trace CMake scripts with variable expansion"
complete -c rcmake -n '__rcmake_in_mode cpack' -s h -l help -d "Print CPack usage"

# Subcommand: lsp
complete -c rcmake -n __rcmake_needs_subcommand -f -a lsp -d "Start the CMake language server over stdio"
complete -c rcmake -n '__rcmake_in_mode lsp' -l stdio -d "Communicate over stdio"
complete -c rcmake -n '__rcmake_in_mode lsp' -l clientProcessId -x -d "Parent client process ID"
complete -c rcmake -n '__rcmake_in_mode lsp' -s h -l help -d "Print language server help"
complete -c rcmake -n '__rcmake_in_mode lsp' -s V -l version -d "Print language server version"

# Mode: --build
complete -c rcmake -n __rcmake_needs_subcommand -l build -r -a '(__fish_complete_directories)' -d "Build a CMake-generated project tree"
complete -c rcmake -n '__rcmake_in_mode --build' -l preset -x -d "Specify a build preset"
complete -c rcmake -n '__rcmake_in_mode --build' -l presets-file -r -F -d "Specify path to a presets file"
complete -c rcmake -n '__rcmake_in_mode --build' -l list-presets -f -a "defined" -d "List build presets"
complete -c rcmake -n '__rcmake_in_mode --build' -s j -l parallel -f -d "Build in parallel"
complete -c rcmake -n '__rcmake_in_mode --build' -s t -l target -x -d "Build specified target"
complete -c rcmake -n '__rcmake_in_mode --build' -l config -x -a "Debug Release RelWithDebInfo MinSizeRel" -d "Choose build configuration"
complete -c rcmake -n '__rcmake_in_mode --build' -l clean-first -d "Build target clean first"
complete -c rcmake -n '__rcmake_in_mode --build' -l resolve-package-references -x -a "on only off" -d "Restore package references during build"
complete -c rcmake -n '__rcmake_in_mode --build' -s v -l verbose -d "Enable verbose build output"

# Mode: --install
complete -c rcmake -n __rcmake_needs_subcommand -l install -r -a '(__fish_complete_directories)' -d "Install a CMake-generated project tree"
complete -c rcmake -n '__rcmake_in_mode --install' -l config -x -a "Debug Release RelWithDebInfo MinSizeRel" -d "Choose build configuration"
complete -c rcmake -n '__rcmake_in_mode --install' -l component -x -d "Install only the specified component"
complete -c rcmake -n '__rcmake_in_mode --install' -l default-directory-permissions -x -d "Set default install directory permissions"
complete -c rcmake -n '__rcmake_in_mode --install' -s j -l parallel -x -d "Install in parallel"
complete -c rcmake -n '__rcmake_in_mode --install' -l prefix -r -a '(__fish_complete_directories)' -d "Override CMAKE_INSTALL_PREFIX"
complete -c rcmake -n '__rcmake_in_mode --install' -l strip -d "Strip installed binaries"
complete -c rcmake -n '__rcmake_in_mode --install' -s v -l verbose -d "Enable verbose install output"

# Mode: --open
complete -c rcmake -n __rcmake_needs_subcommand -l open -r -a '(__fish_complete_directories)' -d "Open generated project in associated application"

# Mode: --workflow
complete -c rcmake -n __rcmake_needs_subcommand -l workflow -d "Run a workflow preset"
complete -c rcmake -n '__rcmake_in_mode --workflow' -l preset -x -d "Workflow preset to execute"
complete -c rcmake -n '__rcmake_in_mode --workflow' -l presets-file -r -F -d "Path to a presets file"
complete -c rcmake -n '__rcmake_in_mode --workflow' -l list-presets -f -a "defined" -d "List workflow presets"
complete -c rcmake -n '__rcmake_in_mode --workflow' -l fresh -d "Configure a fresh build tree"

# Mode: -E
complete -c rcmake -n __rcmake_needs_subcommand -s E -d "CMake command mode"
complete -c rcmake -n '__rcmake_needs_second_token -E' -f -a "bin2c capabilities cat chdir compare_files copy copy_directory copy_directory_if_different copy_directory_if_newer copy_if_different copy_if_newer create_hardlink create_symlink echo echo_append env environment false make_directory md5sum remove remove_directory rename rm sha1sum sha224sum sha256sum sha384sum sha512sum sleep tar time touch touch_nocreate true"

# Native rcmake extension flags (configure mode)
complete -c rcmake -n __rcmake_is_configure -l diagnostics -x -a "cmake\t'CMake diagnostic format' rich\t'Rich terminal diagnostics'" -d "Select diagnostic format (--diagnostics=cmake|rich)"
complete -c rcmake -n __rcmake_is_configure -l profile -a '(__fish_complete_directories)' -d "Write configure profile (--profile[=<dir>])"
complete -c rcmake -n __rcmake_is_configure -l no-probe-cache -d "Disable the compiler probe cache"
complete -c rcmake -n __rcmake_is_configure -l export-compile-commands -d "Export compile_commands.json (including multi-config files)"
complete -c rcmake -n __rcmake_is_configure -l fetch-lock -x -a "off\t'Disable lockfile' auto\t'Verify when lockfile exists' update\t'Update lockfile' frozen\t'Require locked sources'" -d "Select FetchContent lock mode (--fetch-lock=<mode>)"
complete -c rcmake -n __rcmake_is_configure -l hermetic -f -a "warn\t'Report undeclared inputs' strict\t'Reject undeclared inputs'" -d "Check for undeclared configure inputs (--hermetic[=warn|strict])"

# Standard CMake configure flags
complete -c rcmake -n __rcmake_is_configure -s S -r -a '(__fish_complete_directories)' -d "Specify source directory"
complete -c rcmake -n __rcmake_is_configure -s B -r -a '(__fish_complete_directories)' -d "Specify build directory"
complete -c rcmake -n __rcmake_is_configure -s C -r -F -d "Pre-load initial cache script"
complete -c rcmake -n __rcmake_is_configure -s D -x -d "Create or update a cache entry"
complete -c rcmake -n __rcmake_is_configure -s U -x -d "Remove matching cache entries"
complete -c rcmake -n __rcmake_is_configure -s G -x -a "'Ninja' 'Ninja Multi-Config' 'Unix Makefiles' 'Xcode' 'Visual Studio 17 2022' 'Visual Studio 18 2026'" -d "Specify build system generator"
complete -c rcmake -n __rcmake_is_configure -s T -x -d "Specify generator toolset"
complete -c rcmake -n __rcmake_is_configure -s A -x -d "Specify generator platform"
complete -c rcmake -n __rcmake_is_configure -l toolchain -r -F -d "Specify toolchain file"
complete -c rcmake -n __rcmake_is_configure -l install-prefix -r -a '(__fish_complete_directories)' -d "Specify CMAKE_INSTALL_PREFIX"
complete -c rcmake -n __rcmake_is_configure -l project-file -r -F -d "Specify alternate project file name"
complete -c rcmake -n __rcmake_is_configure -l preset -x -d "Specify a configure preset"
complete -c rcmake -n __rcmake_is_configure -l presets-file -r -F -d "Specify path to a presets file"
complete -c rcmake -n __rcmake_is_configure -l list-presets -f -a "configure build test package workflow all defined configure-defined build-defined test-defined package-defined workflow-defined all-defined" -d "List available presets"
complete -c rcmake -n __rcmake_is_configure -l fresh -d "Configure a fresh build tree"
complete -c rcmake -n __rcmake_is_configure -s P -r -F -d "Process script mode"
complete -c rcmake -n __rcmake_is_configure -s N -d "View mode only"
complete -c rcmake -n __rcmake_is_configure -s L -d "List non-advanced cached variables"
complete -c rcmake -n __rcmake_is_configure -l find-package -d "Legacy pkg-config like mode"
complete -c rcmake -n __rcmake_is_configure -l graphviz -r -F -d "Generate Graphviz dependency graph"
complete -c rcmake -n __rcmake_is_configure -l sarif-output -r -F -d "Write SARIF 2.1.0 diagnostics to file"
complete -c rcmake -n __rcmake_is_configure -l system-information -F -d "Dump system information"
complete -c rcmake -n __rcmake_is_configure -l print-config-dir -d "Print CMake config directory"
complete -c rcmake -n __rcmake_is_configure -l log-level -x -a "ERROR WARNING NOTICE STATUS VERBOSE DEBUG TRACE" -d "Set message verbosity level"
complete -c rcmake -n __rcmake_is_configure -l log-context -d "Prepend log messages with context"
complete -c rcmake -n __rcmake_is_configure -l debug-trycompile -d "Keep try_compile build tree"
complete -c rcmake -n __rcmake_is_configure -l debug-output -d "Enable debug output"
complete -c rcmake -n __rcmake_is_configure -l debug-find -d "Enable find command debug output"
complete -c rcmake -n __rcmake_is_configure -l debug-find-pkg -x -d "Limit debug-find to packages"
complete -c rcmake -n __rcmake_is_configure -l debug-find-var -x -d "Limit debug-find to result variables"
complete -c rcmake -n __rcmake_is_configure -l debugger -d "Enable the Debug Adapter Protocol server (Unix)"
complete -c rcmake -n __rcmake_is_configure -l debugger-pipe -r -F -d "Unix domain socket path for the debugger"
complete -c rcmake -n __rcmake_is_configure -l debugger-dap-log -r -F -d "Write DAP traffic log to file"
complete -c rcmake -n __rcmake_is_configure -l trace -d "Enable trace mode"
complete -c rcmake -n __rcmake_is_configure -l trace-expand -d "Enable trace mode with variable expansion"
complete -c rcmake -n __rcmake_is_configure -l trace-format -x -a "human json-v1" -d "Set trace output format"
complete -c rcmake -n __rcmake_is_configure -l trace-source -r -F -d "Trace only this CMake file"
complete -c rcmake -n __rcmake_is_configure -l trace-redirect -r -F -d "Redirect trace output to file"
complete -c rcmake -n __rcmake_is_configure -l check-system-vars -d "Check variable usage in system files"
complete -c rcmake -n __rcmake_is_configure -l warn-uninitialized -d "Warn about uninitialized values"
complete -c rcmake -n __rcmake_is_configure -l no-warn-unused-cli -d "Do not warn about unused CLI variables"
complete -c rcmake -n __rcmake_is_configure -l compile-no-warning-as-error -d "Ignore COMPILE_WARNING_AS_ERROR"
complete -c rcmake -n __rcmake_is_configure -l link-no-warning-as-error -d "Ignore LINK_WARNING_AS_ERROR"
complete -c rcmake -n __rcmake_is_configure -l profiling-format -x -a "google-trace" -d "Set CMake profiling format"
complete -c rcmake -n __rcmake_is_configure -l profiling-output -r -F -d "Set CMake profiling output path"
complete -c rcmake -n __rcmake_is_configure -s h -s H -l help -d "Print usage information and exit"
complete -c rcmake -n __rcmake_is_configure -l version -f -a "json-v1" -d "Print version information and exit"
complete -c rcmake -n __rcmake_is_configure -l help-full -F -d "Print all help manuals"
complete -c rcmake -n __rcmake_is_configure -l help-manual -x -d "Print one help manual"
complete -c rcmake -n __rcmake_is_configure -l help-manual-list -F -d "List available help manuals"
complete -c rcmake -n __rcmake_is_configure -l help-command -x -d "Print help for one command"
complete -c rcmake -n __rcmake_is_configure -l help-command-list -F -d "List commands with help available"
complete -c rcmake -n __rcmake_is_configure -l help-commands -F -d "Print cmake-commands manual"
complete -c rcmake -n __rcmake_is_configure -l help-diagnostic -x -d "Print help for one diagnostic"
complete -c rcmake -n __rcmake_is_configure -l help-diagnostic-list -F -d "List diagnostics with help available"
complete -c rcmake -n __rcmake_is_configure -l help-diagnostics -F -d "Print cmake-diagnostics manual"
complete -c rcmake -n __rcmake_is_configure -l help-module -x -d "Print help for one module"
complete -c rcmake -n __rcmake_is_configure -l help-module-list -F -d "List modules with help available"
complete -c rcmake -n __rcmake_is_configure -l help-modules -F -d "Print cmake-modules manual"
complete -c rcmake -n __rcmake_is_configure -l help-policy -x -d "Print help for one policy"
complete -c rcmake -n __rcmake_is_configure -l help-policy-list -F -d "List policies with help available"
complete -c rcmake -n __rcmake_is_configure -l help-policies -F -d "Print cmake-policies manual"
complete -c rcmake -n __rcmake_is_configure -l help-property -x -d "Print help for one property"
complete -c rcmake -n __rcmake_is_configure -l help-property-list -F -d "List properties with help available"
complete -c rcmake -n __rcmake_is_configure -l help-properties -F -d "Print cmake-properties manual"
complete -c rcmake -n __rcmake_is_configure -l help-variable -x -d "Print help for one variable"
complete -c rcmake -n __rcmake_is_configure -l help-variable-list -F -d "List variables with help available"
complete -c rcmake -n __rcmake_is_configure -l help-variables -F -d "Print cmake-variables manual"
