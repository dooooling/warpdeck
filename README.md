# WarpDeck

Cloudflare WARP Web Manager — 在单容器内动态管理多个 WARP 实例，通过 SOCKS5 / HTTP 代理对外提供服务(数据面走真实 WARP,`curl` 可见 `warp=on`)。

- 技术设计与开发文档:[DESIGN_AND_DEVELOPMENT.md](DESIGN_AND_DEVELOPMENT.md)
- 开发实施计划:[DEVELOPMENT_PLAN.md](DEVELOPMENT_PLAN.md)
- 文档索引:[docs/README.md](docs/README.md)

## Quick Start

依赖:Docker + Docker Compose(仅服务器端;管理界面为内置 Web UI,无需另装前端)。

```bash
# 1. 构建镜像(WARP 依赖在构建期内自动下载并做 SHA256 校验;
#    中国网络下加 --proxy socks5h://host.docker.internal:10808 走宿主代理)
cargo xtask release

# 2. 按需创建 .env(不创建则用安全默认值,见官方 .env.example)
Copy-Item .env.example .env

# 3. 启动并打开管理页
docker compose up -d
# 浏览器访问 http://127.0.0.1:9000 → 首次 setup 创建管理员 → 新建实例
```

首次初始化:打开管理页 → 创建管理员账号(密码使用 Argon2id 存储)→ 创建 WARP 实例 → 实例进入 `Healthy` 后即可通过代理访问互联网。

## Ports

| 端口 | 用途 | 说明 |
|---|---|---|
| `9000` | Web 管理界面 + REST API | 默认只绑定宿主机 `127.0.0.1` |
| `11080` | SOCKS5 代理 | 容器内固定;Host 映射见 `.env` |
| `18080` | HTTP 代理 | 容器内固定;Host 映射见 `.env` |
| `40000+instance_id` | WARP 实例内部端口 | 仅容器内 loopback,**禁止**发布到宿主机 |

Host 端口映射只由 Compose `.env` 管理(`WEB_HOST_BIND` / `WEB_HOST_PORT` / `SOCKS5_HOST_BIND` …),Web UI 不提供修改。

## Configuration

`docker-compose-环境变量`(见 `.env.example`):

- `WEB_HOST_BIND` / `SOCKS5_HOST_BIND` / `HTTP_HOST_BIND` — Host 绑定地址,默认 `127.0.0.1`(仅本机)。公网暴露需显式设为 `0.0.0.0`,并务必在管理页开启代理认证。
- `*_HOST_PORT` — Host 发布端口,默认 9000/11080/18080。
- `WARPDECK_MASTER_KEY` — 主密钥(留空则首次启动自动生成并持久化为 `master.key`,0600)。secret(代理密码等)用它加密存储。
- `WARPDECK_LOG` — 日志级别,默认 `info`。

重启生效方式:`docker compose up -d`。

## Accounts & Profiles(账号档案)

v0.2 起支持**多账号档案**:每个实例创建时绑定一个档案(不选即默认免费档),档案决定该实例的 WARP 出口线路与凭据(免费 WARP / WARP+ / Zero Trust)。

- 默认档案 `free` 内置且不可删除;WARP+ 档案需 license;Zero Trust 档案需 Org + Service Token(Client ID / Client Secret,加密存储,API 永不回显明文)。
- **WARP+ 单设备绑定**:同一 license 只能同时用于一个实例,请为每个档案准备独立的 key,禁止一 key 多档案复用。
- 创建实例时可选择档案;详情页可改绑(下次重启自动生效);删除仍被实例绑定的档案会被后端拒绝(409)。
- Zero Trust 换线由 mdm.xml(Service Token)在实例内自动注册完成,无需交互式登录;数据面 `curl https://cloudflare.com/cdn-cgi/trace` 可见 `warp=on`。

详见 DESIGN §16.9 / §17.6 / §19.6。

## Security Notes

- 默认最小暴露:Web 与代理端口都只绑定宿主机 loopback;公网访问需显式配置绑定 + 开启认证。
- 代理认证:管理页可启用 SOCKS5/HTTP 用户名密码认证(支持网络 allowlist 与连接/速率限制)。认证关闭时管理页显示警示。
- Secret 加密存储:XChaCha20-Poly1305,主密钥 `WARPDECK_MASTER_KEY` 或 `master.key`(0600);API 永不回显密码明文;日志经统一 redactor 过滤,不落盘 license/密码。
- 会话:Cookie `HttpOnly + SameSite=Lax`,mutation 需 CSRF token,登出即失效。
- 无任意命令执行面:API 只暴露领域动作,不存在 `warp-cli` 透传。
- 镜像最小权限:不挂 Docker Socket、不使用 `privileged`;仅需 `--device /dev/net/tun` + `NET_ADMIN`(`compose.yml` 已配置)。

## Backup & Restore

备份/恢复操作数据卷整体快照(DB + master.key + WARP 注册态,恢复后免重新注册):

```bash
cargo xtask backup                          # 产物在 backups/
cargo xtask restore --archive <备份文件路径>
cargo xtask backups
```

原理(见 DESIGN §28.3):先 `compose stop` 落盘 WAL,再打包 `warpdeck-data` 卷;恢复时校验归档含 `warpdeck.db` 与 `master.key` 后清卷解包并启动。备份归档含加密密钥,请妥善保管。

## Upgrade

- 镜像升级:`docker compose pull`(或重新 build)→ `docker compose up -d`。数据卷不删,配置与实例保留。
- 数据库 schema 由内嵌 migration 管理(幂等、只追加);从上一支持版本升级自动完成,已应用项跳过,历史数据保留(P12-010 覆盖了升级路径测试)。
- 版本标识:`/api/v1/system/version`、`/api/v1/health` 与镜像 LABEL 上报 `0.1.0-<git sha>`。

## Troubleshooting

| 症状 | 排查 |
|---|---|
| 实例一直 `starting` / `failed` | `docker logs warpdeck-warpdeck-1`;实例日志见管理页;注册失败会自动 backoff 重试 |
| 代理连不通 | 确认 ≥1 实例 `Healthy`(无 healthy upstream 时代理拒绝连接,保证不直连泄露);确认 `.env` 端口映射 |
| `warp=off` | 实例未走 WARP 隧道上线;重试 / 重启实例 |
| 忘记管理员密码 | 无自恢复路径:从备份恢复,或用含 `master.key` 与 DB 的备份重建环境 |

## License

WarpDeck 本体（Rust 后端 / Web 前端 / 脚本 / 文档）以 **MIT License** 授权，见
[LICENSE](LICENSE)。

发布镜像（warpdeck 运行时镜像）内嵌组件适用各自许可证，请在使用/再分发前确认：

- **Cloudflare WARP**（`cloudflare-warp_2026.6.880.0_amd64.deb`）：Cloudflare 客户端软件，
  受 Cloudflare 服务条款与客户端许可约束；默认仅面向个人/非商业使用，商业使用需另行确认。
- **Rust crates / npm 包**：各依赖按其自身许可证（MIT/Apache-2.0/BSD 等）授权；
  SBOM 见发布产物的 `scans/`。