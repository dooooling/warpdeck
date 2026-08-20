#!/usr/bin/env bash
# WarpDeck dev-base: Cloudflare WARP install (DESIGN 23.3).
# 用法: install-warp.sh [local-deb-path]
# 可选第一参数：本地已下载的 cloudflare-warp deb（跳过 pkg.cloudflareclient.com 下载，
# 但仍从 apt 源安装依赖）。不传则走官方 apt 源完整安装。
set -euo pipefail

# No TTY during build: prevent debconf interactive prompts.
export DEBIAN_FRONTEND=noninteractive

LOCAL_DEB="${1:-}"

if [ -n "${LOCAL_DEB}" ] && [ ! -f "${LOCAL_DEB}" ]; then
  echo "[warp-install] error: local deb not found: ${LOCAL_DEB}" >&2
  exit 1
fi

echo "[warp-install] stage=prepare-tools"

if [ -n "${LOCAL_DEB}" ]; then
  echo "[warp-install] stage=install-local-deb (trimmed repack)"
  # cloudflare-warp 的 Depends 声明了完整 GUI 栈(libwebkit2gtk-4.1-0 连带
  # libLLVM20/mesa/GTK3/主题图标, 合计 ~360MB), 但 headless daemon 运行时只链
  # tss2/nss/dbus/systemd(ldd /usr/bin/warp-svc 验证, 零 webkit/LLVM)。
  # 这里重打包 deb 并剪掉 GUI 相关 Depends, 安装扫描到的原始 deb 已由调用方
  # 做 sha256 固定校验, 重打包属于本地变换不改变既有 pin。
  REPACK_DIR="$(mktemp -d)"
  UNPACKED="${REPACK_DIR}/unpacked"
  TRIMMED_DEB="${REPACK_DIR}/cloudflare-warp-trimmed.deb"
  mkdir -p "${UNPACKED}"
  dpkg-deb -R "${LOCAL_DEB}" "${UNPACKED}"
  sed -E -i 's/, *(libwebkit2gtk-4\.1-0|libayatana-appindicator3-1|desktop-file-utils|gnupg2)//g' \
    "${UNPACKED}/DEBIAN/control"
  # headless 不需要的组件, 一并从包内剔除(一次性容器实测 warp-svc 启动与
  # warp-cli IPC 均正常): GUI 任务栏 Flutter 引擎 /usr/lib/warp 48M,
  # 诊断工具 warp-diag 32M / warp-dex 30M。注: WarpDeck 只经 WarpControl 调用
  # status/registration/connect 等显式命令, 永不调用 `warp-cli diag`。
  rm -rf "${UNPACKED}/usr/lib/warp" "${UNPACKED}/usr/bin/warp-diag" \
    "${UNPACKED}/usr/bin/warp-dex"
  rm -f "${UNPACKED}/usr/bin/warp-taskbar"
  dpkg-deb -Zgzip -b "${UNPACKED}" "${TRIMMED_DEB}"
  apt-get install -y --no-install-recommends "${TRIMMED_DEB}"
  # warp-diag/warp-dex 不在包内(dpkg -S 无归属), 是 postinst 释放的, 必须在
  # 安装完成后删除; 删除不破坏 dpkg 状态。
  rm -f /usr/bin/warp-diag /usr/bin/warp-dex
  rm -rf "${REPACK_DIR}"
  # deb 可能是 Dockerfile 的 bind mount(只读, rm 会报 Device busy)或 COPY 残留,
  # 两种情况下都不需要它存在于镜像里, 失败可忽略。
  rm -f "${LOCAL_DEB}" 2>/dev/null || true
else
  echo "[warp-install] stage=add-keyring"
  curl -fsSL --retry 5 --retry-all-errors --retry-delay 3 --max-time 300 https://pkg.cloudflareclient.com/pubkey.gpg \
    | gpg --yes --dearmor --output /usr/share/keyrings/cloudflare-warp-archive-keyring.gpg

  echo "[warp-install] stage=add-apt-source"
  echo "deb [signed-by=/usr/share/keyrings/cloudflare-warp-archive-keyring.gpg] https://pkg.cloudflareclient.com/ $(lsb_release -cs) main" \
    > /etc/apt/sources.list.d/cloudflare-client.list

  echo "[warp-install] stage=apt-update"
  apt-get update

  echo "[warp-install] stage=apt-install-cloudflare-warp"
  apt-get install -y --no-install-recommends cloudflare-warp
fi

echo "[warp-install] stage=done"
