# Tests for scripts/windows/safe-delete.ps1.
#
# Plain PowerShell assertions; no Pester dependency.
# Run: pwsh -NoProfile -NonInteractive -File scripts/windows/tests/safe-delete.tests.ps1

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:ScriptPath = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..' 'safe-delete.ps1'))
$script:PwshPath = Join-Path $PSHOME 'pwsh.exe'
if (-not (Test-Path -LiteralPath $script:PwshPath)) {
    $script:PwshPath = (Get-Command pwsh -ErrorAction Stop).Source
}

$script:Passed = 0
$script:Failed = 0
$script:Skipped = 0
$script:Sandboxes = [System.Collections.Generic.List[string]]::new()

function Assert-Equal {
    param($Expected, $Actual, [string] $Name)

    if ($Expected -eq $Actual) {
        $script:Passed++
        Write-Host "PASS $Name"
    }
    else {
        $script:Failed++
        Write-Host "FAIL $Name (expected '$Expected', got '$Actual')" -ForegroundColor Red
    }
}

function Assert-True {
    param([bool] $Condition, [string] $Name, [string] $Detail = '')

    if ($Condition) {
        $script:Passed++
        Write-Host "PASS $Name"
    }
    else {
        $script:Failed++
        $suffix = if ($Detail) { " ($Detail)" } else { '' }
        Write-Host "FAIL $Name$suffix" -ForegroundColor Red
    }
}

function Skip-Test {
    param([string] $Reason)

    $script:Skipped++
    Write-Host "SKIP $Reason" -ForegroundColor Yellow
}

function New-TestSandbox {
    $base = Join-Path ([IO.Path]::GetTempPath()) ('safe-delete-tests-' + [guid]::NewGuid().ToString('N'))
    $root = Join-Path $base 'root'
    New-Item -ItemType Directory -Path $root -Force | Out-Null
    $script:Sandboxes.Add($base)

    return [pscustomobject]@{
        Base = $base
        Root = $root
        Log  = (Join-Path $base 'safe-delete.log')
    }
}

function Invoke-SafeDelete {
    param(
        [string] $TargetPath,
        [string] $AllowedRoot,
        [string] $LogFile,
        [string] $Reason = 'test'
    )

    $arguments = @(
        '-NoProfile', '-NonInteractive', '-File', $script:ScriptPath,
        '-Path', $TargetPath,
        '-LogPath', $LogFile,
        '-Reason', $Reason
    )
    if (-not [string]::IsNullOrEmpty($AllowedRoot)) {
        $arguments += @('-AllowedRoot', $AllowedRoot)
    }

    $code = -1
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $raw = & $script:PwshPath @arguments 2>&1
        $code = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previous
    }

    return [pscustomobject]@{
        ExitCode = $code
        Output   = (($raw | Out-String).Trim())
    }
}

function Get-LastLogEntry {
    param([string] $LogFile)

    if (-not (Test-Path -LiteralPath $LogFile)) {
        return $null
    }
    $lines = @(Get-Content -LiteralPath $LogFile)
    if ($lines.Count -eq 0) {
        return $null
    }
    return ($lines[$lines.Count - 1] | ConvertFrom-Json)
}

function Assert-LogAction {
    param([string] $LogFile, [string] $Expected, [string] $Name)

    $entry = Get-LastLogEntry $LogFile
    if ($null -eq $entry) {
        Assert-True $false $Name 'no log entry written'
        return
    }
    Assert-Equal $Expected $entry.action $Name
}

function Invoke-Test {
    param([string] $Name, [scriptblock] $Body)

    Write-Host "== $Name"
    try {
        & $Body
    }
    catch {
        $script:Failed++
        Write-Host "FAIL $Name (unhandled: $($_.Exception.Message))" -ForegroundColor Red
    }
}

# ---------------------------------------------------------------------------

function Test-DeleteDirectory {
    $sb = New-TestSandbox
    $target = Join-Path $sb.Root 'build'
    New-Item -ItemType Directory -Path (Join-Path $target 'sub') -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $target 'sub\artifact.txt') -Value 'x' -Encoding utf8

    $result = Invoke-SafeDelete -TargetPath $target -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 0 $result.ExitCode 'delete directory: exit 0'
    Assert-True (-not (Test-Path -LiteralPath $target)) 'delete directory: target removed'
    Assert-Equal ([IO.Path]::GetFullPath($target)) $result.Output 'delete directory: prints deleted path'
    Assert-LogAction $sb.Log 'deleted' 'delete directory: log action deleted'

    $entry = Get-LastLogEntry $sb.Log
    Assert-Equal ([IO.Path]::GetFullPath($target)) $entry.resolved 'delete directory: log resolved path'

    $names = @(Get-ChildItem -LiteralPath $sb.Base -Force | ForEach-Object Name | Sort-Object)
    Assert-Equal 'root,safe-delete.log' ($names -join ',') 'delete directory: no archive directory created'
}

function Test-DeleteFile {
    $sb = New-TestSandbox
    $target = Join-Path $sb.Root 'old.log'
    Set-Content -LiteralPath $target -Value 'contents' -Encoding utf8

    $result = Invoke-SafeDelete -TargetPath $target -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 0 $result.ExitCode 'delete file: exit 0'
    Assert-True (-not (Test-Path -LiteralPath $target)) 'delete file: target removed'
    Assert-LogAction $sb.Log 'deleted' 'delete file: log action deleted'
}

function Test-RejectDriveRoot {
    $sb = New-TestSandbox
    $driveRoot = [IO.Path]::GetPathRoot($env:SystemRoot)

    $result = Invoke-SafeDelete -TargetPath $driveRoot -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject drive root: exit 2'
    Assert-True (Test-Path -LiteralPath $driveRoot) 'reject drive root: drive still exists'
    Assert-True ($result.Output -match 'rejected') 'reject drive root: stderr mentions rejection'
    Assert-LogAction $sb.Log 'rejected' 'reject drive root: log action rejected'
}

function Test-RejectWindowsDirectory {
    $sb = New-TestSandbox
    $target = $env:SystemRoot

    $result = Invoke-SafeDelete -TargetPath $target -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject system directory: exit 2'
    Assert-True (Test-Path -LiteralPath $target) 'reject system directory: directory still exists'
}

function Test-RejectRelativePath {
    $sb = New-TestSandbox

    $result = Invoke-SafeDelete -TargetPath '..\..\Windows' -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject relative path: exit 2'
}

function Test-RejectBareRelativePath {
    $sb = New-TestSandbox
    $target = Join-Path $sb.Root 'build'
    New-Item -ItemType Directory -Path $target -Force | Out-Null

    Push-Location $sb.Root
    try {
        $result = Invoke-SafeDelete -TargetPath 'build' -AllowedRoot $sb.Root -LogFile $sb.Log
    }
    finally {
        Pop-Location
    }

    Assert-Equal 2 $result.ExitCode 'reject bare relative path: exit 2'
    Assert-True (Test-Path -LiteralPath $target) 'reject bare relative path: target untouched'
}

function Test-RejectWildcard {
    $sb = New-TestSandbox
    $keep = Join-Path $sb.Root 'keep.txt'
    Set-Content -LiteralPath $keep -Value 'keep' -Encoding utf8

    $result = Invoke-SafeDelete -TargetPath (Join-Path $sb.Root '*') -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject wildcard: exit 2'
    Assert-True (Test-Path -LiteralPath $keep) 'reject wildcard: files untouched'
}

function Test-RejectUnc {
    $sb = New-TestSandbox

    $result = Invoke-SafeDelete -TargetPath '\\server\share\folder' -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject UNC path: exit 2'
}

function Test-RejectDeviceNamespace {
    $sb = New-TestSandbox

    $result = Invoke-SafeDelete -TargetPath '\\?\C:\Windows' -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject device namespace: exit 2'
}

function Test-RejectOutsideRoot {
    $sb = New-TestSandbox
    $outside = Join-Path $sb.Base 'outside'
    New-Item -ItemType Directory -Path $outside -Force | Out-Null
    $victim = Join-Path $outside 'victim.txt'
    Set-Content -LiteralPath $victim -Value 'keep' -Encoding utf8

    $result = Invoke-SafeDelete -TargetPath $victim -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject outside root: exit 2'
    Assert-True (Test-Path -LiteralPath $victim) 'reject outside root: target untouched'
}

function Test-RejectAllowedRoot {
    $sb = New-TestSandbox

    $result = Invoke-SafeDelete -TargetPath $sb.Root -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject allowed root: exit 2'
    Assert-True (Test-Path -LiteralPath $sb.Root) 'reject allowed root: root still exists'
}

function Test-NotFound {
    $sb = New-TestSandbox
    $target = Join-Path $sb.Root 'missing-target'

    $result = Invoke-SafeDelete -TargetPath $target -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 0 $result.ExitCode 'not found: exit 0'
    Assert-LogAction $sb.Log 'not_found' 'not found: log action not_found'
}

function Test-RejectAmbiguousComponent {
    $sb = New-TestSandbox

    $result = Invoke-SafeDelete -TargetPath (Join-Path $sb.Root 'bad. ') -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject trailing dot/space: exit 2'
}

function Test-RejectDriveRelative {
    $sb = New-TestSandbox

    $result = Invoke-SafeDelete -TargetPath 'C:foo' -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject drive-relative path: exit 2'
}

function Test-RejectWhitespacePath {
    $sb = New-TestSandbox

    $result = Invoke-SafeDelete -TargetPath '   ' -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject whitespace path: exit 2'
}

function Test-DefaultRootIsWorkingDirectory {
    $sb = New-TestSandbox
    $target = Join-Path $sb.Root 'solo'
    New-Item -ItemType Directory -Path $target -Force | Out-Null

    Push-Location $sb.Root
    try {
        $result = Invoke-SafeDelete -TargetPath $target -LogFile $sb.Log
    }
    finally {
        Pop-Location
    }

    Assert-Equal 0 $result.ExitCode 'default root: exit 0'
    Assert-True (-not (Test-Path -LiteralPath $target)) 'default root: target removed'
}

function Test-MultipleAllowedRoots {
    $sb = New-TestSandbox
    $secondRoot = Join-Path $sb.Base 'second-root'
    New-Item -ItemType Directory -Path $secondRoot -Force | Out-Null
    $target = Join-Path $secondRoot 'build'
    New-Item -ItemType Directory -Path $target -Force | Out-Null

    $result = Invoke-SafeDelete -TargetPath $target -AllowedRoot "C:\does-not-exist;$secondRoot" -LogFile $sb.Log

    Assert-Equal 0 $result.ExitCode 'multiple roots: exit 0'
    Assert-True (-not (Test-Path -LiteralPath $target)) 'multiple roots: target removed'
}

function Test-AllowedRootFromEnvironment {
    $sb = New-TestSandbox
    $otherRoot = Join-Path $sb.Base 'other-root'
    New-Item -ItemType Directory -Path $otherRoot -Force | Out-Null
    $target = Join-Path $otherRoot 'tmp'
    New-Item -ItemType Directory -Path $target -Force | Out-Null

    $env:GROK_SAFE_DELETE_ROOTS = "C:\does-not-exist;$otherRoot"
    try {
        $result = Invoke-SafeDelete -TargetPath $target -LogFile $sb.Log
    }
    finally {
        Remove-Item Env:GROK_SAFE_DELETE_ROOTS -ErrorAction SilentlyContinue
    }

    Assert-Equal 0 $result.ExitCode 'env roots: exit 0'
    Assert-True (-not (Test-Path -LiteralPath $target)) 'env roots: target removed'
}

function Test-RejectJunctionComponent {
    $sb = New-TestSandbox
    $outside = Join-Path $sb.Base 'outside'
    New-Item -ItemType Directory -Path $outside -Force | Out-Null
    $victim = Join-Path $outside 'victim.txt'
    Set-Content -LiteralPath $victim -Value 'keep' -Encoding utf8

    $junction = Join-Path $sb.Root 'link'
    try {
        New-Item -ItemType Junction -Path $junction -Target $outside -ErrorAction Stop | Out-Null
    }
    catch {
        Skip-Test "junction component: cannot create junction ($($_.Exception.Message))"
        return
    }

    $result = Invoke-SafeDelete -TargetPath (Join-Path $junction 'victim.txt') -AllowedRoot $sb.Root -LogFile $sb.Log

    Assert-Equal 2 $result.ExitCode 'reject junction component: exit 2'
    Assert-True (Test-Path -LiteralPath $victim) 'reject junction component: target untouched'
}

function Test-AllowedRootIsJunction {
    $sb = New-TestSandbox
    $realRoot = Join-Path $sb.Base 'real-root'
    New-Item -ItemType Directory -Path $realRoot -Force | Out-Null
    $linkRoot = Join-Path $sb.Base 'link-root'
    try {
        New-Item -ItemType Junction -Path $linkRoot -Target $realRoot -ErrorAction Stop | Out-Null
    }
    catch {
        Skip-Test "allowed root junction: cannot create junction ($($_.Exception.Message))"
        return
    }

    $target = Join-Path $linkRoot 'build'
    New-Item -ItemType Directory -Path $target -Force | Out-Null

    $result = Invoke-SafeDelete -TargetPath $target -AllowedRoot $linkRoot -LogFile $sb.Log

    Assert-Equal 0 $result.ExitCode 'allowed root junction: exit 0'
    Assert-True (-not (Test-Path -LiteralPath $target)) 'allowed root junction: target removed'
}

# ---------------------------------------------------------------------------

try {
    Invoke-Test 'delete directory' { Test-DeleteDirectory }
    Invoke-Test 'delete file' { Test-DeleteFile }
    Invoke-Test 'reject drive root' { Test-RejectDriveRoot }
    Invoke-Test 'reject system directory' { Test-RejectWindowsDirectory }
    Invoke-Test 'reject relative path' { Test-RejectRelativePath }
    Invoke-Test 'reject bare relative path' { Test-RejectBareRelativePath }
    Invoke-Test 'reject wildcard' { Test-RejectWildcard }
    Invoke-Test 'reject UNC path' { Test-RejectUnc }
    Invoke-Test 'reject device namespace' { Test-RejectDeviceNamespace }
    Invoke-Test 'reject outside root' { Test-RejectOutsideRoot }
    Invoke-Test 'reject allowed root' { Test-RejectAllowedRoot }
    Invoke-Test 'not found' { Test-NotFound }
    Invoke-Test 'reject trailing dot/space' { Test-RejectAmbiguousComponent }
    Invoke-Test 'reject drive-relative path' { Test-RejectDriveRelative }
    Invoke-Test 'reject whitespace path' { Test-RejectWhitespacePath }
    Invoke-Test 'default root is working directory' { Test-DefaultRootIsWorkingDirectory }
    Invoke-Test 'multiple allowed roots' { Test-MultipleAllowedRoots }
    Invoke-Test 'allowed roots from environment' { Test-AllowedRootFromEnvironment }
    Invoke-Test 'reject junction component' { Test-RejectJunctionComponent }
    Invoke-Test 'allowed root junction' { Test-AllowedRootIsJunction }
}
finally {
    foreach ($sandbox in $script:Sandboxes) {
        Remove-Item -LiteralPath $sandbox -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host ''
Write-Host ("passed: {0}, failed: {1}, skipped: {2}" -f $script:Passed, $script:Failed, $script:Skipped)
if ($script:Failed -gt 0) {
    exit 1
}
exit 0
