Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-True {
    param(
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )
    if (!$Condition) {
        throw "断言失败：$Message"
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
    } '安装器错误地允许 PackageDir 与 InstallDir 重叠'

    $overlapHome = Join-Path $testRoot 'overlap-home'
    New-Item -ItemType Directory -Path $overlapHome -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $overlapHome 'auth.json') -Value 'must survive' -Encoding Ascii
    Assert-Throws {
        & $installer -PackageDir $package -InstallDir (Join-Path $overlapHome 'community-bin') `
            -GrokHome $overlapHome -NoPathUpdate -Force -Confirm:$false
    } '安装器错误地允许 InstallDir 与 GROK_HOME 重叠'
    Assert-True (Test-Path -LiteralPath (Join-Path $overlapHome 'auth.json')) '拒绝重叠路径时修改了共享认证数据'

    $defaultInstall = Join-Path $testRoot 'default-install'
    $defaultOutput = @(& $installer -PackageDir $package -InstallDir $defaultInstall `
        -GrokHome (Join-Path $testRoot 'unused-home') -NoPathUpdate -Confirm:$false 6>&1)
    $defaultOutputText = $defaultOutput -join "`n"
    Assert-True ($defaultOutputText.Contains('安装完成：')) '安装器未输出中文完成提示'
    Assert-True (!$defaultOutputText.Contains('Installation complete:')) '安装器仍输出旧英文完成提示'
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'grok-zh.exe')) '未安装 grok-zh.exe'
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'agent-zh.cmd')) '未安装 agent-zh.cmd'
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'rg.exe')) '未在 grok-zh.exe 同目录安装 rg.exe'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $defaultInstall 'grok.cmd'))) '默认安装不应创建 grok.cmd'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $defaultInstall 'agent.cmd'))) '默认安装不应创建 agent.cmd'

    & $installer -PackageDir $package -InstallDir $defaultInstall `
        -GrokHome (Join-Path $testRoot 'unused-home') -NoPathUpdate -Confirm:$false
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall '.grok-zh-install.json')) '重新安装后未保留安装器归属标记'

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
    Assert-True (Test-Path -LiteralPath (Join-Path $takeoverInstall 'grok.cmd')) '命令接管未创建 grok.cmd'
    Assert-True (Test-Path -LiteralPath (Join-Path $takeoverInstall 'agent.cmd')) '命令接管未创建 agent.cmd'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $fakeOfficialBin 'grok.exe'))) '未移动官方 grok.exe'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $fakeOfficialBin 'agent.exe'))) '未移动官方 agent.exe'
    Assert-True (Test-Path -LiteralPath (Join-Path $fakeHome 'auth.json')) '修改了共享 auth.json'
    Assert-True (Test-Path -LiteralPath (Join-Path $fakeHome 'config.toml')) '修改了共享 config.toml'
    Assert-True (@(Get-ChildItem -LiteralPath (Join-Path $takeoverInstall 'official-backup') -Filter 'grok.exe' -Recurse -File).Count -eq 1) '缺少官方 grok.exe 备份'
    Assert-True (@(Get-ChildItem -LiteralPath (Join-Path $takeoverInstall 'official-backup') -Filter 'agent.exe' -Recurse -File).Count -eq 1) '缺少官方 agent.exe 备份'

    & $installer -PackageDir $package -InstallDir $takeoverInstall -GrokHome $fakeHome `
        -NoPathUpdate -Confirm:$false
    Assert-True (!(Test-Path -LiteralPath (Join-Path $takeoverInstall 'grok.cmd'))) '未启用接管的重新安装仍保留 grok.cmd'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $takeoverInstall 'agent.cmd'))) '未启用接管的重新安装仍保留 agent.cmd'
    Assert-True (@(Get-ChildItem -LiteralPath (Join-Path $takeoverInstall 'official-backup') -Filter 'grok.exe' -Recurse -File).Count -eq 1) '重新安装丢失了官方 grok.exe 备份'
    Assert-True (@(Get-ChildItem -LiteralPath (Join-Path $takeoverInstall 'official-backup') -Filter 'agent.exe' -Recurse -File).Count -eq 1) '重新安装丢失了官方 agent.exe 备份'

    $whatIfInstall = Join-Path $testRoot 'whatif-install'
    $whatIfHome = Join-Path $testRoot 'whatif-home'
    $whatIfOfficialBin = Join-Path $whatIfHome 'bin'
    New-Item -ItemType Directory -Path $whatIfOfficialBin -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $whatIfOfficialBin 'grok.exe') -Value 'must survive WhatIf' -Encoding Ascii
    Set-Content -LiteralPath (Join-Path $whatIfHome 'auth.json') -Value 'must survive WhatIf' -Encoding Ascii
    & $installer -PackageDir $package -InstallDir $whatIfInstall `
        -GrokHome $whatIfHome -UninstallOfficial -NoPathUpdate -WhatIf
    Assert-True (!(Test-Path -LiteralPath $whatIfInstall)) '-WhatIf 不应创建安装目录'
    Assert-True (Test-Path -LiteralPath (Join-Path $whatIfOfficialBin 'grok.exe')) '-WhatIf 不应移动官方命令'
    Assert-True (Test-Path -LiteralPath (Join-Path $whatIfHome 'auth.json')) '-WhatIf 不应修改共享认证数据'

    Write-Host 'Windows 安装器测试通过。'
} finally {
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
