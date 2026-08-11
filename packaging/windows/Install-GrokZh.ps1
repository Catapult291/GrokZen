<#
.SYNOPSIS
安装或升级 Grok Build 中文社区版的 Windows 完整安装包。

.DESCRIPTION
安装前校验 SHA256SUMS.txt 及必需文件，将程序部署到独立安装目录，并可选择更新用户 Path、提供 grok/agent 兼容命令，或备份现有官方命令。共享的 GROK_HOME 用户数据不会被删除。

.PARAMETER PackageDir
已解压安装包的根目录。默认使用本脚本所在目录。

.PARAMETER InstallDir
程序安装目录。默认使用当前用户 LocalAppData 下的 Programs\grok-zh\bin。

.PARAMETER GrokHome
共享 GROK_HOME 数据目录。默认读取 GROK_HOME 环境变量，否则使用当前用户目录下的 .grok。

.PARAMETER OverrideOfficialCommands
额外创建 grok 和 agent 兼容命令，但不移动已有官方可执行文件。

.PARAMETER UninstallOfficial
先备份再移走 GROK_HOME\bin 中现有的官方 grok.exe/agent.exe，并创建对应兼容命令。不会删除共享用户数据。

.PARAMETER NoPathUpdate
不修改当前用户 Path。

.PARAMETER Force
允许安装到缺少本安装器归属标记的现有目录。使用前请先检查目录内容。

.EXAMPLE
& .\Install-GrokZh.ps1

使用默认路径安装，并将安装目录置于当前用户 Path 首位。

.EXAMPLE
& .\Install-GrokZh.ps1 -NoPathUpdate -WhatIf

预览安装操作，不修改用户 Path，也不写入文件。
#>
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
        throw '路径不能为空。'
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
        throw '无法确定当前用户的 LocalAppData 目录。'
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
        throw '无法确定当前用户的个人资料目录。'
    }
    return Join-Path $profile '.grok'
}

function Read-AndVerifyManifest {
    param([Parameter(Mandatory = $true)][string]$Root)

    $manifestPath = Join-Path $Root 'SHA256SUMS.txt'
    if (!(Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        throw "安装包缺少校验清单：$manifestPath"
    }

    $hashes = @{}
    foreach ($line in Get-Content -LiteralPath $manifestPath) {
        if ([string]::IsNullOrWhiteSpace($line)) {
            continue
        }
        if ($line -notmatch '^\s*([0-9A-Fa-f]{64})\s+\*?(.+?)\s*$') {
            throw "SHA256SUMS.txt 行格式无效：$line"
        }
        $expected = $matches[1].ToUpperInvariant()
        $name = $matches[2].Trim()
        if ($name -ne [IO.Path]::GetFileName($name) -or $name.Contains(':')) {
            throw "安装包校验清单包含非根目录路径：$name"
        }
        if ($hashes.ContainsKey($name)) {
            throw "安装包校验清单包含重复条目：$name"
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
            throw "安装包校验清单缺少必需条目：$name"
        }
    }

    foreach ($name in $hashes.Keys) {
        $source = Join-Path $Root $name
        if (!(Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "安装包缺少文件：$source"
        }
        $actual = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash.ToUpperInvariant()
        if ($actual -ne $hashes[$name]) {
            throw "$name 的 SHA-256 不匹配。预期 $($hashes[$name])，实际 $actual。"
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
            Write-Warning "恢复原用户 Path 失败：$($_.Exception.Message)"
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
        Write-Host "在 $OfficialBin 中未找到官方 grok.exe 或 agent.exe。"
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
            note = '仅移动了命令可执行文件；共享的 GROK_HOME 数据未更改。'
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
                    Write-Warning "恢复 $($record.original_path) 失败：$($_.Exception.Message)"
                }
            }
        }
        throw "无法备份官方命令；现有文件已保留或恢复。请关闭正在使用 grok.exe/agent.exe 的进程后重试。详细信息：$($_.Exception.Message)"
    }

    foreach ($record in $records) {
        Write-Host "已备份并从 PATH 所在目录移除官方命令：$($record.original_path)"
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
    throw 'GrokHome/GROK_HOME 必须是绝对路径，且不得包含未展开的环境变量。'
}
$GrokHome = Resolve-FullPath $GrokHome
$officialBin = Resolve-FullPath (Join-Path $GrokHome 'bin')
$provideOfficialNames = $OverrideOfficialCommands.IsPresent -or $UninstallOfficial.IsPresent

if (Test-PathsOverlap $PackageDir $InstallDir) {
    throw 'PackageDir 与 InstallDir 不能相同，也不能互相包含。'
}
if (Test-PathsOverlap $InstallDir $GrokHome) {
    throw 'InstallDir 不能与共享的 GROK_HOME 数据目录重叠；请使用独立的默认程序目录。'
}

$manifest = Read-AndVerifyManifest $PackageDir
$installMarker = Join-Path $InstallDir '.grok-zh-install.json'
if ((Test-Path -LiteralPath $InstallDir) -and
    !(Test-Path -LiteralPath $installMarker -PathType Leaf) -and
    !$Force.IsPresent) {
    throw "InstallDir 已存在，但不归本安装器管理：$InstallDir。请先检查目录内容，确认安全后再使用 -Force。"
}

$operationParts = [Collections.Generic.List[string]]::new()
$operationParts.Add('安装 grok-zh 和 agent-zh')
if ($provideOfficialNames) {
    $operationParts.Add('提供 grok 和 agent 兼容命令')
}
if (!$NoPathUpdate.IsPresent) {
    $operationParts.Add('将安装目录置于用户 Path 首位')
}
if ($UninstallOfficial.IsPresent) {
    $operationParts.Add('备份并移除指定的官方 grok.exe/agent.exe 命令')
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
Write-Host "安装完成：$InstallDir"
Write-Host '默认命令：grok-zh、agent-zh'
if ($provideOfficialNames) {
    Write-Host '已启用命令接管：grok 和 agent 现在使用本安装目录中的可恢复兼容脚本。'
}
if ($UninstallOfficial.IsPresent) {
    Write-Host "官方命令处理结果：已将 $($movedOfficial.Count) 个文件移入备份；共享数据目录 $GrokHome 未更改。"
}
if ($NoPathUpdate.IsPresent) {
    Write-Host '未修改用户 Path（-NoPathUpdate）。'
} else {
    Write-Host '安装目录已置于用户 Path 首位。请重新打开其他终端窗口。'
}
Write-Host '验证命令：grok-zh --version; agent-zh --help'
