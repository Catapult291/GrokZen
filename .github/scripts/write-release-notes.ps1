[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $CurrentTag,

    [Parameter(Mandatory = $true)]
    [string] $OutputPath,

    [string] $Repository = $env:GITHUB_REPOSITORY,

    [string] $GitHubToken = $env:GH_TOKEN
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

function Test-ReleaseTag([string] $Tag) {
    if ($Tag -notmatch '^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?<prerelease>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$') {
        return $false
    }
    if ($matches.ContainsKey('prerelease') -and $matches.prerelease) {
        foreach ($identifier in ($matches.prerelease -split '\.')) {
            if ($identifier -match '^\d+$' -and $identifier.Length -gt 1 -and $identifier.StartsWith('0')) {
                return $false
            }
        }
    }
    return $true
}

if (!(Test-ReleaseTag $CurrentTag)) {
    throw "CurrentTag 必须是无 build metadata、无数字前导零的 SemVer Tag：$CurrentTag"
}

function Invoke-Git {
    param(
        [Parameter(Mandatory = $true)]
        [string[]] $Arguments,
        [switch] $AllowFailure
    )

    $output = @(& git @Arguments 2>&1)
    $exitCode = $LASTEXITCODE
    $global:LASTEXITCODE = 0
    if ($exitCode -ne 0 -and !$AllowFailure) {
        throw "git $($Arguments -join ' ') 失败（exit $exitCode）：$($output -join [Environment]::NewLine)"
    }
    [pscustomobject]@{
        ExitCode = $exitCode
        Lines = @($output | ForEach-Object { $_.ToString() })
    }
}

function Get-PublishedReleaseTags {
    if (!$GitHubToken -or !$Repository) {
        throw '必须提供 GitHub token 和 repository，防止把失败但残留的 Git tag 误当作已发布基线。'
    }

    $headers = @{
        Accept = 'application/vnd.github+json'
        Authorization = "Bearer $GitHubToken"
        'X-GitHub-Api-Version' = '2022-11-28'
    }
    $tags = [System.Collections.Generic.List[string]]::new()
    for ($page = 1; ; $page++) {
        $uri = "https://api.github.com/repos/$Repository/releases?per_page=100&page=$page"
        $pageResult = Invoke-RestMethod -Method Get -Uri $uri -Headers $headers
        $releases = @($pageResult)
        foreach ($release in $releases) {
            if (!$release.draft -and (Test-ReleaseTag ([string]$release.tag_name))) {
                $tags.Add([string]$release.tag_name)
            }
        }
        if ($releases.Count -lt 100) { break }
    }
    return @($tags)
}

$currentCommitResult = Invoke-Git -Arguments @('rev-parse', '--verify', "$CurrentTag^{commit}")
$currentCommit = $currentCommitResult.Lines[0].Trim()
$publishedTags = @(Get-PublishedReleaseTags)

$previousTag = $null
$firstParentCommits = Invoke-Git -Arguments @('rev-list', '--first-parent', "$currentCommit^") -AllowFailure
if ($firstParentCommits.ExitCode -eq 0) {
    $publishedByCommit = @{}
    foreach ($tag in $publishedTags) {
        if ($tag -eq $CurrentTag) { continue }
        $tagCommitResult = Invoke-Git -Arguments @('rev-parse', '--verify', "$tag^{commit}") -AllowFailure
        if ($tagCommitResult.ExitCode -ne 0 -or $tagCommitResult.Lines.Count -eq 0) { continue }
        $tagCommit = $tagCommitResult.Lines[0].Trim()
        if (!$publishedByCommit.ContainsKey($tagCommit)) {
            $publishedByCommit[$tagCommit] = $tag
        }
    }
    foreach ($commit in $firstParentCommits.Lines) {
        $candidate = $commit.Trim()
        if ($publishedByCommit.ContainsKey($candidate)) {
            $previousTag = $publishedByCommit[$candidate]
            break
        }
    }
}

if ($previousTag) {
    $range = "$previousTag..$CurrentTag"
    $log = Invoke-Git -Arguments @(
        '-c', 'i18n.logOutputEncoding=UTF-8',
        'log', '--reverse', '--first-parent', '--format=- %s (`%h`)', $range, '--'
    )
    Write-Host "Release notes 基线：$previousTag"
} else {
    $log = Invoke-Git -Arguments @(
        '-c', 'i18n.logOutputEncoding=UTF-8',
        'log', '-1', '--format=- %s (`%h`)', $CurrentTag, '--'
    )
    Write-Host 'Release notes 基线：无已发布 Release，仅记录当前 Tag 提交。'
}

$lines = @($log.Lines | Where-Object { ![string]::IsNullOrWhiteSpace($_) })
if ($lines.Count -eq 0) {
    $lines = @('- （无新增提交）')
}

$parent = Split-Path -Parent $OutputPath
if ($parent) {
    [IO.Directory]::CreateDirectory($parent) | Out-Null
}
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[IO.File]::WriteAllText(
    $OutputPath,
    (($lines -join [Environment]::NewLine) + [Environment]::NewLine),
    $utf8NoBom
)

Write-Host "Release notes 已写入 $OutputPath（$($lines.Count) 条提交）。"
