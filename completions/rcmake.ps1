# SPDX-License-Identifier: MIT OR Apache-2.0
# PowerShell argument completer for rcmake (Windows PowerShell 5.1 and PowerShell 7+).

Register-ArgumentCompleter -Native -CommandName rcmake -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $elements = @($commandAst.CommandElements | ForEach-Object { $_.Extent.Text })
    $first = if ($elements.Count -ge 2) { $elements[1] } else { '' }
    $second = if ($elements.Count -ge 3) { $elements[2] } else { '' }
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

    $matchCsvValues = {
        param([string[]]$values, [string]$prefix, [string]$needle)
        $head = ''
        $tail = $needle
        $idx = $needle.LastIndexOf(',')
        if ($idx -ge 0) {
            $head = $needle.Substring(0, $idx + 1)
            $tail = $needle.Substring($idx + 1)
        }
        & $matchValues $values "$prefix$head" $tail
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
    $langs = @('C', 'CXX', 'CUDA', 'HIP', 'Fortran', 'OBJC', 'OBJCXX', 'Swift', 'ASM')
    $diffSections = @('targets', 'compile', 'link', 'toolchain', 'packages', 'cache')
    $generators = @(
        'Ninja',
        'Ninja Multi-Config',
        'Unix Makefiles',
        'Xcode',
        'Visual Studio 17 2022',
        'Visual Studio 18 2026'
    )
    $cpackGenerators = @(
        'TGZ', 'TXZ', 'TZST', 'TBZ2', 'TZ', 'TAR', 'STGZ',
        'ZIP', 'ZIP_DEFLATE', 'ZIP_STORE', 'ZIP_BZ2', 'ZIP_LZMA', 'ZIP_LZMA2', 'ZIP_ZSTD',
        '7Z', '7Z_STORE', '7Z_DEFLATE', '7Z_BZ2', '7Z_LZMA', '7Z_LZMA2', '7Z_ZSTD', '7Z_PPMD',
        'DEB', 'RPM', 'External', 'Bundle', 'DragNDrop', 'productbuild',
        'NSIS', 'NSIS64', 'WIX', 'IFW'
    )
    $listPresetTypes = @(
        'configure', 'build', 'test', 'package', 'workflow', 'all', 'defined',
        'configure-defined', 'build-defined', 'test-defined', 'package-defined',
        'workflow-defined', 'all-defined'
    )
    $logLevels = @('ERROR', 'WARNING', 'NOTICE', 'STATUS', 'VERBOSE', 'DEBUG', 'TRACE')

    $inSubcommand = ($elements.Count -gt 2 -or [string]::IsNullOrEmpty($wordToComplete))

    # Subcommand: rcmake explain
    if ($first -eq 'explain' -and $inSubcommand) {
        if ($wordToComplete -like '--color=*') {
            return & $matchValues @('auto', 'always', 'never') '--color=' $wordToComplete.Substring(8)
        }
        if ($wordToComplete -like '--config=*') {
            return & $matchValues $configs '--config=' $wordToComplete.Substring(9)
        }
        if ($wordToComplete -like '--command-config=*') {
            return & $matchValues $configs '--command-config=' $wordToComplete.Substring(17)
        }
        if ($wordToComplete -like '--output-config=*') {
            return & $matchValues $configs '--output-config=' $wordToComplete.Substring(16)
        }
        if ($wordToComplete -like '--lang=*') {
            return & $matchValues $langs '--lang=' $wordToComplete.Substring(7)
        }
        if ($wordToComplete -like '--link-lang=*') {
            return & $matchValues $langs '--link-lang=' $wordToComplete.Substring(12)
        }
        switch ($prev) {
            '--color' { return & $matchValues @('auto', 'always', 'never') '' $wordToComplete }
            '--config' { return & $matchValues $configs '' $wordToComplete }
            '--command-config' { return & $matchValues $configs '' $wordToComplete }
            '--output-config' { return & $matchValues $configs '' $wordToComplete }
            '--lang' { return & $matchValues $langs '' $wordToComplete }
            '--link-lang' { return & $matchValues $langs '' $wordToComplete }
        }
        $explainOpts = @(
            @{ Name = 'why-not'; Tip = 'Explain why find_package failed for a package' }
            @{ Name = '-B'; Tip = 'Read CMakeCache.txt from this build directory' }
            @{ Name = '--target'; Tip = 'Look up a target or alias (or consumer target in eval mode)' }
            @{ Name = '--var'; Tip = 'Look up a variable' }
            @{ Name = '--config'; Tip = 'Select a target configuration' }
            @{ Name = '--filter'; Tip = 'Select an exact target value or compiler flag' }
            @{ Name = '--flag'; Tip = 'Show where a compile or link flag comes from' }
            @{ Name = '--define'; Tip = 'Show where a preprocessor definition comes from' }
            @{ Name = '--include'; Tip = 'Show where an include directory comes from' }
            @{ Name = '--dep'; Tip = 'Show link and order-only paths to a dependency target' }
            @{ Name = '--find'; Tip = 'Explain how find_package searched for a package' }
            @{ Name = '--call'; Tip = 'Show only the Nth find_package call' }
            @{ Name = '--all-candidates'; Tip = 'Show every tested path in find_package' }
            @{ Name = '--fresh'; Tip = 'Re-evaluate find_package instead of reading saved trace' }
            @{ Name = '--eval'; Tip = 'Evaluate a generator expression' }
            @{ Name = '--eval-property'; Tip = 'Evaluate a property owned by the target' }
            @{ Name = '--lang'; Tip = 'Set $<COMPILE_LANGUAGE>' }
            @{ Name = '--link-lang'; Tip = 'Set $<LINK_LANGUAGE>' }
            @{ Name = '--directory'; Tip = 'Look up target names from this source directory' }
            @{ Name = '--link'; Tip = 'Follow $<LINK_ONLY:...> usage' }
            @{ Name = '--command-config'; Tip = 'Set $<COMMAND_CONFIG>' }
            @{ Name = '--output-config'; Tip = 'Set $<OUTPUT_CONFIG>' }
            @{ Name = '--no-steps'; Tip = 'Show only the final evaluated result' }
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

    # Subcommand: rcmake diff-config
    if ($first -eq 'diff-config' -and $inSubcommand) {
        if ($wordToComplete -like '--only=*') {
            return & $matchCsvValues $diffSections '--only=' $wordToComplete.Substring(7)
        }
        if ($wordToComplete -like '--cache=*') {
            return & $matchValues @('key', 'all', 'none') '--cache=' $wordToComplete.Substring(8)
        }
        if ($wordToComplete -like '--color=*') {
            return & $matchValues @('auto', 'always', 'never') '--color=' $wordToComplete.Substring(8)
        }
        if ($wordToComplete -like '--config=*') {
            return & $matchValues $configs '--config=' $wordToComplete.Substring(9)
        }
        switch ($prev) {
            '--only' { return & $matchCsvValues $diffSections '' $wordToComplete }
            '--cache' { return & $matchValues @('key', 'all', 'none') '' $wordToComplete }
            '--color' { return & $matchValues @('auto', 'always', 'never') '' $wordToComplete }
            '--config' { return & $matchValues $configs '' $wordToComplete }
        }
        $diffOpts = @(
            @{ Name = '--preset'; Tip = 'Use binary directory of a configure preset' }
            @{ Name = '-S'; Tip = 'Read presets from this source directory' }
            @{ Name = '--config'; Tip = 'Compare configurations (a or a:b)' }
            @{ Name = '--target'; Tip = 'Compare only this target' }
            @{ Name = '--only'; Tip = 'Compare only comma-separated sections' }
            @{ Name = '--cache'; Tip = 'Cache entries to compare (key, all, none)' }
            @{ Name = '--no-normalize'; Tip = 'Keep absolute paths without normalization' }
            @{ Name = '--prepare'; Tip = 'Create File API query files in each directory' }
            @{ Name = '--json'; Tip = 'Write one JSON report to stdout' }
            @{ Name = '--color=auto'; Tip = 'Use terminal color automatically' }
            @{ Name = '--color=always'; Tip = 'Force terminal color' }
            @{ Name = '--color=never'; Tip = 'Disable terminal color' }
            @{ Name = '-h'; Tip = 'Show diff-config help' }
            @{ Name = '--help'; Tip = 'Show diff-config help' }
        )
        return & $matchOptions $diffOpts $wordToComplete
    }

    # Subcommand: rcmake fmt
    if ($first -eq 'fmt' -and $inSubcommand) {
        $fmtOpts = @(
            @{ Name = '--check'; Tip = 'Report files that would change without writing' }
            @{ Name = '--diff'; Tip = 'Print unified diffs without writing' }
            @{ Name = '--stdin'; Tip = 'Read from stdin and write formatted bytes to stdout' }
            @{ Name = '--stdin-filepath'; Tip = 'Resolve .rcmake.toml using this path for --stdin' }
            @{ Name = '-h'; Tip = 'Show fmt help' }
            @{ Name = '--help'; Tip = 'Show fmt help' }
        )
        return & $matchOptions $fmtOpts $wordToComplete
    }

    # Subcommand: rcmake lint
    if ($first -eq 'lint' -and $inSubcommand) {
        $lintOpts = @(
            @{ Name = '--sarif'; Tip = 'Emit SARIF 2.1.0 JSON to stdout' }
            @{ Name = '--list-rules'; Tip = 'List all built-in lint rules and default severities' }
            @{ Name = '--deny-warnings'; Tip = 'Exit with status 1 on warning findings' }
            @{ Name = '-h'; Tip = 'Show lint help' }
            @{ Name = '--help'; Tip = 'Show lint help' }
        )
        return & $matchOptions $lintOpts $wordToComplete
    }

    # Subcommand: rcmake verify
    if ($first -eq 'verify' -and $inSubcommand) {
        $seenSep = $false
        for ($i = 2; $i -lt $elements.Count; $i++) {
            if ($elements[$i] -eq '--') {
                $seenSep = $true
                break
            }
        }
        if (-not $seenSep) {
            $verifyOpts = @(
                @{ Name = '--oracle'; Tip = 'Path to the reference CMake 4.5.1 executable' }
                @{ Name = '--timeout'; Tip = 'Overall wall-clock timeout in seconds' }
                @{ Name = '--json'; Tip = 'Write one JSON report to stdout' }
                @{ Name = '-h'; Tip = 'Show verify help' }
                @{ Name = '--help'; Tip = 'Show verify help' }
                @{ Name = '--'; Tip = 'Separate verify options from configure arguments' }
            )
            return & $matchOptions $verifyOpts $wordToComplete
        }
    }

    # Subcommand: rcmake doctor
    if ($first -eq 'doctor' -and $inSubcommand) {
        $doctorOpts = @(
            @{ Name = '-B'; Tip = 'Check build directory drift' }
            @{ Name = '-h'; Tip = 'Show doctor help' }
            @{ Name = '--help'; Tip = 'Show doctor help' }
        )
        return & $matchOptions $doctorOpts $wordToComplete
    }

    # Subcommand: rcmake cache
    if ($first -eq 'cache' -and $inSubcommand) {
        if ($elements.Count -eq 2 -or ($elements.Count -eq 3 -and -not [string]::IsNullOrEmpty($wordToComplete))) {
            $cacheActions = @(
                @{ Name = 'status'; Tip = 'Show cache size and entry counts (default)' }
                @{ Name = 'dir'; Tip = 'Print the probe cache directory' }
                @{ Name = 'clear'; Tip = 'Remove cache entries, blobs, and staged candidates' }
                @{ Name = 'export'; Tip = 'Write valid probe entries to a portable archive' }
                @{ Name = 'import'; Tip = 'Read probe entries from a portable archive' }
                @{ Name = '-h'; Tip = 'Show cache help' }
                @{ Name = '--help'; Tip = 'Show cache help' }
            )
            return & $matchOptions $cacheActions $wordToComplete
        }
        if ($second -eq 'import') {
            $importOpts = @(
                @{ Name = '--sha256'; Tip = 'Expected 64-hex-digit SHA-256 digest for trusted import' }
                @{ Name = '-h'; Tip = 'Show import help' }
                @{ Name = '--help'; Tip = 'Show import help' }
            )
            return & $matchOptions $importOpts $wordToComplete
        }
        return & $matchOptions @(
            @{ Name = '-h'; Tip = 'Show cache help' }
            @{ Name = '--help'; Tip = 'Show cache help' }
        ) $wordToComplete
    }

    # Subcommand: rcmake ccmake
    if ($first -eq 'ccmake' -and $inSubcommand) {
        if ($prev -eq '-G') {
            return & $matchValues $generators '' $wordToComplete
        }
        $ccmakeOpts = @(
            @{ Name = '-S'; Tip = 'Specify source directory' }
            @{ Name = '-B'; Tip = 'Specify build directory' }
            @{ Name = '-C'; Tip = 'Pre-load initial cache script' }
            @{ Name = '-D'; Tip = 'Create or update a cache entry' }
            @{ Name = '-U'; Tip = 'Remove matching cache entries' }
            @{ Name = '-G'; Tip = 'Specify a build system generator' }
            @{ Name = '-T'; Tip = 'Specify generator toolset' }
            @{ Name = '-A'; Tip = 'Specify generator platform' }
            @{ Name = '--toolchain'; Tip = 'Specify toolchain file' }
            @{ Name = '--install-prefix'; Tip = 'Specify CMAKE_INSTALL_PREFIX' }
            @{ Name = '--project-file'; Tip = 'Specify alternate project file name' }
            @{ Name = '-h'; Tip = 'Print ccmake usage' }
            @{ Name = '--help'; Tip = 'Print ccmake usage' }
            @{ Name = '--version'; Tip = 'Print ccmake version' }
            @{ Name = '--version=json-v1'; Tip = 'Print ccmake version as JSON' }
        )
        return & $matchOptions $ccmakeOpts $wordToComplete
    }

    # Subcommand: rcmake ctest
    if ($first -eq 'ctest' -and $inSubcommand) {
        if ($wordToComplete -like '--show-only=*') {
            return & $matchValues @('human', 'json-v1', 'json-v1-raw') '--show-only=' $wordToComplete.Substring(12)
        }
        if ($wordToComplete -like '--list-presets=*') {
            return & $matchValues @('defined') '--list-presets=' $wordToComplete.Substring(15)
        }
        switch ($prev) {
            '-C' { return & $matchValues $configs '' $wordToComplete }
            '--show-only' { return & $matchValues @('human', 'json-v1', 'json-v1-raw') '' $wordToComplete }
        }
        $ctestOpts = @(
            @{ Name = '--test-dir'; Tip = 'Directory containing tests' }
            @{ Name = '-C'; Tip = 'Choose configuration to test' }
            @{ Name = '-R'; Tip = 'Include tests matching regex' }
            @{ Name = '-E'; Tip = 'Exclude tests matching regex' }
            @{ Name = '-L'; Tip = 'Include labels matching regex' }
            @{ Name = '-LE'; Tip = 'Exclude labels matching regex' }
            @{ Name = '-j'; Tip = 'Run tests in parallel' }
            @{ Name = '--parallel'; Tip = 'Run tests in parallel' }
            @{ Name = '--output-on-failure'; Tip = 'Output logs of failed tests' }
            @{ Name = '--output-junit'; Tip = 'Write JUnit XML report' }
            @{ Name = '--preset'; Tip = 'Specify a test preset' }
            @{ Name = '--presets-file'; Tip = 'Path to presets file' }
            @{ Name = '--list-presets'; Tip = 'List test presets' }
            @{ Name = '--list-presets=defined'; Tip = 'List defined test presets' }
            @{ Name = '--rerun-failed'; Tip = 'Rerun previously failed tests' }
            @{ Name = '--stop-on-failure'; Tip = 'Stop after first failure' }
            @{ Name = '--repeat'; Tip = 'Repeat tests (mode:n)' }
            @{ Name = '--timeout'; Tip = 'Default test timeout in seconds' }
            @{ Name = '-N'; Tip = 'Disable execution and show tests' }
            @{ Name = '--show-only=human'; Tip = 'Show tests in human format' }
            @{ Name = '--show-only=json-v1'; Tip = 'Show tests in json-v1 format' }
            @{ Name = '--show-only=json-v1-raw'; Tip = 'Show tests in raw json-v1 format' }
            @{ Name = '-S'; Tip = 'Run a CTest dashboard script' }
            @{ Name = '-V'; Tip = 'Enable verbose output' }
            @{ Name = '-VV'; Tip = 'Enable extra verbose output' }
            @{ Name = '-Q'; Tip = 'Make CTest quiet' }
            @{ Name = '--version'; Tip = 'Print CTest version' }
            @{ Name = '-h'; Tip = 'Print CTest usage' }
            @{ Name = '--help'; Tip = 'Print CTest usage' }
        )
        return & $matchOptions $ctestOpts $wordToComplete
    }

    # Subcommand: rcmake cpack
    if ($first -eq 'cpack' -and $inSubcommand) {
        if ($wordToComplete -like '--list-presets=*') {
            return & $matchValues @('defined') '--list-presets=' $wordToComplete.Substring(15)
        }
        switch ($prev) {
            '-G' { return & $matchValues $cpackGenerators '' $wordToComplete }
            '-C' { return & $matchValues $configs '' $wordToComplete }
        }
        $cpackOpts = @(
            @{ Name = '-G'; Tip = 'Override CPACK_GENERATOR' }
            @{ Name = '-C'; Tip = 'Select build configurations' }
            @{ Name = '-D'; Tip = 'Set a CPack variable' }
            @{ Name = '--config'; Tip = 'Select CPack config file' }
            @{ Name = '-P'; Tip = 'Set package name' }
            @{ Name = '-R'; Tip = 'Set package version' }
            @{ Name = '-B'; Tip = 'Set package output directory' }
            @{ Name = '--vendor'; Tip = 'Set package vendor' }
            @{ Name = '--preset'; Tip = 'Use a package preset' }
            @{ Name = '--presets-file'; Tip = 'Select presets file' }
            @{ Name = '--list-presets'; Tip = 'List package presets' }
            @{ Name = '--list-presets=defined'; Tip = 'List defined package presets' }
            @{ Name = '-V'; Tip = 'Enable verbose output' }
            @{ Name = '--verbose'; Tip = 'Enable verbose output' }
            @{ Name = '--debug'; Tip = 'Enable debug output' }
            @{ Name = '--trace'; Tip = 'Trace CMake scripts' }
            @{ Name = '--trace-expand'; Tip = 'Trace CMake scripts with variable expansion' }
            @{ Name = '--version'; Tip = 'Print CPack version' }
            @{ Name = '-h'; Tip = 'Print CPack usage' }
            @{ Name = '--help'; Tip = 'Print CPack usage' }
        )
        return & $matchOptions $cpackOpts $wordToComplete
    }

    # Subcommand: rcmake lsp
    if ($first -eq 'lsp' -and $inSubcommand) {
        $lspOpts = @(
            @{ Name = '--stdio'; Tip = 'Communicate over standard input and standard output' }
            @{ Name = '--clientProcessId='; Tip = 'Parent client process identifier' }
            @{ Name = '-h'; Tip = 'Print language server help' }
            @{ Name = '--help'; Tip = 'Print language server help' }
            @{ Name = '-V'; Tip = 'Print language server version' }
            @{ Name = '--version'; Tip = 'Print language server version' }
        )
        return & $matchOptions $lspOpts $wordToComplete
    }

    # Mode: rcmake --build
    if ($first -eq '--build' -and $inSubcommand) {
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
    if ($first -eq '--install' -and $inSubcommand) {
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
    if ($first -eq '--workflow' -and $inSubcommand) {
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
        @{ Name = 'explain'; Tip = 'Explain targets, variables, find_package, or genex' }
        @{ Name = 'diff-config'; Tip = 'Compare two configured build directories' }
        @{ Name = 'fmt'; Tip = 'Losslessly format CMake listfiles' }
        @{ Name = 'lint'; Tip = 'Run static lint rules on CMake listfiles' }
        @{ Name = 'verify'; Tip = 'Verify configure parity against CMake 4.5.1' }
        @{ Name = 'doctor'; Tip = 'Inspect tools, probe cache, and build drift' }
        @{ Name = 'cache'; Tip = 'Manage the compiler probe cache' }
        @{ Name = 'ccmake'; Tip = 'Start the interactive cache editor TUI (Unix)' }
        @{ Name = 'ctest'; Tip = 'Run the bundled CTest driver' }
        @{ Name = 'cpack'; Tip = 'Run the bundled CPack driver' }
        @{ Name = 'lsp'; Tip = 'Start the CMake language server over stdio' }
        @{ Name = '--diagnostics=cmake'; Tip = 'Use CMake diagnostic format' }
        @{ Name = '--diagnostics=rich'; Tip = 'Use rich terminal diagnostics' }
        @{ Name = '--profile'; Tip = 'Write configure profile to <build>/rcmake-profile' }
        @{ Name = '--profile='; Tip = 'Write configure profile to specified directory' }
        @{ Name = '--no-probe-cache'; Tip = 'Disable compiler probe cache for this configure' }
        @{ Name = '--export-compile-commands'; Tip = 'Export compile_commands.json (including multi-config files)' }
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
        @{ Name = '--open'; Tip = 'Open generated project in associated application' }
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
        @{ Name = '--debugger'; Tip = 'Enable the Debug Adapter Protocol server (Unix)' }
        @{ Name = '--debugger-pipe='; Tip = 'Unix domain socket path for the debugger' }
        @{ Name = '--debugger-dap-log='; Tip = 'Write DAP traffic log to file' }
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
