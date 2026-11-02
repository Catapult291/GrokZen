# Safe-delete for Windows: the only supported way for the agent to delete files.
#
# Validates one literal path against allowed roots and Windows path hazards,
# then deletes it. Validation failures exit 2 without touching the filesystem.
# The script performs real deletion: no archive, recycle-bin, or move fallback.
#
# Exit codes: 0 deleted or not found, 2 rejected, 3 deletion failed.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [AllowEmptyString()]
    [string] $Path,

    [string] $Reason = '',

    [string[]] $AllowedRoot,

    [string] $LogPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($PSVersionTable.PSVersion.Major -lt 7) {
    [Console]::Error.WriteLine('safe-delete: PowerShell 7 (pwsh) or newer is required.')
    exit 2
}

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

$script:LogFile = $null
if (-not [string]::IsNullOrWhiteSpace($LogPath)) {
    $script:LogFile = [IO.Path]::GetFullPath($LogPath)
}
elseif (-not [string]::IsNullOrWhiteSpace($env:GROK_SAFE_DELETE_LOG)) {
    $script:LogFile = [IO.Path]::GetFullPath($env:GROK_SAFE_DELETE_LOG)
}
elseif (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
    $script:LogFile = Join-Path $env:LOCALAPPDATA 'Grok\safe-delete.log'
}
else {
    $script:LogFile = Join-Path ([IO.Path]::GetTempPath()) 'grok-safe-delete.log'
}

try {
    $logDirectory = [IO.Path]::GetDirectoryName($script:LogFile)
    if (-not [string]::IsNullOrWhiteSpace($logDirectory)) {
        New-Item -ItemType Directory -Force -Path $logDirectory | Out-Null
    }
}
catch {
    [Console]::Error.WriteLine("safe-delete: cannot create log directory: $($_.Exception.Message)")
    $script:LogFile = $null
}

function Write-LogEntry {
    param(
        [Parameter(Mandatory = $true)][string] $Action,
        [string] $Original = '',
        [string] $Resolved = '',
        [string] $Root = '',
        [string] $ReasonText = '',
        [string] $ErrorText = ''
    )

    if (-not $script:LogFile) {
        return
    }

    $entry = [ordered]@{
        timestamp    = (Get-Date).ToString('o')
        action       = $Action
        user         = $env:USERNAME
        original     = $Original
        resolved     = $Resolved
        allowed_root = $Root
        reason       = $ReasonText
        error        = $ErrorText
    }

    try {
        $line = ConvertTo-Json -InputObject $entry -Compress -Depth 3
        Add-Content -LiteralPath $script:LogFile -Value $line -Encoding utf8
    }
    catch {
        [Console]::Error.WriteLine("safe-delete: cannot write log: $($_.Exception.Message)")
    }
}

function Stop-Rejected {
    param(
        [Parameter(Mandatory = $true)][string] $Message,
        [string] $Original = '',
        [string] $Resolved = '',
        [string] $Root = '',
        [string] $ReasonText = ''
    )

    Write-LogEntry -Action 'rejected' -Original $Original -Resolved $Resolved -Root $Root -ReasonText $ReasonText -ErrorText $Message
    [Console]::Error.WriteLine("safe-delete: rejected: $Message")
    exit 2
}

# ---------------------------------------------------------------------------
# Path helpers
# ---------------------------------------------------------------------------

function ConvertTo-NormalizedPath {
    param([Parameter(Mandatory = $true)][string] $Value)

    $full = [IO.Path]::GetFullPath($Value)
    $root = [IO.Path]::GetPathRoot($full)
    if ($full.Length -gt $root.Length) {
        $full = $full.TrimEnd([char[]]@('\', '/'))
    }
    return $full
}

function Test-SamePath {
    param([string] $Left, [string] $Right)
    return [string]::Equals($Left, $Right, [StringComparison]::OrdinalIgnoreCase)
}

function Test-PathInside {
    param([string] $Child, [string] $Parent)

    if (Test-SamePath $Child $Parent) {
        return $false
    }
    $prefix = if ($Parent.EndsWith('\') -or $Parent.EndsWith('/')) { $Parent } else { $Parent + '\' }
    return $Child.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
}

function Test-PathAtOrInside {
    param([string] $Child, [string] $Parent)
    return ((Test-SamePath $Child $Parent) -or (Test-PathInside $Child $Parent))
}

function Test-PathHasAmbiguousComponent {
    param([string] $Value)

    foreach ($segment in $Value.Split([char[]]@('\', '/'), [StringSplitOptions]::RemoveEmptyEntries)) {
        if ($segment -eq '.' -or $segment -eq '..') {
            continue
        }
        if ($segment.EndsWith(' ') -or $segment.EndsWith('.')) {
            return $true
        }
    }
    return $false
}

# ---------------------------------------------------------------------------
# Literal-path checks
# ---------------------------------------------------------------------------

if ([string]::IsNullOrWhiteSpace($Path)) {
    Stop-Rejected -Message 'path is empty' -Original $Path -ReasonText $Reason
}

if ($Path.IndexOfAny([char[]]@('*', '?', '[')) -ge 0) {
    Stop-Rejected -Message 'path must be literal; wildcard characters are not allowed' -Original $Path -ReasonText $Reason
}

if ($Path -match '^[A-Za-z]:(?![\\/])') {
    Stop-Rejected -Message 'drive-relative paths are not allowed' -Original $Path -ReasonText $Reason
}

if ($Path -match '^\\\\[?.]\\') {
    Stop-Rejected -Message 'device namespace paths are not allowed' -Original $Path -ReasonText $Reason
}

if ($Path -match '^\\\\') {
    Stop-Rejected -Message 'UNC paths are not allowed' -Original $Path -ReasonText $Reason
}

if ($Path -match '^[\\/]') {
    Stop-Rejected -Message 'root-relative paths are not allowed' -Original $Path -ReasonText $Reason
}

try {
    $resolved = ConvertTo-NormalizedPath $Path
}
catch {
    Stop-Rejected -Message "path cannot be normalized: $($_.Exception.Message)" -Original $Path -ReasonText $Reason
}

if (Test-PathHasAmbiguousComponent $Path) {
    Stop-Rejected -Message 'path component ends with a space or dot' -Original $Path -Resolved $resolved -ReasonText $Reason
}

if (Test-PathHasAmbiguousComponent $resolved) {
    Stop-Rejected -Message 'path component ends with a space or dot' -Original $Path -Resolved $resolved -ReasonText $Reason
}

if ($resolved.Split([char[]]@('\', '/'), [StringSplitOptions]::RemoveEmptyEntries) -contains '..') {
    Stop-Rejected -Message 'path still contains a parent-directory segment after normalization' -Original $Path -Resolved $resolved -ReasonText $Reason
}

# ---------------------------------------------------------------------------
# Protected targets
# ---------------------------------------------------------------------------

$pathRoot = [IO.Path]::GetPathRoot($resolved)
if (Test-SamePath $resolved $pathRoot) {
    Stop-Rejected -Message 'target is a drive root' -Original $Path -Resolved $resolved -ReasonText $Reason
}

$protectedExact = [System.Collections.Generic.List[string]]::new()
foreach ($candidate in @($env:USERPROFILE, [Environment]::GetFolderPath('UserProfile'), (Join-Path $pathRoot 'Users'))) {
    if (-not [string]::IsNullOrWhiteSpace($candidate)) {
        $normalized = ConvertTo-NormalizedPath $candidate
        if (-not $protectedExact.Contains($normalized)) {
            $protectedExact.Add($normalized)
        }
    }
}

$protectedTrees = [System.Collections.Generic.List[string]]::new()
$treeCandidates = [System.Collections.Generic.List[string]]::new()
foreach ($candidate in @($env:SystemRoot, $env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:ProgramData, (Join-Path $pathRoot 'Windows'))) {
    if (-not [string]::IsNullOrWhiteSpace($candidate)) {
        $treeCandidates.Add($candidate)
    }
}
foreach ($candidate in $treeCandidates) {
    try {
        $normalized = ConvertTo-NormalizedPath $candidate
    }
    catch {
        continue
    }
    if (-not $protectedTrees.Contains($normalized)) {
        $protectedTrees.Add($normalized)
    }
}

foreach ($tree in $protectedTrees) {
    if (Test-PathAtOrInside $resolved $tree) {
        Stop-Rejected -Message "target is a protected system directory: $tree" -Original $Path -Resolved $resolved -ReasonText $Reason
    }
}

foreach ($exact in $protectedExact) {
    if (Test-SamePath $resolved $exact) {
        Stop-Rejected -Message 'target is the user profile root' -Original $Path -Resolved $resolved -ReasonText $Reason
    }
}

# Any directory directly under <drive>\Users is a profile root, including
# profiles that do not belong to the current user.
if (Test-SamePath ([IO.Path]::GetDirectoryName($resolved)) (Join-Path $pathRoot 'Users')) {
    Stop-Rejected -Message 'target is a user profile root' -Original $Path -Resolved $resolved -ReasonText $Reason
}

# ---------------------------------------------------------------------------
# Allowed roots
# ---------------------------------------------------------------------------

$rootCandidates = [System.Collections.Generic.List[string]]::new()
if ($null -ne $AllowedRoot -and @($AllowedRoot).Count -gt 0) {
    foreach ($candidate in @($AllowedRoot)) {
        if ([string]::IsNullOrWhiteSpace($candidate)) {
            continue
        }
        foreach ($part in $candidate.Split(';')) {
            $rootCandidates.Add($part)
        }
    }
}
elseif (-not [string]::IsNullOrWhiteSpace($env:GROK_SAFE_DELETE_ROOTS)) {
    foreach ($candidate in $env:GROK_SAFE_DELETE_ROOTS.Split(';')) {
        $rootCandidates.Add($candidate)
    }
}
else {
    $rootCandidates.Add((Get-Location).Path)
}

$allowedRoots = [System.Collections.Generic.List[string]]::new()
foreach ($candidate in $rootCandidates) {
    if ([string]::IsNullOrWhiteSpace($candidate)) {
        continue
    }
    try {
        $normalized = ConvertTo-NormalizedPath $candidate.Trim()
    }
    catch {
        [Console]::Error.WriteLine("safe-delete: ignoring invalid allowed root '$candidate': $($_.Exception.Message)")
        continue
    }
    if (-not $allowedRoots.Contains($normalized)) {
        $allowedRoots.Add($normalized)
    }
}

if ($allowedRoots.Count -eq 0) {
    Stop-Rejected -Message 'no usable allowed root is configured' -Original $Path -Resolved $resolved -ReasonText $Reason
}

foreach ($candidate in $allowedRoots) {
    if (Test-SamePath $resolved $candidate) {
        Stop-Rejected -Message 'target is an allowed root' -Original $Path -Resolved $resolved -Root $candidate -ReasonText $Reason
    }
}

$matchedRoot = $null
foreach ($candidate in $allowedRoots) {
    if (Test-PathInside $resolved $candidate) {
        if ($null -eq $matchedRoot -or $candidate.Length -gt $matchedRoot.Length) {
            $matchedRoot = $candidate
        }
    }
}

if ($null -eq $matchedRoot) {
    Stop-Rejected -Message 'target is outside the allowed roots' -Original $Path -Resolved $resolved -ReasonText $Reason
}

# ---------------------------------------------------------------------------
# Reparse points
# ---------------------------------------------------------------------------
# Every existing component between the target and the allowed root must be a
# real file or directory. The allowed root itself may be a junction or symlink:
# a workspace is often reached through one.

$current = $resolved
$targetItem = $null
while ($true) {
    $item = Get-Item -LiteralPath $current -Force -ErrorAction SilentlyContinue
    if ($null -ne $item) {
        if (Test-SamePath $current $resolved) {
            $targetItem = $item
        }
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            Stop-Rejected -Message "path component is a reparse point: $current" -Original $Path -Resolved $resolved -Root $matchedRoot -ReasonText $Reason
        }
    }

    $parent = [IO.Path]::GetDirectoryName($current)
    if ([string]::IsNullOrEmpty($parent) -or (Test-SamePath $parent $current)) {
        break
    }
    if (Test-SamePath $parent $matchedRoot) {
        break
    }
    $current = $parent
    if (-not (Test-PathAtOrInside $current $matchedRoot)) {
        break
    }
}

if ($null -eq $targetItem) {
    Write-LogEntry -Action 'not_found' -Original $Path -Resolved $resolved -Root $matchedRoot -ReasonText $Reason
    exit 0
}

# ---------------------------------------------------------------------------
# Deletion
# ---------------------------------------------------------------------------

try {
    Remove-Item -LiteralPath $resolved -Recurse -Force -Confirm:$false -ErrorAction Stop
}
catch {
    $message = $_.Exception.Message
    Write-LogEntry -Action 'failed' -Original $Path -Resolved $resolved -Root $matchedRoot -ReasonText $Reason -ErrorText $message
    [Console]::Error.WriteLine("safe-delete: failed to delete ${resolved}: $message")
    exit 3
}

if (Test-Path -LiteralPath $resolved) {
    $message = 'target still exists after Remove-Item'
    Write-LogEntry -Action 'failed' -Original $Path -Resolved $resolved -Root $matchedRoot -ReasonText $Reason -ErrorText $message
    [Console]::Error.WriteLine("safe-delete: failed to delete ${resolved}: $message")
    exit 3
}

Write-LogEntry -Action 'deleted' -Original $Path -Resolved $resolved -Root $matchedRoot -ReasonText $Reason
Write-Output $resolved
exit 0
