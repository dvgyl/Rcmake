# SPDX-License-Identifier: MIT OR Apache-2.0
# Fish completion for rcmake.

function __rcmake_first_arg
    set -l tokens (commandline -opc)
    if test (count $tokens) -ge 2
        echo $tokens[2]
    end
end

function __rcmake_in_mode
    set -l mode (__rcmake_first_arg)
    test "$mode" = "$argv[1]"
end

function __rcmake_is_configure
    set -l mode (__rcmake_first_arg)
    not contains -- "$mode" explain --build --install --workflow -E
end

function __rcmake_needs_subcommand
    set -l tokens (commandline -opc)
    test (count $tokens) -eq 1
end

function __rcmake_needs_e_cmd
    set -l tokens (commandline -opc)
    test (count $tokens) -eq 2; and test "$tokens[2]" = "-E"
end

# Subcommand: explain
complete -c rcmake -n __rcmake_needs_subcommand -f -a explain -d "Explain target flags or variable provenance"
complete -c rcmake -n '__rcmake_in_mode explain' -s B -r -a '(__fish_complete_directories)' -d "Read CMakeCache.txt from build directory"
complete -c rcmake -n '__rcmake_in_mode explain' -l target -d "Look up a target or alias"
complete -c rcmake -n '__rcmake_in_mode explain' -l var -d "Look up a variable"
complete -c rcmake -n '__rcmake_in_mode explain' -l config -x -a "Debug Release RelWithDebInfo MinSizeRel" -d "Select a target configuration"
complete -c rcmake -n '__rcmake_in_mode explain' -l filter -x -d "Filter by target value or compiler flag"
complete -c rcmake -n '__rcmake_in_mode explain' -l json -d "Write JSON report to stdout"
complete -c rcmake -n '__rcmake_in_mode explain' -l verbose -d "Show full values and call stacks"
complete -c rcmake -n '__rcmake_in_mode explain' -l color -x -a "auto always never" -d "Set terminal color mode"
complete -c rcmake -n '__rcmake_in_mode explain' -s h -l help -d "Show explain help"

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

# Mode: --workflow
complete -c rcmake -n __rcmake_needs_subcommand -l workflow -d "Run a workflow preset"
complete -c rcmake -n '__rcmake_in_mode --workflow' -l preset -x -d "Workflow preset to execute"
complete -c rcmake -n '__rcmake_in_mode --workflow' -l presets-file -r -F -d "Path to a presets file"
complete -c rcmake -n '__rcmake_in_mode --workflow' -l list-presets -f -a "defined" -d "List workflow presets"
complete -c rcmake -n '__rcmake_in_mode --workflow' -l fresh -d "Configure a fresh build tree"

# Mode: -E
complete -c rcmake -n __rcmake_needs_subcommand -s E -d "CMake command mode"
complete -c rcmake -n __rcmake_needs_e_cmd -f -a "bin2c capabilities cat chdir compare_files copy copy_directory copy_directory_if_different copy_directory_if_newer copy_if_different copy_if_newer create_hardlink create_symlink echo echo_append env environment false make_directory md5sum remove remove_directory rename rm sha1sum sha224sum sha256sum sha384sum sha512sum sleep tar time touch touch_nocreate true"

# Native rcmake extension flags (configure mode)
complete -c rcmake -n __rcmake_is_configure -l diagnostics -x -a "cmake\t'CMake diagnostic format' rich\t'Rich terminal diagnostics'" -d "Select diagnostic format (--diagnostics=cmake|rich)"
complete -c rcmake -n __rcmake_is_configure -l profile -a '(__fish_complete_directories)' -d "Write configure profile (--profile[=<dir>])"
complete -c rcmake -n __rcmake_is_configure -l no-probe-cache -d "Disable the compiler probe cache"
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
