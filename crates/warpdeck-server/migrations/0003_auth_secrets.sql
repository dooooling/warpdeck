-- 0003_auth_secrets.sql
-- P8-001..012 认证与会话 + 加密 secret 存储（DESIGN §16.1/16.2/16.5/16.6）：
--   users          管理员账号（Argon2id 哈希；MVP 单管理员，表结构不写死单用户）
--   sessions       服务端会话（id=随机 UUID；csrf_token 绑定会话，DESIGN §16.2 补充列；
--                  HttpOnly cookie 只存 session id）
--   secrets        加密 secret（XChaCha20-Poly1305；kind 唯一 = 主身份，ciphertext 永不明文；
--                  proxy_config.proxy_password_secret_id 列保留但 P8 起弃用，统一走 kind 索引，
--                  避免双写不一致）
--   account_config 账号模式（free/warp_plus/zero_trust 互斥，DESIGN §16.5；
--                  secret 通过 secrets.kind 关联，不建引用列——kind 即稳定标识）
-- 时间统一 ISO8601 UTC 文本（与 0001/0002 一致）。

CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE sessions (
    id           TEXT PRIMARY KEY,
    user_id      INTEGER NOT NULL,
    -- P8-004/006：会话绑定 CSRF token（mutation 需 X-CSRF-Token 一致）。
    csrf_token   TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE TABLE secrets (
    id          INTEGER PRIMARY KEY,
    kind        TEXT NOT NULL UNIQUE,
    ciphertext  BLOB NOT NULL,
    nonce       BLOB NOT NULL,
    key_version INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE account_config (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    mode          TEXT NOT NULL DEFAULT 'free'
                  CHECK (mode IN ('free', 'warp_plus', 'zero_trust')),
    zero_trust_org TEXT,
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
