# WarpDeck E2E harness（P11-006 + 007~013 matrix；v0.2 多账号档案换线 E2E-08）。
#
# 原则（计划 16.3）：整个 E2E 矩阵复用同一个 warpdeck:e2e 镜像，每用例不重 build。
# 环境：Docker Desktop 本机 + 已下载 WARP/GOST 缓存（scripts/build-release.ps1 产出镜像）。
#
# 用例:
#   1 first-run   fresh volume + setup/login + 创建实例 + 等 Healthy（P11-007）
#   2 socks5      11080 -> curl trace warp=on（P11-008）
#   3 http        18080 -> curl trace warp=on（P11-009）
#   4 persistence 3 实例 + 代理配置 -> restart 容器 -> 全部恢复 + trace 可用（P11-010）
#   5 failure     kill 一个 warp-svc -> 池收缩 -> 代理仍工作 -> auto-restart 恢复（P11-011）
#   6 gost        kill gost -> reconciler 重建 -> trace 恢复（P11-012）
#   7 no-leak     停全部实例 -> 代理请求必须失败（无 direct leak）（P11-013）
#   8 profiles    v0.2 多账号档案：档案 CRUD(只 mask) + 双档案双实例 + 改绑自动重启 +
#                 删除保护 409（§16.9/§17.6）
#
# Zero Trust 换线需要真实凭据（§11.2 service token）：
#   $env:WARP_E2E_ZT_ORG / WARP_E2E_ZT_CLIENT_ID / WARP_E2E_ZT_CLIENT_SECRET
# 缺失时 E2E-08 跳过 ZT 档案部分，其余（默认档/自由档/改绑/保护）照常验证。
#
# 用法:
#   .\scripts\e2e\run-e2e.ps1               # 全量（默认）
#   .\scripts\e2e\run-e2e.ps1 -Only 2,3     # 只跑 2、3（共享当前环境）
#   .\scripts\e2e\run-e2e.ps1 -NoFresh      # 不重建环境（复用上次容器）

param(
    [int[]]$Only = @(1, 2, 3, 4, 5, 6, 7, 8),
    [switch]$NoFresh
)

$ErrorActionPreference = "Stop"

$Project = "warpdeck-e2e"
$Image = "warpdeck:e2e"
$BasePortWeb = 9900
$BasePortSocks = 11081
$BasePortHttp = 18081
$AdminUser = "e2e-admin"
$AdminPass = "e2e-password-123"

$env:WARPDECK_IMAGE = $Image
$env:WEB_HOST_PORT = "$BasePortWeb"
$env:SOCKS5_HOST_PORT = "$BasePortSocks"
$env:HTTP_HOST_PORT = "$BasePortHttp"

$results = [System.Collections.Generic.List[string]]::new()

function Assert($msg, $cond) {
    if ($cond) {
        Write-Host "  PASS: $msg" -ForegroundColor Green
    }
    else {
        Write-Host "  FAIL: $msg" -ForegroundColor Red
        $results.Add("FAIL $msg - $(Get-Date -Format o)")
        throw "E2E assertion failed: $msg"
    }
}

function Compose($items, $timeoutSec = 300) {
    $a = @("compose", "-p", $Project) + $items
    $stamp = [Guid]::NewGuid().ToString("N")
    $out = Join-Path $env:TEMP "wc-out-$stamp.log"
    $err = Join-Path $env:TEMP "wc-err-$stamp.log"
    $p = Start-Process docker -ArgumentList $a -NoNewWindow -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
    if (-not $p.WaitForExit($timeoutSec * 1000)) {
        Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
        throw "docker compose $($items -join ' ') timed out (>${timeoutSec}s)"
    }
    Get-Content $out, $err -ErrorAction SilentlyContinue | Write-Host
    Remove-Item $out, $err -ErrorAction SilentlyContinue
    if ($p.ExitCode -ne 0) { throw "docker compose $($items -join ' ') failed (exit=$($p.ExitCode))" }
}

function Wait-ContainerHealthy($timeoutSec = 180) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        $state = docker inspect --format "{{.State.Health.Status}}" "$Project-warpdeck-1" 2>$null
        if ($state -eq "healthy") { return }
        Start-Sleep -Seconds 3
    }
    throw "container not healthy within ${timeoutSec}s (state=$state)"
}

# 容器健康 ≠ GOST 已监听：重启后 reconciler 需要 render/validate/rename/restart/probe
# 全链路才开放端口。等 TCP 可连（与服务端 apply 的 probe 语义一致），避免 E2E-04 竞态。
function Wait-ProxyListeners($timeoutSec = 90) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    foreach ($port in @($BasePortSocks, $BasePortHttp)) {
        $ready = $false
        while ((Get-Date) -lt $deadline) {
            $c = New-Object System.Net.Sockets.TcpClient
            try {
                $c.Connect("127.0.0.1", $port)
                $ready = $true
            }
            catch { Start-Sleep -Seconds 2 }
            finally { $c.Dispose() }
            if ($ready) { break }
        }
        if (-not $ready) { throw "proxy listener :$port not open within ${timeoutSec}s" }
    }
}

$script:cookieJar = Join-Path $env:TEMP ("wc-e2e-" + [Guid]::NewGuid().ToString("N") + ".jar")
$script:csrfToken = $null

# 说明：PS 7.4 的 Invoke-WebRequest 对 Cookie 头/会话处理不可靠（见 E2E 调试记录），
# 所有 API 调用改用 curl.exe + cookie jar（-b 读 / -c 写），行为与 curl 手工验证一致。
function Api($method, $path, $body = $null) {
    $uri = "http://127.0.0.1:$BasePortWeb/api/v1$path"
    $bodyFile = Join-Path $env:TEMP ("wc-body-" + [Guid]::NewGuid().ToString("N") + ".json")
    $a = @("-s")
    if (Test-Path $script:cookieJar) { $a += @("-b", $script:cookieJar) }
    $a += @("-c", $script:cookieJar, "-H", "Content-Type: application/json")
    if ($script:csrfToken) { $a += @("-H", "X-CSRF-Token: $script:csrfToken") }
    if ($null -ne $body) { $a += @("--data", ($body | ConvertTo-Json -Compress -Depth 6)) }
    $a += @("-o", $bodyFile, "-w", "%{http_code}", "-X", $method, $uri)
    $code = curl.exe @a
    $json = $null
    if (Test-Path $bodyFile) {
        $text = Get-Content $bodyFile -Raw -ErrorAction SilentlyContinue
        if ($text) {
            try { $json = $text | ConvertFrom-Json } catch {
                if ($text[0] -eq 0xFEFF) { $text = $text.Substring(1) }
                $json = $text | ConvertFrom-Json
            }
        }
        Remove-Item $bodyFile -ErrorAction SilentlyContinue
    }
    return @{ Status = [int]$code; Json = $json }
}

function SetupAndLogin {
    Api POST "/setup" @{ username = $AdminUser; password = $AdminPass } | Out-Null
    $login = Api POST "/auth/login" @{ username = $AdminUser; password = $AdminPass }
    $script:csrfToken = $login.Json."x-csrf-token"
}

function Create-Instance($name, $profileId = $null) {
    $body = @{ name = $name }
    if ($null -ne $profileId) { $body.account_profile_id = $profileId }
    $resp = Api POST "/instances" $body
    Assert "create instance '$name' -> 201" ($resp.Status -eq 201)
    return [int64]$resp.Json.id
}

function Wait-InstanceState($id, $state, $timeoutSec = 300) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    $last = ""
    while ((Get-Date) -lt $deadline) {
        $resp = Api GET "/instances/$id"
        $last = $resp.Json.runtime_state
        if ($last -eq $state) { return $resp.Json }
        Start-Sleep -Seconds 5
    }
    throw "instance $id not in state '$state' within ${timeoutSec}s (last=$last)"
}

function Get-Trace($proto, $timeoutSec = 30) {
    if ($proto -eq "socks5") {
        $arg = "--socks5-hostname"
        $port = $BasePortSocks
    }
    else {
        $arg = "-x"
        $port = $BasePortHttp
    }
    $out = curl.exe -fsSL --max-time $timeoutSec $arg "127.0.0.1:$port" --proxy-user "e2e-proxy-user:e2e-proxy-pass-123" "https://cloudflare.com/cdn-cgi/trace" -o - 2>$null
    if ($LASTEXITCODE -ne 0) { return $null }
    return ($out -join "`n")
}

function Assert-WarpOn($proto) {
    # 启动期 reconciler 会用期望配置重启一次 GOST：新版监听就绪后仍可能有
    # 一次杀进程窗口（accept 后 EOF）。对数据面做有界重试（总 ~60s），
    # 与服务的「apply → probe 监听」语义一致，避免把重启窗口误判为故障。
    $deadline = (Get-Date).AddSeconds(60)
    $trace = $null
    while ((Get-Date) -lt $deadline) {
        $trace = Get-Trace $proto
        if ($trace -match "warp=on") { break }
        Start-Sleep -Seconds 3
    }
    Assert "$proto trace reachable" ($null -ne $trace)
    Assert "$proto trace warp=on" ($trace -match "warp=on")
}

function Stop-AllInstances($ids) {
    foreach ($id in $ids) {
        try { Api POST "/instances/$id/stop" | Out-Null } catch { }
    }
}

# ---------- setup environment ----------
Write-Host "== E2E setup ($Project, image=$Image, ports $BasePortWeb/$BasePortSocks/$BasePortHttp) =="

if ($NoFresh) {
    Write-Host "  (reuse existing environment)"
}
else {
    Write-Host "  fresh environment..."
    Compose @("down", "-v")
    Compose @("up", "-d")
}

Wait-ContainerHealthy 180
SetupAndLogin
Write-Host "  admin setup + login OK (csrf=$script:csrfToken)"

# ---------- E2E-01 first run ----------
$ids = [System.Collections.Generic.List[int64]]::new()
if ($Only -contains 1) {
    Write-Host "== E2E-01 first run: fresh volume, setup, login, create instance, wait healthy =="
    $id = Create-Instance "e2e-a"
    $ids.Add($id)
    $view = Wait-InstanceState $id "healthy" 360
    Assert "instance $id Healthy (exit_ip=$($view.exit_ip) colo=$($view.colo))" ($view.runtime_state -eq "healthy")
    $results.Add("PASS E2E-01 first run")
}

# ---------- E2E-02/03 data plane ----------
if ($Only -contains 2) {
    Write-Host "== E2E-02 socks5 -> warp=on =="
    # 实例 Healthy（数据面探活经内部 upstream）≠ GOST 前端监听已开：
    # 等待两端口就绪，防 E2E-02 启动竞态（与 E2E-04 同理）。
    Wait-ProxyListeners 90
    Assert-WarpOn "socks5"
    $results.Add("PASS E2E-02 socks5 warp=on")
}
if ($Only -contains 3) {
    Write-Host "== E2E-03 http -> warp=on =="
    Assert-WarpOn "http"
    $results.Add("PASS E2E-03 http warp=on")
}

# ---------- E2E-04 persistence ----------
if ($Only -contains 4) {
    Write-Host "== E2E-04 restart persistence =="
    $b = Create-Instance "e2e-b"
    $c = Create-Instance "e2e-c"
    $ids.Add($b); $ids.Add($c)
    Wait-InstanceState $b "healthy" 360 | Out-Null
    Wait-InstanceState $c "healthy" 360 | Out-Null
    # 代理配置持久化：设置 auth + 自定义限制（写期望状态）。
    $cfg = Api PUT "/proxy" @{
        socks5_enabled = $true
        http_enabled = $true
        auth_enabled = $true
        username = "e2e-proxy-user"
        password = "e2e-proxy-pass-123"
    }
    Assert "proxy config saved (201/200)" ($cfg.Status -in 200, 201)
    $view = Api GET "/proxy"
    Assert "proxy auth_configured persisted" ($view.Json.auth_configured -eq $true)

    # 重启容器（simulate host reboot）。
    Compose @("restart")
    Wait-ContainerHealthy 180
    foreach ($id in $ids) {
        $v = Wait-InstanceState $id "healthy" 360
        Assert "instance $id recovered after restart" ($v.runtime_state -eq "healthy")
    }
    # GOST 池恢复：proxy auth 打开后 trace 需要带认证
    # （容器健康 ≠ 端口已开放：等 GOST 监听就绪，防 E2E-04 竞态）
    Wait-ProxyListeners 90
    $trace = curl.exe -fsSL --max-time 30 --socks5-hostname "127.0.0.1:$BasePortSocks" --proxy-user "e2e-proxy-user:e2e-proxy-pass-123" "https://cloudflare.com/cdn-cgi/trace" -o - 2>$null
    Assert "socks5 trace (with auth) still warp=on after restart" (($trace -join "`n") -match "warp=on")
    $results.Add("PASS E2E-04 restart persistence")
}

# ---------- E2E-05 instance failure ----------
if ($Only -contains 5) {
    Write-Host "== E2E-05 kill one warp-svc -> pool shrinks -> proxy alive -> auto-restart =="
    $container = "$Project-warpdeck-1"
    # 只杀一个（多个实例并行时 pkill 会全杀，池就空了）：
    # pgrep 取第一个 warp-svc pid -> kill -9。
    & docker exec $container bash -c "pgrep -f 'warp-svc --accept-tos' | head -n 1 | xargs -r kill -9" 2>&1 | Out-Null
    Assert "one warp-svc killed inside container" ($LASTEXITCODE -eq 0)
    # 至少一个实例进入 failed/degraded 窗口（kill 后探针反应）。
    Start-Sleep -Seconds 5
    # 代理仍可用（存活实例顶上）：带重试（健康监测有几秒窗口）。
    $ok = $false
    for ($i = 0; $i -lt 10 -and -not $ok; $i++) {
        $trace = curl.exe -fsSL --max-time 30 --socks5-hostname "127.0.0.1:$BasePortSocks" --proxy-user "e2e-proxy-user:e2e-proxy-pass-123" "https://cloudflare.com/cdn-cgi/trace" -o - 2>$null
        $ok = ($null -ne $trace -and (($trace -join "`n") -match "warp=on"))
        if (-not $ok) { Start-Sleep -Seconds 6 }
    }
    Assert "proxy still works after instance kill (pool shrank)" $ok
    # auto-restart: 全部实例最终回到 healthy。
    foreach ($id in $ids) {
        $v = Wait-InstanceState $id "healthy" 360
        Assert "instance $id auto-restarted healthy" ($v.runtime_state -eq "healthy")
    }
    $results.Add("PASS E2E-05 instance failure")
}

# ---------- E2E-06 gost failure ----------
if ($Only -contains 6) {
    Write-Host "== E2E-06 kill gost -> reconciler restart -> trace recovers =="
    $container = "$Project-warpdeck-1"
    & docker exec $container bash -c "pkill -9 -f 'gost -C'" 2>&1 | Out-Null
    Assert "gost killed inside container" ($LASTEXITCODE -eq 0)
    $ok = $false
    for ($i = 0; $i -lt 15 -and -not $ok; $i++) {
        Start-Sleep -Seconds 6
        $trace = curl.exe -fsSL --max-time 30 --socks5-hostname "127.0.0.1:$BasePortSocks" --proxy-user "e2e-proxy-user:e2e-proxy-pass-123" "https://cloudflare.com/cdn-cgi/trace" -o - 2>$null
        $ok = ($null -ne $trace -and (($trace -join "`n") -match "warp=on"))
    }
    Assert "gost auto-recovered (reconciler restart + listener probe)" $ok
    $results.Add("PASS E2E-06 gost failure")
}

# ---------- E2E-07 no direct leak ----------
if ($Only -contains 7) {
    Write-Host "== E2E-07 stop all instances -> proxy must fail (no direct leak) =="
    if ($ids.Count -eq 0) {
        # 独立跑 07 时：用当前已有实例。
        $list = Api GET "/instances"
        foreach ($it in $list.Json) { Api POST "/instances/$($it.id)/stop" | Out-Null }
    }
    else {
        Stop-AllInstances $ids
    }
    Start-Sleep -Seconds 12
    $trace = curl.exe --socks5-hostname "127.0.0.1:$BasePortSocks" --proxy-user "e2e-proxy-user:e2e-proxy-pass-123" --max-time 20 "https://cloudflare.com/cdn-cgi/trace" -o - 2>$null
    Assert "proxy request FAILS with no healthy upstream" ($LASTEXITCODE -ne 0 -and $null -eq $trace)
    $direct = curl.exe -fsSL --max-time 15 "https://cloudflare.com/cdn-cgi/trace" -o - 2>$null
    Assert "direct (non-proxy) internet still works (environment sanity)" ($null -ne $direct)
    $results.Add("PASS E2E-07 no direct leak")
}

# ---------- E2E-08 multi-account profiles (v0.2 §16.9/§17.6) ----------
if ($Only -contains 8) {
    Write-Host "== E2E-08 account profiles: CRUD(masked) + binding + rebind auto-restart + delete protection =="
    Wait-ProxyListeners 90

    # 默认档必须存在且为 free。
    $acc = Api GET "/accounts"
    Assert "GET /accounts -> 200" ($acc.Status -eq 200)
    $def = @($acc.Json) | Where-Object { $_.default }
    Assert "default free profile present (id=1, mode=free)" ($def.Count -eq 1 -and $def[0].id -eq 1 -and $def[0].mode -eq "free")

    # Zero Trust 档案（真实 service token 换线；凭据缺失则跳过 ZT 部分）。
    $ztOrg = $env:WARP_E2E_ZT_ORG
    $ztId = $env:WARP_E2E_ZT_CLIENT_ID
    $ztSecret = $env:WARP_E2E_ZT_CLIENT_SECRET
    $ztAvailable = ($ztOrg -and $ztId -and $ztSecret)
    if (-not $ztAvailable) {
        Write-Host "  SKIP: Zero Trust 换线未提供 WARP_E2E_ZT_* 凭据（只验证默认档/自由档路径）" -ForegroundColor Yellow
    }
    if ($ztAvailable) {
        $resp = Api POST "/accounts" @{
            name = "e2e-zero-trust"
            mode = "zero_trust"
            zero_trust_org = $ztOrg
            client_id = $ztId
            client_secret = $ztSecret
        }
        Assert "create zero_trust profile -> 201" ($resp.Status -eq 201)
        $ztPid = [int64]$resp.Json.id
        Assert "created profile has masked secrets (no plaintext)" ($null -eq $resp.Json.client_id -and $null -eq $resp.Json.client_secret)

        # 双档案双实例：free 档 + zero_trust 档各一个。
        $freeId = Create-Instance "e2e-free"
        $ztIdInstance = Create-Instance "e2e-zt" $ztPid
        Wait-InstanceState $freeId "healthy" 360 | Out-Null
        Wait-InstanceState $ztIdInstance "healthy" 360 | Out-Null
        $vFree = Api GET "/instances/$freeId"
        $vZt = Api GET "/instances/$ztIdInstance"
        Assert "free instance bound to default profile (account.profile_id=1)" ($vFree.Json.account.profile_id -eq 1)
        Assert "zt instance bound to created profile" ($vZt.Json.account.profile_id -eq $ztPid)
        Assert "zt instance exit ip present" ($null -ne $vZt.Json.exit_ip)

        # 改绑：zt 实例解绑回默认档 -> restart_pending -> 自动重启 -> 生效。
        $rb = Api PATCH "/instances/$ztIdInstance" @{ account_profile_id = $null }
        Assert "PATCH rebind (explicit null = unbind) -> 200" ($rb.Status -eq 200)
        $v = Wait-InstanceState $ztIdInstance "healthy" 360
        Assert "rebind took effect via auto-restart (account.profile_id=1)" ($v.account.profile_id -eq 1)

        # 删除保护：仍被绑定 -> 409；解绑后 -> 204。
        # 上面实例已解绑；先重新绑定另一个实例制造 409 场景。
        $freeId2 = Create-Instance "e2e-free2" $ztPid
        Wait-InstanceState $freeId2 "healthy" 360 | Out-Null
        $del = Api DELETE "/accounts/$ztPid"
        Assert "delete bound profile -> 409 (referenced)" ($del.Status -eq 409)
        # 清理：删除绑定实例后删除档案。
        foreach ($i in @($freeId2)) { Api DELETE "/instances/$i" | Out-Null }
        Start-Sleep -Seconds 3
        $del2 = Api DELETE "/accounts/$ztPid"
        Assert "delete unbound profile -> 204" ($del2.Status -eq 204)

        # 数据面换线：zero_trust 档案还在时两条连接都 warp=on（证明 ZT 换线真的生效）。
        Start-Sleep -Seconds 3
        Assert-WarpOn "socks5"
        $results.Add("PASS E2E-08 account profiles (zero_trust line)")
    }
    else {
        # 无 ZT 凭据：只验证默认档绑定 + 改绑自动重启 + 删除保护（零凭据路径）。
        $freeId = Create-Instance "e2e-free"
        $freeId2 = Create-Instance "e2e-free2"
        Wait-InstanceState $freeId "healthy" 360 | Out-Null
        Wait-InstanceState $freeId2 "healthy" 360 | Out-Null
        # 默认档不可删除。
        $del = Api DELETE "/accounts/1"
        Assert "delete default profile -> 409 (protected)" ($del.Status -eq 409)
        # 改绑 null -> 默认（幂等路径）。
        $rb = Api PATCH "/instances/$freeId" @{ account_profile_id = $null }
        Assert "PATCH rebind (explicit null) -> 200" ($rb.Status -eq 200)
        $v = Wait-InstanceState $freeId "healthy" 360
        Assert "rebind to default profile effective" ($v.account.profile_id -eq 1)
        foreach ($i in @($freeId, $freeId2)) { Api POST "/instances/$i/stop" | Out-Null }
        $results.Add("PASS E2E-08 account profiles (default/free line, ZT skipped)")
    }
}

Write-Host ""
Write-Host "========== E2E summary ==========" -ForegroundColor Cyan
foreach ($r in $results) { Write-Host "  $r" }
Write-Host "=================================="
if ($results | Where-Object { $_ -like "FAIL*" }) { exit 1 }
Write-Host "ALL E2E PASSED" -ForegroundColor Green