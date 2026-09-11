$ErrorActionPreference = 'Stop'
$scriptPath = Join-Path (Split-Path -Parent $PSScriptRoot) 'validate-release-policy.ps1'
$policy = [pscustomobject]@{ publish_legacy_sha256 = $true; legacy_bridge_tag = $null }
if (!(& $scriptPath -Policy $policy -Releases @() -CurrentTag release-v1.0.16)) { throw '过渡期应继续发布 sidecar。' }
$policy.publish_legacy_sha256 = $false
function Assert-Fails($Action) {
    $failed = $false
    try { & $Action | Out-Null } catch { $failed = $true }
    if (!$failed) { throw '不完整的退休策略未被拒绝。' }
}
Assert-Fails { & $scriptPath -Policy $policy -Releases @() -CurrentTag release-v1.0.30 }
$policy.legacy_bridge_tag = 'release-v1.0.20'
$assets = @('windows-x86_64-gnu.zip', 'macos-aarch64.tar.gz', 'linux-x86_64-gnu.tar.gz') | ForEach-Object {
    $name = "grok-zh-1.0.20-$_"
    foreach ($file in @($name, "$name.sha256")) { [pscustomobject]@{ name = $file; state = 'uploaded'; digest = 'sha256:' + ('ab' * 32) } }
}
$bridge = [pscustomobject]@{ tag_name = $policy.legacy_bridge_tag; draft = $false; prerelease = $false; immutable = $true; assets = @($assets) }
$oldest = [pscustomobject]@{ tag_name = 'v1.0.8'; draft = $false; prerelease = $false; immutable = $true; assets = @(
    'grok-zh-1.0.8-windows-x86_64-gnu.zip', 'grok-zh-1.0.8-windows-x86_64-gnu.zip.sha256'
) | ForEach-Object { [pscustomobject]@{ name = $_; state = 'uploaded'; digest = 'sha256:' + ('ab' * 32) } } }
if (& $scriptPath -Policy $policy -Releases @($bridge, $oldest) -CurrentTag release-v1.0.30) { throw '退休策略应只公开归档。' }
if (& $scriptPath -Policy $policy -Releases @($bridge, $oldest) -CurrentTag release-v1.0.30-rc.1) { throw '预发布也可以停止 sidecar。' }
Assert-Fails { & $scriptPath -Policy $policy -Releases @($bridge) -CurrentTag release-v1.0.30 }
$oldest.immutable = $false
Assert-Fails { & $scriptPath -Policy $policy -Releases @($bridge, $oldest) -CurrentTag release-v1.0.30 }
$oldest.immutable = $true
$savedOldAssets = $oldest.assets; $oldest.assets = @($oldest.assets[0])
Assert-Fails { & $scriptPath -Policy $policy -Releases @($bridge, $oldest) -CurrentTag release-v1.0.30 }
$oldest.assets = $savedOldAssets
Assert-Fails { & $scriptPath -Policy $policy -Releases @($bridge) -CurrentTag v1.0.8 }
Assert-Fails { & $scriptPath -Policy $policy -Releases @($bridge) -CurrentTag release-v1.0.20 }
$bridge.immutable = $false
Assert-Fails { & $scriptPath -Policy $policy -Releases @($bridge, $oldest) -CurrentTag release-v1.0.30 }
$bridge.immutable = $true; $bridge.assets = @($assets | Select-Object -Skip 1)
Assert-Fails { & $scriptPath -Policy $policy -Releases @($bridge, $oldest) -CurrentTag release-v1.0.30 }
$policy.legacy_bridge_tag = 'release-v1.0.13'
Assert-Fails { & $scriptPath -Policy $policy -Releases @() -CurrentTag release-v1.0.30 }
Write-Host 'Release 过渡与退休策略测试通过。'
