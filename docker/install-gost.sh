#!/usr/bin/env bash
# WarpDeck dev-base: GOST 安装脚本（§23.2：固定版本 + sha256 校验，禁止查 latest）。
# 用法: install-gost.sh <arch: amd64|arm64> [local-tarball-path]
# 可选第二参数：本地已下载的 tarball 路径（跳过网络下载，仍做 sha256 校验）。
# 版本与哈希不内置任何副本：GOST_VERSION / EXPECTED_GOST_SHA256 必须由调用方
# 提供（两个 Dockerfile 均从 crates/xtask/src/versions.json 用 jq 解析后导出）。
set -euo pipefail

: "${GOST_VERSION:?GOST_VERSION must be set (source of truth: crates/xtask/src/versions.json)}"
: "${EXPECTED_GOST_SHA256:?EXPECTED_GOST_SHA256 must be set (source of truth: crates/xtask/src/versions.json)}"

ARCH="${1:-amd64}"
LOCAL_TARBALL="${2:-}"

FILE="gost_${GOST_VERSION}_linux_${ARCH}.tar.gz"
URL="https://github.com/go-gost/gost/releases/download/v${GOST_VERSION}/${FILE}"

if [ -n "${LOCAL_TARBALL}" ]; then
  echo "[gost-install] using local tarball: ${LOCAL_TARBALL}"
  cp "${LOCAL_TARBALL}" "${FILE}"
else
  curl -fsSL --retry 5 --retry-all-errors --retry-delay 3 --max-time 300 -o "${FILE}" "${URL}"
fi
echo "${EXPECTED_GOST_SHA256}  ${FILE}" | sha256sum -c -
tar -xzf "${FILE}" -C /usr/bin/ gost
rm -f "${FILE}"
chmod +x /usr/bin/gost
