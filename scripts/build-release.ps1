# WarpDeck release image 构建（P11-001 / §23.2）。
# 大文件依赖从宿主机缓存目录读取（断点续传，不入 Git）：
#   GOST:   $HOME/.cache/warpdeck/gost/gost_3.2.6_linux_amd64.tar.gz
#   WARP:   $HOME/.cache/warpdeck/warp/cloudflare-warp_2026.6.880.0_amd64.deb
# 环境变量 WARPDECK_GOST_CACHE / WARPDECK_WARP_CACHE 可覆盖。
# 下载脚本：scripts/download-dev-base-deps.ps1
#
# 用法:
#   .\scripts\build-release.ps1                 # 产出 warpdeck:local（开发/发布）
#   .\scripts\build-release.ps1 -Tag warpdeck:e2e   # E2E 单镜像（P11-006 矩阵复用）
#
# P11 预算：整个 E2E matrix 复用同一个 e2e tag，禁止每个用例重 build。

param(
    [string]$Tag = "warpdeck:local"
)

$ErrorActionPreference = "Stop"

$gostCache = $env:WARPDECK_GOST_CACHE
if (-not $gostCache) {
    $gostCache = Join-Path $HOME ".cache\warpdeck\gost\gost_3.2.6_linux_amd64.tar.gz"
}
$warpCache = $env:WARPDECK_WARP_CACHE
if (-not $warpCache) {
    $warpCache = Join-Path $HOME ".cache\warpdeck\warp\cloudflare-warp_2026.6.880.0_amd64.deb"
}

if (-not (Test-Path $gostCache)) {
    Write-Error "GOST cache missing: $gostCache (run scripts/download-dev-base-deps.ps1)"
}
if (-not (Test-Path $warpCache)) {
    Write-Error "WARP cache missing: $warpCache (run scripts/download-dev-base-deps.ps1)"
}

# P12-001（补齐 P11-002）：依赖 pin 强制校验 SHA256，防替换/损坏。
# 固定版本：gost v3.2.6、cloudflare-warp 2026.6.880.0（换版本须同步更新下述哈希）。
$expected = @{
    $gostCache = "B39037B0380EA001FB3C0C28441C2E10BFC694F90682739A65B53E55DCE5238B"
    $warpCache = "648A7C7E9085F8E50D32A2ADCACB0C2049FB72EBEB02EBE913BECADEE3AB0D4C"
}
foreach ($f in $expected.Keys) {
    $h = (Get-FileHash $f -Algorithm SHA256).Hash.ToUpperInvariant()
    if ($h -ne $expected[$f]) {
        Write-Error "checksum mismatch: $f (got $h, expected $($expected[$f]))"
    }
    Write-Host "checksum OK ($((Split-Path $f -Leaf))): $h"
}

$gostDir = Split-Path $gostCache
$warpDir = Split-Path $warpCache

# P12-012：版本元数据。格式 `0.1.0-<git短sha>`（无 git 时回退 0.1.0-dev）。
# 经 --build-arg 注入 Dockerfile，LABEL + ENV（服务端 /api/v1/system 上报同一版本）。
$pkgVersion = "0.1.0"
$gitSha = (& git rev-parse --short HEAD 2>$null | Select-Object -First 1)
if (-not $gitSha) { $gitSha = "dev" }
$version = "$pkgVersion-$gitSha"

$dockerArgs = @(
    'build',
    '--progress=plain',
    "--build-context", "gostcache=$gostDir",
    "--build-context", "warpcache=$warpDir",
    "--build-arg", "WARPDECK_VERSION=$version",
    '-t', $Tag,
    '.'
)

Write-Host "building $Tag with version=$version"
docker @dockerArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "OK: $Tag"
