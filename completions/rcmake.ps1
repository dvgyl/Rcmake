# SPDX-License-Identifier: MIT OR Apache-2.0
# PowerShell argument completer for rcmake (Windows PowerShell 5.1 and PowerShell 7+).

Register-ArgumentCompleter -Native -CommandName rcmake -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $elements = @($commandAst.CommandElements | ForEach-Object { $_.Extent.Text })
    $first = if ($elements.Count -ge 2) { $elements[1] } else { '' }
    $prev = ''
    if ($elements.Count -ge 2) {
        if ([string]::IsNullOrEmpty($wordToComplete)) {
            $prev = $elements[$elements.Count - 1]
        } elseif ($elements.Count -ge 3) {
            $prev = $elements[$elements.Count - 2]
        }
    }

    $newResult = {
        param([string]$text, [string]$listText, [string]$tooltip)
        [System.Management.Automation.CompletionResult]::new(
            $text,
            $listText,
            [System.Management.Automation.CompletionResultType]::ParameterValue,
            $tooltip
        )
    }

    $matchValues = {
        param([string[]]$values, [string]$prefix, [string]$needle)
        foreach ($val in $values) {
            if ($val -like "$needle*") {
                $full = "$prefix$val"
                $quoted = if ($full -match '\s') { "`"$full`"" } else { $full }
                & $newResult $quoted $full $full
            }
        }
    }

    $matchOptions = {
        param([hashtable[]]$options, [string]$needle)
        foreach ($opt in $options) {
            if ($opt.Name -like "$needle*") {
                [System.Management.Automation.CompletionResult]::new(
                    $opt.Name,
                    $opt.Name,
                    [System.Management.Automation.CompletionResultType]::ParameterName,
                    $opt.Tip
                )
            }
        }
    }

    $configs = @('Debug', 'Release', 'RelWithDebInfo', 'MinSizeRel')
    $generators = @(
        'Ninja',
        'Ninja Multi-Config',
        'Unix Makefiles',
        'Xcode',
        'Visual Studio 17 2022',
        'Visual Studio 18 2026'
    )
    $listPresetTypes = @(
        'configure', 'build', 'test', 'package', 'workflow', 'all', 'defined',
        'configure-defined', 'build-defined', 'test-defined', 'package-defined',
        'workflow-defined', 'all-defined'
    )
    $logLevels = @('ERROR', 'WARNING', 'NOTICE', 'STATUS', 'VERBOSE', 'DEBUG', 'TRACE')

    # Subcommand: rcmake explain
    if ($first -eq 'explain' -and ($elements.Count -gt 2 -or [string]::IsNullOrEmpty($wordToComplete))) {
        if ($wordToComplete -like '--color=*') {
            return & $matchValues @('auto', 'always', 'never') '--color=' $wordToComplete.Substring(8)
        }
        if ($wordToComplete -like '--config=*') {
            return & $matchValues $configs '--config=' $wordToComplete.Substring(9)
        }
        if ($prev -eq '--color') {
            return & $matchValues @('auto', 'always', 'never') '' $wordToComplete
        }
        if ($prev -eq '--config') {
            return & $matchValues $configs '' $wordToComplete
        }
        $explainOpts = @(
            @{ Name = '-B'; Tip = 'Read CMakeCache.txt from this build directory' }
            @{ Name = '--target'; Tip = 'Look up a target or alias' }
            @{ Name = '--var'; Tip = 'Look up a variable' }
            @{ Name = '--config'; Tip = 'Select a target configuration' }
            @{ Name = '--filter'; Tip = 'Select an exact target value or compiler flag' }
            @{ Name = '--json'; Tip = 'Write one JSON report to stdout' }
            @{ Name = '--verbose'; Tip = 'Show full values and call stacks' }
            @{ Name = '--color=auto'; Tip = 'Use terminal color automatically' }
            @{ Name = '--color=always'; Tip = 'Force terminal color' }
            @{ Name = '--color=never'; Tip = 'Disable terminal color' }
            @{ Name = '-h'; Tip = 'Show explain help' }
            @{ Name = '--help'; Tip = 'Show explain help' }
        )
        return & $matchOptions $explainOpts $wordToComplete
    }

    # Mode: rcmake --build
    if ($first -eq '--build' -and ($elements.Count -gt 2 -or [string]::IsNullOrEmpty($wordToComplete))) {
        if ($wordToComplete -like '--resolve-package-references=*') {
            return & $matchValues @('on', 'only', 'off') '--resolve-package-references=' $wordToComplete.Substring(29)
        }
        if ($wordToComplete -like '--list-presets=*') {
            return & $matchValues @('defined') '--list-presets=' $wordToComplete.Substring(15)
        }
        if ($wordToComplete -like '--config=*') {
            return & $matchValues $configs '--config=' $wordToComplete.Substring(9)
        }
        if ($prev -eq '--config') {
            return & $matchValues $configs '' $wordToComplete
        }
        if ($prev -eq '--resolve-package-references') {
            return & $matchValues @('on', 'only', 'off') '' $wordToComplete
        }
        $buildOpts = @(
            @{ Name = '--preset'; Tip = 'Specify a build preset' }
            @{ Name = '--presets-file'; Tip = 'Specify path to a presets file' }
            @{ Name = '--list-presets'; Tip = 'List available build presets' }
            @{ Name = '--list-presets=defined'; Tip = 'List all defined build presets' }
            @{ Name = '-j'; Tip = 'Build in parallel' }
            @{ Name = '--parallel'; Tip = 'Build in parallel' }
            @{ Name = '-t'; Tip = 'Build specified target' }
            @{ Name = '--target'; Tip = 'Build specified target' }
            @{ Name = '--config'; Tip = 'Choose build configuration' }
            @{ Name = '--clean-first'; Tip = 'Build target clean first' }
            @{ Name = '--resolve-package-references=on'; Tip = 'Restore package references during build' }
            @{ Name = '--resolve-package-references=only'; Tip = 'Only restore package references' }
            @{ Name = '--resolve-package-references=off'; Tip = 'Disable package reference restore' }
            @{ Name = '-v'; Tip = 'Enable verbose output' }
            @{ Name = '--verbose'; Tip = 'Enable verbose output' }
        )
        return & $matchOptions $buildOpts $wordToComplete
    }

    # Mode: rcmake --install
    if ($first -eq '--install' -and ($elements.Count -gt 2 -or [string]::IsNullOrEmpty($wordToComplete))) {
        if ($wordToComplete -like '--config=*') {
            return & $matchValues $configs '--config=' $wordToComplete.Substring(9)
        }
        if ($prev -eq '--config') {
            return & $matchValues $configs '' $wordToComplete
        }
        $installOpts = @(
            @{ Name = '--config'; Tip = 'Choose build configuration' }
            @{ Name = '--component'; Tip = 'Install only the specified component' }
            @{ Name = '--default-directory-permissions'; Tip = 'Set default directory permissions' }
            @{ Name = '-j'; Tip = 'Install in parallel' }
            @{ Name = '--parallel'; Tip = 'Install in parallel' }
            @{ Name = '--prefix'; Tip = 'Override CMAKE_INSTALL_PREFIX' }
            @{ Name = '--strip'; Tip = 'Strip installed binaries' }
            @{ Name = '-v'; Tip = 'Enable verbose output' }
            @{ Name = '--verbose'; Tip = 'Enable verbose output' }
        )
        return & $matchOptions $installOpts $wordToComplete
    }

    # Mode: rcmake --workflow
    if ($first -eq '--workflow' -and ($elements.Count -gt 2 -or [string]::IsNullOrEmpty($wordToComplete))) {
        if ($wordToComplete -like '--list-presets=*') {
            return & $matchValues @('defined') '--list-presets=' $wordToComplete.Substring(15)
        }
        $workflowOpts = @(
            @{ Name = '--preset'; Tip = 'Workflow preset to execute' }
            @{ Name = '--presets-file'; Tip = 'Path to a presets file' }
            @{ Name = '--list-presets'; Tip = 'List available workflow presets' }
            @{ Name = '--list-presets=defined'; Tip = 'List all defined workflow presets' }
            @{ Name = '--fresh'; Tip = 'Configure a fresh build tree' }
        )
        return & $matchOptions $workflowOpts $wordToComplete
    }

    # Mode: rcmake -E
    if ($first -eq '-E' -and ($elements.Count -eq 2 -or ($elements.Count -eq 3 -and -not [string]::IsNullOrEmpty($wordToComplete)))) {
        $eCmds = @(
            'bin2c', 'capabilities', 'cat', 'chdir', 'compare_files', 'copy',
            'copy_directory', 'copy_directory_if_different', 'copy_directory_if_newer',
            'copy_if_different', 'copy_if_newer', 'create_hardlink', 'create_symlink',
            'echo', 'echo_append', 'env', 'environment', 'false', 'make_directory',
            'md5sum', 'remove', 'remove_directory', 'rename', 'rm', 'sha1sum',
            'sha224sum', 'sha256sum', 'sha384sum', 'sha512sum', 'sleep', 'tar',
            'time', 'touch', 'touch_nocreate', 'true'
        )
        return & $matchValues $eCmds '' $wordToComplete
    }

    # Configure mode: --flag=value completions
    if ($wordToComplete -like '--diagnostics=*') {
        return & $matchValues @('cmake', 'rich') '--diagnostics=' $wordToComplete.Substring(14)
    }
    if ($wordToComplete -like '--fetch-lock=*') {
        return & $matchValues @('off', 'auto', 'update', 'frozen') '--fetch-lock=' $wordToComplete.Substring(13)
    }
    if ($wordToComplete -like '--hermetic=*') {
        return & $matchValues @('warn', 'strict') '--hermetic=' $wordToComplete.Substring(11)
    }
    if ($wordToComplete -like '--log-level=*') {
        return & $matchValues $logLevels '--log-level=' $wordToComplete.Substring(12)
    }
    if ($wordToComplete -like '--trace-format=*') {
        return & $matchValues @('human', 'json-v1') '--trace-format=' $wordToComplete.Substring(15)
    }
    if ($wordToComplete -like '--profiling-format=*') {
        return & $matchValues @('google-trace') '--profiling-format=' $wordToComplete.Substring(19)
    }
    if ($wordToComplete -like '--list-presets=*') {
        return & $matchValues $listPresetTypes '--list-presets=' $wordToComplete.Substring(15)
    }
    if ($wordToComplete -like '--version=*') {
        return & $matchValues @('json-v1') '--version=' $wordToComplete.Substring(10)
    }

    # Configure mode: value following a previous option
    switch ($prev) {
        '-G' { return & $matchValues $generators '' $wordToComplete }
        '--log-level' { return & $matchValues $logLevels '' $wordToComplete }
        '--loglevel' { return & $matchValues $logLevels '' $wordToComplete }
        '--trace-format' { return & $matchValues @('human', 'json-v1') '' $wordToComplete }
        '--profiling-format' { return & $matchValues @('google-trace') '' $wordToComplete }
        '--list-presets' { return & $matchValues $listPresetTypes '' $wordToComplete }
    }

    $configureOpts = @(
        @{ Name = 'explain'; Tip = 'Explain target flags or variable provenance' }
        @{ Name = '--diagnostics=cmake'; Tip = 'Use CMake diagnostic format' }
        @{ Name = '--diagnostics=rich'; Tip = 'Use rich terminal diagnostics' }
        @{ Name = '--profile'; Tip = 'Write configure profile to <build>/rcmake-profile' }
        @{ Name = '--profile='; Tip = 'Write configure profile to specified directory' }
        @{ Name = '--no-probe-cache'; Tip = 'Disable compiler probe cache for this configure' }
        @{ Name = '--fetch-lock=off'; Tip = 'Disable FetchContent lockfile' }
        @{ Name = '--fetch-lock=auto'; Tip = 'Verify FetchContent lockfile when present' }
        @{ Name = '--fetch-lock=update'; Tip = 'Update FetchContent lockfile' }
        @{ Name = '--fetch-lock=frozen'; Tip = 'Require pinned FetchContent sources' }
        @{ Name = '--hermetic'; Tip = 'Report undeclared configure inputs (warn)' }
        @{ Name = '--hermetic=warn'; Tip = 'Report undeclared configure inputs' }
        @{ Name = '--hermetic=strict'; Tip = 'Reject undeclared configure inputs' }
        @{ Name = '-S'; Tip = 'Specify source directory' }
        @{ Name = '-B'; Tip = 'Specify build directory' }
        @{ Name = '-C'; Tip = 'Pre-load initial cache script' }
        @{ Name = '-D'; Tip = 'Create or update a cache entry' }
        @{ Name = '-U'; Tip = 'Remove matching entries from cache' }
        @{ Name = '-G'; Tip = 'Specify a build system generator' }
        @{ Name = '-T'; Tip = 'Specify generator toolset' }
        @{ Name = '-A'; Tip = 'Specify generator platform' }
        @{ Name = '--toolchain'; Tip = 'Specify toolchain file' }
        @{ Name = '--install-prefix'; Tip = 'Specify CMAKE_INSTALL_PREFIX' }
        @{ Name = '--project-file'; Tip = 'Specify alternate project file name' }
        @{ Name = '--preset'; Tip = 'Specify a configure preset' }
        @{ Name = '--presets-file'; Tip = 'Specify path to a presets file' }
        @{ Name = '--list-presets'; Tip = 'List available presets' }
        @{ Name = '--fresh'; Tip = 'Configure a fresh build tree' }
        @{ Name = '--build'; Tip = 'Build a generated project tree' }
        @{ Name = '--install'; Tip = 'Install a generated project tree' }
        @{ Name = '--workflow'; Tip = 'Run a workflow preset' }
        @{ Name = '-E'; Tip = 'CMake command mode' }
        @{ Name = '-P'; Tip = 'Process script mode' }
        @{ Name = '-N'; Tip = 'View mode only' }
        @{ Name = '-L'; Tip = 'List non-advanced cached variables' }
        @{ Name = '-LA'; Tip = 'List all cached variables' }
        @{ Name = '-LH'; Tip = 'List non-advanced cached variables with help' }
        @{ Name = '-LAH'; Tip = 'List all cached variables with help' }
        @{ Name = '--find-package'; Tip = 'Legacy pkg-config like mode' }
        @{ Name = '--graphviz='; Tip = 'Generate Graphviz dependency graph' }
        @{ Name = '--sarif-output='; Tip = 'Write SARIF 2.1.0 diagnostics' }
        @{ Name = '--system-information'; Tip = 'Dump system information' }
        @{ Name = '--print-config-dir'; Tip = 'Print CMake config directory' }
        @{ Name = '--log-level='; Tip = 'Set message verbosity level' }
        @{ Name = '--log-context'; Tip = 'Prepend log messages with context' }
        @{ Name = '--debug-trycompile'; Tip = 'Do not delete try_compile build tree' }
        @{ Name = '--debug-output'; Tip = 'Enable debug output' }
        @{ Name = '--debug-find'; Tip = 'Enable find command debug output' }
        @{ Name = '--debug-find-pkg='; Tip = 'Limit debug-find to packages' }
        @{ Name = '--debug-find-var='; Tip = 'Limit debug-find to result variables' }
        @{ Name = '--trace'; Tip = 'Enable trace mode' }
        @{ Name = '--trace-expand'; Tip = 'Trace mode with variable expansion' }
        @{ Name = '--trace-format='; Tip = 'Set trace output format' }
        @{ Name = '--trace-source='; Tip = 'Trace only this CMake file' }
        @{ Name = '--trace-redirect='; Tip = 'Redirect trace output to file' }
        @{ Name = '--check-system-vars'; Tip = 'Find variable usage problems in system files' }
        @{ Name = '--warn-uninitialized'; Tip = 'Warn about uninitialized values' }
        @{ Name = '--no-warn-unused-cli'; Tip = 'Do not warn about unused CLI variables' }
        @{ Name = '--compile-no-warning-as-error'; Tip = 'Ignore COMPILE_WARNING_AS_ERROR' }
        @{ Name = '--link-no-warning-as-error'; Tip = 'Ignore LINK_WARNING_AS_ERROR' }
        @{ Name = '--profiling-format='; Tip = 'Set CMake profiling format' }
        @{ Name = '--profiling-output='; Tip = 'Set CMake profiling output path' }
        @{ Name = '-Wdev'; Tip = 'Enable developer warnings' }
        @{ Name = '-Wno-dev'; Tip = 'Suppress developer warnings' }
        @{ Name = '-Werror=dev'; Tip = 'Make developer warnings errors' }
        @{ Name = '-Wno-error=dev'; Tip = 'Make developer warnings not errors' }
        @{ Name = '-Wdeprecated'; Tip = 'Enable deprecation warnings' }
        @{ Name = '-Wno-deprecated'; Tip = 'Suppress deprecation warnings' }
        @{ Name = '-h'; Tip = 'Print usage information and exit' }
        @{ Name = '--help'; Tip = 'Print usage information and exit' }
        @{ Name = '--version'; Tip = 'Print version information and exit' }
        @{ Name = '--version=json-v1'; Tip = 'Print version information as JSON' }
        @{ Name = '--help-full'; Tip = 'Print all help manuals' }
        @{ Name = '--help-command'; Tip = 'Print help for one command' }
        @{ Name = '--help-command-list'; Tip = 'List commands with help available' }
        @{ Name = '--help-diagnostic'; Tip = 'Print help for one diagnostic' }
        @{ Name = '--help-diagnostic-list'; Tip = 'List diagnostics with help available' }
        @{ Name = '--help-manual'; Tip = 'Print one help manual' }
        @{ Name = '--help-manual-list'; Tip = 'List available help manuals' }
        @{ Name = '--help-module'; Tip = 'Print help for one module' }
        @{ Name = '--help-module-list'; Tip = 'List modules with help available' }
        @{ Name = '--help-policy'; Tip = 'Print help for one policy' }
        @{ Name = '--help-policy-list'; Tip = 'List policies with help available' }
        @{ Name = '--help-property'; Tip = 'Print help for one property' }
        @{ Name = '--help-property-list'; Tip = 'List properties with help available' }
        @{ Name = '--help-variable'; Tip = 'Print help for one variable' }
        @{ Name = '--help-variable-list'; Tip = 'List variables with help available' }
    )

    return & $matchOptions $configureOpts $wordToComplete
}
