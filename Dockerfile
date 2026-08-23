# syntax=docker/dockerfile:1.7
# WarpDeck release image（P11-001 完整版）。
#
# 多阶段：node builder（vite build）→ rust builder（cargo release）→ runtime。
# Runtime 含数据面全量：Cloudflare WARP（固定 deb）+ GOST（固定版本 + sha256）+ D-Bus
# + CA + tini。构建期经 docker/fetch-deps.sh 下载依赖（断点续传 + cache mount 持久 +
# 强制 SHA256 校验）；中国网络下用 --build-arg DL_PROXY=socks5h://host.docker.internal:10808
# 走宿主代理（需代理端允许 LAN），CI/海外直连即可。
# 入口脚本：`cargo xtask release`（crates/xtask）。URL/哈希/版本的唯一来源是
# crates/xtask/src/versions.json：runtime 阶段直接从 build context COPY 后用 jq
# 解析，Dockerfile 不再保存任何默认值副本（手动 `docker build` 同样零参数生效）。

# ---------- frontend builder ----------
# node 大版本与 CI（setup-node 24）及 web/package.json engines（>=24）保持一致；
# pnpm 版本与 packageManager 字段 / ci.yml 三处一致。
FROM node:24-slim AS web-builder

RUN npm config set registry https://registry.npmmirror.com \
    && npm install -g pnpm@11.22.0

WORKDIR /build
COPY web/package.json web/pnpm-lock.yaml web/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ .
RUN pnpm build

# ---------- rust builder ----------
# 必须与 runtime 同系 glibc（ubuntu:24.04=glibc 2.39，rust:*-bookworm=2.36 兼容）。
FROM rust:1.96.0 AS rust-builder

# 与 docker/Dockerfile.dev-rust 同款 aliyun sparse 源（国内网络）。
RUN mkdir -p /usr/local/cargo \
    && printf '[source.crates-io]\nreplace-with = "aliyun"\n\n[source.aliyun]\nregistry = "sparse+https://mirrors.aliyun.com/crates.io-index/"\n' > /usr/local/cargo/config.toml

ENV CARGO_TERM_COLOR=never
WORKDIR /build
COPY Cargo.toml ./
COPY crates/ ./crates/
RUN cargo build --release --package warpdeck-server

# ---------- runtime ----------
FROM ubuntu:24.04 AS runtime

# P12-012：版本元数据（`cargo xtask release` 注入 0.1.0-<git sha>）。
ARG WARPDECK_VERSION=0.1.0-dev
# 构建期代理（P12-001）：如 socks5h://host.docker.internal:10808，需代理端允许 LAN；
# 留空直连（CI/海外网络即可）。产物落 cache mount，跨构建免重复下载。
# GOST/WARP 的 URL/SHA256/版本不设 ARG：唯一来源 = crates/xtask/src/versions.json，
# 由下方 COPY + jq 直接消费，杜绝「Dockerfile 默认值」这第二份副本。
ARG DL_PROXY=""
LABEL org.opencontainers.image.title="WarpDeck" \
      org.opencontainers.image.version="${WARPDECK_VERSION}" \
      org.opencontainers.image.revision="${WARPDECK_VERSION}" \
      org.opencontainers.image.description="Cloudflare WARP multi-instance manager with SOCKS5/HTTP proxy"

# P11-003 最小权限审计结论（root 是数据面硬性要求，不是偷懒）：
# - warp-svc 需要创建 tun 设备 → 必须 root 或容器级 --device /dev/net/tun +
#   --cap-add NET_ADMIN（compose 提供）；容器内无法降权运行数据面。
# - 不安装 sudo / ssh / 编译工具 / 包管理器运行时依赖：镜像内没有任何提权机制，
#   "最小权限" = 不提供多余权限通道，而非额外包装一层 sudo。
# - 构建期安装后清空 apt lists 与包缓存；/var/lib/warpdeck 与 /run/warpdeck
#   是仅有的两个可写数据目录（volume 持久化）。
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    sed -i \
        -e 's|http://archive.ubuntu.com/ubuntu|http://mirrors.aliyun.com/ubuntu|g' \
        -e 's|http://security.ubuntu.com/ubuntu|http://mirrors.aliyun.com/ubuntu|g' \
        -e 's/^Components: main restricted$/Components: main restricted universe multiverse/' \
        /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get update -o Acquire::Retries=5 \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg jq lsb-release dbus tini iproute2

COPY docker/install-warp.sh docker/install-gost.sh docker/fetch-deps.sh /usr/local/bin/

# versions.json 是 GOST/WARP 版本/URL/SHA256 的唯一来源；jq 仅在构建期使用，
# 安装完成后 purge（保持 P11-003 最小镜像面不变）。
COPY crates/xtask/src/versions.json /tmp/versions.json

# P12-001(补齐 P11-002)：构建期下载 + 双重 sha256 校验（fetch-deps.sh 对下载产物、
# EXPECTED_GOST_SHA256 传入 install-gost.sh 复核同源取值）。deb/tarball 不落镜像层：
# 下载在 cache mount，安装后临时副本即弃。
# install-warp.sh 对 deb 做"剪 GUI 依赖的重打包"(webkit/LLVM/mesa 等 ~360MB),
# apt lists 必须留到 WARP/GOST 安装之后才清理(install-warp.sh 依赖包索引)。
#
# 提取健壮性（2026-08-22 审查补强）：RUN 脚本默认无 set -e，jq 失败（键缺失输出
# null、JSON 损坏退出非零）会静默变空串直到 fetch 阶段才费解报错——故 set -eu +
# jq -er '.x // error(...)' 硬门禁。dash 下同一句 export 内自引用不保证看到左侧
# 新值，因此用「裸赋值列表（保证从左到右）+ 单独 export」；注释不放进续行内，
# 避免依赖前端对续行内整行注释的剥离行为。
RUN --mount=type=cache,target=/dl-cache,sharing=locked \
    set -eu \
    && WARP_DEB_URL="$(jq -er '.warp.url // error("versions.json: .warp.url missing")' /tmp/versions.json)" \
    && WARP_DEB_SHA256="$(jq -er '.warp.sha256 // error("versions.json: .warp.sha256 missing")' /tmp/versions.json)" \
    && GOST_TARBALL_URL="$(jq -er '.gost.url // error("versions.json: .gost.url missing")' /tmp/versions.json)" \
    && GOST_TARBALL_SHA256="$(jq -er '.gost.sha256 // error("versions.json: .gost.sha256 missing")' /tmp/versions.json)" \
    && GOST_VERSION="$(jq -er '.gost.version // error("versions.json: .gost.version missing")' /tmp/versions.json)" \
    && EXPECTED_GOST_SHA256="${GOST_TARBALL_SHA256}" \
    && export DL_PROXY WARP_DEB_URL WARP_DEB_SHA256 GOST_TARBALL_URL GOST_TARBALL_SHA256 GOST_VERSION EXPECTED_GOST_SHA256 \
    && bash /usr/local/bin/fetch-deps.sh "${WARP_DEB_URL}" "${WARP_DEB_SHA256}" 60000000 "/dl-cache/${WARP_DEB_URL##*/}" \
    && bash /usr/local/bin/fetch-deps.sh "${GOST_TARBALL_URL}" "${GOST_TARBALL_SHA256}" 9000000 "/dl-cache/${GOST_TARBALL_URL##*/}" \
    && echo 'sha256 of pinned WARP deb and GOST tarball verified' \
    && cp "/dl-cache/${WARP_DEB_URL##*/}" /tmp/cloudflare-warp.deb \
    && bash /usr/local/bin/install-warp.sh /tmp/cloudflare-warp.deb \
    && bash /usr/local/bin/install-gost.sh amd64 "/dl-cache/${GOST_TARBALL_URL##*/}" \
    && rm -f /usr/local/bin/install-warp.sh /usr/local/bin/install-gost.sh /usr/local/bin/fetch-deps.sh /tmp/versions.json \
    && apt-get purge -y jq \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/lib/warpdeck /run/warpdeck

WORKDIR /app
COPY --from=rust-builder /build/target/release/warpdeck-server /app/warpdeck-server
COPY --from=web-builder /build/dist /app/ui

ENV WARPDECK_DATA_DIR=/var/lib/warpdeck \
    WARPDECK_RUNTIME_DIR=/run/warpdeck \
    WARPDECK_UI_DIR=/app/ui \
    WARPDECK_BIND=0.0.0.0 \
    WARPDECK_PORT=9000 \
    WARPDECK_VERSION=${WARPDECK_VERSION}

EXPOSE 9000 11080 18080

# HEALTHCHECK 反映 manager 基本 readiness（P11-005：不做昂贵外网 probe）。
HEALTHCHECK --interval=15s --timeout=5s --start-period=10s \
  CMD curl -fsS http://127.0.0.1:9000/api/v1/health > /dev/null || exit 1

# tini 作为 PID 1：正确转发信号 + 收割孤儿（warp-svc/dbus/gost 子进程）。
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/app/warpdeck-server"]
