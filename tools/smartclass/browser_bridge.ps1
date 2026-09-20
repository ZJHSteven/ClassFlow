<#
.SYNOPSIS
    ClassFlow 的 SmartClass 本地浏览器桥。

.DESCRIPTION
    这个桥只监听本机回环地址，默认是 http://127.0.0.1:8790/。

    它不读取、不导出、不落盘 Cookie 或 token，而是通过 browser-harness-js/CDP
    附加到用户已经正常登录的 SmartClass“录播课堂”标签页，让页面自己的
    `services.CreateGetMyVideosRequest()` 生成并发送官方请求。

    Rust 后端只需要请求：

        GET /v1/smartclass/videos?start_date=2026-09-01&end_date=2026-09-20&page_number=1&page_size=12

    返回：

        {
          "authenticated": true,
          "total_count": 24,
          "data": [ ... SmartClass 原始录播对象 ... ]
        }

    安全边界：
    - 默认只绑定 127.0.0.1，不直接暴露到局域网/公网。
    - 日志不包含 Cookie value、账号密码、platform_token 或 csrkToken。
    - 页面未登录时返回 authenticated=false，不自动填写验证码或绕过登录。
#>

[CmdletBinding()]
param(
    [string]$ListenPrefix = 'http://127.0.0.1:8790/',
    [int]$BrowserDebugPort = 9222
)

$ErrorActionPreference = 'Stop'
$browserHarness = (Get-Command 'browser-harness-js' -ErrorAction Stop).Source

function Invoke-BrowserHarness {
    <#
    .SYNOPSIS
        通过 stdin 调 browser-harness-js，规避 Windows .cmd 的多行参数转义问题。
    #>
    param(
        [Parameter(Mandatory = $true)]
        [string]$Code,
        [string]$SessionId = ''
    )

    if ([string]::IsNullOrWhiteSpace($SessionId)) {
        $output = $Code | & $browserHarness 2>&1
    }
    else {
        $output = $Code | & $browserHarness '--session' $SessionId 2>&1
    }

    $exitCode = $LASTEXITCODE
    $text = (($output | ForEach-Object { $_.ToString() }) -join "`n").Trim()
    if ($exitCode -ne 0) {
        throw "browser-harness-js 调用失败：$text"
    }
    return $text
}

function Get-SmartClassSessionId {
    <#
    .SYNOPSIS
        找到当前录播课堂页并返回一个新的 target-scoped CDP session。

    .DESCRIPTION
        每次请求都重新 attach，避免旧 session 因刷新/重建而失效。
        如果标签页被关闭，只创建正常 CourseList 页面，不尝试自动登录。
    #>
    $browserVersion = Invoke-RestMethod -Uri "http://127.0.0.1:$BrowserDebugPort/json/version" -TimeoutSec 5
    $wsUrl = [string]$browserVersion.webSocketDebuggerUrl
    if ([string]::IsNullOrWhiteSpace($wsUrl)) {
        throw "Chrome 调试端口 $BrowserDebugPort 没有返回 webSocketDebuggerUrl"
    }
    $escapedWsUrl = $wsUrl.Replace("'", "\\'")

    $code = @"
await session.connect({ wsUrl: '$escapedWsUrl' });
let tabs = await listPageTargets();
let target = tabs.find(t => t.url.includes('tmu.smartclass.cn/UserSpace/CourseList.aspx'));
if (!target) {
    const created = await session.Target.createTarget({ url: 'https://tmu.smartclass.cn/UserSpace/CourseList.aspx' });
    await new Promise(resolve => setTimeout(resolve, 2500));
    target = { targetId: created.targetId };
}
return String(await session.attach(target.targetId));
"@

    $sessionId = Invoke-BrowserHarness -Code $code
    if ([string]::IsNullOrWhiteSpace($sessionId)) {
        throw '无法附加 SmartClass CourseList 标签页'
    }
    return $sessionId.Trim()
}

function Get-SmartClassVideoPage {
    <#
    .SYNOPSIS
        让 SmartClass 页面自身发起一页 GetMyVideoList 请求。

    .PARAMETER StartDate
        YYYY-MM-DD。
    .PARAMETER EndDate
        YYYY-MM-DD。
    .PARAMETER PageNumber
        从 1 开始。
    .PARAMETER PageSize
        官方页面默认 12；ClassFlow 也默认 12。
    #>
    param(
        [Parameter(Mandatory = $true)][string]$StartDate,
        [Parameter(Mandatory = $true)][string]$EndDate,
        [Parameter(Mandatory = $true)][int]$PageNumber,
        [Parameter(Mandatory = $true)][int]$PageSize
    )

    $sessionId = Get-SmartClassSessionId
    $paramsJson = [ordered]@{
        startDate  = $StartDate
        endDate    = $EndDate
        pageNumber = $PageNumber
        pageSize   = $PageSize
    } | ConvertTo-Json -Compress

    # 参数先由 PowerShell 序列化成 JSON，再替换进一个固定占位符。
    # 这里故意使用“单引号 here-string”，避免 PowerShell 解释 JavaScript 模板字符串里的 `$` / `${...}`。
    $codeTemplate = @'
const expression = `(() => {
    const p = __CLASSFLOW_PARAMS__;
    if (typeof services === 'undefined' || typeof services.CreateGetMyVideosRequest !== 'function') {
        return JSON.stringify({ authenticated: false, total_count: 0, data: [], error: 'CourseList 页面脚本尚未就绪' });
    }
    const request = services.CreateGetMyVideosRequest();
    request.PageSize = Number(p.pageSize);
    request.PageNumber = Number(p.pageNumber);
    request.Sort = 'StartTime';
    request.Order = 1;
    request.StartDate = p.startDate;
    request.EndDate = p.endDate;
    request.IncludePublic = '';

    return new Promise(async resolve => {
        try {
            // 昨晚的过夜实验已经实测：正常 GET CourseList.aspx 会刷新 SmartClass 的在线状态，
            // 因此每轮第一页查询前先做一次相同的轻量 heartbeat。分页 2+ 不重复，避免额外流量。
            if (Number(p.pageNumber) === 1) {
                const heartbeat = await fetch('/UserSpace/CourseList.aspx', {
                    method: 'GET',
                    credentials: 'include',
                    cache: 'no-store',
                    redirect: 'follow'
                });
                if (!heartbeat.ok) {
                    resolve(JSON.stringify({
                        authenticated: false,
                        total_count: 0,
                        data: [],
                        error: 'CourseList heartbeat HTTP ' + heartbeat.status
                    }));
                    return;
                }
            }

            request.send()
                .done(result => resolve(JSON.stringify({
                    authenticated: true,
                    total_count: Number(result && result.TotalCount || 0),
                    data: Array.isArray(result && result.Data) ? result.Data : []
                })))
                .fail(error => resolve(JSON.stringify({
                    authenticated: false,
                    total_count: 0,
                    data: [],
                    error: typeof error === 'string' ? error : 'GetMyVideoList 请求失败'
                })));
        } catch (error) {
            resolve(JSON.stringify({
                authenticated: false,
                total_count: 0,
                data: [],
                error: String(error)
            }));
        }
    });
})()`;
const result = await session.Runtime.evaluate({
    expression,
    awaitPromise: true,
    returnByValue: true
});
return result.result.value;
'@
    $code = $codeTemplate.Replace('__CLASSFLOW_PARAMS__', $paramsJson)

    $json = Invoke-BrowserHarness -Code $code -SessionId $sessionId
    if ([string]::IsNullOrWhiteSpace($json)) {
        throw 'SmartClass 页面返回了空结果'
    }
    # 先解析再重新序列化，可以在桥层尽早发现页面返回的不是合法 JSON。
    return (($json | ConvertFrom-Json) | ConvertTo-Json -Depth 20 -Compress)
}

function Write-JsonResponse {
    param(
        [Parameter(Mandatory = $true)]$Context,
        [Parameter(Mandatory = $true)][int]$StatusCode,
        [Parameter(Mandatory = $true)][string]$Json
    )

    $bytes = [Text.Encoding]::UTF8.GetBytes($Json)
    $Context.Response.StatusCode = $StatusCode
    $Context.Response.ContentType = 'application/json; charset=utf-8'
    $Context.Response.ContentLength64 = $bytes.Length
    $Context.Response.OutputStream.Write($bytes, 0, $bytes.Length)
    $Context.Response.OutputStream.Close()
}

function Test-IsoDate {
    param([string]$Value)
    $parsed = [datetime]::MinValue
    return [datetime]::TryParseExact(
        $Value,
        'yyyy-MM-dd',
        [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::None,
        [ref]$parsed
    )
}

$listener = [Net.HttpListener]::new()
$listener.Prefixes.Add($ListenPrefix)
$listener.Start()
Write-Host "SmartClass browser bridge listening on $ListenPrefix"

try {
    while ($listener.IsListening) {
        $context = $listener.GetContext()
        try {
            $path = $context.Request.Url.AbsolutePath
            if ($path -eq '/health') {
                Write-JsonResponse -Context $context -StatusCode 200 -Json '{"status":"ok","service":"smartclass-browser-bridge"}'
                continue
            }

            if ($path -ne '/v1/smartclass/videos') {
                Write-JsonResponse -Context $context -StatusCode 404 -Json '{"error":"not found"}'
                continue
            }

            $startDate = $context.Request.QueryString['start_date']
            $endDate = $context.Request.QueryString['end_date']
            $pageNumberRaw = $context.Request.QueryString['page_number']
            $pageSizeRaw = $context.Request.QueryString['page_size']

            $pageNumber = 0
            $pageSize = 0
            $valid = (Test-IsoDate $startDate) -and
                (Test-IsoDate $endDate) -and
                [int]::TryParse($pageNumberRaw, [ref]$pageNumber) -and
                [int]::TryParse($pageSizeRaw, [ref]$pageSize) -and
                $pageNumber -ge 1 -and $pageSize -ge 1 -and $pageSize -le 100

            if (-not $valid) {
                Write-JsonResponse -Context $context -StatusCode 400 -Json '{"error":"invalid query"}'
                continue
            }

            $json = Get-SmartClassVideoPage -StartDate $startDate -EndDate $endDate -PageNumber $pageNumber -PageSize $pageSize
            Write-JsonResponse -Context $context -StatusCode 200 -Json $json
        }
        catch {
            # 错误消息只描述桥/页面状态，不包含 Cookie 或 token。
            $safeMessage = $_.Exception.Message.Replace('"', '\"')
            Write-JsonResponse -Context $context -StatusCode 502 -Json ("{`"error`":`"$safeMessage`"}")
        }
    }
}
finally {
    if ($listener.IsListening) {
        $listener.Stop()
    }
    $listener.Close()
}
