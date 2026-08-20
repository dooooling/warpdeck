-- 0005_account_profiles.sql
-- v0.2 多账号档案（DESIGN §16.9 / §17.6；PLAN §27.2 任务 A）：
--   account_profiles    账号档案（free / warp_plus / zero_trust；内置默认档 id=1，不可删除）
--   warp_instances      加 account_profile_id（NULL = 默认 free 档，运行时按默认解析）
--   secrets             加 profile_id 维度；UNIQUE(kind) -> UNIQUE(kind, profile_id)；
--                       老数据保留 profile_id=NULL（全局/系统级 secret，kind 互斥语义不变）
-- 兼容性：
--   - 0004 之后的现有数据原地保留（secret 密文、实例、账号配置均不动）
--   - 老进程读不到新表/新列（向前只读不兼容）：升级顺序要求先停旧实例再迁移
-- 时间统一 ISO8601 UTC 文本（与 0001-0004 一致）。

CREATE TABLE account_profiles (
    id             INTEGER PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,
    mode           TEXT NOT NULL DEFAULT 'free'
                   CHECK (mode IN ('free', 'warp_plus', 'zero_trust')),
    zero_trust_org TEXT,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- 内置默认档：新实例未指定档案时绑定它；删除保护由应用层强制（DESIGN §16.9）。
INSERT INTO account_profiles (id, name, mode) VALUES (1, 'free', 'free');

-- 实例 -> 档案：NULL = 默认 free 档（不 UPDATE 老数据，保持字段值即“未指定”语义）。
ALTER TABLE warp_instances ADD COLUMN account_profile_id INTEGER REFERENCES account_profiles(id);

-- secrets 增加档案维度。SQLite 无法直接修改列级约束，重建表：
--   UNIQUE(kind, profile_id) 中 profile_id 为 NULL 的行在 SQL 语义下互不冲突，
--   因此“全局 kind 唯一”（老数据）与“档案内 (kind, profile_id) 唯一”可共存。
CREATE TABLE secrets_v2 (
    id          INTEGER PRIMARY KEY,
    kind        TEXT NOT NULL,
    profile_id  INTEGER REFERENCES account_profiles(id) ON DELETE CASCADE,
    ciphertext  BLOB NOT NULL,
    nonce       BLOB NOT NULL,
    key_version INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (kind, profile_id)
);

INSERT INTO secrets_v2 (id, kind, profile_id, ciphertext, nonce, key_version, created_at, updated_at)
SELECT id, kind, NULL, ciphertext, nonce, key_version, created_at, updated_at FROM secrets;

DROP TABLE secrets;
ALTER TABLE secrets_v2 RENAME TO secrets;

CREATE INDEX idx_secrets_profile ON secrets (profile_id);