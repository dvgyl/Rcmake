# SPDX-License-Identifier: MIT OR Apache-2.0
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$Prefix = $(if ($env:LOCALAPPDATA) { "$env:LOCALAPPDATA\rcmake" } else { '' }),
    [string]$Archive = '',
    [switch]$AliasCmake,
    [switch]$Force,
    [switch]$Uninstall,
    [switch]$Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Show-Usage {
    $usage = @'
Usage: install.ps1 [-Version <version>] [-Prefix <dir>] [-Archive <path>] [-AliasCmake] [-Force]
       install.ps1 -Uninstall [-Prefix <dir>]

Install rcmake.exe and rcmake-lsp.exe into <Prefix>\bin, the bundled CMake 4.5.1
data into <Prefix>\share\cmake-4.5, and the PowerShell completion script into
<Prefix>\share\rcmake\completions\rcmake.ps1 when present in the archive.
The default Prefix is %LOCALAPPDATA%\rcmake.

Options:
  -Version <ver>     Release version to download, or "latest" (default: latest).
  -Prefix <dir>      Installation root (default: %LOCALAPPDATA%\rcmake).
  -Archive <path>    Install a local .zip archive. The script checks
                     <path>.sha256 if that file exists.
  -AliasCmake        Also create cmake.exe, ctest.exe, and cpack.exe aliases
                     from rcmake.exe (hardlink, falling back to copy).
  -Force             Replace existing cmake.exe, ctest.exe, or cpack.exe files.
  -Uninstall         Remove rcmake.exe, rcmake-lsp.exe, rcmake-owned aliases,
                     share\cmake-4.5, and share\rcmake\completions\rcmake.ps1
                     from Prefix.
  -Help              Show this help.

Requirements: Windows PowerShell 5.1 or PowerShell 7+.
'@
    Write-Host $usage
}

function Stop-Install {
    param([Parameter(Mandatory)][string]$Message)
    throw "install.ps1: $Message"
}

function Resolve-FullPath {
    param([Parameter(Mandatory)][string]$Path)
    if ([IO.Path]::IsPathRooted($Path)) {
        return [IO.Path]::GetFullPath($Path)
    }
    return [IO.Path]::GetFullPath([IO.Path]::Combine((Get-Location).ProviderPath, $Path))
}

function Get-ItemOrNull {
    param([Parameter(Mandatory)][string]$Path)
    try {
        return Get-Item -LiteralPath $Path -Force -ErrorAction Stop
    } catch {
        return $null
    }
}

function Get-FileSha256OrNull {
    param([Parameter(Mandatory)][string]$Path)
    $item = Get-ItemOrNull -Path $Path
    if ($null -eq $item -or $item.PSIsContainer) {
        return $null
    }
    try {
        return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
    } catch {
        return $null
    }
}

function Test-RcmakeAlias {
    param(
        [Parameter(Mandatory)][string]$AliasPath,
        [Parameter(Mandatory)][string]$RcmakePath,
        [AllowNull()][string]$RcmakeHash
    )
    $item = Get-ItemOrNull -Path $AliasPath
    if ($null -eq $item) {
        return $false
    }
    $isReparse = [bool]($item.Attributes -band [IO.FileAttributes]::ReparsePoint)
    if ($item.PSIsContainer -and -not $isReparse) {
        return $false
    }
    if ($isReparse -and $null -ne $item.PSObject.Properties['Target']) {
        $targets = @($item.Target)
        $rcmakeFull = [IO.Path]::GetFullPath($RcmakePath)
        foreach ($target in $targets) {
            if ($null -eq $target) {
                continue
            }
            $targetText = $target.ToString()
            if ($targetText -ieq 'rcmake.exe' -or $targetText -ieq '.\rcmake.exe') {
                return $true
            }
            try {
                $resolvedTarget = if ([IO.Path]::IsPathRooted($targetText)) {
                    [IO.Path]::GetFullPath($targetText)
                } else {
                    [IO.Path]::GetFullPath([IO.Path]::Combine((Split-Path -Parent $AliasPath), $targetText))
                }
                if ($resolvedTarget -ieq $rcmakeFull) {
                    return $true
                }
            } catch {
                # Ignore non-path reparse targets.
            }
        }
    }
    if ($item.PSIsContainer -or [string]::IsNullOrEmpty($RcmakeHash)) {
        return $false
    }
    $aliasHash = Get-FileSha256OrNull -Path $AliasPath
    return (-not [string]::IsNullOrEmpty($aliasHash)) -and ($aliasHash -ceq $RcmakeHash)
}

function Install-AliasExecutable {
    param(
        [Parameter(Mandatory)][string]$SourcePath,
        [Parameter(Mandatory)][string]$AliasPath
    )
    $existing = Get-ItemOrNull -Path $AliasPath
    if ($null -ne $existing) {
        Remove-Item -LiteralPath $AliasPath -Force
    }
    try {
        New-Item -ItemType HardLink -Path $AliasPath -Value $SourcePath -Force | Out-Null
        return 'hardlink'
    } catch {
        Copy-Item -LiteralPath $SourcePath -Destination $AliasPath -Force
        return 'copy'
    }
}

function Enable-Tls12 {
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        try {
            [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        } catch {
            # Ignore when the runtime already enforces modern TLS defaults.
        }
    }
}

function Invoke-Download {
    param(
        [Parameter(Mandatory)][string]$Uri,
        [Parameter(Mandatory)][string]$Destination
    )
    Enable-Tls12
    $previousProgress = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'
    try {
        Invoke-WebRequest -Uri $Uri -OutFile $Destination -UseBasicParsing -TimeoutSec 600
    } finally {
        $ProgressPreference = $previousProgress
    }
}

function Read-ChecksumEntries {
    param([Parameter(Mandatory)][string]$ChecksumPath)
    $entries = @()
    foreach ($line in @(Get-Content -LiteralPath $ChecksumPath -Encoding Ascii)) {
        if ($line -match '^\s*([0-9a-fA-F]{64})\s+\*?(.+?)\s*$') {
            $entries += [pscustomobject]@{
                Hash = $Matches[1].ToLowerInvariant()
                Name = $Matches[2]
            }
        }
    }
    return ,$entries
}

function Assert-ArchiveChecksum {
    param(
        [Parameter(Mandatory)][string]$ArchivePath,
        [Parameter(Mandatory)][string]$ChecksumPath
    )
    $archiveName = [IO.Path]::GetFileName($ArchivePath)
    $entries = Read-ChecksumEntries -ChecksumPath $ChecksumPath
    $expected = @($entries | Where-Object { $_.Name -ceq $archiveName } | ForEach-Object { $_.Hash })
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $ArchivePath).Hash.ToLowerInvariant()
    if (@($expected).Count -ne 1 -or $expected[0] -cne $actual) {
        Stop-Install "SHA-256 mismatch for $archiveName."
    }
    Write-Host "Verified the SHA-256 checksum of $archiveName."
}

function Expand-RcmakeArchive {
    param(
        [Parameter(Mandatory)][string]$ArchivePath,
        [Parameter(Mandatory)][string]$StageDir
    )
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem

    $fullStage = [IO.Path]::GetFullPath($StageDir)
    if (-not $fullStage.EndsWith([IO.Path]::DirectorySeparatorChar)) {
        $fullStage += [IO.Path]::DirectorySeparatorChar
    }

    $top = $null
    $zip = [IO.Compression.ZipFile]::OpenRead($ArchivePath)
    try {
        foreach ($entry in $zip.Entries) {
            $rawName = $entry.FullName
            if ([string]::IsNullOrEmpty($rawName) -or $rawName.IndexOf([char]0) -ge 0) {
                Stop-Install "Invalid archive path '$rawName'."
            }
            if ($rawName.StartsWith('/') -or $rawName.StartsWith('\') -or $rawName -match '^[A-Za-z]:') {
                Stop-Install "Invalid archive path '$rawName'."
            }

            $isDir = $rawName.EndsWith('/') -or $rawName.EndsWith('\')
            $trimmed = $rawName.TrimEnd('/', '\')
            $parts = @($trimmed -split '[\\/]')
            if (@($parts).Count -eq 0) {
                Stop-Install "Invalid archive path '$rawName'."
            }
            foreach ($part in $parts) {
                if ([string]::IsNullOrEmpty($part) -or $part -eq '.' -or $part -eq '..' -or $part.IndexOf(':') -ge 0) {
                    Stop-Install "Invalid archive path '$rawName'."
                }
            }

            if ($null -eq $top) {
                $top = $parts[0]
            }
            if ($parts[0] -cne $top) {
                Stop-Install 'The archive must have one top-level directory.'
            }
            if (@($parts).Count -eq 1) {
                if (-not $isDir) {
                    Stop-Install 'The archive must have one top-level directory.'
                }
                continue
            }

            $relParts = @($parts[1..($parts.Length - 1)])
            $relPosix = $relParts -join '/'
            $isBinary = ($relPosix -ceq 'bin/rcmake.exe') -or ($relPosix -ceq 'bin/rcmake-lsp.exe')
            $isData = ($relPosix -ceq 'share/cmake-4.5') -or $relPosix.StartsWith('share/cmake-4.5/', [StringComparison]::Ordinal)
            $isCompletion = ($relPosix -ceq 'share/rcmake/completions/rcmake.ps1')
            if (-not ($isBinary -or $isData -or $isCompletion)) {
                continue
            }
            if (($isBinary -or $isCompletion) -and $isDir) {
                Stop-Install "Invalid archive entry '$rawName'."
            }

            $relNative = $relParts -join [IO.Path]::DirectorySeparatorChar
            $destPath = [IO.Path]::GetFullPath((Join-Path $StageDir $relNative))
            if (-not $destPath.StartsWith($fullStage, [StringComparison]::OrdinalIgnoreCase)) {
                Stop-Install "Invalid archive path '$rawName'."
            }

            if ($isDir) {
                New-Item -ItemType Directory -Path $destPath -Force | Out-Null
            } else {
                $parentDir = Split-Path -Parent $destPath
                if (-not (Test-Path -LiteralPath $parentDir)) {
                    New-Item -ItemType Directory -Path $parentDir -Force | Out-Null
                }
                if (Test-Path -LiteralPath $destPath) {
                    Stop-Install "Duplicate archive entry '$rawName'."
                }
                $source = $entry.Open()
                try {
                    $destination = [IO.File]::Open($destPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
                    try {
                        $source.CopyTo($destination)
                    } finally {
                        $destination.Dispose()
                    }
                } finally {
                    $source.Dispose()
                }
            }
        }
    } finally {
        $zip.Dispose()
    }
}

function Normalize-PathEntry {
    param([AllowNull()][string]$Entry)
    if ([string]::IsNullOrWhiteSpace($Entry)) {
        return ''
    }
    $trimmed = $Entry.Trim().Trim('"')
    if ([string]::IsNullOrWhiteSpace($trimmed)) {
        return ''
    }
    try {
        return [IO.Path]::GetFullPath($trimmed).TrimEnd('\', '/')
    } catch {
        return $trimmed.TrimEnd('\', '/')
    }
}

if ($Help) {
    Show-Usage
    return
}

if ([string]::IsNullOrWhiteSpace($Prefix)) {
    Stop-Install 'Specify -Prefix when LOCALAPPDATA is not set.'
}

$prefixFull = Resolve-FullPath -Path $Prefix
$binDir = Join-Path $prefixFull 'bin'
$dataDir = Join-Path (Join-Path $prefixFull 'share') 'cmake-4.5'
$completionDir = Join-Path (Join-Path (Join-Path $prefixFull 'share') 'rcmake') 'completions'
$completionPath = Join-Path $completionDir 'rcmake.ps1'
$rcmakePath = Join-Path $binDir 'rcmake.exe'
$lspPath = Join-Path $binDir 'rcmake-lsp.exe'
$tools = @('cmake.exe', 'ctest.exe', 'cpack.exe')

if ($Uninstall) {
    if ($Archive -or $AliasCmake -or $Force -or $PSBoundParameters.ContainsKey('Version')) {
        Stop-Install 'Use -Uninstall alone (optionally with -Prefix).'
    }

    $removed = @()
    $rcmakeHash = Get-FileSha256OrNull -Path $rcmakePath
    foreach ($tool in $tools) {
        $aliasPath = Join-Path $binDir $tool
        if (Test-RcmakeAlias -AliasPath $aliasPath -RcmakePath $rcmakePath -RcmakeHash $rcmakeHash) {
            Remove-Item -LiteralPath $aliasPath -Force
            $removed += $aliasPath
        }
    }

    foreach ($binaryPath in @($rcmakePath, $lspPath)) {
        $item = Get-ItemOrNull -Path $binaryPath
        if ($null -ne $item -and -not $item.PSIsContainer) {
            Remove-Item -LiteralPath $binaryPath -Force
            $removed += $binaryPath
        }
    }

    if (Test-Path -LiteralPath $dataDir -PathType Container) {
        $cmakePath = Join-Path $binDir 'cmake.exe'
        if ($null -ne (Get-ItemOrNull -Path $cmakePath)) {
            Write-Host "Kept $dataDir because $cmakePath is another CMake installation."
        } else {
            Remove-Item -LiteralPath $dataDir -Recurse -Force
            $removed += $dataDir
        }
    }

    $completionItem = Get-ItemOrNull -Path $completionPath
    if ($null -ne $completionItem -and -not $completionItem.PSIsContainer -and -not ($completionItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        Remove-Item -LiteralPath $completionPath -Force
        $removed += $completionPath
        foreach ($dirToPrune in @($completionDir, (Split-Path -Parent $completionDir))) {
            if ((Test-Path -LiteralPath $dirToPrune -PathType Container) -and (@(Get-ChildItem -LiteralPath $dirToPrune -Force).Count -eq 0)) {
                Remove-Item -LiteralPath $dirToPrune -Force
            }
        }
    }

    if (@($removed).Count -eq 0) {
        Write-Host "Found no rcmake installation in $prefixFull."
    } else {
        Write-Host 'Removed:'
        foreach ($item in $removed) {
            Write-Host "  $item"
        }
    }

    $cacheDir = if ($env:RCMAKE_CACHE_DIR) {
        $env:RCMAKE_CACHE_DIR
    } elseif ($env:LOCALAPPDATA) {
        Join-Path (Join-Path $env:LOCALAPPDATA 'rcmake') 'cache'
    } else {
        ''
    }
    if ($cacheDir -and (Test-Path -LiteralPath $cacheDir -PathType Container)) {
        Write-Host "The probe and fetch cache stays in $cacheDir. Delete it to free disk space."
    }
    return
}

if ($Archive -and $PSBoundParameters.ContainsKey('Version')) {
    Stop-Install 'Use either -Archive or -Version, not both.'
}

# Pre-check all alias collisions before downloading or writing anything.
# A partial alias set would run a mix of rcmake and another CMake.
$existingRcmakeHash = Get-FileSha256OrNull -Path $rcmakePath
$existingOwnedAliases = @()
foreach ($tool in $tools) {
    $aliasPath = Join-Path $binDir $tool
    if (Test-RcmakeAlias -AliasPath $aliasPath -RcmakePath $rcmakePath -RcmakeHash $existingRcmakeHash) {
        $existingOwnedAliases += $tool
    }
}

if ($AliasCmake) {
    $blocked = @()
    foreach ($tool in $tools) {
        $aliasPath = Join-Path $binDir $tool
        $item = Get-ItemOrNull -Path $aliasPath
        if ($null -eq $item) {
            continue
        }
        $isReparse = [bool]($item.Attributes -band [IO.FileAttributes]::ReparsePoint)
        if ($item.PSIsContainer -and -not $isReparse) {
            Stop-Install "$aliasPath is a directory. Remove it, then run the installer again."
        }
        if ($existingOwnedAliases -contains $tool) {
            continue
        }
        $blocked += $aliasPath
    }
    if (@($blocked).Count -gt 0 -and -not $Force) {
        $lines = @(
            'These files exist and are not owned by rcmake:'
        ) + @($blocked | ForEach-Object { "  $_" }) + @(
            'Run again with -Force to replace them, or omit -AliasCmake.'
            'The installer changed nothing.'
        )
        Stop-Install ($lines -join [Environment]::NewLine)
    }
}

$target = 'x86_64-pc-windows-msvc'
$releases = 'https://github.com/dvgyl/rcmake/releases'
$versionPattern = '^[0-9]+\.[0-9]+\.[0-9]+([-+][A-Za-z0-9.+-]+)?$'

if (-not $Archive -and $Version -ne 'latest') {
    $cleanVersionCheck = $Version -replace '^v', ''
    if ($cleanVersionCheck -notmatch $versionPattern) {
        Stop-Install "Invalid version '$Version'."
    }
}

$downloadWork = Join-Path ([IO.Path]::GetTempPath()) ("rcmake-download-" + [Guid]::NewGuid().ToString('N'))
$stageWork = $null
try {
    $archivePath = ''
    $checksumPath = ''

    if ($Archive) {
        $archivePath = Resolve-FullPath -Path $Archive
        if (-not (Test-Path -LiteralPath $archivePath -PathType Leaf)) {
            Stop-Install "$archivePath does not exist."
        }
        $sidecar = "$archivePath.sha256"
        if (Test-Path -LiteralPath $sidecar -PathType Leaf) {
            $checksumPath = $sidecar
        } else {
            Write-Warning "$sidecar does not exist. The installer cannot verify the SHA-256 checksum of this archive."
        }
    } else {
        New-Item -ItemType Directory -Path $downloadWork -Force | Out-Null
        if ($Version -eq 'latest') {
            $checksumUrl = "$releases/latest/download/SHA256SUMS.txt"
            $checksumPath = Join-Path $downloadWork 'SHA256SUMS.txt'
            Write-Host 'Download the latest release checksums.'
            try {
                Invoke-Download -Uri $checksumUrl -Destination $checksumPath
            } catch {
                Stop-Install "Could not download $checksumUrl ($($_.Exception.Message)). If no release is published yet, clone the repository and build from source with scripts/stage.sh (or scripts/install.sh --from-source on Linux or macOS)."
            }
            $archiveRegex = "^rcmake-[0-9]+\.[0-9]+\.[0-9]+([-+][A-Za-z0-9.+-]+)?-$([regex]::Escape($target))\.zip$"
            $entries = Read-ChecksumEntries -ChecksumPath $checksumPath
            $matched = @($entries | Where-Object { $_.Name -cmatch $archiveRegex })
            if (@($matched).Count -ne 1) {
                Stop-Install "No unique release archive for $target."
            }
            $archiveName = $matched[0].Name
            $archiveUrl = "$releases/latest/download/$archiveName"
        } else {
            $cleanVersion = $Version -replace '^v', ''
            $tag = "v$cleanVersion"
            $archiveName = "rcmake-$cleanVersion-$target.zip"
            $archiveUrl = "$releases/download/$tag/$archiveName"
            $sidecarUrl = "$archiveUrl.sha256"
            $sumsUrl = "$releases/download/$tag/SHA256SUMS.txt"
            $checksumPath = Join-Path $downloadWork "$archiveName.sha256"
            Write-Host "Download release checksums for $tag."
            try {
                Invoke-Download -Uri $sidecarUrl -Destination $checksumPath
            } catch {
                $checksumPath = Join-Path $downloadWork 'SHA256SUMS.txt'
                try {
                    Invoke-Download -Uri $sumsUrl -Destination $checksumPath
                } catch {
                    Stop-Install "Could not download $sidecarUrl or $sumsUrl ($($_.Exception.Message)). If no release is published yet, clone the repository and build from source with scripts/stage.sh (or scripts/install.sh --from-source on Linux or macOS)."
                }
            }
        }

        $archivePath = Join-Path $downloadWork $archiveName
        Write-Host "Download $archiveName."
        try {
            Invoke-Download -Uri $archiveUrl -Destination $archivePath
        } catch {
            Stop-Install "Could not download $archiveUrl ($($_.Exception.Message)). If no release is published yet, clone the repository and build from source with scripts/stage.sh (or scripts/install.sh --from-source on Linux or macOS)."
        }
    }

    if ($checksumPath) {
        Assert-ArchiveChecksum -ArchivePath $archivePath -ChecksumPath $checksumPath
    }

    New-Item -ItemType Directory -Path $prefixFull -Force | Out-Null
    $stageWork = Join-Path $prefixFull ('.rcmake-install-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stageWork -Force | Out-Null

    Expand-RcmakeArchive -ArchivePath $archivePath -StageDir $stageWork

    $stagedBin = Join-Path (Join-Path $stageWork 'bin') 'rcmake.exe'
    if (-not (Test-Path -LiteralPath $stagedBin -PathType Leaf)) {
        Stop-Install 'Missing bin/rcmake.exe in the archive.'
    }
    $stagedLsp = Join-Path (Join-Path $stageWork 'bin') 'rcmake-lsp.exe'
    $hasLsp = Test-Path -LiteralPath $stagedLsp -PathType Leaf
    $stagedCompletion = Join-Path (Join-Path (Join-Path (Join-Path $stageWork 'share') 'rcmake') 'completions') 'rcmake.ps1'
    $hasCompletion = Test-Path -LiteralPath $stagedCompletion -PathType Leaf

    $stagedData = Join-Path (Join-Path $stageWork 'share') 'cmake-4.5'
    foreach ($required in @('Modules\CMake.cmake', 'Templates\CPackConfig.cmake.in')) {
        if (-not (Test-Path -LiteralPath (Join-Path $stagedData $required) -PathType Leaf)) {
            Stop-Install "Missing $required in the archive."
        }
    }

    $null = & $stagedBin --version
    if ($LASTEXITCODE -ne 0) {
        Stop-Install "Staged rcmake.exe failed --version check (exit code $LASTEXITCODE)."
    }

    New-Item -ItemType Directory -Path $binDir -Force | Out-Null
    New-Item -ItemType Directory -Path (Split-Path -Parent $dataDir) -Force | Out-Null

    foreach ($checkTarget in @(
        @{ Path = $rcmakePath; Present = $true },
        @{ Path = $lspPath; Present = $hasLsp },
        @{ Path = $completionPath; Present = $hasCompletion }
    )) {
        if ($checkTarget.Present) {
            $existing = Get-ItemOrNull -Path $checkTarget.Path
            if ($null -ne $existing -and $existing.PSIsContainer -and -not ($existing.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                Stop-Install "$($checkTarget.Path) is a directory. Remove it, then run the installer again."
            }
        }
    }

    $backupData = Join-Path $stageWork 'previous-data'
    $hadPreviousData = Test-Path -LiteralPath $dataDir
    if ($hadPreviousData) {
        Move-Item -LiteralPath $dataDir -Destination $backupData
    }
    try {
        Move-Item -LiteralPath $stagedData -Destination $dataDir
        Move-Item -LiteralPath $stagedBin -Destination $rcmakePath -Force
    } catch {
        if (Test-Path -LiteralPath $dataDir) {
            Move-Item -LiteralPath $dataDir -Destination $stagedData
        }
        if ($hadPreviousData -and (Test-Path -LiteralPath $backupData)) {
            Move-Item -LiteralPath $backupData -Destination $dataDir
        }
        throw
    }

    $installed = @($rcmakePath)
    if ($hasLsp) {
        Move-Item -LiteralPath $stagedLsp -Destination $lspPath -Force
        $installed += $lspPath
    }

    $aliasesToWrite = if ($AliasCmake) { $tools } else { $existingOwnedAliases }
    foreach ($tool in $aliasesToWrite) {
        $aliasPath = Join-Path $binDir $tool
        $mode = Install-AliasExecutable -SourcePath $rcmakePath -AliasPath $aliasPath
        $installed += "$aliasPath -> rcmake.exe ($mode)"
    }
    $installed += $dataDir
    if ($hasCompletion) {
        New-Item -ItemType Directory -Path $completionDir -Force | Out-Null
        Move-Item -LiteralPath $stagedCompletion -Destination $completionPath -Force
        $installed += $completionPath
    }

    $versionOutput = & $rcmakePath --version
    if ($LASTEXITCODE -ne 0) {
        Stop-Install "Installed rcmake.exe failed --version check (exit code $LASTEXITCODE)."
    }
    $displayName = 'rcmake'
    foreach ($line in @($versionOutput | ForEach-Object { $_.ToString() })) {
        if ($line.ToLowerInvariant().StartsWith('rcmake ')) {
            $displayName = $line.ToLowerInvariant()
            break
        }
    }
    Write-Host "Installed $displayName into ${prefixFull}:"
    foreach ($item in $installed) {
        Write-Host "  $item"
    }
} finally {
    if ($stageWork -and (Test-Path -LiteralPath $stageWork)) {
        Remove-Item -LiteralPath $stageWork -Recurse -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $downloadWork) {
        Remove-Item -LiteralPath $downloadWork -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$binNormalized = Normalize-PathEntry -Entry $binDir
$onPath = $false
if ($env:Path) {
    foreach ($entry in @($env:Path -split ';')) {
        if ((Normalize-PathEntry -Entry $entry) -ieq $binNormalized) {
            $onPath = $true
            break
        }
    }
}

$escapedBin = $binDir.Replace("'", "''")
$addPathCommand = "[Environment]::SetEnvironmentVariable('Path', '$escapedBin;' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
if (-not $onPath) {
    Write-Host ''
    Write-Host "$binDir is not on PATH. Run this PowerShell command to add it to your user PATH, then open a new terminal:"
    Write-Host "  $addPathCommand"
} elseif ($AliasCmake -or (@($existingOwnedAliases).Count -gt 0)) {
    $firstCmake = Get-Command cmake -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $firstCmake) {
        $firstDir = Normalize-PathEntry -Entry (Split-Path -Parent $firstCmake.Source)
        if ($firstDir -ine $binNormalized) {
            Write-Host ''
            Write-Host "$($firstCmake.Source) comes before $(Join-Path $binDir 'cmake.exe') on PATH. Put $binDir first to use rcmake as cmake:"
            Write-Host "  $addPathCommand"
        }
    }
}
