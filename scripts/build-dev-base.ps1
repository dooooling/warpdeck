# WarpDeck dev-base 构建（计划 P2-012 / 设计 §23.3）。
# 只在 Dockerfile.dev-base / docker/*.sh / OS 包 / WARP/GOST 安装逻辑变化时重建。
# 后端普通代码变化不经过此脚本（bind mount Linux binary 进运行容器即可）。
# Linux/WSL 等价命令: docker build -t warpdeck-dev-base:1 -f docker/Dockerfile.dev-base .
#
# 大文件依赖从宿主机缓存目录读取（断点续传，不入 Git），避免构建期
# 访问 pkg.cloudflareclient.com（中国网络下被重置）和 github release（慢）。
#   缓存目录默认：
#     GOST:   $HOME/.cache/warpdeck/gost/gost_3.2.6_linux_amd64.tar.gz
#     WARP:   $HOME/.cache/warpdeck/warp/cloudflare-warp_2026.6.880.0_amd64.deb
# 可用环境变量 WARPDECK_GOST_CACHE / WARPDECK_WARP_CACHE 覆盖。
# 下载脚本：scripts/download-dev-base-deps.ps1

$ErrorActionPreference = "Stop"

$gostCache = $env:WARPDECK_GOST_CACHE
if (-not $gostCache) {
    $gostCache = Join-Path $HOME ".cache\warpdeck\gost\gost_3.2.6_linux_amd64.tar.gz"
}
$warpCache = $env:WARPDECK_WARP_CACHE
if (-not $warpCache) {
    $warpCache = Join-Path $HOME ".cache\warpdeck\warp\cloudflare-warp_2026.6.880.0_amd64.deb"
}

$gostDir = Split-Path $gostCache
$warpDir = Split-Path $warpCache

if (-not (Test-Path $gostCache)) {
    Write-Error "GOST cache missing: $gostCache (run scripts/download-dev-base-deps.ps1)"
}
if (-not (Test-Path $warpCache)) {
    Write-Error "WARP cache missing: $warpCache (run scripts/download-dev-base-deps.ps1)"
}

$dockerArgs = @(
    'build',
    '--progress=plain',
    "--build-context", "gostcache=$gostDir",
    "--build-context", "warpcache=$warpDir",
    '-t', 'warpdeck-dev-base:1',
    '-f', 'docker/Dockerfile.dev-base',
    '.'
)

docker @dockerArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "OK: warpdeck-dev-base:1"