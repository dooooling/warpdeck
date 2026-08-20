-- 0001_settings.sql
-- 基础 schema framework（P1-007）：settings 键值表（DESIGN §16.7）。
-- 时间统一为 ISO8601 UTC 文本，由数据库层生成默认值，避免应用层时钟漂移。

CREATE TABLE settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);