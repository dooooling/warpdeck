-- 0007_restart_generation.sql
-- P1 审查 R2#1（API 绕过 Reconciler 直接重启）的 generation 化改造：
--   POST /api/v1/instances/:id/restart 不再直接调用 runtime，而是把重启请求
--   写成单调递增的命令代数；Reconciler 是唯一执行者：
--     restart_command_generation    期望侧：API 每次 restart 命令 +1
--     observed_restart_generation   实际侧：Reconciler 完成 start/restart 后
--                                   追平到已处理的命令代数（单调，不回退）
--   两者不等 = 有待执行的重启命令。停机期间排队多条命令自然合并为最新一条。
--
-- 删除路径不需要新列：API 只删期望行（202），运行中实例由 Reconciler 的
-- 孤儿收敛逻辑停止——消除旧「先停进程后删行」窗口里 Reconciler 复活实例的竞态。

ALTER TABLE warp_instances ADD COLUMN restart_command_generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE warp_instances ADD COLUMN observed_restart_generation INTEGER NOT NULL DEFAULT 0;
