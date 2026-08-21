# WarpDeck 文档索引

## 权威设计文档

- [技术设计与开发文档](../DESIGN_AND_DEVELOPMENT.md) — 系统"是什么/为什么"：架构、模型、REST API、安全设计、Docker、测试规范
- [开发实施计划](../DEVELOPMENT_PLAN.md) — 系统"怎么建/何时完成"：P0–P12 分阶段计划、Gate、Milestones、风险登记
- [README（Quick Start / 端口 / 安全 / 备份 / 升级 / 故障排查）](../README.md)

## 架构决策记录（ADR）

（当前无有效 ADR；决策记录会依需补充。）

## 构建与运维命令

统一入口 `cargo xtask`（实现见 `crates/xtask`；依赖 URL/哈希单源 `crates/xtask/src/versions.json`）：

- `cargo xtask release [--proxy ...]` — release 镜像构建（版本标注 `0.1.0-<git sha>`）
- `cargo xtask backup` / `restore --archive <p>` / `backups` — 数据卷备份 / 恢复（P12-009）
- `cargo xtask e2e [--only N,N] [--no-fresh]` — 真实数据面 E2E 矩阵（P11）；CI 由路径触发工作流 `docker-e2e.yml`
- 其余任务（dev-base / in-container / check-linux / smoke-dev-base）见 `AGENTS.md` Commands

## 约定

- 设计变更必须先更新 `DESIGN_AND_DEVELOPMENT.md`，再同步 `DEVELOPMENT_PLAN.md`（见计划 §1）。
- 开发与测试纪律、测试层级（L0–L6）、Docker 构建红线见设计文档 §25。
- 错误码注册表、事件注册表、配置注册表见计划 §18。

## 许可证说明

本仓库代码/文档采用 **MIT License**（见 [LICENSE](../LICENSE)）。镜像内嵌组件（Cloudflare WARP / GOST / 依赖包）适用各自许可证，发布再分发前需确认；SBOM 见发布产物 `scans/`。