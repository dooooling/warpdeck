-- 0006_instance_restart_pending.sql
-- v0.2 多账号档案变更驱动（DESIGN §16.9 / DEVELOPMENT_PLAN §27.2 任务 C）：
--   档案凭据/模式更新需要重启绑定实例（应用失败必须上浮，不得静默成功）。
--   由于期望状态只驻留 SQLite，实例需要一个显式「待重启」标记：
--     restart_pending = 1  -> 下一轮 Reconciler 收敛时按序重启该实例
--                            （running -> runtime.restart；未运行 -> start）
--                            成功后由 Reconciler 清零。
--   仅对 enabled=1 且 desired_state='running' 的实例触发（不干扰已停止实例）。
--   0005 已有的实例默认 restart_pending = 0（不触发重启）。

ALTER TABLE warp_instances ADD COLUMN restart_pending INTEGER NOT NULL DEFAULT 0
    CHECK (restart_pending IN (0, 1));