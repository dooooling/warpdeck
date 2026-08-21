#!/usr/bin/env bash
# WarpDeck 构建期依赖下载（2026-08-21 起取代宿主缓存方案）。
#
# 设计：
# - 断点续传循环：curl -C - 分片拉取，直到 字节数达标 且 SHA256 匹配；
# - 产物落 BuildKit cache mount（调用方 --mount=type=cache,target=/dl-cache），
#   跨构建持久——等效旧「宿主 ~/.cache/warpdeck」但不再占用/依赖宿主目录；
# - 代理经 DL_PROXY 环境变量注入（--build-arg DL_PROXY=socks5h://host.docker.internal:10808，
#   需代理端允许 LAN；CI/海外直连则留空）；
# - SHA256 为硬门禁：不匹配即失败，绝不带病安装（P12-001 纪律不变）。
#
# 用法: fetch-deps.sh <url> <expected_sha256> <min_bytes> <out_path>
set -euo pipefail

URL="$1"; EXPECTED="$2"; MIN_BYTES="$3"; OUT="$4"

mkdir -p "$(dirname "${OUT}")"

verify() {
  [ -f "${OUT}" ] || return 1
  local sz
  sz=$(stat -c%s "${OUT}" 2>/dev/null || echo 0)
  [ "${sz}" -ge "${MIN_BYTES}" ] || return 1
  echo "${EXPECTED}  ${OUT}" | sha256sum -c --status
}

if verify; then
  echo "[fetch] cache hit: ${OUT}"
  exit 0
fi

for i in $(seq 1 30); do
  # curl 退出码 33 = RANGE 不被支持（文件已完整等），与 0 一并视为无害。
  rc=0
  if [ -n "${DL_PROXY:-}" ]; then
    curl -sS -L --http1.1 -C - --max-time 120 -x "${DL_PROXY}" -o "${OUT}" "${URL}" || rc=$?
  else
    curl -sS -L --http1.1 -C - --max-time 120 -o "${OUT}" "${URL}" || rc=$?
  fi
  if [ "${rc}" -ne 0 ] && [ "${rc}" -ne 33 ]; then
    echo "[fetch] curl exit ${rc} (round ${i})" >&2
  fi
  if verify; then
    echo "[fetch] OK: ${OUT} ($(stat -c%s "${OUT}") bytes)"
    exit 0
  fi
  sleep 2
done

echo "[fetch] failed after 30 rounds: ${OUT}" >&2
exit 1
