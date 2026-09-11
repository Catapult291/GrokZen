param(
    [Parameter(Mandatory = $true)]$Policy,
    [Parameter(Mandatory = $true)][AllowEmptyCollection()][object[]]$Releases,
    [Parameter(Mandatory = $true)][string]$CurrentTag
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($Policy.publish_legacy_sha256 -isnot [bool]) { throw 'publish_legacy_sha256 必须是布尔值。' }
if ($Policy.publish_legacy_sha256) { return $true }
if ($CurrentTag -cnotmatch '^release-v(\d+\.\d+\.\d+)(?:-.+)?$') { throw '旧 v* 桥接发布必须保留独立校验文件。' }
$currentVersion = [version]$matches[1]
$bridgeTag = [string]$Policy.legacy_bridge_tag
if ($bridgeTag -cnotmatch '^release-v(\d+\.\d+\.\d+)$' -or [version]$matches[1] -ge $currentVersion) {
    throw '关闭独立校验文件前，必须指定已发布且版本更早的正式 legacy_bridge_tag。'
}
$version = $matches[1]
$bridges = @($Releases | Where-Object { $_.tag_name -ceq $bridgeTag })
if ($bridges.Count -ne 1 -or $bridges[0].draft -or $bridges[0].prerelease -or !$bridges[0].immutable) {
    throw '必须永久保留一个不可变的正式过渡 Release。'
}
$expected = @('windows-x86_64-gnu.zip', 'macos-aarch64.tar.gz', 'linux-x86_64-gnu.tar.gz') |
    ForEach-Object { $name = "grok-zh-$version-$_"; $name; "$name.sha256" } | Sort-Object
$assets = @($bridges[0].assets)
$actual = @($assets | ForEach-Object name | Sort-Object)
if ($assets.Count -ne 6 -or ($actual -join "`n") -cne ($expected -join "`n") -or
    @($assets | Where-Object { $_.state -cne 'uploaded' -or $_.digest -cnotmatch '^sha256:[0-9a-fA-F]{64}$' }).Count -ne 0) {
    throw '指定过渡 Release 必须保留完整的三平台安装包和三个独立校验文件。'
}
$oldest = @($Releases | Where-Object { $_.tag_name -ceq 'v1.0.8' })
$oldNames = @('grok-zh-1.0.8-windows-x86_64-gnu.zip', 'grok-zh-1.0.8-windows-x86_64-gnu.zip.sha256')
if ($oldest.Count -ne 1 -or $oldest[0].draft -or $oldest[0].prerelease -or !$oldest[0].immutable) {
    throw '必须同时永久保留最早客户端使用的不可变正式 v1.0.8 桥接 Release。'
}
$oldAssets = @($oldest[0].assets)
if ($oldAssets.Count -ne 2 -or ((@($oldAssets | ForEach-Object name | Sort-Object)) -join "`n") -cne ($oldNames -join "`n") -or
    @($oldAssets | Where-Object { $_.state -cne 'uploaded' -or $_.digest -cnotmatch '^sha256:[0-9a-fA-F]{64}$' }).Count -ne 0) {
    throw 'v1.0.8 必须保留原有 Windows 安装包和独立校验文件。'
}
return $false
