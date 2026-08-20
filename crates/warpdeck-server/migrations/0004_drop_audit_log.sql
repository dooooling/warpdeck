-- 0004_drop_audit_log.sql
-- 移除审计日志功能：删除 audit_log 表（DESIGN Δ：audit_log 不再需要）。
-- 0002 保持原样（已应用的 migration 不可修改，sqlx checksum 校验）。

DROP TABLE IF EXISTS audit_log;