[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'Medium')]
param(
    [string]$PackageDir = $PSScriptRoot,
    [string]$InstallDir,
    [string]$GrokHome,
    [switch]$OverrideOfficialCommands,
    [switch]$UninstallOfficial,
    [switch]$NoPathUpdate,
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Resolve-FullPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    $expanded = $Path.Trim().Trim('"')
    if ([string]::IsNullOrWhiteSpace($expanded)) {
        throw 'Path must not be empty.'
    }
    $full = [IO.Path]::GetFullPath($expanded)
    $root = [IO.Path]::GetPathRoot($full)
    if ([StringComparer]::OrdinalIgnoreCase.Equals($full, $root)) {
        return $full
    }
    return $full.TrimEnd(
        [IO.Path]::DirectorySeparatorChar,
        [IO.Path]::AltDirectorySeparatorChar
    )
}

function Test-SamePath {
    param(
        [Parameter(Mandatory = $true)][string]$Left,
        [Parameter(Mandatory = $true)][string]$Right
    )

    return [StringComparer]::OrdinalIgnoreCase.Equals(
        (Resolve-FullPath $Left),
        (Resolve-FullPath $Right)
    )
}

function Test-PathsOverlap {
    param(
        [Parameter(Mandatory = $true)][string]$Left,
        [Parameter(Mandatory = $true)][string]$Right
    )

    $leftPath = Resolve-FullPath $Left
    $rightPath = Resolve-FullPath $Right
    if ([StringComparer]::OrdinalIgnoreCase.Equals($leftPath, $rightPath)) {
        return $true
    }
    $separator = [IO.Path]::DirectorySeparatorChar
    $leftPrefix = if ($leftPath.EndsWith($separator)) { $leftPath } else { "$leftPath$separator" }
    $rightPrefix = if ($rightPath.EndsWith($separator)) { $rightPath } else { "$rightPath$separator" }
    return $leftPath.StartsWith($rightPrefix, [StringComparison]::OrdinalIgnoreCase) -or
        $rightPath.StartsWith($leftPrefix, [StringComparison]::OrdinalIgnoreCase)
}

function Get-DefaultInstallDir {
    $localAppData = [Environment]::GetFolderPath('LocalApplicationData')
    if ([string]::IsNullOrWhiteSpace($localAppData)) {
        $localAppData = $env:LOCALAPPDATA
    }
    if ([string]::IsNullOrWhiteSpace($localAppData)) {
        throw 'Unable to resolve the current user LocalAppData directory.'
    }
    return Join-Path $localAppData 'Programs\grok-zh\bin'
}

function Get-DefaultGrokHome {
    if (![string]::IsNullOrWhiteSpace($env:GROK_HOME)) {
        return $env:GROK_HOME
    }
    $profile = [Environment]::GetFolderPath('UserProfile')
    if ([string]::IsNullOrWhiteSpace($profile)) {
        $profile = $env:USERPROFILE
    }
    if ([string]::IsNullOrWhiteSpace($profile)) {
        throw 'Unable to resolve the current user profile directory.'
    }
    return Join-Path $profile '.grok'
}

function Read-AndVerifyManifest {
    param([Parameter(Mandatory = $true)][string]$Root)

    $manifestPath = Join-Path $Root 'SHA256SUMS.txt'
    if (!(Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        throw "Package manifest is missing: $manifestPath"
    }

    $hashes = @{}
    foreach ($line in Get-Content -LiteralPath $manifestPath) {
        if ([string]::IsNullOrWhiteSpace($line)) {
            continue
        }
        if ($line -notmatch '^\s*([0-9A-Fa-f]{64})\s+\*?(.+?)\s*$') {
            throw "Malformed SHA256SUMS.txt line: $line"
        }
        $expected = $matches[1].ToUpperInvariant()
        $name = $matches[2].Trim()
        if ($name -ne [IO.Path]::GetFileName($name) -or $name.Contains(':')) {
            throw "Package manifest contains a non-root path: $name"
        }
        if ($hashes.ContainsKey($name)) {
            throw "Package manifest contains a duplicate entry: $name"
        }
        $hashes[$name] = $expected
    }

    $required = @(
        'grok-zh.exe',
        'agent-zh.cmd',
        'rg.exe',
        'Install-GrokZh.ps1',
        'INSTALL-WINDOWS.md'
    )
    foreach ($name in $required) {
        if (!$hashes.ContainsKey($name)) {
            throw "Package manifest is missing required entry: $name"
        }
    }

    foreach ($name in $hashes.Keys) {
        $source = Join-Path $Root $name
        if (!(Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Package file is missing: $source"
        }
        $actual = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash.ToUpperInvariant()
        if ($actual -ne $hashes[$name]) {
            throw "SHA-256 mismatch for $name. Expected $($hashes[$name]), got $actual."
        }
    }

    return $hashes
}

function Write-CommandShim {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][bool]$AgentMode
    )

    $content = if ($AgentMode) {
        @'
@echo off
"%~dp0grok-zh.exe" agent %*
exit /b %ERRORLEVEL%
'@
    } else {
        @'
@echo off
"%~dp0grok-zh.exe" %*
exit /b %ERRORLEVEL%
'@
    }
    Set-Content -LiteralPath $Path -Value $content.Trim() -Encoding Ascii
}

function Get-NormalizedPathEntry {
    param([string]$Entry)

    if ([string]::IsNullOrWhiteSpace($Entry)) {
        return $null
    }
    $trimmed = $Entry.Trim().Trim('"')
    $expanded = [Environment]::ExpandEnvironmentVariables($trimmed)
    try {
        return (Resolve-FullPath $expanded)
    } catch {
        return $expanded.TrimEnd('\', '/')
    }
}

function Add-UserPathEntry {
    param([Parameter(Mandatory = $true)][string]$Directory)

    $normalizedDirectory = Get-NormalizedPathEntry $Directory
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $oldProcessPath = $env:Path
    $kept = [Collections.Generic.List[string]]::new()
    $userEntries = if ($null -eq $userPath) { @() } else { @($userPath.Split([char]';')) }
    foreach ($entry in $userEntries) {
        if ([string]::IsNullOrWhiteSpace($entry)) {
            $kept.Add($entry)
            continue
        }
        $normalized = Get-NormalizedPathEntry $entry
        if (![StringComparer]::OrdinalIgnoreCase.Equals($normalized, $normalizedDirectory)) {
            $kept.Add($entry.Trim())
        }
    }
    $newUserEntries = @($Directory) + @($kept)

    $processKept = [Collections.Generic.List[string]]::new()
    $processEntries = if ($null -eq $env:Path) { @() } else { @($env:Path.Split([char]';')) }
    foreach ($entry in $processEntries) {
        if ([string]::IsNullOrWhiteSpace($entry)) {
            $processKept.Add($entry)
            continue
        }
        $normalized = Get-NormalizedPathEntry $entry
        if (![StringComparer]::OrdinalIgnoreCase.Equals($normalized, $normalizedDirectory)) {
            $processKept.Add($entry.Trim())
        }
    }

    try {
        [Environment]::SetEnvironmentVariable('Path', ($newUserEntries -join ';'), 'User')
        $env:Path = (@($Directory) + @($processKept)) -join ';'
    } catch {
        try {
            [Environment]::SetEnvironmentVariable('Path', $userPath, 'User')
            $env:Path = $oldProcessPath
        } catch {
            Write-Warning "Failed to restore the previous user Path: $($_.Exception.Message)"
        }
        throw
    }
}

function Move-OfficialCommandsToBackup {
    param(
        [Parameter(Mandatory = $true)][string]$OfficialBin,
        [Parameter(Mandatory = $true)][string]$CommunityInstallDir
    )

    $candidates = @('grok.exe', 'agent.exe')
    $existing = @($candidates | Where-Object {
        Test-Path -LiteralPath (Join-Path $OfficialBin $_) -PathType Leaf
    })
    if ($existing.Count -eq 0) {
        Write-Host "No official grok.exe or agent.exe was found in $OfficialBin."
        return @()
    }

    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $token = [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $backupDir = Join-Path $CommunityInstallDir "official-backup\$stamp-$token"
    $records = [Collections.Generic.List[object]]::new()

    foreach ($name in $existing) {
        $source = Join-Path $OfficialBin $name
        $destination = Join-Path $backupDir $name
        $hash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
        $records.Add([ordered]@{
            name = $name
            original_path = $source
            backup_path = $destination
            sha256 = $hash
        })
    }

    $moved = [Collections.Generic.List[object]]::new()
    try {
        New-Item -ItemType Directory -Path $backupDir -Force | Out-Null
        foreach ($record in $records) {
            $source = $record.original_path
            $destination = $record.backup_path
            Move-Item -LiteralPath $source -Destination $destination
            $moved.Add($record)
        }

        $manifestTemp = Join-Path $backupDir 'official-backup.json.tmp'
        [ordered]@{
            moved_at = (Get-Date).ToString('o')
            note = 'Only command executables were moved. Shared GROK_HOME data was not changed.'
            files = @($records)
        } | ConvertTo-Json -Depth 5 | Set-Content `
            -LiteralPath $manifestTemp -Encoding UTF8
        Move-Item -LiteralPath $manifestTemp `
            -Destination (Join-Path $backupDir 'official-backup.json')
    } catch {
        for ($index = $moved.Count - 1; $index -ge 0; $index--) {
            $record = $moved[$index]
            if ((Test-Path -LiteralPath $record.backup_path) -and
                !(Test-Path -LiteralPath $record.original_path)) {
                try {
                    Move-Item -LiteralPath $record.backup_path -Destination $record.original_path
                } catch {
                    Write-Warning "Failed to restore $($record.original_path): $($_.Exception.Message)"
                }
            }
        }
        throw "Unable to back up the official commands. Existing files were retained or restored. $($_.Exception.Message)"
    }

    foreach ($record in $records) {
        Write-Host "Backed up and removed the official command from its PATH directory: $($record.original_path)"
    }
    return @($records)
}

$PackageDir = Resolve-FullPath $PackageDir
if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Get-DefaultInstallDir
}
$InstallDir = Resolve-FullPath $InstallDir
if ([string]::IsNullOrWhiteSpace($GrokHome)) {
    $GrokHome = Get-DefaultGrokHome
}
if (![IO.Path]::IsPathRooted($GrokHome)) {
    throw 'GrokHome/GROK_HOME must be an absolute, already-expanded path.'
}
$GrokHome = Resolve-FullPath $GrokHome
$officialBin = Resolve-FullPath (Join-Path $GrokHome 'bin')
$provideOfficialNames = $OverrideOfficialCommands.IsPresent -or $UninstallOfficial.IsPresent

if (Test-PathsOverlap $PackageDir $InstallDir) {
    throw 'PackageDir and InstallDir must not be the same or contain one another.'
}
if (Test-PathsOverlap $InstallDir $GrokHome) {
    throw 'InstallDir must not overlap the shared GROK_HOME data tree. Use the separate default program directory.'
}

$manifest = Read-AndVerifyManifest $PackageDir
$installMarker = Join-Path $InstallDir '.grok-zh-install.json'
if ((Test-Path -LiteralPath $InstallDir) -and
    !(Test-Path -LiteralPath $installMarker -PathType Leaf) -and
    !$Force.IsPresent) {
    throw "InstallDir already exists but is not owned by this installer: $InstallDir. Use -Force only after reviewing it."
}

$operationParts = [Collections.Generic.List[string]]::new()
$operationParts.Add('install grok-zh and agent-zh')
if ($provideOfficialNames) {
    $operationParts.Add('provide grok and agent command shims')
}
if (!$NoPathUpdate.IsPresent) {
    $operationParts.Add('prepend the install directory to the user Path')
}
if ($UninstallOfficial.IsPresent) {
    $operationParts.Add('back up and remove exact official grok.exe/agent.exe commands')
}
$operation = ($operationParts -join ', ')
if (!$PSCmdlet.ShouldProcess($InstallDir, $operation)) {
    return
}

$parent = Split-Path -Parent $InstallDir
New-Item -ItemType Directory -Path $parent -Force | Out-Null
$token = [Guid]::NewGuid().ToString('N').Substring(0, 8)
$stage = "$InstallDir.stage.$PID-$token"
$previous = $null

try {
    New-Item -ItemType Directory -Path $stage | Out-Null
    foreach ($name in $manifest.Keys) {
        Copy-Item -LiteralPath (Join-Path $PackageDir $name) -Destination (Join-Path $stage $name)
    }
    foreach ($name in @('SHA256SUMS.txt', 'BUILD-INFO.txt', 'LICENSE-grok-build.txt')) {
        $source = Join-Path $PackageDir $name
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            Copy-Item -LiteralPath $source -Destination (Join-Path $stage $name)
        }
    }
    $licenseSource = Join-Path $PackageDir 'licenses'
    if (Test-Path -LiteralPath $licenseSource -PathType Container) {
        Copy-Item -LiteralPath $licenseSource -Destination (Join-Path $stage 'licenses') -Recurse
    }

    if ($provideOfficialNames) {
        Write-CommandShim -Path (Join-Path $stage 'grok.cmd') -AgentMode:$false
        Write-CommandShim -Path (Join-Path $stage 'agent.cmd') -AgentMode:$true
    }

    $existingOfficialBackup = Join-Path $InstallDir 'official-backup'
    if (Test-Path -LiteralPath $existingOfficialBackup -PathType Container) {
        Copy-Item -LiteralPath $existingOfficialBackup `
            -Destination (Join-Path $stage 'official-backup') -Recurse
    }

    $version = 'unknown'
    $buildInfo = Join-Path $PackageDir 'BUILD-INFO.txt'
    if (Test-Path -LiteralPath $buildInfo -PathType Leaf) {
        $versionLine = Get-Content -LiteralPath $buildInfo | Where-Object { $_ -match '^Version:\s*(.+)$' } | Select-Object -First 1
        if ($versionLine -and $versionLine -match '^Version:\s*(.+)$') {
            $version = $matches[1].Trim()
        }
    }

    if (Test-Path -LiteralPath $InstallDir) {
        $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
        $previous = "$InstallDir.previous.$stamp-$token"
        Move-Item -LiteralPath $InstallDir -Destination $previous
    }

    try {
        [ordered]@{
            product = 'grok-build-zh'
            version = $version
            installed_at = (Get-Date).ToString('o')
            install_dir = $InstallDir
            commands = if ($provideOfficialNames) {
                @('grok-zh', 'agent-zh', 'grok', 'agent')
            } else {
                @('grok-zh', 'agent-zh')
            }
            previous_install_backup = $previous
            official_command_home = $GrokHome
        } | ConvertTo-Json -Depth 4 | Set-Content `
            -LiteralPath (Join-Path $stage '.grok-zh-install.json') -Encoding UTF8

        Move-Item -LiteralPath $stage -Destination $InstallDir
    } catch {
        if ($previous -and !(Test-Path -LiteralPath $InstallDir) -and (Test-Path -LiteralPath $previous)) {
            Move-Item -LiteralPath $previous -Destination $InstallDir
        }
        throw
    }
} finally {
    if (Test-Path -LiteralPath $stage) {
        Remove-Item -LiteralPath $stage -Recurse -Force
    }
}

if (!$NoPathUpdate.IsPresent) {
    Add-UserPathEntry $InstallDir
}

$movedOfficial = @()
if ($UninstallOfficial.IsPresent) {
    $movedOfficial = @(Move-OfficialCommandsToBackup `
        -OfficialBin $officialBin `
        -CommunityInstallDir $InstallDir)
}

Write-Host ''
Write-Host "Installation complete: $InstallDir"
Write-Host 'Default commands: grok-zh, agent-zh'
if ($provideOfficialNames) {
    Write-Host 'Command takeover enabled: grok and agent now use reversible shims in this install directory.'
}
if ($UninstallOfficial.IsPresent) {
    Write-Host "Official command result: moved $($movedOfficial.Count) file(s) into backup; shared data at $GrokHome was unchanged."
}
if ($NoPathUpdate.IsPresent) {
    Write-Host 'The user Path was not changed (-NoPathUpdate).'
} else {
    Write-Host 'The install directory is first in the user Path. Reopen other terminal windows.'
}
Write-Host 'Verify with: grok-zh --version; agent-zh --help'
