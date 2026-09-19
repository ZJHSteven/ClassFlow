<#
.SYNOPSIS
    SmartClass 登录态低频保活 / 寿命观测脚本。

.DESCRIPTION
    这个脚本只复用“已经在 Chrome 里完成登录”的 SmartClass 会话，不读取、输出或保存任何 Cookie 值。

    每轮探测做三件事：
      1. 通过 browser-harness-js 找到当前 SmartClass 课程列表标签页；没有时就在默认浏览器上下文新建一个。
      2. 在该标签页所属的正常浏览器会话中请求一次 /UserSpace/CourseList.aspx，作为低频心跳。
      3. 调用页面本身使用的 GetCurrentUser，判断服务端是否仍把当前浏览器识别为已登录用户。

    同时，只读取三个 SmartClass Cookie 的“名称 / 是否 Session Cookie / 过期时间”元数据，用来观察滑动过期行为。
    Cookie 的 value 从始至终都不会进入脚本日志。

    默认每 10 分钟一次，持续 12 小时。脚本运行期间会调用 Windows SetThreadExecutionState：
    只阻止系统进入睡眠，不阻止显示器熄屏；脚本退出后会立即恢复默认电源行为。
#>

[CmdletBinding()]
param(
    [ValidateRange(1, 1440)]
    [int]$IntervalMinutes = 10,

    [ValidateRange(1, 72)]
    [int]$DurationHours = 12,

    # 主要给烟雾测试使用；0 表示不限制轮数，按 DurationHours 运行。
    [ValidateRange(0, 100000)]
    [int]$MaxIterations = 0,

    [string]$LogPath = ""
)

$ErrorActionPreference = 'Stop'

# 日志放在仓库 tmp/ 下。该目录已经被 .gitignore 忽略，不会把实验日志提交进 Git。
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$tmpDir = Join-Path $repoRoot 'tmp'
New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null

if ([string]::IsNullOrWhiteSpace($LogPath)) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $LogPath = Join-Path $tmpDir "smartclass-session-probe-$stamp.jsonl"
}

$summaryPath = [System.IO.Path]::ChangeExtension($LogPath, '.summary.json')

# 直接使用已经安装好的 browser-harness-js；它只通过 CDP 驱动当前 Chrome，不需要复制浏览器凭据。
$browserHarness = (Get-Command 'browser-harness-js' -ErrorAction Stop).Source

# 让过夜实验期间机器不自动睡眠，否则“中间没有请求”既可能是会话失效，也可能只是 Windows 睡着了。
# 这里只设置 ES_SYSTEM_REQUIRED，不设置 ES_DISPLAY_REQUIRED，所以屏幕仍可正常熄灭。
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class ClassFlowPowerState {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint SetThreadExecutionState(uint esFlags);
}
'@

$ES_CONTINUOUS = 0x80000000
$ES_SYSTEM_REQUIRED = 0x00000001

function Invoke-BrowserHarness {
    <#
    .SYNOPSIS
        调用一次 browser-harness-js，并把退出码和标准输出统一包装起来。

    .PARAMETER Code
        要在 browser-harness-js REPL 中执行的 JavaScript。

    .PARAMETER SessionId
        可选的 CDP target session。传入时，Runtime/Page 等目标域调用会被固定到对应标签页。
    #>
    param(
        [Parameter(Mandatory = $true)]
        [string]$Code,
        [string]$SessionId = ''
    )

    if ([string]::IsNullOrWhiteSpace($SessionId)) {
        $output = & $browserHarness $Code 2>&1
    }
    else {
        $output = & $browserHarness '--session' $SessionId $Code 2>&1
    }

    $exitCode = $LASTEXITCODE
    $text = (($output | ForEach-Object { $_.ToString() }) -join "`n").Trim()

    [pscustomobject]@{
        ExitCode = $exitCode
        Output   = $text
    }
}

function Get-SmartClassSessionId {
    <#
    .SYNOPSIS
        找到当前 SmartClass 课程列表标签页并附加 CDP session。

    .DESCRIPTION
        每轮都重新 attach，避免某个旧 sessionId 因标签页重载或浏览器内部重建而失效。
        如果用户把课程列表页关掉了，就在当前 Chrome 默认上下文重新打开一页。
    #>

    $code = @'
await session.connect();
let tabs = await listPageTargets();
let target = tabs.find(t => t.url.includes('tmu.smartclass.cn/UserSpace/CourseList.aspx'));
if (!target) {
    const created = await session.Target.createTarget({ url: 'https://tmu.smartclass.cn/UserSpace/CourseList.aspx' });
    await new Promise(resolve => setTimeout(resolve, 2500));
    target = { targetId: created.targetId };
}
return String(await session.attach(target.targetId));
'@

    $result = Invoke-BrowserHarness -Code $code
    if ($result.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($result.Output)) {
        throw "无法附加 SmartClass 标签页：$($result.Output)"
    }

    return $result.Output.Trim()
}

function Invoke-SmartClassHeartbeat {
    <#
    .SYNOPSIS
        请求一次课程列表 HTML，作为低频、接近正常浏览器行为的心跳。

    .DESCRIPTION
        这里故意不执行整页 reload，因此不会重复下载 CSS、图片和课程封面。
        只获取 HTML 本身，网络负担远低于人工刷新页面。
    #>
    param([Parameter(Mandatory = $true)][string]$SessionId)

    $code = @'
const expression = `(() => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 15000);
    return fetch('/UserSpace/CourseList.aspx', {
        method: 'GET',
        credentials: 'include',
        cache: 'no-store',
        redirect: 'follow',
        signal: controller.signal
    }).then(async response => {
        const body = await response.text();
        clearTimeout(timer);
        return JSON.stringify({
            ok: response.ok,
            status: response.status,
            final_url: response.url,
            body_length: body.length,
            has_user_info_bootstrap: body.includes('userInfo'),
            has_cookie_bootstrap: body.includes('strCookie')
        });
    }).catch(error => {
        clearTimeout(timer);
        return JSON.stringify({ ok: false, error: String(error) });
    });
})()`;
return (await session.Runtime.evaluate({ expression, awaitPromise: true, returnByValue: true })).result.value;
'@

    $result = Invoke-BrowserHarness -Code $code -SessionId $SessionId
    if ($result.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($result.Output)) {
        throw "SmartClass 心跳失败：$($result.Output)"
    }

    return ($result.Output | ConvertFrom-Json)
}

function Get-SmartClassAuthState {
    <#
    .SYNOPSIS
        用 SmartClass 自己的当前用户接口确认“服务端登录态”是否仍有效。

    .DESCRIPTION
        只把 authenticated / HTTP 状态 / user_type 等诊断字段写入日志；
        不保存姓名、学号、账号或接口返回的任何 token。
    #>
    param([Parameter(Mandatory = $true)][string]$SessionId)

    $code = @'
const expression = `(() => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 15000);
    const url = (typeof getcsrf === 'function')
        ? getcsrf('/Interface/App/Home/GetCurrentUser')
        : '/Interface/App/Home/GetCurrentUser';
    return fetch(url, {
        method: 'GET',
        credentials: 'include',
        cache: 'no-store',
        signal: controller.signal
    }).then(async response => {
        const payload = await response.json().catch(() => null);
        clearTimeout(timer);
        const value = payload && payload.Value ? payload.Value : null;
        const userId = value && Number(value.UserID || 0);
        return JSON.stringify({
            ok: response.ok,
            http_status: response.status,
            api_success: !!(payload && payload.Success === true),
            authenticated: !!(payload && payload.Success === true && userId > 0),
            user_type: value ? (value.UserType ?? null) : null,
            message: payload ? (payload.Message ?? null) : null
        });
    }).catch(error => {
        clearTimeout(timer);
        return JSON.stringify({ ok: false, authenticated: false, error: String(error) });
    });
})()`;
return (await session.Runtime.evaluate({ expression, awaitPromise: true, returnByValue: true })).result.value;
'@

    $result = Invoke-BrowserHarness -Code $code -SessionId $SessionId
    if ($result.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($result.Output)) {
        throw "SmartClass 认证探针失败：$($result.Output)"
    }

    return ($result.Output | ConvertFrom-Json)
}

function Get-SmartClassCookieMetadata {
    <#
    .SYNOPSIS
        读取 SmartClass 关键 Cookie 的非敏感元数据。

    .DESCRIPTION
        这里刻意没有访问 c.value。日志只会出现 Cookie 名称、session/httpOnly 和 expires。
    #>

    $code = @'
await session.connect();
const names = new Set(['PowerCreatorCMSAuthenticationID', 'OnlineUser', 'platform_token']);
const cookies = (await session.Storage.getCookies({})).cookies
    .filter(c => c.domain === 'tmu.smartclass.cn' && names.has(c.name))
    .map(c => ({
        name: c.name,
        expires: c.expires,
        session: c.session,
        httpOnly: c.httpOnly,
        secure: c.secure
    }));
return JSON.stringify(cookies);
'@

    $result = Invoke-BrowserHarness -Code $code
    if ($result.ExitCode -ne 0) {
        throw "读取 Cookie 元数据失败：$($result.Output)"
    }

    if ([string]::IsNullOrWhiteSpace($result.Output)) {
        return @()
    }

    $cookies = @($result.Output | ConvertFrom-Json)
    foreach ($cookie in $cookies) {
        if ($cookie.expires -and [double]$cookie.expires -gt 0) {
            $seconds = [long][Math]::Floor([double]$cookie.expires)
            $cookie | Add-Member -NotePropertyName 'expires_local' -NotePropertyValue ([DateTimeOffset]::FromUnixTimeSeconds($seconds).ToLocalTime().ToString('o'))
        }
        else {
            $cookie | Add-Member -NotePropertyName 'expires_local' -NotePropertyValue $null
        }
    }

    return $cookies
}

function Write-JsonLine {
    param([Parameter(Mandatory = $true)][object]$Value)

    $json = $Value | ConvertTo-Json -Compress -Depth 8
    Add-Content -LiteralPath $LogPath -Value $json -Encoding utf8
}

$startedAt = Get-Date
$deadline = $startedAt.AddHours($DurationHours)
$iteration = 0
$authSuccessCount = 0
$firstAuthFailureAt = $null
$lastAuthSuccessAt = $null

# 立即落一条实验头记录；明早看到日志第一行就能知道当晚使用的频率和终止条件。
Write-JsonLine ([ordered]@{
    type             = 'experiment_start'
    timestamp        = $startedAt.ToString('o')
    interval_minutes = $IntervalMinutes
    duration_hours   = $DurationHours
    max_iterations   = $MaxIterations
    pid              = $PID
})

[void][ClassFlowPowerState]::SetThreadExecutionState($ES_CONTINUOUS -bor $ES_SYSTEM_REQUIRED)

try {
    while ((Get-Date) -lt $deadline) {
        if ($MaxIterations -gt 0 -and $iteration -ge $MaxIterations) {
            break
        }

        $iteration++
        $probeStartedAt = Get-Date
        $record = [ordered]@{
            type          = 'probe'
            iteration     = $iteration
            timestamp     = $probeStartedAt.ToString('o')
            authenticated = $false
            heartbeat     = $null
            auth          = $null
            cookies       = @()
            error         = $null
        }

        try {
            $sessionId = Get-SmartClassSessionId
            $heartbeat = Invoke-SmartClassHeartbeat -SessionId $sessionId
            $auth = Get-SmartClassAuthState -SessionId $sessionId
            $cookies = Get-SmartClassCookieMetadata

            $record.heartbeat = $heartbeat
            $record.auth = $auth
            $record.cookies = $cookies
            $record.authenticated = [bool]$auth.authenticated

            if ($record.authenticated) {
                $authSuccessCount++
                $lastAuthSuccessAt = Get-Date
            }
            elseif ($null -eq $firstAuthFailureAt) {
                $firstAuthFailureAt = Get-Date
            }
        }
        catch {
            $record.error = $_.Exception.Message
            if ($null -eq $firstAuthFailureAt) {
                # 这里先记为“第一次异常”，但最终解释时必须结合 error 判断：
                # 浏览器断开/网络错误不能直接等同于服务器登录过期。
                $firstAuthFailureAt = Get-Date
            }
        }

        $record.probe_elapsed_seconds = [Math]::Round(((Get-Date) - $probeStartedAt).TotalSeconds, 3)
        Write-JsonLine $record

        if ($MaxIterations -gt 0 -and $iteration -ge $MaxIterations) {
            break
        }

        $nextAt = $probeStartedAt.AddMinutes($IntervalMinutes)
        $sleepSeconds = [Math]::Max(1, [int][Math]::Ceiling(($nextAt - (Get-Date)).TotalSeconds))
        Start-Sleep -Seconds $sleepSeconds
    }
}
finally {
    [void][ClassFlowPowerState]::SetThreadExecutionState($ES_CONTINUOUS)

    $endedAt = Get-Date
    $summary = [ordered]@{
        type                  = 'experiment_summary'
        started_at            = $startedAt.ToString('o')
        ended_at              = $endedAt.ToString('o')
        elapsed_hours         = [Math]::Round(($endedAt - $startedAt).TotalHours, 3)
        interval_minutes      = $IntervalMinutes
        probes                = $iteration
        authenticated_probes  = $authSuccessCount
        first_failure_at      = if ($firstAuthFailureAt) { $firstAuthFailureAt.ToString('o') } else { $null }
        last_auth_success_at  = if ($lastAuthSuccessAt) { $lastAuthSuccessAt.ToString('o') } else { $null }
        log_path              = $LogPath
    }

    $summary | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $summaryPath -Encoding utf8
    Write-JsonLine $summary
}

Write-Output "SmartClass session probe finished."
Write-Output "Log: $LogPath"
Write-Output "Summary: $summaryPath"
