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

Install rcmake.exe, rcmake-lsp.exe, and rcmake-verify.exe into <Prefix>\bin, the bundled CMake 4.5.1
data into <Prefix>\share\cmake-4.5, licences and third-party notices into
<Prefix>\share\doc\rcmake, and the PowerShell completion script into
<Prefix>\share\rcmake\completions\rcmake.ps1 when present in the archive.
The default Prefix is %LOCALAPPDATA%\rcmake.

Options:
  -Version <ver>     Release version to download, or "latest" (default: latest).
  -Prefix <dir>      Installation root (default: %LOCALAPPDATA%\rcmake).
  -Archive <path>    Install a local .zip archive. The script checks
                     <path>.sha256 if that file exists.
  -AliasCmake        Also create cmake.exe, ctest.exe, and cpack.exe aliases
                     from rcmake.exe (hardlink, falling back to copy).
  -Force             Allow replacement of unmarked share\cmake-4.5 data.
                     With -AliasCmake, also replace existing aliases.
  -Uninstall         Remove rcmake.exe, rcmake-lsp.exe, rcmake-verify.exe, rcmake-owned aliases,
                     managed share\cmake-4.5 data, share\doc\rcmake, and
                     share\rcmake\completions\rcmake.ps1 from Prefix.
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
        if ($archiveName -cmatch '^rcmake-(\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.+-]+)?)-(?:x86_64|aarch64)-pc-windows-msvc\.zip$') {
            Stop-Install "SHA-256 mismatch for release v$($Matches[1]), archive $archiveName."
        }
        Stop-Install "SHA-256 mismatch for archive $archiveName."
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
            $isBinary = ($relPosix -ceq 'bin/rcmake.exe') -or ($relPosix -ceq 'bin/rcmake-lsp.exe') -or ($relPosix -ceq 'bin/rcmake-verify.exe')
            $isData = ($relPosix -ceq 'share/cmake-4.5') -or $relPosix.StartsWith('share/cmake-4.5/', [StringComparison]::Ordinal)
            $isCompletion = ($relPosix -ceq 'share/rcmake/completions/rcmake.ps1')
            $isDocFile = ($relPosix -ceq 'LICENSE-APACHE') -or ($relPosix -ceq 'LICENSE-MIT') -or ($relPosix -ceq 'THIRD_PARTY_NOTICES.md')
            $isLicenceDir = ($relPosix -ceq 'third_party/licences') -or $relPosix.StartsWith('third_party/licences/', [StringComparison]::Ordinal)
            if (-not ($isBinary -or $isData -or $isCompletion -or $isDocFile -or $isLicenceDir)) {
                continue
            }
            if (($isBinary -or $isCompletion -or $isDocFile) -and $isDir) {
                Stop-Install "Invalid archive entry '$rawName'."
            }

            $destParts = if ($isDocFile -or $isLicenceDir) { @('share', 'doc', 'rcmake') + $relParts } else { $relParts }
            $relNative = $destParts -join [IO.Path]::DirectorySeparatorChar
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

function Get-SafeDocRelParts {
    param([AllowNull()][string]$Line)
    if ([string]::IsNullOrWhiteSpace($Line)) {
        return $null
    }
    $trimmed = $Line.Trim()
    if ($trimmed.IndexOf([char]0) -ge 0 -or $trimmed.StartsWith('/') -or $trimmed.StartsWith('\') -or $trimmed -match '^[A-Za-z]:') {
        return $null
    }
    $parts = @($trimmed -split '[\\/]')
    if (@($parts).Count -eq 0) {
        return $null
    }
    foreach ($part in $parts) {
        if ([string]::IsNullOrEmpty($part) -or $part -eq '.' -or $part -eq '..' -or $part.IndexOf(':') -ge 0) {
            return $null
        }
    }
    return ,$parts
}

function Read-DocManifestEntries {
    param([Parameter(Mandatory)][string]$ManifestPath)
    $entries = @()
    $item = Get-ItemOrNull -Path $ManifestPath
    if ($null -eq $item -or $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        return ,$entries
    }
    foreach ($line in @(Get-Content -LiteralPath $ManifestPath -Encoding UTF8)) {
        $parts = Get-SafeDocRelParts -Line $line
        if ($null -ne $parts) {
            $entries += ($parts -join '/')
        }
    }
    return ,$entries
}

function Assert-DirectoryAncestors {
    param(
        [Parameter(Mandatory)][string]$TargetPath,
        [Parameter(Mandatory)][string]$PrefixRoot,
        [AllowNull()][string]$RejectReparseUnder = $null
    )
    $prefixNorm = [IO.Path]::GetFullPath($PrefixRoot).TrimEnd('\', '/')
    $reparseNorm = if ($RejectReparseUnder) { [IO.Path]::GetFullPath($RejectReparseUnder).TrimEnd('\', '/') } else { $null }
    $ancestors = @()
    $cur = Split-Path -Parent $TargetPath
    while ($cur) {
        $curNorm = [IO.Path]::GetFullPath($cur).TrimEnd('\', '/')
        if ($curNorm -ieq $prefixNorm) {
            break
        }
        $ancestors += $curNorm
        $next = Split-Path -Parent $curNorm
        if (-not $next -or $next -ieq $curNorm) {
            break
        }
        $cur = $next
    }
    [Array]::Reverse($ancestors)
    foreach ($dirPath in $ancestors) {
        $item = Get-ItemOrNull -Path $dirPath
        if ($null -eq $item) {
            continue
        }
        $isReparse = [bool]($item.Attributes -band [IO.FileAttributes]::ReparsePoint)
        if ($reparseNorm -and ($dirPath -ieq $reparseNorm -or $dirPath.StartsWith($reparseNorm + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) -and $isReparse) {
            Stop-Install "$dirPath is a symlink or reparse point. Remove it, then run the installer again."
        }
        if (-not $item.PSIsContainer) {
            Stop-Install "$dirPath exists and is not a directory. Remove it, then run the installer again."
        }
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
$dataMarkerName = '.rcmake-managed'
$docDir = Join-Path (Join-Path (Join-Path $prefixFull 'share') 'doc') 'rcmake'
$docManifestPath = Join-Path $docDir '.rcmake-manifest'
$completionDir = Join-Path (Join-Path (Join-Path $prefixFull 'share') 'rcmake') 'completions'
$completionPath = Join-Path $completionDir 'rcmake.ps1'
$rcmakePath = Join-Path $binDir 'rcmake.exe'
$lspPath = Join-Path $binDir 'rcmake-lsp.exe'
$verifyPath = Join-Path $binDir 'rcmake-verify.exe'
$tools = @('cmake.exe', 'ctest.exe', 'cpack.exe')

function Test-ManagedData {
    param([Parameter(Mandatory)][string]$Path)
    $data = Get-ItemOrNull -Path $Path
    if ($null -eq $data -or -not $data.PSIsContainer -or ($data.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        return $false
    }
    $marker = Get-ItemOrNull -Path (Join-Path $Path $dataMarkerName)
    return ($null -ne $marker -and -not $marker.PSIsContainer -and -not ($marker.Attributes -band [IO.FileAttributes]::ReparsePoint))
}

if ($Uninstall) {
    if ($Archive -or $AliasCmake -or $Force -or $PSBoundParameters.ContainsKey('Version')) {
        Stop-Install 'Use -Uninstall alone (optionally with -Prefix).'
    }

    $prefixItem = Get-ItemOrNull -Path $prefixFull
    if ($null -ne $prefixItem -and ($prefixItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        Stop-Install "$prefixFull is a symlink or reparse point. Remove it, then run the uninstaller again."
    }

    # Pre-flight validate all directory ancestors and manifest targets before removing any file.
    Assert-DirectoryAncestors -TargetPath $rcmakePath -PrefixRoot $prefixFull -RejectReparseUnder $prefixFull
    Assert-DirectoryAncestors -TargetPath $lspPath -PrefixRoot $prefixFull -RejectReparseUnder $prefixFull
    Assert-DirectoryAncestors -TargetPath $dataDir -PrefixRoot $prefixFull -RejectReparseUnder $prefixFull
    Assert-DirectoryAncestors -TargetPath $docDir -PrefixRoot $prefixFull -RejectReparseUnder $prefixFull
    Assert-DirectoryAncestors -TargetPath $docManifestPath -PrefixRoot $prefixFull -RejectReparseUnder $prefixFull
    Assert-DirectoryAncestors -TargetPath (Join-Path (Join-Path $docDir 'third_party') 'licences') -PrefixRoot $prefixFull -RejectReparseUnder $prefixFull
    Assert-DirectoryAncestors -TargetPath $completionPath -PrefixRoot $prefixFull -RejectReparseUnder $prefixFull

    $docItemCheck = Get-ItemOrNull -Path $docDir
    if ($null -ne $docItemCheck -and ($docItemCheck.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        Stop-Install "$docDir is a symlink or reparse point. Remove it, then run the uninstaller again."
    }
    $dataItemCheck = Get-ItemOrNull -Path $dataDir
    if ($null -ne $dataItemCheck -and ($dataItemCheck.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        Stop-Install "$dataDir is a symlink or reparse point. Remove it, then run the uninstaller again."
    }
    $manifestCheck = Get-ItemOrNull -Path $docManifestPath
    if ($null -ne $manifestCheck -and -not $manifestCheck.PSIsContainer -and -not ($manifestCheck.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        $docDirFullPre = [IO.Path]::GetFullPath($docDir).TrimEnd('\', '/')
        foreach ($relPosix in (Read-DocManifestEntries -ManifestPath $docManifestPath)) {
            $relNative = ($relPosix -split '/') -join [IO.Path]::DirectorySeparatorChar
            $targetDocPre = [IO.Path]::GetFullPath((Join-Path $docDir $relNative))
            if ($targetDocPre.StartsWith($docDirFullPre + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
                Assert-DirectoryAncestors -TargetPath $targetDocPre -PrefixRoot $prefixFull -RejectReparseUnder $prefixFull
            }
        }
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

    foreach ($binaryPath in @($rcmakePath, $lspPath, $verifyPath)) {
        $item = Get-ItemOrNull -Path $binaryPath
        if ($null -ne $item -and -not $item.PSIsContainer) {
            Remove-Item -LiteralPath $binaryPath -Force
            $removed += $binaryPath
        }
    }

    if (Test-Path -LiteralPath $dataDir -PathType Container) {
        $cmakePath = Join-Path $binDir 'cmake.exe'
        if (-not (Test-ManagedData -Path $dataDir)) {
            Write-Host "Kept $dataDir because it has no rcmake ownership marker."
        } elseif ($null -ne (Get-ItemOrNull -Path $cmakePath)) {
            Write-Host "Kept $dataDir because $cmakePath is another CMake installation."
        } else {
            Remove-Item -LiteralPath $dataDir -Recurse -Force
            $removed += $dataDir
        }
    }

    $docItem = Get-ItemOrNull -Path $docDir
    if ($null -ne $docItem -and $docItem.PSIsContainer -and -not ($docItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        $manifestItem = Get-ItemOrNull -Path $docManifestPath
        $docDirFull = [IO.Path]::GetFullPath($docDir).TrimEnd('\', '/')
        if ($null -ne $manifestItem -and -not $manifestItem.PSIsContainer -and -not ($manifestItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            foreach ($relPosix in (Read-DocManifestEntries -ManifestPath $docManifestPath)) {
                $relNative = ($relPosix -split '/') -join [IO.Path]::DirectorySeparatorChar
                $targetDoc = [IO.Path]::GetFullPath((Join-Path $docDir $relNative))
                if (-not $targetDoc.StartsWith($docDirFull + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
                    continue
                }
                $targetItem = Get-ItemOrNull -Path $targetDoc
                if ($null -ne $targetItem -and -not $targetItem.PSIsContainer -and -not ($targetItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                    Remove-Item -LiteralPath $targetDoc -Force
                    $removed += $targetDoc
                    $parent = Split-Path -Parent $targetDoc
                    while ($parent -and ([IO.Path]::GetFullPath($parent).TrimEnd('\', '/') -ine $docDirFull)) {
                        if ((Test-Path -LiteralPath $parent -PathType Container) -and (@(Get-ChildItem -LiteralPath $parent -Force).Count -eq 0)) {
                            Remove-Item -LiteralPath $parent -Force
                            $parent = Split-Path -Parent $parent
                        } else {
                            break
                        }
                    }
                }
            }
            Remove-Item -LiteralPath $docManifestPath -Force
        } else {
            foreach ($noticeName in @('LICENSE-APACHE', 'LICENSE-MIT', 'THIRD_PARTY_NOTICES.md')) {
                $targetDoc = Join-Path $docDir $noticeName
                $targetItem = Get-ItemOrNull -Path $targetDoc
                if ($null -ne $targetItem -and -not $targetItem.PSIsContainer -and -not ($targetItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                    Remove-Item -LiteralPath $targetDoc -Force
                    $removed += $targetDoc
                }
            }
        }
        foreach ($dirToPrune in @(
            (Join-Path (Join-Path $docDir 'third_party') 'licences'),
            (Join-Path $docDir 'third_party'),
            $docDir,
            (Split-Path -Parent $docDir)
        )) {
            $pruneItem = Get-ItemOrNull -Path $dirToPrune
            if ($null -ne $pruneItem -and $pruneItem.PSIsContainer -and -not ($pruneItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -and (@(Get-ChildItem -LiteralPath $dirToPrune -Force).Count -eq 0)) {
                Remove-Item -LiteralPath $dirToPrune -Force
            }
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
$releases = 'https://github.com/rcmake/rcmake/releases'
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
            $sumsUrl = "$releases/download/$tag/SHA256SUMS.txt"
            $checksumPath = Join-Path $downloadWork 'SHA256SUMS.txt'
            Write-Host "Download release checksums for $tag."
            try {
                Invoke-Download -Uri $sumsUrl -Destination $checksumPath
            } catch {
                Stop-Install "Could not download $sumsUrl ($($_.Exception.Message)). If no release is published yet, clone the repository and build from source with scripts/stage.sh (or scripts/install.sh --from-source on Linux or macOS)."
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
    $stagedVerify = Join-Path (Join-Path $stageWork 'bin') 'rcmake-verify.exe'
    $hasVerify = Test-Path -LiteralPath $stagedVerify -PathType Leaf
    $stagedCompletion = Join-Path (Join-Path (Join-Path (Join-Path $stageWork 'share') 'rcmake') 'completions') 'rcmake.ps1'
    $hasCompletion = Test-Path -LiteralPath $stagedCompletion -PathType Leaf
    $stagedDoc = Join-Path (Join-Path (Join-Path $stageWork 'share') 'doc') 'rcmake'
    $stagedDocFiles = @()
    if (Test-Path -LiteralPath $stagedDoc -PathType Container) {
        $stagedDocPrefix = [IO.Path]::GetFullPath($stagedDoc).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
        foreach ($docFile in @(Get-ChildItem -LiteralPath $stagedDoc -File -Recurse -Force | Sort-Object -Property FullName)) {
            $relSub = $docFile.FullName.Substring($stagedDocPrefix.Length)
            $stagedDocFiles += [pscustomobject]@{
                Source = $docFile.FullName
                RelNative = $relSub
                RelPosix = $relSub.Replace('\', '/')
            }
        }
    }
    $hasDocs = @($stagedDocFiles).Count -gt 0

    $stagedData = Join-Path (Join-Path $stageWork 'share') 'cmake-4.5'
    foreach ($required in @('Modules\CMake.cmake', 'Templates\CPackConfig.cmake.in')) {
        if (-not (Test-Path -LiteralPath (Join-Path $stagedData $required) -PathType Leaf)) {
            Stop-Install "Missing $required in the archive."
        }
    }

    [IO.File]::WriteAllText((Join-Path $stagedData $dataMarkerName), "Managed by rcmake.`n")
    $null = & $stagedBin --version
    if ($LASTEXITCODE -ne 0) {
        Stop-Install "Staged rcmake.exe failed --version check (exit code $LASTEXITCODE)."
    }

    # Validate all destination paths and parent directories before modifying any installed file.
    Assert-DirectoryAncestors -TargetPath $rcmakePath -PrefixRoot $prefixFull
    Assert-DirectoryAncestors -TargetPath $dataDir -PrefixRoot $prefixFull
    if (-not $Force -and $null -ne (Get-ItemOrNull -Path $dataDir) -and -not (Test-ManagedData -Path $dataDir)) {
        Stop-Install "$dataDir has no rcmake ownership marker. Another CMake installation may own it. Use -Prefix <separate-dir> or -Force to replace the data."
    }
    if ($hasCompletion) {
        Assert-DirectoryAncestors -TargetPath $completionPath -PrefixRoot $prefixFull
    }
    foreach ($checkTarget in @(
        @{ Path = $rcmakePath; Present = $true },
        @{ Path = $lspPath; Present = $hasLsp },
        @{ Path = $verifyPath; Present = $hasVerify },
        @{ Path = $completionPath; Present = $hasCompletion }
    )) {
        if ($checkTarget.Present) {
            $existing = Get-ItemOrNull -Path $checkTarget.Path
            if ($null -ne $existing -and $existing.PSIsContainer -and -not ($existing.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                Stop-Install "$($checkTarget.Path) is a directory. Remove it, then run the installer again."
            }
        }
    }
    if ($hasDocs) {
        Assert-DirectoryAncestors -TargetPath $docManifestPath -PrefixRoot $prefixFull -RejectReparseUnder $docDir
        $existingManifest = Get-ItemOrNull -Path $docManifestPath
        if ($null -ne $existingManifest -and ($existingManifest.PSIsContainer -or ($existingManifest.Attributes -band [IO.FileAttributes]::ReparsePoint))) {
            Stop-Install "$docManifestPath is a directory or symlink. Remove it, then run the installer again."
        }
        foreach ($docEntry in $stagedDocFiles) {
            $destDoc = Join-Path $docDir $docEntry.RelNative
            Assert-DirectoryAncestors -TargetPath $destDoc -PrefixRoot $prefixFull -RejectReparseUnder $docDir
            $existingDocFile = Get-ItemOrNull -Path $destDoc
            if ($null -ne $existingDocFile -and ($existingDocFile.PSIsContainer -or ($existingDocFile.Attributes -band [IO.FileAttributes]::ReparsePoint))) {
                Stop-Install "$destDoc is a directory or symlink. Remove it, then run the installer again."
            }
        }
    }

    $rollbackDir = Join-Path $stageWork 'rollback'
    New-Item -ItemType Directory -Path $rollbackDir -Force | Out-Null
    $rollbackOps = New-Object System.Collections.ArrayList
    $createdDirs = New-Object System.Collections.ArrayList

    $ensureDir = {
        param([string]$DirPath)
        $prefixNorm = [IO.Path]::GetFullPath($prefixFull).TrimEnd('\', '/')
        $missing = @()
        $cur = [IO.Path]::GetFullPath($DirPath).TrimEnd('\', '/')
        while ($cur -and ($cur -ine $prefixNorm) -and -not (Test-Path -LiteralPath $cur)) {
            $missing += $cur
            $next = Split-Path -Parent $cur
            if (-not $next -or $next -ieq $cur) {
                break
            }
            $cur = $next
        }
        [Array]::Reverse($missing)
        foreach ($pathToCreate in $missing) {
            New-Item -ItemType Directory -Path $pathToCreate -Force | Out-Null
            [void]$createdDirs.Add($pathToCreate)
        }
    }

    $backupDestination = {
        param([string]$DestPath)
        $existingItem = Get-ItemOrNull -Path $DestPath
        if ($null -ne $existingItem) {
            $savedPath = Join-Path $rollbackDir ([string]$rollbackOps.Count)
            Move-Item -LiteralPath $DestPath -Destination $savedPath
            [void]$rollbackOps.Add(@{ Dest = $DestPath; Backup = $savedPath })
        } else {
            [void]$rollbackOps.Add(@{ Dest = $DestPath; Backup = $null })
        }
    }

    $installed = @()
    $versionOutput = $null
    try {
        & $ensureDir $binDir
        & $ensureDir (Split-Path -Parent $dataDir)

        & $backupDestination $dataDir
        Move-Item -LiteralPath $stagedData -Destination $dataDir

        & $backupDestination $rcmakePath
        Move-Item -LiteralPath $stagedBin -Destination $rcmakePath -Force
        $installed += $rcmakePath

        if ($hasLsp) {
            & $backupDestination $lspPath
            Move-Item -LiteralPath $stagedLsp -Destination $lspPath -Force
            $installed += $lspPath
        }

        if ($hasVerify) {
            & $backupDestination $verifyPath
            Move-Item -LiteralPath $stagedVerify -Destination $verifyPath -Force
            $installed += $verifyPath
        }

        $aliasesToWrite = if ($AliasCmake) { $tools } else { $existingOwnedAliases }
        foreach ($tool in $aliasesToWrite) {
            $aliasPath = Join-Path $binDir $tool
            & $backupDestination $aliasPath
            $mode = Install-AliasExecutable -SourcePath $rcmakePath -AliasPath $aliasPath
            $installed += "$aliasPath -> rcmake.exe ($mode)"
        }
        $installed += $dataDir

        if ($hasDocs) {
            $previouslyOwned = Read-DocManifestEntries -ManifestPath $docManifestPath
            $newDocSet = @{}
            foreach ($docEntry in $stagedDocFiles) {
                $newDocSet[$docEntry.RelPosix] = $true
            }
            & $ensureDir $docDir
            foreach ($docEntry in $stagedDocFiles) {
                $destDoc = Join-Path $docDir $docEntry.RelNative
                & $ensureDir (Split-Path -Parent $destDoc)
                & $backupDestination $destDoc
                Move-Item -LiteralPath $docEntry.Source -Destination $destDoc -Force
                $installed += $destDoc
            }
            $docDirFull = [IO.Path]::GetFullPath($docDir).TrimEnd('\', '/')
            foreach ($oldRelPosix in $previouslyOwned) {
                if ($newDocSet.ContainsKey($oldRelPosix)) {
                    continue
                }
                $oldRelNative = ($oldRelPosix -split '/') -join [IO.Path]::DirectorySeparatorChar
                $oldDocPath = [IO.Path]::GetFullPath((Join-Path $docDir $oldRelNative))
                if (-not $oldDocPath.StartsWith($docDirFull + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
                    continue
                }
                $oldItem = Get-ItemOrNull -Path $oldDocPath
                if ($null -ne $oldItem -and -not $oldItem.PSIsContainer -and -not ($oldItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                    & $backupDestination $oldDocPath
                    $parent = Split-Path -Parent $oldDocPath
                    while ($parent -and ([IO.Path]::GetFullPath($parent).TrimEnd('\', '/') -ine $docDirFull)) {
                        if ((Test-Path -LiteralPath $parent -PathType Container) -and (@(Get-ChildItem -LiteralPath $parent -Force).Count -eq 0)) {
                            Remove-Item -LiteralPath $parent -Force
                            $parent = Split-Path -Parent $parent
                        } else {
                            break
                        }
                    }
                }
            }
            $stagedManifest = Join-Path $stageWork 'new-doc-manifest'
            $manifestLines = @($newDocSet.Keys | Sort-Object)
            [IO.File]::WriteAllText($stagedManifest, (($manifestLines -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding($false)))
            & $backupDestination $docManifestPath
            Move-Item -LiteralPath $stagedManifest -Destination $docManifestPath -Force
        }

        if ($hasCompletion) {
            & $ensureDir $completionDir
            & $backupDestination $completionPath
            Move-Item -LiteralPath $stagedCompletion -Destination $completionPath -Force
            $installed += $completionPath
        }

        $versionOutput = & $rcmakePath --version
        if ($LASTEXITCODE -ne 0) {
            Stop-Install "Installed rcmake.exe failed --version check (exit code $LASTEXITCODE)."
        }
    } catch {
        for ($i = $rollbackOps.Count - 1; $i -ge 0; $i--) {
            $op = $rollbackOps[$i]
            $curItem = Get-ItemOrNull -Path $op.Dest
            if ($null -ne $curItem) {
                if ($curItem.PSIsContainer -and -not ($curItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                    Remove-Item -LiteralPath $op.Dest -Recurse -Force -ErrorAction SilentlyContinue
                } else {
                    Remove-Item -LiteralPath $op.Dest -Force -ErrorAction SilentlyContinue
                }
            }
            if ($op.Backup -and (Test-Path -LiteralPath $op.Backup)) {
                $restoreParent = Split-Path -Parent $op.Dest
                if ($restoreParent -and -not (Test-Path -LiteralPath $restoreParent)) {
                    New-Item -ItemType Directory -Path $restoreParent -Force -ErrorAction SilentlyContinue | Out-Null
                }
                Move-Item -LiteralPath $op.Backup -Destination $op.Dest -Force -ErrorAction SilentlyContinue
            }
        }
        for ($i = $createdDirs.Count - 1; $i -ge 0; $i--) {
            $dirPath = $createdDirs[$i]
            if ((Test-Path -LiteralPath $dirPath -PathType Container) -and (@(Get-ChildItem -LiteralPath $dirPath -Force).Count -eq 0)) {
                Remove-Item -LiteralPath $dirPath -Force -ErrorAction SilentlyContinue
            }
        }
        throw
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

Write-Host 'Run rcmake --version to check the installation.'
