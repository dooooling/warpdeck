#!/usr/bin/env bash
# WarpDeck dev-base: GOST 安装脚本（§23.2：固定版本 + sha256 校验，禁止查 latest）。
# 用法: install-gost.sh <arch: amd64|arm64> [local-tarball-path]
# 可选第二参数：本地已下载的 tarball 路径（跳过网络下载，仍做 sha256 校验）。
# 哈希来源优先级：EXPECTED_GOST_SHA256 环境变量（cargo xtask 从 versions.json 注入，
# 单一事实来源）→ 内置 per-arch 表（手动构建兜底）。
set -euo pipefail

# 固定版本 + sha256 校验；发布仓库 go-gost/gost（ginuerzh 为旧组织名）。
GOST_VERSION="${GOST_VERSION:-3.2.6}"
ARCH="${1:-amd64}"
LOCAL_TARBALL="${2:-}"

if [ -n "${EXPECTED_GOST_SHA256:-}" ]; then
  GOST_SHA256="${EXPECTED_GOST_SHA256}"
else
  case "${ARCH}" in
    amd64) GOST_SHA256="b39037b0380ea001fb3c0c28441c2e10bfc694f90682739a65b53e55dce5238b" ;;
    arm64) GOST_SHA256="f674c8f4a033dc1dfd4f0d5e9602fbe5b0d0f81307bf3794f44b5b5d6d622eae" ;;
    *) echo "unsupported arch: ${ARCH}" >&2; exit 1 ;;
  esac
fi

FILE="gost_${GOST_VERSION}_linux_${ARCH}.tar.gz"
URL="https://github.com/go-gost/gost/releases/download/v${GOST_VERSION}/${FILE}"

if [ -n "${LOCAL_TARBALL}" ]; then
  echo "[gost-install] using local tarball: ${LOCAL_TARBALL}"
  cp "${LOCAL_TARBALL}" "${FILE}"
else
  curl -fsSL --retry 5 --retry-all-errors --retry-delay 3 --max-time 300 -o "${FILE}" "${URL}"
fi
echo "${GOST_SHA256}  ${FILE}" | sha256sum -c -
tar -xzf "${FILE}" -C /usr/bin/ gost
rm -f "${FILE}"
chmod +x /usr/bin/gost