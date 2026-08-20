# WarpDeck dev-base 冒烟（计划 P2-012 / 设计 §23.3 + §23.3.1）。
#
# 两部分：
#   A. 组件齐备性（无需 tun/特权）：warp-cli/warp-svc/gost/dbus/tini 可执行，
#      容器出网可达 cloudflare trace。
#   B. 真实数据面（需要 --device /dev/net/tun --cap-add NET_ADMIN）：
#      免费注册 -> mode proxy -> port 40000 -> connect -> curl socks5 trace，
#      期望 warp=on（无需 WARP+ license，实测 2026-08）。
#
# 用法:
#   .\scripts\smoke-dev-base.ps1                    # 仅 A
#   .\scripts\smoke-dev-base.ps1 -Full              # A + B（需 docker 给 tun/root）
#   docker run ... 见 "完整数据面冒烟" 注释段

param(
    [switch]$Full
)

$ErrorActionPreference = "Stop"

# ---------- A. 组件齐备性 ----------
& docker run --rm warpdeck-dev-base:1 bash -c "set -e
  command -v warp-cli && command -v warp-svc
  gost -V
  dbus-daemon --version
  tini --version | grep -i tini
  echo 'components-ok'" | Out-Null
if ($global:LASTEXITCODE -ne 0) { Write-Error "components check failed"; exit 1 }

$trace = & docker run --rm warpdeck-dev-base:1 bash -c "curl -fsSL --max-time 20 https://cloudflare.com/cdn-cgi/trace"
if ($global:LASTEXITCODE -ne 0) { Write-Error "trace unreachable"; exit 1 }
Write-Host "direct trace: $($trace -split "`n" | Select-String 'warp=')"
if (($trace -join "`n") -notmatch "warp=") { Write-Error "missing warp= field"; exit 1 }

Write-Host "OK: dev-base components smoke passed"

# ---------- B. 真实数据面 ----------
if ($Full) {
    # warp-svc 需建 tun、dbus system bus 需写 /run/dbus → 必须以 root 运行。
    $warpTrace = & docker run --rm --user root --device /dev/net/tun --cap-add NET_ADMIN `
        --entrypoint bash warpdeck-dev-base:1 -c "set -e
          mkdir -p /var/run/dbus
          dbus-daemon --system --fork
          warp-svc --accept-tos >/tmp/warp-svc.log 2>&1 &
          # warp-cli status 在 daemon 未启动时也 exit 0，不能作为就绪判据；
          # 改轮询 warp-svc 的 IPC socket（warp-svc 日志确认其路径）。
          for i in 1 2 3 4 5; do
            [ -S /run/cloudflare-warp/warp_service ] && break || sleep 2
          done
          warp-cli --accept-tos registration new
          warp-cli --accept-tos mode proxy
          warp-cli --accept-tos proxy port 40000
          warp-cli --accept-tos connect
          for i in 1 2 3 4 5; do
            warp-cli --accept-tos status | grep -q 'Connected' && break || sleep 3
          done
          warp-cli --accept-tos status
          curl -fsSL --max-time 30 --socks5-hostname 127.0.0.1:40000 https://cloudflare.com/cdn-cgi/trace ||
            { echo '=== warp-svc.log ==='; cat /tmp/warp-svc.log; exit 1; }"
    if ($global:LASTEXITCODE -ne 0) { Write-Error "data plane smoke failed"; exit 1 }
    Write-Host $warpTrace
    if (($warpTrace -join "`n") -notmatch "warp=on") { Write-Error "expected warp=on"; exit 1 }
    Write-Host "OK: dev-base data plane smoke passed (warp=on)"
}