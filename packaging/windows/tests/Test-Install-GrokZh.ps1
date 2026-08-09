Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
    param(
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )
    if (!$Condition) {
        throw "Assertion failed: $Message"
    }
}

function Assert-Throws {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Action,
        [Parameter(Mandatory = $true)][string]$Message
    )
    $threw = $false
    try {
        & $Action
    } catch {
        $threw = $true
    }
    Assert-True $threw $Message
}

$windowsDir = Split-Path -Parent $PSScriptRoot
$installer = Join-Path $windowsDir 'Install-GrokZh.ps1'
$agentShim = Join-Path $windowsDir 'agent-zh.cmd'
$guide = Join-Path $windowsDir 'INSTALL-WINDOWS.md'
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ("grok-zh-installer-test-" + [Guid]::NewGuid().ToString('N'))

try {
    $package = Join-Path $testRoot 'package'
    New-Item -ItemType Directory -Path $package -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $package 'grok-zh.exe') -Value 'fake-grok-zh' -Encoding Ascii
    Set-Content -LiteralPath (Join-Path $package 'rg.exe') -Value 'fake-ripgrep' -Encoding Ascii
    Copy-Item -LiteralPath $agentShim -Destination (Join-Path $package 'agent-zh.cmd')
    Copy-Item -LiteralPath $installer -Destination (Join-Path $package 'Install-GrokZh.ps1')
    Copy-Item -LiteralPath $guide -Destination (Join-Path $package 'INSTALL-WINDOWS.md')
    Set-Content -LiteralPath (Join-Path $package 'BUILD-INFO.txt') `
        -Value "Version: installer-test`nTarget: x86_64-pc-windows-gnu" -Encoding UTF8

    $names = @('grok-zh.exe', 'agent-zh.cmd', 'rg.exe', 'Install-GrokZh.ps1', 'INSTALL-WINDOWS.md')
    $lines = foreach ($name in $names) {
        $hash = (Get-FileHash -LiteralPath (Join-Path $package $name) -Algorithm SHA256).Hash
        "$hash  $name"
    }
    $lines | Set-Content -LiteralPath (Join-Path $package 'SHA256SUMS.txt') -Encoding UTF8

    Assert-Throws {
        & $installer -PackageDir $package -InstallDir $testRoot `
            -GrokHome (Join-Path $testRoot 'unused-home') -NoPathUpdate -Force -Confirm:$false
    } 'installer allowed PackageDir and InstallDir to overlap'

    $overlapHome = Join-Path $testRoot 'overlap-home'
    New-Item -ItemType Directory -Path $overlapHome -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $overlapHome 'auth.json') -Value 'must survive' -Encoding Ascii
    Assert-Throws {
        & $installer -PackageDir $package -InstallDir (Join-Path $overlapHome 'community-bin') `
            -GrokHome $overlapHome -NoPathUpdate -Force -Confirm:$false
    } 'installer allowed InstallDir to overlap GROK_HOME'
    Assert-True (Test-Path -LiteralPath (Join-Path $overlapHome 'auth.json')) 'overlap rejection changed shared auth data'

    $defaultInstall = Join-Path $testRoot 'default-install'
    & $installer -PackageDir $package -InstallDir $defaultInstall `
        -GrokHome (Join-Path $testRoot 'unused-home') -NoPathUpdate -Confirm:$false
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'grok-zh.exe')) 'grok-zh.exe was not installed'
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'agent-zh.cmd')) 'agent-zh.cmd was not installed'
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'rg.exe')) 'rg.exe was not installed beside grok-zh.exe'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $defaultInstall 'grok.cmd'))) 'default install unexpectedly created grok.cmd'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $defaultInstall 'agent.cmd'))) 'default install unexpectedly created agent.cmd'

    & $installer -PackageDir $package -InstallDir $defaultInstall `
        -GrokHome (Join-Path $testRoot 'unused-home') -NoPathUpdate -Confirm:$false
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall '.grok-zh-install.json')) 'reinstall did not retain installer ownership marker'

    $fakeHome = Join-Path $testRoot 'shared-home'
    $fakeOfficialBin = Join-Path $fakeHome 'bin'
    New-Item -ItemType Directory -Path $fakeOfficialBin -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $fakeOfficialBin 'grok.exe') -Value 'fake-official-grok' -Encoding Ascii
    Set-Content -LiteralPath (Join-Path $fakeOfficialBin 'agent.exe') -Value 'fake-official-agent' -Encoding Ascii
    Set-Content -LiteralPath (Join-Path $fakeHome 'auth.json') -Value '{"token":"must-survive"}' -Encoding Ascii
    Set-Content -LiteralPath (Join-Path $fakeHome 'config.toml') -Value '# must survive' -Encoding Ascii

    $takeoverInstall = Join-Path $testRoot 'takeover-install'
    & $installer -PackageDir $package -InstallDir $takeoverInstall -GrokHome $fakeHome `
        -UninstallOfficial -NoPathUpdate -Confirm:$false
    Assert-True (Test-Path -LiteralPath (Join-Path $takeoverInstall 'grok.cmd')) 'takeover did not create grok.cmd'
    Assert-True (Test-Path -LiteralPath (Join-Path $takeoverInstall 'agent.cmd')) 'takeover did not create agent.cmd'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $fakeOfficialBin 'grok.exe'))) 'official grok.exe was not moved'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $fakeOfficialBin 'agent.exe'))) 'official agent.exe was not moved'
    Assert-True (Test-Path -LiteralPath (Join-Path $fakeHome 'auth.json')) 'shared auth.json was changed'
    Assert-True (Test-Path -LiteralPath (Join-Path $fakeHome 'config.toml')) 'shared config.toml was changed'
    Assert-True (@(Get-ChildItem -LiteralPath (Join-Path $takeoverInstall 'official-backup') -Filter 'grok.exe' -Recurse -File).Count -eq 1) 'official grok.exe backup is missing'
    Assert-True (@(Get-ChildItem -LiteralPath (Join-Path $takeoverInstall 'official-backup') -Filter 'agent.exe' -Recurse -File).Count -eq 1) 'official agent.exe backup is missing'

    & $installer -PackageDir $package -InstallDir $takeoverInstall -GrokHome $fakeHome `
        -NoPathUpdate -Confirm:$false
    Assert-True (!(Test-Path -LiteralPath (Join-Path $takeoverInstall 'grok.cmd'))) 'reinstall without takeover retained grok.cmd'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $takeoverInstall 'agent.cmd'))) 'reinstall without takeover retained agent.cmd'
    Assert-True (@(Get-ChildItem -LiteralPath (Join-Path $takeoverInstall 'official-backup') -Filter 'grok.exe' -Recurse -File).Count -eq 1) 'reinstall lost the official grok.exe backup'
    Assert-True (@(Get-ChildItem -LiteralPath (Join-Path $takeoverInstall 'official-backup') -Filter 'agent.exe' -Recurse -File).Count -eq 1) 'reinstall lost the official agent.exe backup'

    $whatIfInstall = Join-Path $testRoot 'whatif-install'
    $whatIfHome = Join-Path $testRoot 'whatif-home'
    $whatIfOfficialBin = Join-Path $whatIfHome 'bin'
    New-Item -ItemType Directory -Path $whatIfOfficialBin -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $whatIfOfficialBin 'grok.exe') -Value 'must survive WhatIf' -Encoding Ascii
    Set-Content -LiteralPath (Join-Path $whatIfHome 'auth.json') -Value 'must survive WhatIf' -Encoding Ascii
    & $installer -PackageDir $package -InstallDir $whatIfInstall `
        -GrokHome $whatIfHome -UninstallOfficial -NoPathUpdate -WhatIf
    Assert-True (!(Test-Path -LiteralPath $whatIfInstall)) '-WhatIf created an install directory'
    Assert-True (Test-Path -LiteralPath (Join-Path $whatIfOfficialBin 'grok.exe')) '-WhatIf moved an official command'
    Assert-True (Test-Path -LiteralPath (Join-Path $whatIfHome 'auth.json')) '-WhatIf changed shared auth data'

    Write-Host 'Windows installer tests passed.'
} finally {
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
