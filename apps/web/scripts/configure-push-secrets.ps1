$ErrorActionPreference = 'Stop'

<#
.SYNOPSIS
    为 ClassFlow Worker 生成一对独立 VAPID 密钥并直接写入 Cloudflare secrets。

.DESCRIPTION
    密钥只在当前 PowerShell 进程内存中短暂存在：
    1. web-push CLI 生成；
    2. private/public 分别通过 stdin 写入 wrangler secret；
    3. 不打印、不写磁盘、不进入 Git。

    公钥虽然本身不敏感，但仍作为 Worker secret 保存，避免配置文件里维护两份事实源；
    浏览器通过 /api/push/config 获取它。
#>

$keysJson = & npx web-push generate-vapid-keys --json
if ($LASTEXITCODE -ne 0) {
    throw "web-push VAPID key generation failed, exit=$LASTEXITCODE"
}

$keys = $keysJson | ConvertFrom-Json
if (-not $keys.publicKey -or -not $keys.privateKey) {
    throw 'web-push did not return a complete VAPID key pair'
}

$keys.privateKey | & npx wrangler secret put VAPID_PRIVATE_KEY
if ($LASTEXITCODE -ne 0) {
    throw "wrangler failed to store VAPID_PRIVATE_KEY, exit=$LASTEXITCODE"
}

$keys.publicKey | & npx wrangler secret put VAPID_PUBLIC_KEY
if ($LASTEXITCODE -ne 0) {
    throw "wrangler failed to store VAPID_PUBLIC_KEY, exit=$LASTEXITCODE"
}

Write-Host 'ClassFlow VAPID secrets configured successfully.'
