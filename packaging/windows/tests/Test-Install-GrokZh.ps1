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

function Invoke-InteractiveInstaller {
    param(
        [Parameter(Mandatory = $true)][string]$InstallerPath,
        [Parameter(Mandatory = $true)][string]$PackagePath,
        [Parameter(Mandatory = $true)][string]$InstallPath,
        [Parameter(Mandatory = $true)][string]$SharedHome,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$InputLines
    )

    $powerShellExe = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = $powerShellExe
    $scriptedAnswers = $InputLines -join ';'
    $startInfo.Arguments = '-NoLogo -NoProfile -ExecutionPolicy Bypass -File "{0}" -PackageDir "{1}" -InstallDir "{2}" -GrokHome "{3}" -NoPathUpdate -InteractiveCommandSetup -ScriptedCommandSetupAnswers "{4}" -ShowProgress' -f `
        $InstallerPath, $PackagePath, $InstallPath, $SharedHome, $scriptedAnswers
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true

    $process = [Diagnostics.Process]::Start($startInfo)
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    foreach ($line in $InputLines) {
        $process.StandardInput.WriteLine($line)
    }
    $process.StandardInput.Close()
    $process.WaitForExit()
    return [pscustomobject]@{
        ExitCode = $process.ExitCode
        Stdout = $stdoutTask.Result
        Stderr = $stderrTask.Result
    }
}

$windowsDir = Split-Path -Parent $PSScriptRoot
$installer = Join-Path $windowsDir 'Install-GrokZh.ps1'
$agentShim = Join-Path $windowsDir 'agent-zh.cmd'
$oneClickLauncher = Join-Path $windowsDir '一键安装.cmd'
$commandSetupLauncher = Join-Path $windowsDir '[可选]替换原始启动方式.cmd'
$guide = Join-Path $windowsDir 'INSTALL-WINDOWS.md'
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ("grok-zh-installer-test-" + [Guid]::NewGuid().ToString('N'))

try {
    $package = Join-Path $testRoot 'package'
    New-Item -ItemType Directory -Path $package -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $package 'grok-zh.exe') -Value 'fake-grok-zh' -Encoding Ascii
    Set-Content -LiteralPath (Join-Path $package 'rg.exe') -Value 'fake-ripgrep' -Encoding Ascii
    Copy-Item -LiteralPath $agentShim -Destination (Join-Path $package 'agent-zh.cmd')
    Copy-Item -LiteralPath $oneClickLauncher -Destination (Join-Path $package '一键安装.cmd')
    Copy-Item -LiteralPath $commandSetupLauncher -Destination (Join-Path $package '[可选]替换原始启动方式.cmd')
    Copy-Item -LiteralPath $installer -Destination (Join-Path $package 'Install-GrokZh.ps1')
    Copy-Item -LiteralPath $guide -Destination (Join-Path $package 'INSTALL-WINDOWS.md')
    Set-Content -LiteralPath (Join-Path $package 'BUILD-INFO.txt') `
        -Value "Version: installer-test`nTarget: x86_64-pc-windows-gnu" -Encoding UTF8
    Set-Content -LiteralPath (Join-Path $package 'LICENSE-grok-build.txt') `
        -Value 'Apache-2.0 test license' -Encoding UTF8
    $licenseFiles = [ordered]@{
        'licenses/ripgrep/COPYING' = 'ripgrep COPYING'
        'licenses/ripgrep/LICENSE-MIT' = 'ripgrep MIT'
        'licenses/ripgrep/UNLICENSE' = 'ripgrep UNLICENSE'
        'licenses/project/THIRD-PARTY-NOTICES' = 'project third-party notices'
        'licenses/project/THIRD_PARTY_NOTICES.md' = 'project tool notices'
        'licenses/project/NOTICE' = 'project NOTICE'
    }
    foreach ($entry in $licenseFiles.GetEnumerator()) {
        $path = Join-Path $package $entry.Key
        New-Item -ItemType Directory -Path (Split-Path -Parent $path) -Force | Out-Null
        Set-Content -LiteralPath $path -Value $entry.Value -Encoding UTF8
    }

    $names = @(
        'grok-zh.exe',
        'agent-zh.cmd',
        'rg.exe',
        '一键安装.cmd',
        '[可选]替换原始启动方式.cmd',
        'Install-GrokZh.ps1',
        'INSTALL-WINDOWS.md',
        'LICENSE-grok-build.txt',
        'BUILD-INFO.txt',
        'licenses/ripgrep/COPYING',
        'licenses/ripgrep/LICENSE-MIT',
        'licenses/ripgrep/UNLICENSE',
        'licenses/project/THIRD-PARTY-NOTICES',
        'licenses/project/THIRD_PARTY_NOTICES.md',
        'licenses/project/NOTICE'
    )
    $lines = foreach ($name in $names) {
        $hash = (Get-FileHash -LiteralPath (Join-Path $package $name) -Algorithm SHA256).Hash
        "$hash  $name"
    }
    [IO.File]::WriteAllLines(
        (Join-Path $package 'SHA256SUMS.txt'),
        [string[]]$lines,
        (New-Object Text.UTF8Encoding($false))
    )

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
    Assert-True ($defaultOutputText.Contains('新终端，输入 grok-zh')) '安装器未提示在新终端启动 grok-zh'
    Assert-True ($defaultOutputText.Contains('[可选]替换原始启动方式.cmd')) '安装器未提示可选命令接管入口'
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'grok-zh.exe')) '未安装 grok-zh.exe'
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'agent-zh.cmd')) '未安装 agent-zh.cmd'
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'rg.exe')) '未在 grok-zh.exe 同目录安装 rg.exe'
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'BUILD-INFO.txt')) '未安装受校验的构建信息'
    Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall 'LICENSE-grok-build.txt')) '未安装受校验的主许可证'
    foreach ($relativeLicense in $licenseFiles.Keys) {
        Assert-True (Test-Path -LiteralPath (Join-Path $defaultInstall $relativeLicense) -PathType Leaf) "未安装受校验的许可证文件：$relativeLicense"
    }
    Assert-True (!(Test-Path -LiteralPath (Join-Path $defaultInstall '一键安装.cmd'))) '一键入口只应保留在解压包根，不应复制到安装目录'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $defaultInstall '[可选]替换原始启动方式.cmd'))) '可选入口只应保留在解压包根，不应复制到安装目录'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $defaultInstall 'Install-GrokZh.ps1'))) '安装脚本只应保留在解压包根，不应复制到运行目录'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $defaultInstall 'INSTALL-WINDOWS.md'))) '安装说明只应保留在解压包根，不应复制到运行目录'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $defaultInstall 'grok.cmd'))) '默认安装不应创建 grok.cmd'
    Assert-True (!(Test-Path -LiteralPath (Join-Path $defaultInstall 'agent.cmd'))) '默认安装不应创建 agent.cmd'
    $installedManifestPath = Join-Path $defaultInstall 'SHA256SUMS.txt'
    Assert-True (Test-Path -LiteralPath $installedManifestPath -PathType Leaf) '安装目录缺少自身文件校验清单'
    foreach ($line in Get-Content -LiteralPath $installedManifestPath -Encoding UTF8) {
        Assert-True ($line -match '^([0-9A-Fa-f]{64})  (.+)$') "安装目录校验清单格式无效：$line"
        $installedHash = $matches[1].ToUpperInvariant()
        $installedName = $matches[2]
        $installedPath = Join-Path $defaultInstall $installedName
        Assert-True (Test-Path -LiteralPath $installedPath -PathType Leaf) "安装目录校验清单引用了不存在的文件：$installedName"
        Assert-True ((Get-FileHash -LiteralPath $installedPath -Algorithm SHA256).Hash -eq $installedHash) "安装目录文件哈希不匹配：$installedName"
    }
    $installedManifestText = Get-Content -LiteralPath $installedManifestPath -Raw -Encoding UTF8
    Assert-True (!$installedManifestText.Contains('一键安装.cmd')) '安装目录校验清单不应引用仅位于解压包根的一键入口'
    Assert-True (!$installedManifestText.Contains('[可选]替换原始启动方式.cmd')) '安装目录校验清单不应引用仅位于解压包根的可选入口'

    $progressInstall = Join-Path $testRoot 'progress-install'
    $progressOutput = @(& $installer -PackageDir $package -InstallDir $progressInstall `
        -GrokHome (Join-Path $testRoot 'unused-progress-home') -NoPathUpdate `
        -ShowProgress -Confirm:$false 6>&1)
    $progressOutputText = $progressOutput -join "`n"
    Assert-True (Test-Path -LiteralPath (Join-Path $progressInstall 'grok-zh.exe')) '-ShowProgress 安装未完成'
    Assert-True ($progressOutputText.Contains('[1/4]')) '-ShowProgress 未输出校验阶段进度'
    Assert-True ($progressOutputText.Contains('[2/4]')) '-ShowProgress 未输出复制阶段进度'

    $ps5DefaultInstall = Join-Path $testRoot 'ps5-default-package-install'
    $packageInstaller = Join-Path $package 'Install-GrokZh.ps1'
    $ps5Output = @(& "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" `
        -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass `
        -File $packageInstaller `
        -InstallDir $ps5DefaultInstall `
        -GrokHome (Join-Path $testRoot 'unused-ps5-home') `
        -NoPathUpdate 2>&1)
    Assert-True ($LASTEXITCODE -eq 0) "Windows PowerShell 5.1 -File 默认 PackageDir 调用失败：$($ps5Output -join "`n")"
    Assert-True (Test-Path -LiteralPath (Join-Path $ps5DefaultInstall 'grok-zh.exe')) 'PS5.1 -File 未从脚本目录解析默认 PackageDir'

    $interactiveKeepInstall = Join-Path $testRoot 'interactive-keep-install'
    $interactiveKeep = Invoke-InteractiveInstaller `
        -InstallerPath $packageInstaller `
        -PackagePath $package `
        -InstallPath $interactiveKeepInstall `
        -SharedHome (Join-Path $testRoot 'interactive-keep-home') `
        -InputLines @('1')
    Assert-True ($interactiveKeep.ExitCode -eq 0) "交互方案 1 执行失败：$($interactiveKeep.Stderr)"
    Assert-True (Test-Path -LiteralPath (Join-Path $interactiveKeepInstall 'grok.cmd')) '交互方案 1 未创建 grok.cmd'
    Assert-True (Test-Path -LiteralPath (Join-Path $interactiveKeepInstall 'agent.cmd')) '交互方案 1 未创建 agent.cmd'
    # Windows PowerShell 5.1 redirects Chinese text through the active OEM
    # codepage, so assert the ASCII command tokens that identify the branch.
    Assert-True ($interactiveKeep.Stdout.Contains('grok --version; agent --help')) '交互方案 1 的验证命令未切换到 grok/agent'
    Assert-True (!$interactiveKeep.Stdout.Contains('grok-zh --version; agent-zh --help')) '交互方案 1 仍输出普通安装的验证命令'

    $interactiveEofInstall = Join-Path $testRoot 'interactive-eof-install'
    $interactiveEof = Invoke-InteractiveInstaller `
        -InstallerPath $packageInstaller `
        -PackagePath $package `
        -InstallPath $interactiveEofInstall `
        -SharedHome (Join-Path $testRoot 'interactive-eof-home') `
        -InputLines @()
    Assert-True ($interactiveEof.ExitCode -eq 0) "交互输入结束时未能安全取消：$($interactiveEof.Stderr)"
    Assert-True (!(Test-Path -LiteralPath $interactiveEofInstall)) '交互输入结束后仍创建了安装目录'

    $interactiveNoOfficialInstall = Join-Path $testRoot 'interactive-no-official-install'
    $interactiveNoOfficial = Invoke-InteractiveInstaller `
        -InstallerPath $packageInstaller `
        -PackagePath $package `
        -InstallPath $interactiveNoOfficialInstall `
        -SharedHome (Join-Path $testRoot 'interactive-no-official-home') `
        -InputLines @('2')
    Assert-True ($interactiveNoOfficial.ExitCode -eq 0) "未安装官方程序时交互方案 2 执行失败：$($interactiveNoOfficial.Stderr)"
    Assert-True (Test-Path -LiteralPath (Join-Path $interactiveNoOfficialInstall 'grok.cmd')) '无官方程序时交互方案 2 未退化为命令接管'
    Assert-True (Test-Path -LiteralPath (Join-Path $interactiveNoOfficialInstall 'agent.cmd')) '无官方程序时交互方案 2 未创建 agent.cmd'

    $interactiveRejectHome = Join-Path $testRoot 'interactive-reject-home'
    $interactiveRejectBin = Join-Path $interactiveRejectHome 'bin'
    New-Item -ItemType Directory -Path $interactiveRejectBin -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $interactiveRejectBin 'grok.exe') -Value 'unsigned-program' -Encoding Ascii
    $interactiveRejectInstall = Join-Path $testRoot 'interactive-reject-install'
    $interactiveReject = Invoke-InteractiveInstaller `
        -InstallerPath $packageInstaller `
        -PackagePath $package `
        -InstallPath $interactiveRejectInstall `
        -SharedHome $interactiveRejectHome `
        -InputLines @('2', '3')
    Assert-True ($interactiveReject.ExitCode -eq 0) "未签名程序拒绝流程执行失败：$($interactiveReject.Stderr)"
    Assert-True (!(Test-Path -LiteralPath $interactiveRejectInstall)) '拒绝未签名程序后取消仍创建了安装目录'
    Assert-True (Test-Path -LiteralPath (Join-Path $interactiveRejectBin 'grok.exe')) '交互菜单错误地移动了未签名程序'

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
    $takeoverOutput = @(& $installer -PackageDir $package -InstallDir $takeoverInstall -GrokHome $fakeHome `
        -UninstallOfficial -NoPathUpdate -Confirm:$false 6>&1)
    $takeoverOutputText = $takeoverOutput -join "`n"
    Assert-True ($takeoverOutputText.Contains('新终端，输入 grok 启动中文版')) '命令接管后仍未提示使用 grok 启动'
    Assert-True ($takeoverOutputText.Contains('输入 agent 启动代理模式')) '命令接管后仍未提示使用 agent 启动代理模式'
    Assert-True (!$takeoverOutputText.Contains('新终端，输入 grok-zh 启动中文版')) '命令接管后仍把 grok-zh 显示为主启动命令'
    Assert-True ($takeoverOutputText.Contains('验证命令：grok --version; agent --help')) '命令接管后的验证命令未切换到 grok/agent'
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

    $oneClickText = Get-Content -LiteralPath $oneClickLauncher -Raw
    $commandSetupText = Get-Content -LiteralPath $commandSetupLauncher -Raw
    Assert-True ($oneClickText.Contains('ExecutionPolicy Bypass')) '一键入口未使用仅限子进程的 ExecutionPolicy Bypass'
    Assert-True ($oneClickText.Contains('%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe')) '一键入口未使用受信任的系统 Windows PowerShell 路径'
    Assert-True ($oneClickText.Contains('-PackageDir "%~dp0."')) '一键入口未安全传入解压包目录'
    Assert-True ($oneClickText.Contains('pause')) '一键入口未在安装结束后等待用户关闭窗口'
    Assert-True ($oneClickText.Contains('INSTALL_EXIT')) '一键入口未保留安装脚本退出码'
    Assert-True ($oneClickText.Contains('-ShowProgress')) '一键入口未启用安装进度'
    Assert-True ($oneClickText.Contains('%~dp0Install-GrokZh.ps1')) '一键入口未从自身目录定位安装器'
    Assert-True ($commandSetupText.Contains('-InteractiveCommandSetup')) '可选入口未启用交互式命令接管菜单'
    Assert-True ($commandSetupText.Contains('-ShowProgress')) '可选入口未启用安装进度'
    Assert-True ($commandSetupText.Contains('%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe')) '可选入口未使用受信任的系统 Windows PowerShell 路径'
    Assert-True ($commandSetupText.Contains('-PackageDir "%~dp0."')) '可选入口未安全传入解压包目录'
    Assert-True ($commandSetupText.Contains('pause')) '可选入口未在安装结束后等待用户关闭窗口'
    Assert-True ($commandSetupText.Contains('INSTALL_EXIT')) '可选入口未保留安装脚本退出码'
    foreach ($launcher in @($oneClickLauncher, $commandSetupLauncher)) {
        $nonAsciiBytes = @([IO.File]::ReadAllBytes($launcher) | Where-Object { $_ -gt 127 })
        Assert-True ($nonAsciiBytes.Count -eq 0) "CMD 启动器内容必须保持 ASCII，避免旧版控制台乱码：$launcher"
    }

    Write-Host 'Windows 安装器测试通过。'
} finally {
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
