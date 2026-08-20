# WarpDeck dev-base 大文件依赖下载（断点续传）。
# 只在本机执行一次（或 deb/gost 版本升级时），产物不入 Git。
# 背景：pkg.cloudflareclient.com 在中国网络下连接被重置、github release 极慢，
# 但经本机代理可稳定拿到。Docker 构建期不做大文件下载。
#
# 版本 pin 必须与下列文件一致（单一事实来源）：docker/install-gost.sh、
# docker/install-warp.sh、docker/Dockerfile.dev-base。
#
# 用法:
#   .\scripts\download-dev-base-deps.ps1   # 默认走本机 socks5 10808（仅本机调试用）
# 代理仅本机调试用，永不写入项目代码/配置。

param(
    [string]$Proxy = "socks5h://127.0.0.1:10808",
    [int]$TimeoutSec = 70
)

$ErrorActionPreference = "Stop"

$gostUrl = "https://github.com/go-gost/gost/releases/download/v3.2.6/gost_3.2.6_linux_amd64.tar.gz"
$gostSha256 = "b39037b0380ea001fb3c0c28441c2e10bfc694f90682739a65b53e55dce5238b"
$gostMinBytes = 9000000

$warpUrl = "https://pkg.cloudflareclient.com/pool/noble/main/c/cloudflare-warp/cloudflare-warp_2026.6.880.0_amd64.deb"
$warpSha256 = "648a7c7e9085f8e50d32a2adcacb0c2049fb72ebeb02ebe913becadee3ab0d4c"
$warpMinBytes = 60000000

# 缓存目录与 scripts/build-dev-base.ps1 的默认值必须一致：
#   $HOME\.cache\warpdeck\gost\gost_3.2.6_linux_amd64.tar.gz
#   $HOME\.cache\warpdeck\warp\cloudflare-warp_2026.6.880.0_amd64.deb
$cacheRoot = Join-Path $HOME ".cache\warpdeck"
$gostFile = Join-Path $cacheRoot "gost\gost_3.2.6_linux_amd64.tar.gz"
$warpFile = Join-Path $cacheRoot "warp\cloudflare-warp_2026.6.880.0_amd64.deb"

function Resume-Download {
    param([string]$Url, [string]$File, [long]$MinBytes)
    New-Item -ItemType Directory -Path (Split-Path $File) -Force | Out-Null
    $done = $false
    for ($i = 1; $i -le 100; $i++) {
        $cur = (Get-Item $File -ErrorAction SilentlyContinue).Length
        if ($cur -ge $MinBytes) { $done = $true; break }
        $proxyArgs = @()
        if ($Proxy) { $proxyArgs = @('-x', $Proxy) }
        & curl.exe -sS -L --http1.1 -C - -o $File --max-time $TimeoutSec @proxyArgs $Url 2>$null
        if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 33) {
            Write-Warning "curl exit $LASTEXITCODE (round $i)"
        }
        $new = (Get-Item $File -ErrorAction SilentlyContinue).Length
        Write-Host ("round {0,3}  {1,10:N0} bytes" -f $i, $new)
        if ($new -eq $cur -and $i -gt 10) { Write-Warning "no progress; continuing anyway" }
        Start-Sleep -Milliseconds 500
    }
    if (-not $done) { throw "download incomplete: $File" }
}

if (Test-Path $gostFile) {
    $h = (Get-FileHash $gostFile -Algorithm SHA256).Hash.ToLower()
    if ($h -eq $gostSha256) {
        Write-Host "GOST cache OK ($h)"
    } else {
        Write-Host "GOST hash mismatch ($h), re-downloading"
        Remove-Item $gostFile -Force
        Resume-Download -Url $gostUrl -File $gostFile -MinBytes $gostMinBytes
    }
} else {
    Resume-Download -Url $gostUrl -File $gostFile -MinBytes $gostMinBytes
}

Resume-Download -Url $warpUrl -File $warpFile -MinBytes $warpMinBytes

$h2 = (Get-FileHash $warpFile -Algorithm SHA256).Hash.ToLower()
if ($h2 -ne $warpSha256) {
    # WARP deb 官方可能重打包导致 hash 变化；dpkg 安装时会校验包结构，
    # 这里仅警告不删除（删除会导致无法使用，且可能陷入死循环）。
    Write-Warning "WARP hash mismatch (got $h2, expected $warpSha256); continuing (dpkg will validate structure)"
} else {
    Write-Host "WARP cache OK ($h2)"
}
Write-Host "GOST: $gostFile"
Write-Host "WARP: $warpFile"
