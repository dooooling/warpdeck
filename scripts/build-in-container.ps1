# 在 warpdeck-dev-rust:1 容器内编译 warpdeck-server 的 Linux ELF 二进制。
#
# 设计（AGENTS.md "Never use repeated docker build as the dev/test loop" 的配套）:
# - 编译镜像 warpdeck-dev-rust:1 固定 toolchain；源码只读 bind-mount。
# - cargo registry 缓存在命名卷 warpdeck-cargo-cache（跨 run 持久，避免每次全量下载依赖）。
# - target 在命名卷 warpdeck-target（跨 run 持久，增量编译；避免 Windows 宿主文件系统
#   FUSE 慢速写放大）。
# - 产物导出到宿主 <repo>/target/linux-artifacts/warpdeck-server（会被 .gitignore 的
#   target/ 覆盖，不入库）。
#
# 用法:
#   powershell -File scripts/build-in-container.ps1
#   可选: -RebuildImage 重建编译镜像; -CleanTarget 清命名卷 target 重编。
param(
    [switch]$RebuildImage,
    [switch]$CleanTarget
)

$ErrorActionPreference = "Stop"

$repo = (Resolve-Path "$PSScriptRoot\..").Path
$image = "warpdeck-dev-rust:1"
$targetVol = "warpdeck-target"
$artifact = "warpdeck-server"
$log = Join-Path $env:TEMP "warpdeck-build-in-container.log"

if ($RebuildImage) {
    docker build -t $image -f "$repo\docker\Dockerfile.dev-rust" $repo
    if ($LASTEXITCODE -ne 0) { throw "docker build failed" }
}

if ($CleanTarget) {
    docker volume rm $targetVol 2>$null
}

# target 命名卷（跨 run 持久，增量编译）。
docker volume create $targetVol | Out-Null

$prevEAP = $ErrorActionPreference
$ErrorActionPreference = "Continue"
# 宿主 ~/.cargo/registry 直接挂载到 CARGO_HOME 下（rust:1.96 镜像 CARGO_HOME=
# /usr/local/cargo，挂错位置会每次全量下载；608+ 个 .crate 平台无关缓存 +
# aliyun sparse 索引，命中后 0 下载）；RUSTC + cargo 均走 toolchain 绝对路径，
# **完全绕过 rustup shim**（rustup 每次启动检查 stable channel 更新，国内直连卡死）。
# toolchain 名以镜像内为准（rust:1.96 内为 1.96.1）。
$toolchainBin = "/usr/local/rustup/toolchains/1.96.1-x86_64-unknown-linux-gnu/bin"
docker run --rm `
    -v "${repo}:/src:ro" `
    -v "$HOME\.cargo\registry:/usr/local/cargo/registry" `
    -v "${targetVol}:/target" `
    -e "CARGO_REGISTRIES_CRATES_IO_INDEX=sparse+https://mirrors.aliyun.com/crates.io-index/" `
    $image `
    sh -c "cd /src && CARGO_TARGET_DIR=/target RUSTC=${toolchainBin}/rustc ${toolchainBin}/cargo build -p warpdeck-server --bin warpdeck-server" `
    2>&1 | Tee-Object -FilePath $log
$buildCode = $LASTEXITCODE
$ErrorActionPreference = $prevEAP
if ($buildCode -ne 0) { throw "cargo build failed (see $log)" }

# 导出产物到宿主 target/linux-artifacts/（gitignore 已覆盖 target/）。
$outDir = Join-Path $repo "target\linux-artifacts"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
docker run --rm `
    -v "${targetVol}:/target" `
    -v "${repo}/target:/host-target" `
    $image `
    sh -c "mkdir -p /host-target/linux-artifacts && cp /target/debug/warpdeck-server /host-target/linux-artifacts/warpdeck-server && chmod +x /host-target/linux-artifacts/warpdeck-server"

if ($LASTEXITCODE -ne 0) { throw "artifact export failed" }

$bin = Join-Path $outDir $artifact
Get-Item $bin | Select-Object FullName, Length, LastWriteTime
Write-Host "log: $log"