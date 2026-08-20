-- 0002_warp_instances.sql
-- P6-001 核心 desired-state 表（DESIGN §16.3/16.4/16.8）：
--   warp_instances  期望状态（enabled/desired_state/auto_restart + P6-007 backoff 列）
--   proxy_config    单行代理配置（id=1 约束；端口固定为容器常量，不入库）
--   audit_log       审计日志（P6 建表，P8 写入）
-- 不保存 PID：短生命周期运行时状态不属于持久化数据（DESIGN §16.3、Gate §11.4）。
-- 时间统一 ISO8601 UTC 文本（与 0001_settings 一致，数据库层生成默认值）。

CREATE TABLE warp_instances (
    id               INTEGER PRIMARY KEY,
    name             TEXT NOT NULL,
    enabled          INTEGER NOT NULL DEFAULT 1,
    desired_state    TEXT NOT NULL DEFAULT 'running'
                     CHECK (desired_state IN ('running', 'stopped')),
    auto_restart     INTEGER NOT NULL DEFAULT 1,
    -- P6-007 restart backoff：上次失败时间与下次允许重试时间（ISO8601 UTC）。
    last_failure_at  TEXT,
    next_retry_at    TEXT,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE proxy_config (
    id                       INTEGER PRIMARY KEY CHECK (id = 1),
    socks5_enabled           INTEGER NOT NULL DEFAULT 1,
    http_enabled             INTEGER NOT NULL DEFAULT 1,
    auth_enabled             INTEGER NOT NULL DEFAULT 0,
    proxy_username           TEXT,
    proxy_password_secret_id INTEGER,
    -- 逗号分隔 CIDR 列表；空/NULL = 不限制（DESIGN §16.4）。
    allowed_ips              TEXT,
    max_connections          INTEGER NOT NULL DEFAULT 10,
    max_rps                  INTEGER NOT NULL DEFAULT 10,
    routing_strategy         TEXT NOT NULL DEFAULT 'round_robin'
                             CHECK (routing_strategy IN ('round_robin')),
    updated_at               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE audit_log (
    id          INTEGER PRIMARY KEY,
    user_id     INTEGER,
    action      TEXT NOT NULL,
    target      TEXT,
    detail_json TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);