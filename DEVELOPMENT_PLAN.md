# WarpDeck 开发实施计划

> 文档类型：Development Plan / Delivery Plan / Work Breakdown Structure  
> 项目：WarpDeck - Cloudflare WARP Web Manager  
> 技术基线：Rust + Axum + Tokio + SQLite + React + TypeScript + GOST + Cloudflare WARP  
> 配套技术设计：`DESIGN_AND_DEVELOPMENT.md`  
> 计划目标：把现有技术设计转换为可以从零执行、逐阶段验收、可直接创建 GitHub Milestones / Issues 的开发路线。  
> MVP 协议范围：**仅 SOCKS5 与 HTTP**。  
> 默认容器端口：Web/API `9000`、SOCKS5 `11080`、HTTP `18080`；WARP 内部端口 `40000 + instance_id`。  
> 开发纪律：**普通开发与普通 PR 不允许通过反复 `docker build` 作为测试循环。**

---

## 目录

- [1. 如何使用本计划](#1-如何使用本计划)
- [2. 项目范围与交付定义](#2-项目范围与交付定义)
- [3. 总体执行原则](#3-总体执行原则)
- [4. 项目阶段总览](#4-项目阶段总览)
- [5. Phase 0：仓库与工程基线](#5-phase-0仓库与工程基线)
- [6. Phase 1：后端 Skeleton 与基础设施](#6-phase-1后端-skeleton-与基础设施)
- [7. Phase 2：单实例 WARP Runtime](#7-phase-2单实例-warp-runtime)
- [8. Phase 3：多实例隔离与生命周期](#8-phase-3多实例隔离与生命周期)
- [9. Phase 4：健康检查与数据面探测](#9-phase-4健康检查与数据面探测)
- [10. Phase 5：GOST Proxy Gateway](#10-phase-5gost-proxy-gateway)
- [11. Phase 6：SQLite Desired State 与 Reconciler](#11-phase-6sqlite-desired-state-与-reconciler)
- [12. Phase 7：REST API](#12-phase-7rest-api)
- [13. Phase 8：认证、会话与 Secret Store](#13-phase-8认证会话与-secret-store)
- [14. Phase 9：React Web UI](#14-phase-9react-web-ui)
- [15. Phase 10：SSE、实时状态与日志](#15-phase-10sse实时状态与日志)
- [16. Phase 11：Docker 集成与 E2E](#16-phase-11docker-集成与-e2e)
- [17. Phase 12：安全加固与 Release](#17-phase-12安全加固与-release)
- [18. 横向工程工作流](#18-横向工程工作流)
- [19. 测试实施计划](#19-测试实施计划)
- [20. Docker 构建与环境控制计划](#20-docker-构建与环境控制计划)
- [21. CI/CD 与 PR 门禁](#21-cicd-与-pr-门禁)
- [22. 风险登记与应对](#22-风险登记与应对)
- [23. GitHub Milestones 与 Issues 组织](#23-github-milestones-与-issues-组织)
- [24. 每日开发执行模板](#24-每日开发执行模板)
- [25. 阶段评审模板](#25-阶段评审模板)
- [26. MVP 最终验收计划](#26-mvp-最终验收计划)
- [27. Release 后续计划](#27-release-后续计划)
- [附录 A：Definition of Ready](#附录-adefinition-of-ready)
- [附录 B：Definition of Done](#附录-bdefinition-of-done)
- [附录 C：推荐命令](#附录-c推荐命令)
- [附录 D：任务状态与优先级](#附录-d任务状态与优先级)

---

# 1. 如何使用本计划

本文件是**实施计划**，不是技术设计的替代品。

两个文档的职责应保持清晰：

| 文档 | 回答的问题 |
|---|---|
| `DESIGN_AND_DEVELOPMENT.md` | 系统应该怎样设计、为什么这样设计、接口和模型是什么 |
| `DEVELOPMENT_PLAN.md` | 从哪里开始、先做什么、依赖什么、怎样测试、什么时候算完成 |

实际开发时，任何工作项至少应对应：

```text
需求 / Issue
   |
   v
本计划中的 Phase + Task ID
   |
   v
设计文档中的技术章节
   |
   v
代码 + 测试 + 文档
   |
   v
阶段 Gate / PR Gate
```

建议原则：

1. 不跳 Phase 的关键 Gate。
2. 可以并行做互不依赖的任务，但不能绕过依赖。
3. 每个 Task 都要有可验证结果。
4. 每个 Phase 结束前执行阶段评审。
5. 设计变化必须先更新设计文档，再更新本计划。
6. 不用 Docker 镜像数量代表“测试充分”。

---

# 2. 项目范围与交付定义

## 2.1 MVP 必须交付

MVP 必须完成：

- 单机运行。
- Web 管理页面。
- 首次管理员初始化。
- 登录、登出、会话管理。
- 动态创建 WARP 实例。
- Start / Stop / Restart / Delete WARP 实例。
- 多实例独立状态目录、Runtime 目录、D-Bus。
- 健康检查。
- Exit IP / Colo / Latency 展示。
- SOCKS5 代理，容器内监听 `11080`。
- HTTP 代理，容器内监听 `18080`。
- 只把 Healthy WARP 实例加入 GOST Pool。
- Round Robin。
- Proxy authentication。
- IP allowlist。
- 基本 rate limit / connection limit。
- Free WARP。
- WARP+ License。
- Zero Trust Service Token Enrollment。
- SQLite 持久化。
- Manager 重启后 Desired State 恢复。
- Reconciler。
- SSE 实时状态。
- 实时/近实时日志查看。
- Secret 加密存储与日志脱敏。
- 单镜像 Release。
- Compose 示例。
- CI / Test / Security Scan / SBOM。

## 2.2 MVP 明确不做

以下功能不进入 v0.1：

- Direct Proxy。
- Shadowsocks。
- WireGuard 自己实现。
- 多主机控制平面。
- Kubernetes Operator。
- 动态修改 Docker Host publish port。
- HTTP CONNECT 之外的复杂协议扩展。
- 复杂策略路由。
- Weighted routing。
- Latency-aware routing。
- 用户多租户 / RBAC。
- 手机 App。
- 自动修改宿主机系统代理。

## 2.3 固定端口基线

| 功能 | Container Port | Host 默认映射 |
|---|---:|---:|
| Web UI + API + SSE | `9000` | `9000` |
| SOCKS5 through WARP | `11080` | `11080` |
| HTTP through WARP | `18080` | `18080` |
| WARP instance 0 | `40000` | 不映射 |
| WARP instance N | `40000 + N` | 不映射 |

宿主机端口通过 Compose `.env` 修改，不通过 Web UI 修改。

## 2.4 MVP 成功标准

最终用户只需：

```bash
docker compose up -d
```

然后访问：

```text
http://localhost:9000
```

创建至少一个实例后：

```bash
curl --socks5-hostname 127.0.0.1:11080 \
  https://cloudflare.com/cdn-cgi/trace
```

和：

```bash
curl -x http://127.0.0.1:18080 \
  https://cloudflare.com/cdn-cgi/trace
```

均能返回：

```text
warp=on
```

---

# 3. 总体执行原则

## 3.1 先建立可测试边界，再接真实 WARP

不得从第一天把业务逻辑直接写死到：

```rust
Command::new("warp-cli")
```

正确顺序：

```text
Domain Model
   -> Runtime Trait
      -> Fake Runtime
      -> Real Runtime
```

目标是让 80% 以上的开发测试不依赖真实 WARP。

## 3.2 先单实例，再多实例

所有多实例复杂度建立在单实例生命周期可靠之后。

禁止一开始同时调：

- 多 WARP；
- GOST；
- Web UI；
- SQLite；
- Docker；
- Zero Trust。

## 3.3 先 Runtime，再控制面

推荐依赖顺序：

```text
Process abstraction
   -> WARP Runtime
   -> Multi-instance
   -> Health
   -> GOST
   -> Persistence/Reconciler
   -> API
   -> Auth
   -> UI
   -> Docker Release
```

## 3.4 Desired State 与 Actual State 分离

数据库负责：

```text
用户希望系统怎样运行
```

Runtime Registry 负责：

```text
当前真实运行状态
```

Reconciler 负责把两者拉齐。

## 3.5 Docker 是交付物，不是日常编译器

默认开发循环：

```text
cargo check
cargo test
cargo run
pnpm dev
```

只有真实 WARP 需要 Linux 容器环境时使用固定 Dev Base。

普通代码改动禁止不断创建新镜像。

## 3.6 每阶段必须有 Stop/Go Gate

任何 Phase 完成后都必须回答：

- 功能是否满足？
- Failure path 是否测过？
- 是否引入 Secret 泄露风险？
- 是否有 orphan process？
- 文档是否同步？
- 是否能进入下阶段？

---

# 4. 项目阶段总览

| Phase | 主题 | 主要结果 | 依赖 | 风险级别 |
|---:|---|---|---|---|
| 0 | 工程基线 | 仓库、工具链、CI Skeleton | 无 | 低 |
| 1 | Backend Skeleton | Axum/SQLite/Tracing/Shutdown | P0 | 低 |
| 2 | Single WARP Runtime | 1 个真实 WARP 可生命周期管理 | P1 | 高 |
| 3 | Multi-instance | 3 实例并行隔离 | P2 | 高 |
| 4 | Health | Control/Data plane health | P2/P3 | 中 |
| 5 | GOST | SOCKS5 + HTTP Proxy Gateway | P3/P4 | 高 |
| 6 | Persistence/Reconciler | Desired State 恢复 | P3/P4/P5 | 高 |
| 7 | REST API | 完整控制 API | P6 | 中 |
| 8 | Auth/Secret | 管理认证和密钥安全 | P7 | 高 |
| 9 | Web UI | 可完成日常管理 | P7/P8 | 中 |
| 10 | SSE/Logs | 实时状态与日志 | P7/P9 | 中 |
| 11 | Docker E2E | 可部署单镜像 | P0-P10 | 高 |
| 12 | Hardening/Release | v0.1 候选发布 | P11 | 高 |

建议并行关系：

```text
Phase 0
   |
Phase 1
   |
Phase 2
   |
Phase 3 --------> Phase 4
   |                |
   +-------> Phase 5
              |
           Phase 6
              |
           Phase 7
          /       \
     Phase 8     UI Skeleton
          \       /
           Phase 9
              |
          Phase 10
              |
          Phase 11
              |
          Phase 12
```

---
# 5. Phase 0：仓库与工程基线

## 5.1 阶段目标

建立一个任何开发者克隆后都能稳定启动的工程骨架。

本阶段**不接 WARP，不接 GOST，不做 Docker Release**。

## 5.2 前置条件

- 已确定项目名与仓库位置。
- 已接受 MVP 范围。
- 已接受端口基线。
- 已接受 Rust + React 技术栈。

## 5.3 工作项

### P0-001 创建 monorepo

建议目录：

```text
warpdeck/
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── crates/
│   └── warpdeck-server/
├── web/
├── migrations/
├── runtime/
├── tests/
├── scripts/
├── docs/
├── docker/
├── .github/workflows/
├── DESIGN_AND_DEVELOPMENT.md
└── DEVELOPMENT_PLAN.md
```

验收：

- `cargo metadata` 成功。
- `pnpm install` 成功。
- 项目目录命名与设计文档一致。

### P0-002 固定 Rust Toolchain

创建：

```text
rust-toolchain.toml
```

要求：

- 使用 stable channel。
- CI 与本地一致。
- 禁止开发者随意依赖 nightly-only feature。

### P0-003 初始化 React + TypeScript

要求：

- Vite。
- TypeScript strict。
- ESLint。
- 基础 test runner。
- React Router。
- 不急着引入大型 UI 框架。

### P0-004 建立统一命令入口

推荐 `justfile` 或 `Makefile`：

```text
check
check-backend
check-web
test
test-unit
test-component
dev-server
dev-web
```

此阶段不要让 `check` 隐式执行 Docker Build。

### P0-005 Git 基线

配置：

```text
.gitignore
.gitattributes
.editorconfig
```

忽略：

```text
target/
web/node_modules/
web/dist/
.env
.env.local
*.db
*.db-shm
*.db-wal
runtime-data/
coverage/
```

### P0-006 CI Skeleton

PR 先只跑：

```text
cargo fmt --check
cargo check
frontend install
frontend typecheck
```

不要在 Phase 0 配置 Docker image build。

### P0-007 建立文档索引

创建：

```text
docs/README.md
```

至少链接：

- 技术设计。
- 开发计划。
- 测试规范章节。
- 许可证说明。

### P0-008 许可证决策记录

在开发开始前确定项目许可证与组件许可边界，书面记录：

- 项目代码/文档采用的许可证（MIT）；
- 镜像内嵌组件（Cloudflare WARP / GOST / 依赖包）各自的许可证与服务条款；
- 发布/再分发边界（README「License」节已列明）。

## 5.4 测试要求

```bash
cargo fmt --check
cargo check --workspace
cd web && pnpm typecheck
```

## 5.5 交付物

- 可 clone 的仓库。
- Rust workspace。
- React workspace。
- CI Skeleton。
- 基础文档。
- 许可证说明。

## 5.6 Phase Gate

```text
[x] cargo check 成功
[x] pnpm typecheck 成功
[x] CI 成功
[x] 没有 Docker Build
[x] 文档入口存在
[x] 许可证说明书面记录
```

### 完成记录（2026-08-17）

```text
cargo check --workspace 通过；web/ pnpm typecheck 通过（Vite + TS strict + oxlint）
CI: .github/workflows/ci.yml 骨架存在，未配置 Docker build
文档入口 docs/README.md + 许可证说明（MIT + 组件许可）
justfile 统一命令入口；migrations/ runtime/ tests/ 为占位骨架（按阶段填充）
rust-toolchain.toml 固定 stable 1.96
```

---

# 6. Phase 1：后端 Skeleton 与基础设施

## 6.1 阶段目标

建立后续所有功能依赖的 Rust 服务框架。

完成后，服务应当具备：

```text
Axum Router
AppState
Config
SQLite
Migration
Tracing
Error Mapping
Graceful Shutdown
Health Endpoint
```

## 6.2 工作项

### P1-001 AppConfig

配置来源建议：

```text
Environment
  -> typed AppConfig
  -> validation
```

初始字段：

```rust
struct AppConfig {
    web_bind: SocketAddr,
    data_dir: PathBuf,
    runtime_dir: PathBuf,
    database_url: String,
}
```

注意：

- `9000/11080/18080` 应集中成常量或 typed defaults。
- API 不允许动态改变 container listener port。

### P1-002 AppState

初始只放：

```text
config
db
shutdown token
```

不要提前把所有未来组件塞进去。

### P1-003 统一 AppError

至少定义：

```text
Validation
NotFound
Conflict
Unauthorized
Forbidden
Database
Runtime
Internal
```

HTTP Handler 不应自行拼字符串错误。

### P1-004 Structured Logging

使用 `tracing`。

基本字段：

```text
request_id
component
event
instance_id
error_code
duration_ms
```

### P1-005 Request ID Middleware

每个 HTTP request：

- 接受已有合法 request id 或生成新值；
- response 返回 request id；
- tracing span 自动包含。

### P1-006 SQLite Connection Pool

要求：

- 初始化 pool。
- 设置合理 busy timeout。
- 开启 WAL（如果设计确认）。
- 应用 migration。

### P1-007 Migration #0001

本阶段只需要建立最基础 schema framework。

可以先建 `settings` 或 migration marker，不必一次性完成全部表。

### P1-008 `/api/v1/health`

返回示例：

```json
{
  "status": "ok",
  "version": "0.1.0-dev"
}
```

不得检查真实 WARP。

### P1-009 Graceful Shutdown

SIGTERM / Ctrl+C：

```text
stop accepting requests
  -> notify background tasks
  -> wait bounded timeout
  -> close db
  -> exit
```

### P1-010 Test Harness

建立：

```text
TestApp
Temp DB
Router in-memory request helper
```

后续 API tests 复用。

## 6.3 推荐代码落点

```text
crates/warpdeck-server/src/
├── main.rs
├── app.rs
├── config.rs
├── error.rs
├── shutdown.rs
├── api/
│   ├── mod.rs
│   └── health.rs
├── db/
│   ├── mod.rs
│   └── migrations.rs
└── observability/
    └── mod.rs
```

## 6.4 测试要求

### L0

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

### L1

测试：

- Config 合法值。
- Config 非法路径/端口。
- Error mapping。
- Request ID。

### L2

测试：

- 临时 SQLite 启动。
- Migration 可重复执行。
- `/health` 返回 200。
- Shutdown 可以完成。

## 6.5 交付物

- 可独立运行的 backend。
- 基础 migration。
- Router test harness。
- Structured logs。

## 6.6 Phase Gate

```text
[x] cargo run 后 /api/v1/health = 200
[x] SQLite migration 自动执行
[x] SIGTERM 不产生 panic
[x] 所有 L0/L1/L2 通过
[x] 不依赖 WARP
[x] 不依赖 GOST
[x] 不需要 Docker image
```

### 完成记录（2026-08-17）

```text
app/config/error/shutdown/db/observability/api 按 §6.3 落点全部就位
L0/L1/L2 通过：config 校验、Error 防泄露、request-id 注入（header + 错误契约 body）、
  SQLite WAL + busy timeout(5s) + 内嵌 migration 幂等、/api/v1/health = 200
实测（Linux 容器真实进程）：health 200 {"status":"ok","version":"0.1.0-dev"}；
  SIGTERM → "shutdown signal received" → "database closed"，无 panic
review 修复：db::connect 对不存在的父目录先 create_dir_all
  （sqlx create_if_missing 只建文件不建目录，此前 data_dir 缺失时启动 panic）
不依赖 WARP / GOST / Docker 构建
```

---

# 7. Phase 2：单实例 WARP Runtime

## 7.1 阶段目标

可靠管理 `instance 0` 的完整生命周期，并且首先建立 Fake Runtime。

这是整个项目第一个高风险阶段。

## 7.2 关键设计要求

业务层不能直接调用 `warp-cli`。

推荐抽象：

```rust
trait WarpControl {
    async fn status(&self, ctx: &InstanceContext) -> Result<WarpCliStatus>;
    async fn register(&self, ctx: &InstanceContext) -> Result<()>;
    async fn set_proxy_mode(&self, ctx: &InstanceContext) -> Result<()>;
    async fn set_proxy_port(&self, ctx: &InstanceContext, port: u16) -> Result<()>;
    async fn connect(&self, ctx: &InstanceContext) -> Result<()>;
    async fn disconnect(&self, ctx: &InstanceContext) -> Result<()>;
}
```

以及：

```rust
trait ProcessSpawner
trait Clock
trait BackoffPolicy
```

## 7.3 工作项

### P2-001 InstanceId / Port 类型

禁止裸 `i64` 在整个代码库随意计算端口。

实现：

```text
InstanceId
InternalProxyPort
instance_port(id)
```

验证：

- id >= 0。
- `40000 + id` 不溢出 `u16`。
- 不与保留端口冲突。

### P2-002 InstancePaths

集中生成：

```text
state_dir
runtime_dir
dbus_dir
dbus_socket
log_path
```

不得手工字符串拼接散落各模块。

### P2-003 FakeWarpControl

至少可以模拟：

```text
NotRegistered
Disconnected
Connected
CommandTimeout
RegistrationFailure
ConnectFailure
```

### P2-004 FakeProcessSpawner

可以验证：

- 启动参数。
- 环境变量。
- kill/reap。
- crash event。

### P2-005 D-Bus Runtime

真实环境中为每实例建立独立 D-Bus socket。

需要实现：

```text
create runtime dir
start dbus-daemon
capture pid/handle
wait socket ready
shutdown/reap
```

### P2-006 warp-svc Spawn

以实例独立环境启动：

```text
STATE_DIRECTORY
RUNTIME_DIRECTORY
DBUS_SYSTEM_BUS_ADDRESS
```

必须记录：

- PID。
- start time。
- exit status。
- stderr summary。

### P2-007 warp-cli Adapter

所有命令必须：

- `Command::new` + `.arg`。
- 不使用 shell 拼接。
- timeout。
- capture stderr。
- structured error。

### P2-008 Readiness Probe

不能仅以 PID 存在判 Ready。

需要：

```text
warp-cli status succeeds
```

并设置 bounded retry/backoff。

### P2-009 Registration Flow

流程：

```text
检查 registration state
   -> 没有注册：registration new
   -> mode proxy
   -> proxy port 40000
   -> connect
   -> status verify
```

### P2-010 Graceful Stop

流程：

```text
disconnect if possible
  -> terminate warp-svc
  -> wait timeout
  -> force kill if needed
  -> stop dbus
  -> reap children
  -> clean runtime dir
```

持久化 state dir 不应被普通 stop 删除。

### P2-011 Crash Watcher

warp-svc 意外退出：

- manager 不退出；
- RuntimeState -> Failed；
- 记录 last error；
- 发内部 event；
- 后续由 Reconciler 决定是否 restart。

本阶段可以先只记录事件，不急着自动重启。

### P2-012 Real WARP Integration Harness

使用固定开发镜像，不构建 Release image。

固定命名：

```text
warpdeck-dev-base:1
```

Dev Base 内包含：

```text
Cloudflare WARP
GOST
D-Bus
curl
ca-certificates
```

普通 Rust binary 通过 bind mount 或复制到运行容器测试。

### 构建约束（实测 2026-08；2026-08-21 修订为构建期内下载）

中国网络下 `pkg.cloudflareclient.com` / GitHub release 直连被重置或极慢。
原「宿主预下载 + build-context 注入」已改为**镜像构建期内下载**，落地做法：

```text
1. docker/fetch-deps.sh（容器内）：断点续传 + BuildKit cache mount（/dl-cache）
   持久 + SHA256 硬校验（不匹配绝不安装）
2. 代理经 --build-arg DL_PROXY=socks5h://host.docker.internal:10808 注入
   （需代理端允许 LAN）；CI/海外直连留空
3. URL/SHA256/版本单一来源 = crates/xtask/src/versions.json，
   `cargo xtask release | dev-base` 经 --build-arg 注入；
   install-gost.sh 另收 EXPECTED_GOST_SHA256 复核同源取值
4. /var/lib/apt/lists 不能用 BuildKit cache mount：
   缓存被清后 RUN 层 CACHED 跳过 apt-get update -> 索引为空 ->
   所有依赖报 "not installable"。只缓存 /var/cache/apt。
5. 构建入口统一 `cargo xtask`（crates/xtask + .cargo/config.toml alias），
   原 scripts/*.ps1 编排层已删除
```

冒烟脚本 `scripts/smoke-dev-base.ps1`（免费注册即可，无需 WARP+ license）。

### 完成记录（2026-08）

```text
docker build -t warpdeck-dev-base:1 .          # 成功，双 build-context
components: warp 2026.6.880.0 / gost v3.2.6 / dbus 1.14.10 / tini
real data plane: registration new -> mode proxy -> port 40000 -> connect
  warp-cli status: Connected / Network: healthy
  curl --socks5-hostname 127.0.0.1:40000 https://cloudflare.com/cdn-cgi/trace
  -> warp=on, colo=LAX
```

### 完成记录补充（2026-08-17，P2 Phase Gate 验收）

```text
L1：99 个单元测试通过（Fake 运行时 + ManualClock 虚拟时间），clippy -D warnings 干净
L4 真实 E2E（p2_gate_check，容器内真实 warp-svc，无任何 Fake）：
  dbus ok -> warp-svc ok -> control-plane ready(2 attempts) -> 真实注册(1 attempt)
  -> flow ok -> DATA_PLANE_READY port=40000
  数据面实测: curl --socks5-hostname 127.0.0.1:40000 https://cloudflare.com/cdn-cgi/trace
  -> warp=on, 出站为 Cloudflare WARP 地址（colo=LAX）
  SIGTERM -> "signal received" -> STOP_OK kill_required=false exit_code=Some(0)，容器 Exited(0)
孤儿/崩溃实查（sh 包装容器，gate 为子进程）：
  kill -9 warp-svc 后 gate 存活不退出（crash 不波及 supervisor）
  SIGTERM 优雅停止后容器内无任何残留进程（含僵尸），GracefulStop 可 reap 已死子进程
实现修复：
  flow.rs verify 由一次性 status 检查改为有界轮询（2s 间隔 / 90s 超时）：
    warp-cli connect 是异步命令，daemon 需数秒完成 QUIC+PQ 握手，此前必误报
    VerifyFailed（实测首连 happy-eyeballs >10s）
  process.rs SIGKILL 断言兼容 Linux signal()=9（此前仅 Windows 上跑过）
编译链（国内网络）：docker/Dockerfile.dev-rust 内嵌 crates-io -> aliyun sparse
  config.toml（env 变量 CARGO_REGISTRIES_CRATES_IO_INDEX 对 crates-io 无效，
  实测 cargo 退回官方 index）；RUSTUP_OFFLINE=1 避免容器内 rustup sync 卡死；
  构建循环：宿主 cargo test/clippy（原生增量，秒级）+ 容器仅增量编译 Linux 二进制
  （warpdeck-target 卷，<10s）+ 固定 dev-base/dev-rust 镜像（不为每次改动重建）
```

## 7.4 测试矩阵

### L1 - Fake

| 场景 | 期望 |
|---|---|
| 未注册实例 | 调用 register |
| 已注册实例 | 不重复注册 |
| warp-cli timeout | typed timeout error |
| connect fail | state=Failed |
| stop 正常 | child reaped |
| stop timeout | force kill |

### L3 - Fake Process Integration

使用 fake `warp-cli` / fake `warp-svc` 可执行文件测试真实 Process Wrapper。

必须测试：

- stdout。
- stderr。
- non-zero exit。
- timeout。
- child crash。
- cancellation。

### L4 - Real WARP

仅 1 个实例。

验证：

```text
instance 0 starts
warp-svc ready
proxy mode active
127.0.0.1:40000 usable
trace warp=on
stop 后无 orphan process
```

## 7.5 Phase Gate

```text
[x] Fake Runtime 测试完整
[x] Real instance 0 可启动
[x] 40000 可真实走 WARP
[x] warp=on
[x] 正常 stop 无 orphan
[x] crash 不导致 manager 退出
[x] timeout path 已测试
[x] 本阶段未为每次代码修改创建 Docker image
```

---

# 8. Phase 3：多实例隔离与生命周期

## 8.1 阶段目标

把单实例 runtime 扩展到多个完全隔离的 `warp-svc`。

正式验收最多使用 3 个真实实例；日常测试保持 1 个。

## 8.2 工作项

### P3-001 Runtime Registry

建立：

```text
InstanceId -> RuntimeHandle
```

RuntimeHandle 包含：

```text
warp pid
dbus pid/process handle
state
started_at
restart_count
cancellation token
```

### P3-002 并发安全

同一 Instance 的 start/stop/restart 必须串行化。

禁止：

```text
两个 API 同时 start 同一实例
```

建议使用 per-instance lock 或 actor-like control。

### P3-003 多实例目录隔离

验证：

```text
instances/0/state
instances/1/state
instances/2/state
```

互不读取。

### P3-004 多实例 Runtime 隔离

```text
/run/warpdeck/instances/0
/run/warpdeck/instances/1
/run/warpdeck/instances/2
```

每个 D-Bus socket 独立。

### P3-005 Internal Port Allocation

```text
0 -> 40000
1 -> 40001
2 -> 40002
```

启动前做端口冲突探测。

### P3-006 Per-instance Status

状态查询必须严格在对应 Runtime env 下执行。

### P3-007 Per-instance Stop / Restart

停止 #1 不得影响 #0/#2。

### P3-008 删除语义设计

建议区分：

```text
Stop = 停止运行但保留 registration/state
Delete = 停止并删除 manager record；是否删除 registration data 需显式参数/确认
Reset Registration = 单独危险操作
```

### P3-009 并发启动节流

不要瞬间启动几十个 WARP registration。

MVP 建议：

- sequential 或小并发。
- instance startup stagger。
- registration backoff。

## 8.3 测试矩阵

### L1/L2

- Runtime Registry insert/remove。
- 同实例并发 start 只执行一次。
- 端口计算。
- paths 隔离。

### L4

真实 3 实例测试：

```text
#0 connected
#1 connected
#2 connected
```

然后：

1. stop #1。
2. 确认 #0/#2 正常。
3. restart #1。
4. kill #2 warp-svc。
5. 确认 manager 正常。

## 8.4 Phase Gate

```text
[x] 3 个实例可同时运行
[x] 每实例 state/runtime/dbus/port 独立
[x] 一个实例失败不影响其他实例
[x] 并发 start/stop 不产生重复 child
[x] 删除语义明确且有测试
[x] 真实测试后没有 orphan
```

### 完成记录（2026-08-17，P3 Phase Gate 验收）

```text
L1/L2：125 个单元测试通过（+16 新增：registry 9 + manager 15 + readiness 桥 1 等），
  clippy -D warnings / cargo fmt 干净
L4 真实 E2E（p3_gate_check，容器内真实 3 实例，无任何 Fake）：
  3 实例全部 Healthy（每实例独立 dbus-daemon + warp-svc + state/runtime 目录 + 端口）
  数据面实测 3 端口全部 warp=on（出站为 Cloudflare WARP 地址）：
    curl --socks5-hostname 127.0.0.1:40000/40001/40002 https://cloudflare.com/cdn-cgi/trace
  Gate 步骤全部通过：
    stop #1 -> #0/#2 不受影响（Healthy），#1 Stopped（exit 0 优雅退出）
    restart #1 -> Healthy，restart_count 递增（stop 后重启语义，§8.4 步骤 2->3）
    kill -9 #2 的 warp-svc -> manager 存活，registry #2 -> Failed，#0 不受影响
    SIGTERM -> 全部优雅停止 STOP_ALL_OK（#2 走崩溃回收路径 exit=9），容器 Exited(0)
  无 orphan：停止后容器进程表零残留
实现要点/修复：
  manager.rs：InstanceManager 全生命周期编排（WarpRuntime trait + runs 全局锁 =
    同实例串行 P3-002 + 全局串行启动节流 P3-009；端口探测 P3-005；crash watcher
    挂接并归还进程句柄；P3-008 删除语义 delete(id, remove_registration)）
  registry.rs：RuntimeRegistry（RwLock<HashMap>，DESIGN §21.2）+ RuntimeState 九态
  readiness 桥（P3 首个真实运行暴露的缺陷）：warp-svc 注册到 D-Bus 有启动窗口，
    spawn 后直接发 warp-cli 配置命令会 ENOENT 连不上 daemon；按 DESIGN §11.2
    "poll status until ready" 补 ReadinessProbe（bounded backoff），实例 0 首次
    启动即修复
  restart 语义：stop 后 restart 合法（Gate 步骤 2->3），仅对未知实例报 NotRunning
  fake.rs：FakeProcessSpawner 对 dbus-daemon 自动创建 --address socket 文件
    （模拟真实 daemon 就绪；超时测试用 set_auto_socket(false) 关闭）
```

---

# 9. Phase 4：健康检查与数据面探测

## 9.1 阶段目标

把“进程存在”升级为“真正可用”。

健康状态至少分三层：

```text
Process
Control Plane
Data Plane
```

## 9.2 工作项

### P4-001 Health Domain Model

建议：

```rust
enum HealthState {
    Unknown,
    Starting,
    Healthy,
    Degraded,
    Unhealthy,
}
```

### P4-002 Process Health

检查：

- child handle 尚未退出。
- exit event。

### P4-003 Control Plane Health

执行实例上下文下：

```text
warp-cli status
```

解析状态必须容错版本差异，不应依赖整段字符串完全相等。

### P4-004 Data Plane Probe

通过该实例的内部 SOCKS5 `40000+id` 请求 Cloudflare trace。

解析：

```text
ip=
colo=
warp=
```

不要依赖字段顺序。

### P4-005 Latency

记录 probe duration。

### P4-006 Failure Threshold

不要单次超时就立刻踢节点。

建议状态转移：

```text
1 次失败 -> Degraded
连续 N 次 -> Unhealthy
连续成功 -> Healthy
```

具体 N 由配置/设计文档确定。

### P4-007 Health Scheduler

要求：

- 有 cancellation。
- 避免所有实例同一毫秒 probe。
- manager shutdown 时停止。

### P4-008 Health Event

发布内部事件：

```text
instance.health_changed
instance.exit_ip_changed
instance.state_changed
```

供后续 GOST/SSE 使用。

## 9.3 测试要求

Fake probe 测试：

- timeout。
- malformed trace。
- `warp=off`。
- IP 缺失。
- colo 缺失。
- latency。
- transient failure。
- recovery。

Real WARP：

- `warp=on` 才判数据面可用。

## 9.4 Phase Gate

```text
[x] Healthy 不等同于 PID alive
[x] data-plane probe 生效
[x] exit ip 可记录
[x] colo 可记录
[x] latency 可记录
[x] transient fail -> Degraded
[x] repeated fail -> Unhealthy
[x] recovery -> Healthy
```

### 完成记录（2026-08-18，P4 Phase Gate 验收）

```text
L1/L2：163 个单元测试通过（+37：probe 15 / health 12 / events 3 / health_monitor 7
  等），clippy -D warnings / cargo fmt 干净
实现（runtime/）：
  probe/：SOCKS5 CONNECT 最小客户端（RFC 1928，无认证，域名目标）+ TLS
    （tokio-rustls + webpki-roots，零外部 curl 依赖）+
    HTTP/1.1 GET trace（Content-Length/chunked 容错）+ 顺序无关 trace 解析
    （ip/colo/warp）；DataPlaneProber trait（Real / Fake）
  health.rs：纯函数判定（LayersReport 三层：进程/控制面/数据面 → HealthVerdict）
    阈值：连续失败 <3 → Degraded，>=3 → Failed（DESIGN §14.5）；
    恢复需连续 2 次成功回 Healthy；进程死亡为硬状态立即 Failed
  events.rs：EventBus（broadcast，P4-008 instance.state_changed /
    health_changed / exit_ip_changed）
  manager：启动尾部数据面验证（bounded 12×5s 等 warp=on，超时 → Degraded
    而非启动失败，健康循环拉回）；collect_health_layers / apply_health_verdict；
    crash watcher 发布 Failed 事件（修 from 状态在 on_crash 前读取）
  health_monitor.rs：周期调度（interval + tick 串行探测天然错开实例）+ cancel
    watch；测试直接驱动 tick，Fake 变体全覆盖
关键决策：
  - start 返回即 Healthy 只发生在数据面验证通过后（Gate "Healthy ≠ PID alive"）；
    数据面建连窗口（P3 实测可达 10s+）内 → Degraded，不阻塞 start
  - 控制面 status 由既有 ReadinessProbe/WarpControl 复用，不重复实现
L4 真实 E2E（p4_gate_check，容器内真实 3 实例 + RealDataPlaneProber）：
  HEALTHY_3：三实例启动即数据面验证通过，exit_ip/colo/latency 全记录
    （均 colo=LAX，latency 428-599ms）
  3 端口 curl SOCKS5 -> warp=on（与 registry 记录的 exit_ip 一致）
  事件流：state_changed starting->healthy / crash 事件正常发布
   kill -9 #2 -> watcher Failed -> restart #2 -> Healthy（新 exit_ip，恢复路径）
   SIGTERM -> 全部优雅停止 STOP_ALL_OK，容器 Exited(0)
```

P4 review 轮（提交 b21bdb5 后，165 tests）修正与确认：

```text
- 启动失败双事件（StateChanged + HealthChanged -> Failed）与成功路径对称：
  fail_start 补事件、Stopped->Starting、start 成功补 HealthChanged、
  do_stop 补 prev(Healthy/Failed)->Stopping、stop 幂等清扫 Failed->Stopped
- apply_health_verdict 窄竞态守卫：get 与 update 之间 watcher/stop 置
  Failed/Stopping 时放弃本次判定（守卫在 update 闭包内，原子）
- warp=off 降级记录 last_error="warp is off"（此前无原因）
- 已知取舍（P6 优化方向，非阻塞）：
  a) 启动尾部数据面 verify 持全局 runs 锁，最坏 ~175s 阻塞其它实例
     生命周期操作（P3-009 全局串行 + P4 verify 12×10s+5s）；缓解：verify
     移出锁会引入新竞态，P6 再评估 per-instance 锁
  b) 健康 tick 串行探测实例，卡住的实例（10s 超时）拖慢整轮；MVP 可接受
  c) 阈值 Failed 实例进程保留、停止探测，恢复 = restart（manual/auto），
     语义见 DESIGN §10.1
```

---

# 10. Phase 5：GOST Proxy Gateway

## 10.1 阶段目标

提供唯一对外数据面：

```text
SOCKS5 :11080
HTTP   :18080
```

两者都只路由到 Healthy WARP 实例。

## 10.2 非目标

本阶段明确不实现：

- Direct Proxy。
- Shadowsocks。
- 其它 listener。

## 10.3 工作项

### P5-001 GostConfig Domain

配置模型不要直接在业务逻辑拼 YAML 字符串。

建议 typed model：

```text
ProxyListener
HealthyNode
AuthConfig
Allowlist
RateLimit
```

### P5-002 GOST Config Renderer

输入：

```text
healthy instances
proxy settings
```

输出：

```text
/var/lib/warpdeck/generated/gost.yaml
```

流程：

```text
render temp
 -> validate
 -> atomic replace
```

### P5-003 Healthy Pool Builder

只加入：

```text
enabled
runtime running
health healthy
internal port reachable
```

### P5-004 GOST Process Supervisor

功能：

- start。
- stop。
- restart。
- watch exit。
- capture stderr。

### P5-005 Empty Pool 行为

空 Healthy Pool 时必须有确定行为。

建议：

- listener 可以保持；
- 请求明确失败；
- Web/API 显示 No Healthy Upstream；
- 不偷偷走 Direct Internet。

### P5-006 SOCKS5 Listener

容器内固定：

```text
0.0.0.0:11080
```

### P5-007 HTTP Listener

容器内固定：

```text
0.0.0.0:18080
```

### P5-008 Proxy Auth

支持 username/password。

Secret 不得进入普通 GET response/log。

### P5-009 IP Allowlist

后端严格校验 CIDR。

非法 CIDR 在保存前失败。

### P5-010 Limits

实现：

- max concurrent connections。
- max request rate（按 GOST 能力）。

### P5-011 Apply Transaction

建议：

```text
validate requested config
 -> render new GOST config
 -> validate GOST config
 -> restart/reload GOST
 -> probe 11080/18080
 -> mark applied
```

失败时不能假装应用成功。

### P5-012 Data Plane Smoke

SOCKS5：

```bash
curl --socks5-hostname 127.0.0.1:11080 \
  https://cloudflare.com/cdn-cgi/trace
```

HTTP：

```bash
curl -x http://127.0.0.1:18080 \
  https://cloudflare.com/cdn-cgi/trace
```

## 10.4 测试要求

### 不依赖 WARP 的测试

GOST upstream 使用 fake SOCKS server。

覆盖：

- SOCKS5 listener。
- HTTP listener。
- auth。
- bad auth。
- allowlist。
- rate limit。
- upstream down。
- config invalid。

### 真实 WARP

使用 1 或 3 个真实 upstream。

验证两种 listener 都 `warp=on`。

## 10.5 Phase Gate

```text
[x] 11080 可用
[x] 18080 可用
[x] 两者都只走 WARP
[x] Empty Pool 不走 Direct
[x] unhealthy instance 自动从 pool 排除
[ ] auth 生效
[ ] CIDR 校验生效
[x] GOST crash 可感知
[x] config apply failure 可见
```

### 完成记录（2026-08-18，P5 Phase Gate 验收）

```text
L1/L2：34 个 proxy 单元测试 + 全仓 199 tests 通过，clippy -D warnings / cargo fmt 干净
L4 真实 E2E（p5_gate_check，dev-base 容器内真实 3 实例 + 真实 GOST 3.2.6，无任何 Fake）：
  HEALTHY_3 → GostManager.apply() → Running { pid, healthy_upstreams: 3 }
  Gate 1 curl_check：SOCKS5 :11080 与 HTTP :18080 均 warp=on（WARP IPv6 出口 ip=2a09:.../2a09:...）
  Gate 2 warp2_excluded：kill -9 warp-svc #2 → registry Failed → apply() 后
    Running pool=2，渲染配置不含 127.0.0.1:40002（崩溃实例自动排除）
  Gate 3 gost_crash：kill -9 GOST → status 刷新为 Failed { exit_code: 9, stderr 尾部摘要 }
  Gate 4 gost_recovered：幂等 apply() → Running（崩溃后恢复路径）
  Gate 5 empty_pool：全实例停止 → apply() → Degraded { reason: "no healthy upstreams" }，
    listener 11080/18080 保留（TCP 可连）；请求明确失败：SOCKS5 curl error 97，
    HTTP curl error 22 (503)——不走 Direct Internet
  SIGTERM → gost.stop() + 实例优雅停止 → STOP_ALL_OK，容器 Exited(0)
E2E 实测发现并修复（本阶段最重要的收敛，非 Fake 可捕捉）：
  a) GOST v3 的 `forwarder` 字段只对 tcp/udp 端口转发生效；socks5/http handler
     必须用 `handler.chain` + `chains[].hops[].nodes`。此前渲染用 forwarder，
     GOST 静默忽略并直连出网（warp=off，违反"只走 WARP"）——手工 curl 对照
     forwarder(warp=off) vs chain(on) 定位，renderer 改为共享 chain-0 + hop selector。
  b) GOST 空 chain 会 fallback 直连：空节点池渲染不可达占位节点 no-upstream
     (127.0.0.1:1)，listener 保留、请求明确失败（SOCKS5 error 97），满足 P5-005。
  c) apply 后立即单次 probe 会误报 Degraded（GOST bind 有启动窗口，实测秒级）：
     listener 探活改为有界重试轮询（250ms × 40），测试覆盖启动窗口吸收。
review 轮（整仓 review 后修复）：
  d) DESIGN §13.4 第 6 步"验证至少一条数据面路径"此前未实现：apply 仅 TCP 探活
     listener；补 DataPlaneProber 注入 + apply 尾段真实 trace 探测（warp=on 才算
     Running，否则 Degraded 带 warp=off 原因）——可拦截 a) 类直连绕过复发。
     注意：实例级（40000+id）与代理级（11080/18080）探测都用同一 prober，
     内部端口恒为 SOCKS5。
  e) healthy_upstreams 计数语义修正：此前用 healthy_ids()（仅 registry 过滤），
     与渲染到 chain 的 build() 结果（含 TCP 探活）不一致；改为 nodes.len()。
  f) HTTP-only 模式修复（review 发现的真实 bug）：数据面验证此前对 18080 发
     SOCKS5 握手，HTTP 代理不响应 → 永远 Degraded。DataPlaneProber 增加
     ProbeProto（Socks5/Http），HTTP 模式走 CONNECT 隧道建链后同路径 TLS+trace；
     测试 http_only_mode_reaches_running_via_http_probe（还断言渲染不含 11080）。
  g) apply 幂等语义升级：配置渲染结果未变且进程存活 → 跳过 stop/start（P6
     reconciler 周期调用不会周期性掐断活跃连接）；配置变化或进程死亡才重启。
     测试拆为 apply_skips_restart_when_config_unchanged /
     apply_restarts_when_config_file_changed；崩溃恢复（Gate 3→4）不受影响。
  h) GOST 启动即崩溃归类修正：wait_listener 每轮探活后查进程退出，发现退出
     立即 Failed（含退出码），不再空转 40 轮后误报 Degraded；FakeProcessSpawner
     增加 set_exit_on_spawn 注入；测试 startup_immediate_exit_is_failed_not_degraded。
  i) YAML 注入防线补洞：username 与 password 同样禁止换行（渲染进 authers 裸
     标量）；新增测试 username_with_newline_rejected。清理死代码
     （GostSupervisor::restart、KillTimeout、StderrUnreadable 及其测试）。
review 轮后 E2E 二次复测：全部 Gate 重跑通过（数据面验证 + diff-skip 生效：
  Gate 2 配置变化触发重启 pid 256→320；Gate 4 进程死亡触发重启 pid 320→352）。
```

未勾选项（auth 生效 / CIDR 校验生效）为 P5-008/P5-009 的容器内行为验证，
依赖 HTTP API 配置入口（P7/P8），届时补测；渲染与校验已由单元测试覆盖。

---
# 11. Phase 6：SQLite Desired State 与 Reconciler

## 11.1 阶段目标

把系统从“API 调一次命令就结束”升级为持续控制平面。

核心模型：

```text
SQLite Desired State
        |
        v
    Reconciler
        |
        v
Runtime Actual State
```

## 11.2 工作项

### P6-001 完整核心 Migration

实现/确认：

```text
warp_instances
proxy_config
settings
```

Auth/Secret 相关表可以在 Phase 8 补齐。

### P6-002 WarpInstanceRepository

提供：

```text
create
get
list
update desired state
delete
```

Domain 不直接依赖 SQLx。

### P6-003 DesiredState

建议至少：

```text
Running
Stopped
```

`enabled` 与 desired state 的语义必须明确，避免重复状态冲突。

### P6-004 Actual Runtime Snapshot

Actual 不应把 PID 当数据库权威状态。

Runtime Registry 提供：

```text
Stopped
Starting
Running
Stopping
Failed
```

### P6-005 Reconcile Loop

每轮：

```text
load desired
 -> inspect runtime
 -> calculate actions
 -> execute bounded actions
 -> publish result
```

要求幂等。

### P6-006 Reconcile Actions

覆盖：

```text
desired running + actual stopped -> start
desired stopped + actual running -> stop
desired running + actual failed + auto_restart -> restart
enabled false -> ensure stopped
```

阈值 Failed 语义（DESIGN §10.1）：阈值 Failed 的实例**进程仍在运行**、健康循环已停止探测——reconciler 是唯一自动恢复路径，`auto_restart` 即为此设计；crash Failed（进程已死）由既有 crash watcher 事件触发 `restart` 路径（P6-010）。`start` API 对阈值 Failed 实例返回 `AlreadyRunning` 属预期（P3-008），UI 提供 restart 操作。

### P6-007 Restart Backoff

避免 crash loop。

至少记录：

```text
restart_count
last_failure_at
next_retry_at
```

### P6-008 Manager Startup Recovery

启动顺序建议：

```text
config
 -> db/migration
 -> runtime registry
 -> load desired state
 -> reconcile instances
 -> health monitor
 -> GOST render/start
 -> web ready
```

可以让 Web 先 bind，但 readiness 应区分 initializing/ready。

### P6-009 Manager Shutdown

关闭时不要错误修改 Desired State。

例如用户希望 instance=Running，Manager shutdown 只是关闭 Actual；下次启动仍应恢复。

### P6-010 Reconcile Trigger

支持：

- 定时 tick。
- mutation 后主动 notify。
- runtime crash event。
- health change event（必要时）。

避免 busy loop。

### P6 实现状态（完成记录 2026-08-18）

- P6-001：`migrations/0002_warp_instances.sql`（warp_instances / proxy_config；无 PID 列）。
- P6-002/003：`db::repo`（`WarpInstanceRepository` trait + sqlx 实现；`DesiredState` 两态；
  `enabled=false` 优先语义；backoff 字段持久化；domain 不接触 sqlx）。
- P6-004：复用 `runtime::registry`（Actual 快照，PID 不落库）。
- P6-005/006：`reconciler` 模块（决策表：should_run、幂等、单实例失败隔离、
  DB 删除收敛孤儿 runtime、阈值 Failed 唯一自动恢复路径 = auto_restart + restart）。
- P6-007：`record_failure` 指数退避（由 last/next 时间戳间距递推翻倍，5s 起、封顶 5min；
  `Clock::now_utc_rfc3339` 支持 ManualClock 确定性测试）。
- P6-008：GOST 配置经 `ProxyApplier` 接缝周期同步（`GostManager::update_settings` +
  `apply` 幂等 diff-skip 不掐活跃连接）；main 启动顺序完整接线随 P7 bootstrap 进行。
- P6-010：tick + `Notify::trigger()` + 事件总线（StateChanged/HealthChanged → Failed 时立即收敛）。
- FakeWarpRuntime：动作同步共享 registry + start/restart 失败注入（P6/§11.3 测试接缝）。
- 测试：reconciler 19 用例覆盖 §11.3（含补缺：3 实例批量 start、连续 10 轮幂等、
  manager 重启恢复 desired、DB 临时失败注入 FlakyRepo 不阻塞且恢复后继续）
  与 §11.4；全 workspace 225 passed；clippy -D warnings 通过。
- 待办：P7 API mutation 后调用 `trigger()`；P6-008 main 启动顺序接线随 P7
  bootstrap 进行；manager 重启恢复的容器 E2E 验证留门禁。

## 11.3 测试要求

### Component Test

使用 SQLite temp DB + Fake Runtime：

1. DB 3 个 running -> fake start 3 个。
2. DB stopped -> 不 start。
3. Actual running + desired stopped -> stop。
4. runtime failed -> backoff。
5. Reconcile 连续执行 10 次不重复创建进程。
6. manager restart simulation 后 desired 恢复。

### Failure Tests

- DB 临时失败。
- start runtime 失败。
- one instance reconcile fail 不阻塞所有实例。

## 11.4 Phase Gate

```text
[x] Reconcile 幂等
[x] Manager restart 恢复 Desired State（P6-008；E2E 验证容器门禁）
[x] Manager shutdown 不破坏 Desired State
[x] failed instance 有 backoff
[x] 单实例 reconcile failure 不拖垮全局
[x] DB 不持久化短生命周期 PID 作为权威状态
```

---

# 12. Phase 7：REST API

## 12.1 阶段目标

通过稳定 API 暴露控制面。

所有 URL 使用：

```text
/api/v1/...
```

## 12.2 实现顺序

优先：

```text
system
instances
proxy
```

随后：

```text
account
settings
logs
```

Auth middleware 在 Phase 8 完成后接入。

## 12.3 工作项

### P7-001 API DTO Layer

要求 Domain Model 与 HTTP DTO 分离。

禁止直接把内部 Process Handle/Secret/路径完整序列化出去。

### P7-002 Error Contract

统一：

```json
{
  "error": {
    "code": "NOT_FOUND",
    "message": "Instance not found",
    "request_id": "..."
  }
}
```

> 实现说明（P7 落地）：`code` 取稳定语义集合 `VALIDATION` / `NOT_FOUND` /
> `CONFLICT` / `INTERNAL`（§12.4 状态码表一一对应），资源细节放 `message`。
> 状态码映射：Validation→422，NotFound→404，Conflict→409，Internal→500；
> `request_id` 与响应头 `X-Request-Id` 一致（observability 中间件提供）。

### P7-003 System API

至少：

```text
GET /api/v1/system/status
GET /api/v1/system/version
```

### P7-004 Instances List/Get

```text
GET /api/v1/instances
GET /api/v1/instances/:id
```

包含：

```text
id
name
desired_state
runtime_state
health
exit_ip
colo
latency_ms
last_error
```

### P7-005 Create Instance

```text
POST /api/v1/instances
```

只写 Desired State/Repository，由 application service/reconciler 执行。

### P7-006 Start/Stop/Restart

设计成显式 command endpoint 或 desired state mutation，保持一致。

例如：

```text
POST /api/v1/instances/:id/start
POST /api/v1/instances/:id/stop
POST /api/v1/instances/:id/restart
```

Restart 可作为 runtime intent，不一定改变最终 Desired State。

### P7-007 Delete Instance

危险操作。

需要明确：

```text
preserve_registration=true/false
```

如果 MVP 不允许参数，则选择最安全默认并在 UI 说明。

### P7-008 Proxy API

```text
GET /api/v1/proxy
PUT /api/v1/proxy
```

不提供 container listener port 修改。

### P7-009 Account API Skeleton

返回 mask 后状态：

```text
mode
configured
license_present
zero_trust_configured
```

绝不返回 secret 明文。

### P7-010 Settings API

只暴露可安全动态调整的设置。

### P7-011 Logs API

先支持分页历史，实时部分 Phase 10。

### P7-009 SSE Events（实现注记）

`GET /api/v1/events`（SSE）：订阅内部 `EventBus`（P4-008），事件名
`state_changed` / `health_changed` / `exit_ip_changed`，payload 仅公开字段
（instance_id/from/to/reason/exit_ip/colo/latency_ms），绝不携带 secret；
keep-alive 15s；lagged 订阅者丢帧（幂等快照可接受）。

### P7-010 Settings API（暂缓）

MVP 无“可安全动态调整”的设置项（容器端口归 Compose `.env`，代理配置走
`/api/v1/proxy`）；P8 后按需补充。

### P7-011 Logs API（分页历史已部分落地）

实例日志分页历史 / 源枚举已落地；实时流留 Phase 10。

## 12.4 API Test Plan

优先使用 Axum Router in-memory tests，不监听真实 TCP。

每个 mutation 至少测试：

```text
happy path
invalid input
not found
conflict
internal application failure
```

HTTP code 规范：

| 场景 | Code |
|---|---:|
| Validation | `400/422` |
| Unauthorized | `401` |
| Forbidden | `403` |
| Not Found | `404` |
| Conflict | `409` |
| Internal | `500` |

## 12.5 Phase Gate

```text
[x] API DTO 与 Domain 分离        —— api/dto.rs（InstanceView/ProxyConfigView/SystemStatusView…）
[x] error contract 统一           —— api/error.rs：VALIDATION/NOT_FOUND/CONFLICT/INTERNAL + request_id
[x] instance CRUD/control 可用    —— /api/v1/instances GET/POST/DELETE + start/stop/restart
[x] proxy config API 可用         —— GET/PUT /api/v1/proxy（端口不可改；无 secret 出站）
[x] Secret 不出现在 GET response  —— proxy 视图只给 auth_configured 布尔；/account 恒 mask
[x] API tests 无需真实 WARP       —— app::TestApp（fake runtime + 临时 sqlite），Router oneshot
```

---

# 13. Phase 8：认证、会话与 Secret Store

## 13.1 阶段目标

让管理面从“开发 API”升级为可安全部署的管理控制面。

## 13.2 工作项

### P8-001 First Run State

系统没有管理员时：

```text
/setup enabled
normal login disabled or redirects setup
```

创建首个管理员后 setup 必须锁定。

### P8-002 Password Hash

使用 Argon2id。

禁止：

- plaintext。
- SHA256(password)。
- 自己发明 hash。

### P8-003 Sessions Table

记录：

```text
session id/hash
user id
created_at
expires_at
last_seen
```

### P8-004 Secure Cookie

要求：

```text
HttpOnly
SameSite
Secure（HTTPS 场景）
合理 expiration
```

### P8-005 Auth Middleware

除：

```text
health
setup status
setup create admin
login
static assets
```

外，管理 API 默认需要认证。

### P8-006 CSRF

所有 cookie-auth mutation 需要 CSRF 防护。

### P8-007 Secret Master Key

生成/加载：

```text
/var/lib/warpdeck/master.key
```

权限必须严格。

### P8-008 Secret Encryption

加密存储：

- WARP+ License。
- Zero Trust Client ID（按安全策略）。
- Zero Trust Client Secret。
- Proxy Password。

### P8-009 Secret API Semantics

GET 只返回：

```text
configured=true
masked_value（必要时）
```

用户提交空字段时，必须明确代表：

```text
keep existing
clear existing
```

不能模糊。

### P8-010 Secret Redaction

建立集中 redactor。

覆盖：

- logs。
- CLI stderr。
- errors。

### P8-011 Login Rate Limit

基础防爆破。

### P8-012 Security Tests

自动检查日志/response 不包含 test secret marker。

例如测试 secret：

```text
TEST_SECRET_DO_NOT_LEAK_123
```

测试结束 grep 所有 capture。

## 13.3 Phase Gate

```text
[x] 首次管理员 setup 只能成功一次
[x] 密码使用 Argon2id
[x] 未登录 mutation 返回 401
[x] CSRF 生效
[x] session 可注销
[x] Secret 数据库存储为 ciphertext
[x] GET 不返回 Secret 明文
[x] 日志不包含 Secret marker
[x] master key 权限正确
```

---

# 14. Phase 9：React Web UI

## 14.1 阶段目标

让用户不需要命令行即可完成 MVP 日常管理。

## 14.2 页面开发顺序

```text
Setup
Login
App Shell
Dashboard
Instances
Instance Detail
Proxy
Account
Logs
Settings
```

## 14.3 工作项

### P9-001 Frontend App Shell

包含：

- Router。
- Auth state。
- API client。
- Error boundary。
- Global notifications。

### P9-002 Typed API Client

集中处理：

```text
base URL
JSON
401
CSRF
request id
error contract
```

页面不自行 `fetch('/api/...')` 散落调用。

### P9-003 Setup Page

输入：

- admin username。
- password。
- confirm password。

### P9-004 Login Page

处理：

- bad password。
- rate limited。
- server unavailable。

### P9-005 Dashboard

展示：

```text
Manager status
instances total
healthy
failed
SOCKS5 status
HTTP status
proxy pool size
```

### P9-006 Instances Page

列表：

```text
name
state
health
exit ip
colo
latency
actions
```

### P9-007 Add Instance

MVP 表单保持简单：

```text
name
auto start
auto restart
```

内部端口自动计算，用户不输入。

### P9-008 Instance Detail

展示：

- Desired/Runtime/Health。
- Exit IP。
- Colo。
- Latency。
- Restart count。
- Last error。
- lifecycle actions。

### P9-009 Proxy Page

仅显示：

```text
SOCKS5 container port 11080
HTTP container port 18080
```

Host 映射提示“由 Compose/.env 管理”。

可修改：

- auth enabled。
- username。
- password replace/clear。
- allowlist。
- limits。

### P9-010 Account Page

支持模式：

```text
Free WARP
WARP+
Zero Trust
```

Secret 输入必须 mask，GET 不回填真实值。

### P9-011 Logs Page

先做历史分页与过滤。

### P9-012 Settings Page

仅放 MVP 可动态设置。

### P9-013 UX State Completeness

所有页面至少处理：

```text
Loading
Empty
Success
Partial/Degraded
Error
Unauthorized
```

### P9-014 Accessibility Baseline

- 按钮有文字/aria label。
- 状态不只靠颜色。
- 表单 label 正确。
- keyboard 可操作。

## 14.4 测试要求

### Unit/Component

- form validation。
- API error mapping。
- auth redirect。
- state badge。

### Playwright Mock E2E

默认使用 Mock API：

- setup -> login。
- create instance。
- stop/restart。
- update proxy config。
- account secret form。

不依赖 Docker/WARP。

## 14.5 Phase Gate

```text
[x] Setup/Login 可用
[x] Dashboard 可读
[x] Instance 生命周期可操作
[x] Proxy 设置可操作
[x] Account 设置不泄露 secret
[x] Loading/Empty/Error 状态完整
[x] Mock E2E 通过
[x] UI 修改不触发完整 WARP Docker build
```

### 完成记录（2026-08-18，P9 Phase Gate 验收）

- 18 个 Playwright Mock E2E 全部通过（`pnpm e2e`，不依赖 Docker/WARP）；
  33 个 vitest 单元/组件测试通过（form validation / API error mapping / auth redirect / Feedback）。
- 修复的集成问题（mock E2E 发现）：
  - mock server 的 `/__mock/reset` 未命中 API 路由（不以 `/api/` 开头），改为显式进入 `handleApi`；
  - `POST /api/v1/setup` 成功后未立即刷新 `setup-status`（staleTime 30s 内守卫仍看旧值），`useSetupMutation` 补 `invalidateQueries`；
  - `/auth/me` 会话恢复查询设置 `refetchInterval: false`，避免未登录时每 5s 触发 401；
  - mock server 补 `GET /api/v1/instances/:id`（详情页轮询所需）；
  - Account 清除凭据对齐后端契约：warp_plus 必须有 license → 清除即回 `free` 模式；
  - E2E 定位器歧义（Start/Restart 子串匹配、label 含状态文案）改 exact/id 选择器。
- Playwright 配置：mock server 串行执行（共享内存状态）、`reuseExistingServer: false`（状态基态确定性）。

---

# 15. Phase 10：SSE、实时状态与日志

## 15.1 阶段目标

用户无需刷新页面即可看到实例和代理变化。

MVP 推荐 SSE，而非复杂 WebSocket 双向协议。

## 15.2 工作项

### P10-001 Event Bus

内部 event 类型：

```text
instance.created
instance.state_changed
instance.health_changed
instance.exit_ip_changed
instance.deleted
proxy.state_changed
system.warning
```

### P10-002 SSE Endpoint

例如：

```text
GET /api/v1/events
```

要求：

- authenticated。
- keepalive。
- disconnect cleanup。
- bounded subscriber buffer。

### P10-003 Event Versioning

Event payload 带：

```text
type
version
timestamp
resource id
data
```

### P10-004 Frontend SSE Client

要求：

- 自动重连。
- 显示 realtime connection state。
- event 后更新 React Query cache。

### P10-005 Log Pipeline

区分：

```text
manager logs
instance logs
gost logs
```

### P10-006 Log History

分页查询，避免一次读完整文件。

### P10-007 Live Logs

可以独立 SSE endpoint 或统一事件流。

必须做：

- buffer 限制。
- secret redaction。
- client disconnect cleanup。

### P10-008 Backpressure

慢客户端不能拖垮 manager。

## 15.3 测试要求

- subscriber connect/disconnect。
- event ordering（在同一 resource 范围）。
- lagged client。
- reconnect。
- secret redaction。
- high log rate buffer behavior。

## 15.4 Phase Gate

```text
[x] Instance state 页面实时更新（useSseEvents → invalidate instance list/detail）
[x] Health 页面实时更新（health_changed 帧 → 同 invalidate）
[x] SSE 重连正常（指数退避 1s→15s，前端 sse.test 覆盖）
[x] 慢客户端不阻塞 runtime（broadcast lagged 语义 + LogBus 独立 1024 容量，P10-008）
[x] Logs 不泄露 Secret（Sensitive 字段级 + CLI 行整行 redact；history/live 双路径）
[x] disconnect 后资源被释放（SSE receiver drop 自动清理；tail watcher 随进程退出）
```

### 完成记录（2026-08-18，P10 Phase Gate 验收）

```text
后端：
- P10-001 EventBus（原有）保留；新增 LogBus（broadcast 1024）
- P10-002 SSE 端点升级：帧契约包裹 {type,version,timestamp,resource_id,data}；
  合并 health + log 两路流；keepalive 15s 保留
- P10-003 事件版本化：envelope 统一包裹，版本 = 1，契约测试锁定字段集合
- P10-005 日志管道：data_dir/logs/manager.log（tracing 文件层，10MB 启动截断）；
  SpawnCommand stdout 重定向（与 stderr 合并 instance-N.log / gost.stderr.log）
- P10-006 Log History：GET /api/v1/logs + /logs/sources（tail 分页，大文件块读）
- P10-007 Live Logs：log.line SSE 帧 + tail watcher（跟随/截断/重建恢复，redact）
  * 监督任务 + 5s 发现周期：运行中新实例日志文件自动补缺（watcher_discovers 测试）
  * 截断检测三件套：len<pos（缩小）、字节签名 probe（truncate+write 同窗口重写）、
    周期性路径级 rescue（Windows delete-pending/file tunneling：身份+长度双校验）
  * 截断/重建＝新纪元：seq 重置为 1；重新 open 从 0 推全部（重建文件非历史）
  * Windows 限制说明：delete-pending 旧句柄读旧对象已由 rescue 覆盖
- P10-008 Backpressure：LogBus broadcast 1024 + lagged 丢行；日志行可丢、历史在文件
前端（P10-004）：
- useSseEvents（EventSource + 指数退避 + 连接状态 + React Query cache 更新）
- RealtimeStatusDot（AppLayout 顶栏连接指示）
- LogsPage 可选 tab：运行时（源选择 + 历史分页 + 实时流 + 自动滚底）/ 审计（原有）
测试：后端 335（+13 logs/events/tail + 3 review 回归），前端 45（+5 sse）全过；fmt/clippy 干净
review 修复（2026-08-18）：
- ① tail watcher rewrite 探测改读 pos-1（原读 pos＝新数据首字节，纯 append 跨轮询必误判
  为截断重写 → 回卷重读 → SSE 实时流重复刷屏）+ 回归测试（watcher_cross_poll_appends_do_not_replay）
- ② SpaFallback 显式拒绝 `..`/`.`/反斜杠段（未认证静态路径防目录穿越）+ 穿越回归测试
- ③ 合并 stdout/stderr 日志：双句柄都改 O_APPEND（stdout 原用 File::create 独立 offset，
  第二轮写覆盖 stderr 字节）；Windows std 校验 create+append+truncate 为 InvalidInput，
  故先 File::create 截断、再双 append 打开 + clobber 回归测试
- ④ Logs read_tail UnexpectedEof 单次重试（并发截断不再 500，仍失败降级空页）
```

---

# 16. Phase 11：Docker 集成与 E2E

## 16.1 阶段目标

第一次把完整系统作为最终用户将运行的单镜像进行验证。

**这是最终镜像构建阶段，不代表此前每个 Phase 都要 build Release image。**

## 16.2 工作项

### P11-001 Multi-stage Dockerfile

建议：

```text
frontend builder
rust builder
runtime image
```

Runtime 包含：

- warpdeck binary。
- static web assets。
- Cloudflare WARP runtime。
- GOST 固定版本。
- D-Bus。
- CA。
- tini/init。

### P11-002 依赖 Pin

禁止 Docker build 时每次请求：

```text
latest GOST release
```

应固定：

```text
version
checksum
```

### P11-003 Least Privilege Review

逐项确认哪些操作真的需要提升权限。

不要无条件 `sudo ALL=(ALL) NOPASSWD:ALL`。

### P11-004 Release Compose

```yaml
ports:
  - "${WEB_HOST_PORT:-9000}:9000"
  - "${SOCKS5_HOST_PORT:-11080}:11080"
  - "${HTTP_HOST_PORT:-18080}:18080"
```

只持久化必要数据目录。

### P11-005 Healthcheck

容器 healthcheck 应反映 manager 基本 readiness，而不是每次做昂贵外网 probe。

### P11-006 E2E Harness

E2E 每轮构建一次：

```text
warpdeck:e2e
```

整个测试矩阵复用同一个 image。

### P11-007 First Run E2E

验证：

```text
fresh volume
compose up
setup admin
login
create instance
wait healthy
```

### P11-008 SOCKS5 E2E

验证 `11080` -> `warp=on`。

### P11-009 HTTP E2E

验证 `18080` -> `warp=on`。

### P11-010 Restart Persistence E2E

```text
create 3 instances
configure proxy
restart container
verify account/config/desired state
verify instances recover
```

### P11-011 Failure E2E

测试：

```text
kill one warp-svc
 -> pool shrinks
 -> proxy still works
 -> instance recover according policy
```

### P11-012 GOST Failure E2E

GOST crash：

- manager 可感知。
- state visible。
- supervised restart（按设计）。

### P11-013 No Direct Leak Test

没有 healthy WARP 时：

- SOCKS5/HTTP 请求必须失败。
- 绝不能绕过 WARP 从宿主机直接出网。

## 16.3 Docker Build Budget

本阶段默认：

```text
代码固定到一个候选 commit
 -> build warpdeck:e2e 一次
 -> 重复使用完成整个 E2E matrix
```

只有以下情况允许重 build：

- Dockerfile/依赖修复。
- 二进制确实修改后需重新验证最终 image。

不要每个测试 case build 一次。

## 16.4 Phase Gate

```text
[x] fresh compose 安装成功
[x] setup/login 成功
[x] SOCKS5 warp=on
[x] HTTP warp=on
[x] 3-instance restart recovery 成功
[x] instance failure 不断代理
[x] no healthy upstream 时没有 direct leak
[x] volume 持久化正确
[x] 单轮 E2E 复用同一个 image
```

P11 完成记录（2026-08-19）：

```text
P11-001 多阶段 Dockerfile（node:22-slim 前端 -> rust:1-bookworm 后端 -> ubuntu:24.04 runtime）：
  runtime 含 warpdeck 二进制 + web static + cloudflare-warp_2026.6.880.0 deb + gost v3.2.6
  + dbus-daemon + tini(ENTRYPOINT) + ca-certificates + healthcheck
P11-002 依赖 pin（2026-08-21 修订）：GOST tarball / WARP deb 构建期内经
  docker/fetch-deps.sh 下载（cache mount 持久 + 断点续传），URL/SHA256 由
  `cargo xtask` 从 versions.json 经 --build-arg 注入，镜像内 sha256 硬校验；
  checksum 校验归入 P12 依赖审计
P11-003 最小权限审计结论：仅 warp-svc 建 /dev/net/tun 需要 root（compose 提供
  --device /dev/net/tun + cap_add NET_ADMIN，非 privileged）；镜像内不装 sudo/ssh/编译工具；
  apt lists 在 WARP/GOST 安装完成后才清理（install-warp.sh 依赖 apt 索引，见 §23.3.1）
P11-004 release compose：image ${WARPDECK_IMAGE:-warpdeck:local} 可注入；端口变量
  ${WEB_HOST_PORT:-9000}/${SOCKS5_HOST_PORT:-11080}/${HTTP_HOST_PORT:-18080}；
  volumes 只持久化 warpdeck-data/warpdeck-run
P11-005 healthcheck：curl /api/v1/health（轻量 readiness，不做外网数据面 probe）
P11-006 E2E harness：scripts/e2e/run-e2e.ps1（单轮复用 warpdeck:e2e 镜像，7 用例）
  - 本机踩坑记录：Docker Compose v5 无 --remove-orphans；up -d 偶发不退出（Start-Process
    + WaitForExit 超时兜底）；PS7.4 IWR 不填充 $resp.Session 且 -SessionVariable 与
    -WebSession 互斥、Cookie 头发送不可靠（401）-> Api 全改 curl.exe -b/-c cookie jar；
    原生命令 splat 后跟参数会被拼接 -> 参数全并入数组再整体 splat
  - 宿主机坑：Logitech lghub_updater 占用 127.0.0.1:9100（对 API 返回 426）-> E2E web 端口改 9900
P11-007~013 E2E 全过（7/7，fresh volume 首跑 + 真实数据面）：
  first-run/setup/login/instance healthy（exit_ip 2a09:... colo=LAX）-> SOCKS5/HTTP warp=on
  -> 3 实例 + proxy auth + 容器 restart 全恢复 -> kill 单个 warp-svc 池收缩不断代理且
  auto-restart 全回 healthy -> kill gost reconciler 自动重建 -> 全停后代理请求必须失败（无 direct leak）
已知环境约束：免费 WARP 3 实例并行注册成功（本机未触发限制，仍不可作为保证）
测试：后端 335、前端 45 全过；E2E matrix 7 用例全过（单镜像 warpdeck:e2e，不重 build）
```

---

# 17. Phase 12：安全加固与 Release

## 17.1 阶段目标

把功能完整的 MVP 变成可发布的 v0.1 候选版本。

## 17.2 工作项

### P12-001 Dependency Audit

Rust / Node 依赖安全扫描。

### P12-002 Container Vulnerability Scan

扫描最终 image。

对发现项分类：

```text
reachable
not reachable
base OS
runtime package
false positive / accepted risk
```

不能只看数量。

### P12-003 SBOM

生成并随 Release 保存。

### P12-004 Secret Leak Audit

扫描：

- logs。
- API responses。
- DB plaintext fields。
- crash reports。
- test artifacts。

### P12-005 Web Security

验证：

- auth bypass。
- CSRF。
- session invalidation。
- cookie flags。
- setup endpoint after initialized。

### P12-006 Command Injection Audit

搜索：

```text
sh -c
bash -c
Command::new
```

逐项检查用户输入是否安全传递。

### P12-007 Path Traversal Audit

尤其检查：

- logs path。
- instance path。
- generated config。

### P12-008 Proxy Exposure Review

确认：

- auth off 时 UI 明确警告。
- public bind 的安全提示。
- allowlist 行为正确。

### P12-009 Backup / Restore

至少验证：

- DB。
- master key。
- WARP registration state。

三者关系文档清晰。

### P12-010 Upgrade Test

从最后一个支持的 pre-release schema 升级到 release schema。

### P12-011 Release Docs

包括：

```text
README
Quick Start
Configuration
Ports
Security Notes
Backup
Upgrade
Troubleshooting
License/Attribution
```

### P12-012 Version Metadata

UI/API/image label 都能显示版本与 commit。

### P12-013 Release Candidate

生成：

```text
v0.1.0-rc.1
```

执行完整 L0-L6。

RC 结果登记（v0.1.0-rc.1，镜像 warpdeck:e2e 0.1.0-ee9318b）：

```text
L0 ✅ cargo fmt --check / clippy -D warnings / pnpm lint / pnpm typecheck
L1 ✅ cargo test --workspace 336/336（含 P12-010 升级迁移测试）；web 38/38
L2 ✅ 组件测试（InstanceManager/SQLite/MockRuntime/EventBus/Reconciler/
      GostConfigRenderer）含于 workspace 测试
L3 ✅ Fake runtime 集成含于 workspace 测试；进程/gost 生命周期用例覆盖
L4 ✅ 开发期真实 WARP（dev-base + 数据面 warp=on 烟测），本 RC 无 runtime
      /WARP 安装变更，复用既有证据
L5 ✅ Docker E2E 全矩阵 7/7（scripts/e2e/run-e2e.ps1，最终镜像）：
      fresh volume setup/first-run、实例 healthy 探活（exit_ip + colo）、
      SOCKS5 warp=on、HTTP warp=on、3 实例容器重启持久化（含 proxy auth）、
      实例 kill → pool 收缩 → auto-restart 全恢复、gost kill → reconciler
      重建 → trace 恢复、空 healthy pool 无直连泄露
security scan ✅ trivy 0.61.1：CRITICAL/HIGH 0（scans/trivy-warpdeck-e2e.json）
image metadata ✅ OCI label + WARDPECK_VERSION=0.1.0-<sha>（/api/v1/health 实证）
startup smoke ✅ E2E fresh install + healthcheck healthy
upgrade/migration ✅ P12-010 原地升级测试 + P12-009 备份恢复实证
```

### P12-014 Final v0.1.0

仅在 RC Gate 全部通过后发布。

P12 进行中状态（随验证持续更新，风格同 P11 结束注）：

```text
P12-001 Dependency Audit ✅ —— cargo audit：0 漏洞（263 依赖，advisory 1217 条；
  RUSTSEC-2023-0071 rsa 忽略有理由，见 .cargo/audit.toml）；pnpm audit（官方 registry）：
  0 漏洞。注：本机访问 index.crates.io 被网络拦截致 cargo-audit yanked 检查 403，
  advisory 扫描本身正常。
P12-002 Container Vulnerability Scan ✅ —— trivy 0.61.1 扫 warpdeck:e2e（ubuntu 24.04，
  363 os pkgs）：CRITICAL/HIGH 0 条（--ignore-unfixed），结果见 scans/trivy-warpdeck-e2e.json。
P12-003 SBOM ✅ —— 新镜像 cyclonedx SBOM：scans/sbom-warpdeck-e2e.json（364 组件）。
P12-004 Secret Leak Audit ✅ —— 单元 + 实机：
  redactor 覆盖 logs/API（GET /proxy 无密码、/secrets 无 license）；
  gost.yaml 含凭据强制 0600（tmp 0600 + rename + set_permissions 兜底，升级遗留
  644 tmp 场景已覆盖）；master.key 0600。
  新 e2e 镜像实机复验：gost.yaml 600、master.key 600。
P12-005 Web Security ✅ —— 实机 9900/11080/18080：setup 二次 409、无 cookie 401、
  无 CSRF 403、cookie HttpOnly+SameSite=Lax、logout 后旧 cookie 401。
P12-006 Command Injection Audit ✅ —— 全仓无 sh -c/bash -c；warp-cli/GOST/kill
  均为 Command::new + .arg，无 shell 拼接；无任意命令执行 API。
P12-007 Path Traversal Audit ✅ —— 所有实例路径仅含数值 InstanceId / 随机 UUID /
  常量；用户 name 只进 DB 不进路径。
P12-008 Proxy Exposure ✅ —— 新增行为（本轮）：
  compose 默认绑定改为 127.0.0.1（WEB/SOCKS5/HTTP_HOST_BIND，.env.example 注释说明），
  §20.5 4 处 compose 片段同步；UI 在 auth off 且任一 listener 开时显示警告横幅；
  allowlist/rate-limit 行为经测试确认。设计文档审计清单 Secrets 5/5、Proxy 4/4、
  Container 6/6 勾选。
E2E 复跑（本轮）：重建 warpdeck:e2e（sha 908b4cff）后全矩阵 7/7 通过；E2E-04 暴露
  启动竞态（容器健康早于 GOST 监听），脚本新增 Wait-ProxyListeners 后稳定通过；
  docker port 确认三端口仅发布至 127.0.0.1。
E2E（本轮终验）：最终镜像（含版本元数据）全矩阵 7/7 通过。启动期再加固：
  容器启动时 reconciler 会用期望配置重启一次 GOST → 新版监听就绪后仍有杀进程
  窗口（accept 后 EOF）→ Assert-WarpOn 增加有界重试（总 ~60s，语义等同服务端
  apply→probe）；E2E-02 前置 Wait-ProxyListeners（实例 Healthy 的数据面探活
  走内部 upstream，不等于 GOST 前端监听已开）。
P12-013 RC ✅ —— 提交 1a0751e + tag v0.1.0-rc.1，RC Gate 全部通过（详见下方 17.3）。
P12-014 Final ✅ —— 许可证终审：本仓库代码/文档采用 MIT（LICENSE 文件），镜像内嵌组件各自许可
  （Cloudflare WARP：Cloudflare 条款、默认个人/非商业；GOST v3.2.6：MIT；依赖包按其
  自身许可，SBOM 见 scans/）已写入 README「License」节与 docs/README.md、DESIGN §33。
  Gate 清单 17.3 全部通过 → 打 tag v0.1.0 正式发布。

P12-009 Backup/Restore ✅ —— scripts/backup-restore.ps1（backup/restore/list）：
  备份 = compose stop → warpdeck-data 卷整体打包（warpdeck.db + master.key +
  instances/ 注册态 + generated/ + logs/）；restore 校验归档含 db+key 后清卷解包。
  实机破坏性验证（E2E 项目）：down -v 清空 → setup/status=false → restore →
  initialized=true、登录成功（Argon2id）、proxy secret 解密成功（auth_configured=true）、
  实例 1 用恢复的 WARP 注册态直接 Healthy 且 exit_ip 正常（免重新注册）。
P12-010 Upgrade ✅ —— db::tests::pre_release_schema_upgrades_in_place_preserving_data：
  旧 schema（0001+0002，运行时 Migrator 装载、checksum 与内嵌一致）写入真实业务数据 →
  当前内嵌 migration 集升级（仅追加 0003）→ 断言数据保全 + 新表可写。
P12-011 Release Docs ✅ —— README 补齐 Quick Start/Ports/Configuration/Security
  Notes/Backup/Upgrade/Troubleshooting/License&Attribution；docs/README.md 索引更新。
P12-012 Version Metadata ✅ —— src/version.rs 统一版本解析（WARPDECK_VERSION 优先，
  health + system/version + 启动日志同一来源）；Dockerfile ARG WARDPECK_VERSION +
  OCI LABEL（version/revision）+ ENV；cargo xtask release 注入 `0.1.0-<git sha>`
  （无 git 回退 dev）。
```

## 17.3 Phase Gate

```text
[x] L0-L6 全部通过（P12-013：静态检查、336 后端 + 38 前端测试、fake-runtime 集成、
    E2E 全矩阵 7/7 于最终镜像）
[x] Security scan 完成（P12-001/002：cargo audit 0、pnpm audit 0、trivy CRITICAL/HIGH 0）
[x] SBOM 生成（P12-003：cyclonedx，364 组件）
[x] Secret leak test 通过（P12-004：redactor/0600/gost.yaml 兜底，实机复验）
[x] Backup/Restore 验证（P12-009：破坏性 restore 实证）
[x] Upgrade 验证（P12-010：旧 schema 原地升级数据保全）
[x] 文档完整（P12-011：README/docs 全量）
[x] License/Attribution 明确（P12-014：MIT + 组件许可列明 + 再分发边界）
[x] Release image 可复现（P12-012：版本元数据 0.1.0-<sha>；Dockerfile 与
    cargo xtask release 固定产物，E2E 于最终镜像复跑通过）
```

---
# 18. 横向工程工作流

Phase 是纵向交付顺序；本章定义贯穿整个项目的横向工作。

## 18.1 Architecture Decision Records

任何会长期影响架构的决策都建议创建 ADR。

首批 ADR：

```text
0001 upstream relationship and licensing
0002 Rust + Axum control plane
0003 SQLite desired state
0004 WARP per-instance isolation
0005 GOST as proxy gateway
0006 SSE over WebSocket for MVP
0007 fixed container ports
0008 Docker dev-base strategy
0009 secret encryption design
```

ADR 状态：

```text
Proposed
Accepted
Superseded
Rejected
```

## 18.2 Error Code Registry

建立集中 error code：

```text
CONFIG_INVALID
INSTANCE_NOT_FOUND
INSTANCE_STATE_CONFLICT
INSTANCE_PORT_EXHAUSTED
WARP_PROCESS_START_FAILED
WARP_CLI_TIMEOUT
WARP_REGISTRATION_FAILED
WARP_CONNECT_FAILED
WARP_HEALTH_FAILED
GOST_CONFIG_INVALID
GOST_START_FAILED
NO_HEALTHY_UPSTREAM
AUTH_REQUIRED
CSRF_INVALID
SECRET_DECRYPT_FAILED
DB_ERROR
```

规则：

- error code 不轻易改名。
- 用户消息可变化。
- 内部错误 context 不直接暴露 API。

## 18.3 Domain Event Registry

集中维护：

```text
instance.created
instance.deleted
instance.state_changed
instance.health_changed
instance.exit_ip_changed
instance.restart_scheduled
proxy.config_changed
proxy.state_changed
account.config_changed
system.warning
```

## 18.4 Config Registry

每个配置项都记录：

```text
name
type
default
dynamic/static
secret/non-secret
validation
requires restart?
```

## 18.5 Observability Baseline

从早期就保留：

```text
request_id
instance_id
component
event
error_code
```

不要等到 Phase 12 才补日志结构。

## 18.6 Documentation Sync

以下变更 PR 必须同步文档：

| 变更 | 文档 |
|---|---|
| API | API 设计/OpenAPI |
| DB | schema/migration |
| Port | Port Plan + Compose |
| Runtime | WARP Runtime 章节 |
| Secret | Security 章节 |
| Test Strategy | 开发测试规范 + 本计划 |
| Release behavior | README/Upgrade |

---

# 19. 测试实施计划

## 19.1 测试分层

| Level | 名称 | Docker | Real WARP | 默认触发 |
|---:|---|---:|---:|---|
| L0 | Static | 否 | 否 | 每次 PR |
| L1 | Unit | 否 | 否 | 每次 PR |
| L2 | Component | 否 | 否 | 每次 PR |
| L3 | Fake Runtime Integration | 否/轻量 | 否 | Runtime 相关 PR |
| L4 | Real WARP Integration | 固定 Dev Base | 是 | WARP/GOST 相关 PR |
| L5 | Final Docker E2E | 是 | 是 | 容器/网络/合并候选 |
| L6 | Release Verification | 是 | 是 | Release |

## 19.2 L0 必测

Backend：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo check --workspace
```

Frontend：

```bash
pnpm lint
pnpm typecheck
```

额外：

- migration naming check。
- generated file cleanliness（如适用）。

## 19.3 L1 必测领域

### Domain

- InstanceId。
- Port allocation。
- DesiredState transitions。
- Health transitions。
- Backoff。
- CIDR validation。
- Config validation。

### API helpers

- error mapping。
- auth decisions。
- CSRF validation。

### Crypto

- encrypt/decrypt roundtrip。
- wrong key fails。
- corrupted ciphertext fails。

### GOST renderer

- node ordering。
- auth。
- allowlist。
- empty pool。

## 19.4 L2 Component Test

组件组合：

```text
Application Service
 + Temp SQLite
 + Fake Runtime
 + Fake Event Bus
```

核心用例：

- create instance -> desired state saved。
- reconciler -> fake start。
- stop -> fake stop。
- crash -> state failure。
- restart backoff。
- proxy update -> generated config model。

## 19.5 L3 Fake Runtime Integration

准备：

```text
tests/fixtures/bin/warp-cli
tests/fixtures/bin/warp-svc
tests/fixtures/bin/gost
```

Fake binary 可以通过环境变量控制：

```text
FAKE_WARP_STATUS=connected
FAKE_WARP_DELAY_MS=5000
FAKE_WARP_EXIT_CODE=1
FAKE_GOST_CRASH_AFTER_MS=100
```

测试真正的：

- process spawn arguments。
- environment isolation。
- timeout。
- stderr handling。
- cancellation。
- child reaping。

## 19.6 L4 Real WARP Integration

### 环境

使用固定：

```text
warpdeck-dev-base:1
```

不要在每次测试前 build 新 base。

### 普通 Runtime PR

```text
instances=1
```

### Multi-instance / Routing PR

```text
instances=3
```

### 必测链路

```text
warp-svc
 -> warp-cli
 -> internal SOCKS 40000+
 -> cloudflare trace
 -> warp=on
```

### 清理要求

测试结束：

```text
stop manager
reap children
remove temp runtime dir
保留/删除 test volume 按用例决定
```

不得执行：

```bash
docker system prune -a --volumes
```

## 19.7 L5 Docker E2E Matrix

| Case | Instances | SOCKS5 | HTTP | Restart | Failure |
|---|---:|---:|---:|---:|---:|
| Fresh install | 1 | ✓ | ✓ | - | - |
| Multi instance | 3 | ✓ | ✓ | - | - |
| Instance stop | 3->2 | ✓ | ✓ | - | ✓ |
| Instance crash | 3->2 | ✓ | ✓ | auto | ✓ |
| Container restart | 3 | ✓ | ✓ | ✓ | - |
| Proxy auth | 1 | ✓ | ✓ | - | bad auth |
| No healthy node | 0 healthy | fail | fail | - | no direct leak |
| Persistence | 3 | ✓ | ✓ | ✓ | - |

## 19.8 L6 Release Verification

Release 必须额外：

- fresh VM/clean environment（CI runner 可代替）。
- image pull test。
- Compose docs command copy-paste test。
- version endpoint。
- SBOM。
- vulnerability scan。
- upgrade migration。
- backup/restore。
- release notes。

## 19.9 Coverage 目标

不建议把总覆盖率数字当唯一目标。

强制高覆盖模块：

```text
state machine
reconciler
port/path allocator
GOST renderer
config validation
secret crypto
API auth/error
```

对外部 CLI wrapper 更重要的是 scenario coverage，而不是行覆盖率。

## 19.10 Flaky Test 规则

Flaky test 不允许长期“重跑到通过”。

出现 flaky：

1. 标记 Issue。
2. 定位 race/time dependency。
3. 用 event/condition 替代固定 sleep。
4. 如果临时 quarantine，必须有 owner 和恢复条件。

禁止：

```text
CI 自动 retry 5 次然后当成功
```

作为默认解决方案。

---

# 20. Docker 构建与环境控制计划

## 20.1 镜像分类

只允许三类：

### A. Dev Base

```text
warpdeck-dev-base:1
```

包含外部系统依赖，不包含频繁变化的应用二进制。

### B. E2E Candidate

```text
warpdeck:e2e
```

每轮测试复用。

### C. Release

```text
warpdeck:0.1.0
warpdeck:latest
```

`latest` 仅 Release Pipeline 更新。

## 20.2 禁止 Tag

禁止：

```text
test1
test2
test-final
test-final2
new
latest-new
aaa
```

## 20.3 什么时候绝对不 build

以下改动默认不 build：

- Rust 业务逻辑。
- REST Handler。
- SQLite Repository。
- CSS。
- React component。
- 文案。
- Unit test。
- Mock/Fake。
- 文档。

## 20.4 允许 build 的条件

- Dev Base 首次创建。
- Cloudflare WARP 安装逻辑变化。
- GOST runtime 版本变化。
- OS package 变化。
- Dockerfile 变化。
- 最终容器网络行为变化需要 E2E。
- Release candidate。

## 20.5 单轮 Build 限额

普通开发任务：

```text
0 次 final image build
```

Real WARP runtime 调试：

```text
复用 dev-base
```

Docker E2E：

```text
默认 1 次 candidate build / commit
```

如果失败来自代码而非 Dockerfile：

- 先在本地/Dev Base 修复并验证；
- 再构建新的 candidate。

## 20.6 Build Cache

使用 BuildKit cache。

不要日常 prune。

只在：

- 磁盘不足。
- cache corruption。

时手工清理。

## 20.7 Volume 规则

测试 volume 命名：

```text
warpdeck-test-<scope>
```

不得连接用户生产 volume。

E2E 的 fresh install 用例必须创建全新 volume。

---

# 21. CI/CD 与 PR 门禁

## 21.1 PR 类型分类

PR Template 增加：

```text
Change type:
[ ] docs only
[ ] frontend only
[ ] backend/domain
[ ] database
[ ] warp runtime
[ ] gost/proxy
[ ] docker/network
[ ] security/auth
```

由类型决定测试层级。

## 21.2 普通 PR Gate

必须：

```text
L0
L1
L2
```

不构建 final Docker image。

## 21.3 Runtime PR Gate

必须：

```text
L0
L1
L2
L3
L4
```

真实 WARP 1 个实例通常足够。

## 21.4 Multi-instance / GOST PR

必须：

```text
L0-L4
```

L4 使用最多 3 个真实实例。

## 21.5 Docker/Network PR

必须：

```text
L0-L5
```

## 21.6 Security/Auth PR

必须：

- normal tests。
- auth negative tests。
- CSRF tests（如相关）。
- secret leak tests。

## 21.7 Release Gate

必须：

```text
L0-L6
```

且 Security Pipeline 成功。

## 21.8 PR 描述模板

```markdown
## Summary

## Why

## Scope

## Risk

## Test Plan

## Manual Verification

## Real WARP
- Tested: yes/no
- Instances: 0/1/3
- SOCKS5: yes/no
- HTTP: yes/no

## Docker Build
- Final image built: yes/no
- Why required:

## Migration

## Security / Secrets

## Docs Updated
```

## 21.9 Merge 前 Checklist

```text
[ ] Review comments resolved
[ ] Required CI green
[ ] No debug print
[ ] No plaintext secret
[ ] No unrelated generated files
[ ] Docs updated
[ ] Migration reviewed
[ ] Test plan reproducible
[ ] Docker build justified if present
```

---

# 22. 风险登记与应对

## R-001 Cloudflare WARP CLI 行为变化

**风险：高**

影响：

- status parser。
- registration command。
- mode/port command。

应对：

- CLI adapter 集中封装。
- parser 做兼容。
- Real WARP smoke workflow。
- runtime version 可查看。

触发条件：

- 新 WARP package 后 L4 失败。

## R-002 多实例状态互相污染

**风险：高**

应对：

- STATE_DIRECTORY 独立。
- RUNTIME_DIRECTORY 独立。
- D-Bus 独立。
- multi-instance isolation tests。

## R-003 Orphan Processes

**风险：高**

应对：

- Process Supervisor。
- bounded shutdown。
- child reaping tests。
- container init。

## R-004 GOST 配置错误导致代理整体中断

**风险：高**

应对：

- render temp。
- validate。
- atomic replace。
- startup probe。
- preserve last known good config（如实现）。

## R-005 无 Healthy Node 时 Direct Leak

**风险：最高**

这是数据面安全性质问题。

要求：

- 明确测试 no-direct-leak。
- GOST 不配置 direct fallback。
- Release Gate 阻断。

## R-006 Secret 泄露

**风险：最高**

来源：

- CLI stderr。
- tracing context。
- API response。
- frontend state。

应对：

- central redactor。
- secret marker test。
- typed secret wrapper（可选）。

## R-007 Docker 镜像膨胀/堆积

**风险：中**

应对：

- dev-base 固定 tag。
- candidate 单轮复用。
- latest 只 Release。
- 文档规定 build budget。

## R-008 SQLite 损坏/锁

**风险：中**

应对：

- WAL/timeout 策略。
- transaction 控制。
- backup docs。
- corruption handling docs。

## R-009 Master Key 丢失

**风险：高**

影响：Secret 无法解密。

应对：

- backup 文档强调 DB + master key 一起备份。
- UI health warning（未来）。

## R-010 许可证边界不清

**风险：高**

应对：

- 许可证决策书面记录（P0-008 / DESIGN §33）。
- 记录直接复制的代码来源。
- Release 前许可证 review。

## R-011 真实 WARP 测试不稳定

**风险：中**

应对：

- 大部分逻辑用 Fake。
- L4 只验证不可模拟部分。
- 不把临时网络抖动转成大量 flaky tests。

## R-012 过早做复杂 UI

**风险：中**

应对：

- UI 以功能和状态完整性优先。
- 动画/主题排在 v0.1 后。

---
# 23. GitHub Milestones 与 Issues 组织

## 23.1 Milestone 结构

建议不按“前端/后端”建 milestone，而按可交付能力建：

| Milestone | 对应阶段 | 完成后可验证结果 |
|---|---|---|
| M0 Foundation | P0-P1 | backend/web/CI skeleton |
| M1 WARP Runtime | P2-P4 | 单/多 WARP + health |
| M2 Proxy Plane | P5 | SOCKS5/HTTP 可走 WARP |
| M3 Control Plane | P6-P8 | persistence/API/auth |
| M4 Web Console | P9-P10 | 浏览器完整管理 |
| M5 Container MVP | P11 | compose + final E2E |
| M6 v0.1 Release | P12 | release candidate/final |

## 23.2 Issue Label

建议：

```text
area/backend
area/runtime
area/proxy
area/db
area/api
area/auth
area/web
area/docker
area/test
area/docs
area/security

kind/feature
kind/bug
kind/refactor
kind/test
kind/docs
kind/chore

priority/p0
priority/p1
priority/p2

risk/high
risk/medium
risk/low

needs-real-warp
needs-docker-e2e
breaking
```

## 23.3 Issue 模板

```markdown
## Goal

## Background

## Scope

## Out of Scope

## Design References

## Implementation Notes

## Acceptance Criteria
- [ ]

## Test Requirements
- [ ] L0
- [ ] L1
- [ ] L2
- [ ] L3
- [ ] L4
- [ ] L5

## Docker Build Needed?
No by default.

## Security Considerations

## Dependencies
```

## 23.4 推荐首批 Issue 清单

### M0 Foundation

#### DEV-001 Bootstrap Rust workspace

- Phase：P0。
- 输出：workspace 可 check。
- Tests：L0。

#### DEV-002 Bootstrap React workspace

- 输出：React TS strict 可运行。
- Tests：typecheck/lint。

#### DEV-003 Add CI static checks

- 不构建 Docker。

#### DEV-004 Add ADR framework and upstream/license ADR

#### DEV-005 Implement AppConfig

#### DEV-006 Implement AppError and HTTP mapping

#### DEV-007 Add structured tracing and request ID

#### DEV-008 Add SQLite pool and migration runner

#### DEV-009 Add graceful shutdown

#### DEV-010 Add Axum test harness

### M1 WARP Runtime

#### DEV-020 Define InstanceId / InstancePaths / Port allocator

#### DEV-021 Define WarpControl trait

#### DEV-022 Define ProcessSpawner abstraction

#### DEV-023 Implement FakeWarpControl

#### DEV-024 Implement fake process fixtures

#### DEV-025 Implement per-instance D-Bus runtime

`needs-real-warp`。

#### DEV-026 Implement warp-svc process wrapper

#### DEV-027 Implement warp-cli adapter

#### DEV-028 Implement single-instance startup flow

#### DEV-029 Implement single-instance graceful stop

#### DEV-030 Add child crash watcher

#### DEV-031 Create fixed Dev Base image

> 此 Issue 创建 `warpdeck-dev-base:1`；后续不为普通测试重复创建。

#### DEV-032 Real WARP single-instance integration test

#### DEV-033 Implement Runtime Registry

#### DEV-034 Add per-instance operation serialization

#### DEV-035 Add multi-instance paths/ports isolation

#### DEV-036 Real 3-instance isolation test

#### DEV-037 Define HealthState and health snapshot

#### DEV-038 Add control-plane health check

#### DEV-039 Add data-plane Cloudflare trace probe

#### DEV-040 Add health thresholds and recovery

### M2 Proxy Plane

#### DEV-050 Define typed GOST configuration model

#### DEV-051 Implement GOST YAML renderer

#### DEV-052 Implement fake upstream proxy test harness

#### DEV-053 Implement GOST supervisor

#### DEV-054 Add SOCKS5 listener `11080`

#### DEV-055 Add HTTP listener `18080`

#### DEV-056 Route healthy WARP nodes only

#### DEV-057 Implement empty pool fail-closed behavior

`risk/high`。

#### DEV-058 Add proxy authentication

#### DEV-059 Add CIDR allowlist validation

#### DEV-060 Add connection/rate limits

#### DEV-061 Add atomic GOST config apply

#### DEV-062 Real WARP SOCKS5/HTTP smoke tests

### M3 Control Plane

#### DEV-070 Add `warp_instances` migration/repository

#### DEV-071 Add `proxy_config` migration/repository

#### DEV-072 Implement DesiredState domain

#### DEV-073 Implement Reconciler

#### DEV-074 Implement restart backoff

#### DEV-075 Implement startup recovery

#### DEV-076 Implement instances REST API

#### DEV-077 Implement proxy REST API

#### DEV-078 Implement settings/system API


#### DEV-079 Add first-run admin setup

#### DEV-080 Add Argon2id password hash

#### DEV-081 Implement sessions

#### DEV-082 Add auth middleware

#### DEV-083 Add CSRF protection

#### DEV-084 Implement master key handling

#### DEV-085 Implement encrypted Secret Store

#### DEV-086 Add secret redaction tests

#### DEV-087 Add WARP+/Zero Trust account API

### M4 Web Console

#### DEV-099 Add typed frontend API client

#### DEV-100 Build setup page

#### DEV-101 Build login page

#### DEV-102 Build application shell

#### DEV-103 Build dashboard

#### DEV-104 Build instances list

#### DEV-105 Build add-instance flow

#### DEV-106 Build instance detail/actions

#### DEV-107 Build proxy settings

#### DEV-108 Build account settings

#### DEV-109 Build settings page

#### DEV-110 Build logs history page

#### DEV-111 Add Mock API Playwright suite

#### DEV-112 Implement internal event bus

#### DEV-113 Implement SSE endpoint

#### DEV-114 Add frontend SSE reconnection

#### DEV-115 Add live logs

### M5 Container MVP

#### DEV-129 Create final multi-stage Dockerfile

`needs-docker-e2e`。

#### DEV-130 Pin GOST/runtime dependencies and checksums

#### DEV-131 Add release Compose

#### DEV-132 Add container healthcheck

#### DEV-133 Build reusable E2E candidate workflow

#### DEV-134 Fresh-install Docker E2E

#### DEV-135 Multi-instance persistence E2E

#### DEV-136 Instance crash/failover E2E

#### DEV-137 No-direct-leak E2E

`priority/p0` + `risk/high`。

#### DEV-138 Proxy auth E2E

### M6 v0.1 Release

#### DEV-149 Dependency audit workflow

#### DEV-150 Image vulnerability scan

#### DEV-151 SBOM generation

#### DEV-152 Security regression suite

#### DEV-153 Backup/restore test and docs

#### DEV-154 Upgrade/migration test

#### DEV-155 Final Quick Start docs

#### DEV-156 Troubleshooting guide

#### DEV-157 License/attribution review

#### DEV-158 Build v0.1.0-rc.1

#### DEV-159 Release v0.1.0

## 23.5 Issue 依赖示例

```text
DEV-021 WarpControl Trait
  -> DEV-023 FakeWarpControl
  -> DEV-027 Real WarpCliAdapter
  -> DEV-028 Single Instance Flow

DEV-039 Data Plane Probe
  -> DEV-056 Healthy Pool
  -> DEV-062 Proxy Smoke

DEV-073 Reconciler
  -> DEV-075 Startup Recovery
  -> DEV-076 Instances API

DEV-086 Secret Store
  -> DEV-088 Account API
  -> DEV-109 Account UI
```

---

# 24. 每日开发执行模板

这里的“每日”指一次实际开发工作循环，不要求团队使用固定日历周期。

## 24.1 开始工作前

```text
[ ] 当前 Issue 有明确 Acceptance Criteria
[ ] 已查看依赖 Issue 状态
[ ] 已查看对应设计章节
[ ] 已确认本任务要求的最高测试层级
[ ] 已确认是否真的需要 Docker build
[ ] 已确认是否涉及 Secret/Runtime/Migration
```

## 24.2 开发顺序

```text
1. 写/更新测试或可验证条件
2. 实现最小变更
3. cargo fmt / lint
4. 运行相关 L1
5. 运行相关 L2
6. 必要时 L3
7. 只有需要真实 WARP 才进入 L4
8. 更新文档
9. 自查 diff
10. 提交 PR
```

## 24.3 Runtime 开发特别流程

```text
Fake test first
   |
Process fixture test
   |
Real binary integration in dev-base
   |
Real WARP 1 instance
   |
Only if multi-instance behavior -> 3 instances
```

## 24.4 UI 开发特别流程

```text
Mock API
  -> component test
  -> Playwright mock E2E
  -> only final integration connects real backend
```

不要因调整 CSS 重新 build WARP image。

## 24.5 工作结束前

```text
[ ] git diff 已检查
[ ] 无 debug println/console.log
[ ] 无临时 secret
[ ] 无新无意义 Docker tag
[ ] 测试产物已清理
[ ] 文档更新
[ ] Issue checklist 更新
```

---

# 25. 阶段评审模板

每个 Phase Gate 建议创建一条 Milestone Review Issue。

模板：

```markdown
# Phase X Review

## Scope Completed

## Deferred Items

## Automated Tests

## Real WARP Verification

## Docker Builds Performed

## Known Risks

## Security Review

## Documentation

## Demo / Evidence

## Gate Decision
- [ ] GO
- [ ] GO with follow-up issues
- [ ] NO-GO
```

## 25.1 GO 条件

可以进入下一阶段：

- Gate 全部核心项满足。
- 未完成事项不破坏下一阶段基础。
- 高风险 bug 有明确处理。

## 25.2 NO-GO 条件

以下问题不得带入后续复杂层：

- WARP child 无法稳定 stop/reap。
- 多实例会互相污染。
- 健康检查把坏节点判 Healthy。
- GOST Empty Pool 会 direct leak。
- Secret 会进入日志/API。
- Reconciler 不幂等。
- Migration 无法重现。

---

# 26. MVP 最终验收计划

## 26.1 环境准备

使用：

- 干净 Docker 环境。
- 全新 WarpDeck data volume。
- Release candidate image。
- 不复用开发 DB。

记录：

```text
OS
Docker version
CPU arch
image digest
WarpDeck version
Cloudflare WARP version
GOST version
```

## 26.2 安装验收

执行：

```bash
docker compose up -d
```

必须：

```text
[ ] 容器启动
[ ] healthcheck 成功
[ ] 9000 可访问
[ ] 未初始化时进入 setup
```

## 26.3 管理员验收

```text
[ ] 创建管理员
[ ] setup 不能重复使用
[ ] 错误密码失败
[ ] 正确密码登录
[ ] logout 后 session 失效
```

## 26.4 单实例验收

```text
[ ] 创建 instance 0
[ ] desired=running
[ ] runtime starting -> running
[ ] health -> healthy
[ ] exit IP 出现
[ ] colo 可显示（有值时）
[ ] latency 可显示
```

## 26.5 SOCKS5 验收

```bash
curl --socks5-hostname 127.0.0.1:11080 \
  https://cloudflare.com/cdn-cgi/trace
```

必须：

```text
warp=on
```

## 26.6 HTTP 验收

```bash
curl -x http://127.0.0.1:18080 \
  https://cloudflare.com/cdn-cgi/trace
```

必须：

```text
warp=on
```

## 26.7 多实例验收

创建 3 个实例：

```text
[ ] #0 Healthy
[ ] #1 Healthy
[ ] #2 Healthy
[ ] internal ports=40000/40001/40002
[ ] Runtime paths 独立
```

连续请求，确认数据面持续可用。

不强制每次请求都出现不同 IP；重点验证 pool 包含多个真实 healthy nodes。

## 26.8 动态生命周期验收

### Stop #1

```text
[ ] #1 从 Healthy Pool 移除
[ ] SOCKS5 仍工作
[ ] HTTP 仍工作
[ ] #0/#2 不受影响
```

### Restart #1

```text
[ ] #1 恢复
[ ] 健康后重新入 pool
```

### Kill #2 warp-svc

```text
[ ] Manager 不退出
[ ] #2 -> Failed/Unhealthy
[ ] GOST pool 移除 #2
[ ] Proxy 继续使用 #0/#1
[ ] 根据策略尝试恢复 #2
```

## 26.9 No Direct Leak 验收

停止所有实例或使其 Unhealthy。

请求：

```text
SOCKS5
HTTP
```

必须：

```text
失败
```

禁止：

```text
请求成功但使用宿主机真实出口
```

这是 v0.1 阻断级验收项。

## 26.10 Proxy Auth 验收

启用认证。

测试：

- 无凭据失败。
- 错密码失败。
- 正确凭据成功。
- SOCKS5/HTTP 都覆盖。

## 26.11 Allowlist 验收

- 合法 CIDR 保存。
- 非法 CIDR API 拒绝。
- 不允许 IP 请求被拒绝（按测试环境可执行范围）。

## 26.12 Persistence 验收

配置：

- 管理员。
- 3 个实例。
- proxy auth。
- WARP account mode。

执行：

```bash
docker compose restart
```

必须：

```text
[ ] 管理员仍存在
[ ] Secret 可解密使用
[ ] Desired State 保留
[ ] 实例自动恢复
[ ] proxy config 保留
[ ] registration state 保留
[ ] SOCKS5/HTTP 恢复可用
```

## 26.13 Secret 验收

测试期间使用唯一 marker。

完成后扫描：

```text
manager logs
instance logs
gost logs
API captures
browser visible data
DB non-secret columns
```

不得出现明文 marker。

## 26.14 Backup/Restore 验收

备份：

```text
DB
master.key
instances registration state
```

在新 volume/环境恢复。

验证：

- 登录。
- Secret 解密。
- instance registration。
- proxy。

## 26.15 Upgrade 验收

从前一 RC schema 升级：

```text
old image/data
 -> new image
 -> migration
 -> startup
 -> functional smoke
```

## 26.16 MVP Final Gate

```text
[ ] Install PASS
[ ] Auth PASS
[ ] Single WARP PASS
[ ] Multi WARP PASS
[ ] SOCKS5 PASS
[ ] HTTP PASS
[ ] No Direct Leak PASS
[ ] Persistence PASS
[ ] Secret PASS
[ ] Backup/Restore PASS
[ ] Upgrade PASS
[ ] Security Scan PASS
[ ] SBOM PASS
[ ] Docs PASS
```

任何以下失败均不得发布 v0.1：

```text
No Direct Leak
Secret leak
Auth bypass
Migration data loss
Orphan process causing runtime corruption
SOCKS5/HTTP cannot reliably warp=on
```

---

# 27. Release 后续计划

v0.1 发布后不要立即扩展协议。

优先观察：

```text
runtime stability
WARP package compatibility
reconcile reliability
gost restart frequency
secret handling
DB migration
actual user deployment problems
```

## 27.1 v0.1.x

只做：

- bug fix。
- compatibility。
- security。
- migration fix。
- documentation。

## 27.2 v0.2 候选主题

多账号档案（Multi-Account Profiles）：见 DESIGN §16.9 / §17.6 / §19.6。

```text
任务切分（建议按此顺序，每步独立可合并）      [状态：A-F 完成，G 可选]
  A. DB migration 0005_account_profiles        [x]
       - 建表 account_profiles；warp_instances 加 account_profile_id；
         secrets 加 profile_id；迁移旧 account_config 单行 -> 默认 free 档案
       - 必须含 migration test（PR 门禁）
  B. 领域层 + Repository                        [x]
       - AccountProfile 模型；SqliteAccountProfileRepository
       - 校验：warp_plus 需 license、zero_trust 需三要素；删除保护（默认/被引用 409）
       - 绑定实例的凭据复用现有 SecretStore（profile_id 维度）
       - §16.9 约束：free 档全局唯一且只读（内置默认档即 free；名称/模式/凭据均
         不可改，PATCH 409）；档案 name 唯一（表 UNIQUE，create/update 409）；被实例
         引用的档案只读（count_bound_to_profile > 0 时 PATCH 409，先解绑再改）；
         WARP+ 单实例（一个 warp_plus 档同一时刻至多绑定一个实例，instances 创建/
         改绑时校验，409）
         —— 随 2026-08-20 收紧（free 多档无技术差异，改为系统保留只读资源）
  C. Runtime：启动注入                           [x]
       - 11.2 凭据步骤改为「按实例绑定的档案取凭据」（SqliteCredentialResolver）
- ZeroTrust 注册 = warp-svc 启动前写实例 state 目录 mdm.xml（service token，
          organization/auth_client_id/auth_client_secret，另含 service_mode=proxy 与
          proxy_port=40000+id——managed 账号禁 CLI 改端口）；非 ZeroTrust 清除残留
        - ZeroTrust connect 竞态（E2E-08 实测）：mdm 注册启动后异步完成（~3s），
          注册前 `warp-cli connect` 报 MissingRegistration——flow 仅对该签名在 60s
          预算内按 2s 有界重试（ZT_CONNECT_RETRY_POLL_INTERVAL/WAIT_TIMEOUT）
        - mdm.xml 必须额外下发 warp_tunnel_protocol=masque（E2E-08 实测）：org
          默认 Wireguard，而 WarpProxy 模式只支持 MASQUE，缺此项连接直接失败；
          值必须用 serde 小写名（写 CLI 大写 MASQUE 会被静默解析回 Wireguard，
          2026-08-20 实测）。proxy 模式对所有账号类型仅支持 MASQUE（客户端
          2025.7.106.1 起弃用 WireGuard 组合）——「实例级隧道协议自选」已评估并
          否决，不再立项
       - 档案变更 => mark dirty via instances（rebind_profile 置 restart_pending）；
         Reconciler 按序重启（复用现有 restart path，apply 失败上浮，禁止静默成功）
  D. REST API                                   [x]
       - /api/v1/accounts CRUD（profile 维度 secret 单次写盘，防止部分更新）
       - instances POST/PATCH 支持 account_profile_id；PATCH 区分「字段缺失 422」与
         「显式 null = 解绑默认档」；GET 永不回明文凭据，只回 mode/mask + 绑定实例数
       - 实例视图含 account {profile_id,name,mode} 摘要（默认档按 id=1 展开）
  E. React UI                                   [x]
       - Accounts 页面（档案列表 + 创建/编辑 modal + 删除保护提示；默认档不可删除，
         仍被引用时后端 409 上浮展示）；实例创建档案选择器；详情页改绑 select +
         「下次重启生效」提示
       - WARP+ 单设备绑定常驻警示文案；改绑提示「下次重启生效」
- 创建/编辑 modal 按 free 唯一过滤 free 选项（已有 free 档时隐藏；free 档编辑按钮
  禁用并提示只读；被引用的档案编辑按钮禁用并提示先解绑）
  F. E2E / 文档                                  [x]
       - Docker E2E 换线验证：free 与 zero_trust 两个档案 -> 两条连接 -> 不同 exit 行为
         （E2E-08 全绿：ZT 实例 healthy + exit_ip，socks5 warp=on，改绑自动重启，
         删除保护 409；期间修复 connect 竞态与 WarpProxy 必须 masque 两个实测问题）
       - 更新 README（每个档案独立 key；Accounts & Profiles 章节）
  G. 拆 PR 时不要跳过单个实例 lifecycle 改动（先单档案，再多档案并行切换）
```

可观测性：

- metrics。
- latency history。
- availability history。
- Prometheus。

## 27.3 v0.3 候选主题

Routing：

- random。
- failover。
- weighted。

必须在 v0.1 Round Robin 稳定后再做。

## 27.4 v0.4 候选主题

Operations：

- backup export/import。
- safe upgrade assistant。
- restore validation。

## 27.5 明确延后

Multi-host / central control plane 直到有真实需求再设计。

---

# 附录 A：Definition of Ready

一个 Issue 在进入开发前应满足：

```text
[ ] Goal 清晰
[ ] Scope 清晰
[ ] Out of Scope 清晰
[ ] Acceptance Criteria 可测试
[ ] 依赖已知
[ ] 对应设计章节已存在
[ ] Security 影响已识别
[ ] DB/API breaking 影响已识别
[ ] 最高测试层级已确定
[ ] 是否需要 Real WARP 已确定
[ ] 是否需要 Docker build 已确定
```

如果这些信息缺失，不建议直接写代码。

---

# 附录 B：Definition of Done

所有功能：

```text
[ ] 实现完成
[ ] 无临时 debug code
[ ] 无未关联 TODO
[ ] fmt/lint/typecheck 通过
[ ] 必要 L1 测试
[ ] 必要 L2 测试
[ ] Error path 测试
[ ] Timeout/cancel 测试（如适用）
[ ] Secret/log 检查
[ ] Migration 测试（如适用）
[ ] API docs 更新（如适用）
[ ] Design/Plan docs 更新
[ ] PR Test Plan 可复现
[ ] Docker build 次数符合规范
```

Runtime/GOST 功能额外：

```text
[ ] L3 已执行
[ ] L4 已执行（如要求）
[ ] child 无 orphan
[ ] failure path 可恢复/可见
[ ] no direct fallback
```

Docker/Release 功能额外：

```text
[ ] L5/L6 已执行
[ ] image tag 合规
[ ] candidate build 被复用
[ ] SBOM/scan（Release）
```

---

# 附录 C：推荐命令

## C.1 Backend 日常

```bash
cargo fmt
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## C.2 Frontend 日常

```bash
cd web
pnpm install
pnpm dev
pnpm lint
pnpm typecheck
pnpm test
```

## C.3 快速总检查

推荐封装：

```bash
just check
```

内容只包含本地静态/测试，不隐式 build Docker。

## C.4 Real WARP Dev

典型流程：

```text
cargo build
 -> 使用固定 warpdeck-dev-base:1
 -> 挂载/更新 binary
 -> restart dev container/process
 -> run L4 smoke
```

## C.5 E2E

只有候选集成时：

```bash
docker build -t warpdeck:e2e .
docker compose -f compose.e2e.yml up -d
```

之后整个矩阵复用 `warpdeck:e2e`。

## C.6 SOCKS5 Smoke

```bash
curl --socks5-hostname 127.0.0.1:11080 \
  https://cloudflare.com/cdn-cgi/trace
```

## C.7 HTTP Smoke

```bash
curl -x http://127.0.0.1:18080 \
  https://cloudflare.com/cdn-cgi/trace
```

## C.8 Docker 诊断

```bash
docker ps
docker logs warpdeck
docker image ls
docker system df
```

不要把：

```bash
docker system prune -a --volumes
```

放到普通 test script。

---

# 附录 D：任务状态与优先级

## D.1 状态

```text
Backlog
Ready
In Progress
In Review
Blocked
Done
```

## D.2 优先级

### P0 - Blocker

- 数据安全。
- direct leak。
- auth bypass。
- data corruption。
- release blocking runtime failure。

### P1 - MVP Required

没有它 v0.1 不完整。

### P2 - Important

可以在不破坏 MVP 核心价值时稍后完成。

### P3 - Future

不进入 v0.1。

## D.3 任务大小建议

建议 Issue 控制在可以独立 Review 的粒度。

可使用：

```text
XS - 单一小修改
S  - 一个清晰组件改动
M  - 一个完整 feature slice
L  - 应继续拆分
XL - 禁止直接进入开发，先拆 Issue
```

例如：

```text
“实现整个 WARP 管理系统” = XL，不可开发
“实现 WarpCliAdapter status/connect” = S/M，可开发
```

---

# 结语：推荐实际启动顺序

如果现在从空仓库开始，严格按下面顺序推进：

```text
1. P0 工程基线
2. P1 Axum/SQLite/Tracing/Test Harness
3. P2 Fake Runtime
4. P2 单实例真实 WARP
5. P3 三实例隔离
6. P4 Health/Data Plane
7. P5 GOST SOCKS5/HTTP
8. P6 Desired State/Reconciler
9. P7 REST API
10. P8 Auth/Secret
11. P9 Web UI
12. P10 SSE/Logs
13. P11 Final Docker/E2E
14. P12 Security/Release
```

任何时候如果发现需要靠“不断 build Docker image”才能验证普通业务逻辑，优先停下来检查抽象边界和测试设计，而不是继续增加镜像。

WarpDeck v0.1 的开发重点不是功能数量，而是四条主链路必须稳定：

```text
WARP Runtime 生命周期
        +
多实例健康管理
        +
SOCKS5 / HTTP Fail-Closed 数据面
        +
安全、持久、可恢复的 Web 控制面
```

只要这四条稳定，后续 Routing、Observability、Backup UI、Multi-host 才有可靠的演进基础。
