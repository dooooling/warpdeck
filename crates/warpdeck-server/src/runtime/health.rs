//! 健康判定领域模型（P4-001/006）。
//!
//! DESIGN §14.4/14.5、计划 P4-006：
//! - 分层报告（进程 / 控制面 / 数据面）→ 单轮判定；
//! - 失败阈值：`consecutive_failures < 3` → Degraded，`>= 3` → Failed
//!   （DESIGN §14.5）；恢复需连续成功（默认 2 次）才回 Healthy
//!   （DESIGN §14.5 "恢复也可要求连续 2 次成功"）。
//! - 进程死亡是硬状态：不叠加网络抖动阈值，立即判 Failed
//!   （进程层由 Crash Watcher 独立覆盖，健康层同步认定）。
//!
//! 本模块全是纯函数，无 IO：所有变体（timeout / warp=off / 缺字段 /
//! 瞬时失败 / 恢复）用单元测试确定性覆盖。

use super::probe::DataPlaneReport;
use super::registry::RuntimeState;

/// 健康阈值配置（P4-006；SQLite settings 接入属 P7 持久化阶段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthConfig {
    /// 连续失败达到该值 → Failed（DESIGN §14.5）。
    pub failure_threshold: u32,
    /// 连续成功达到该值 → Healthy（DESIGN §14.5 恢复判据）。
    pub recovery_success_threshold: u32,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            recovery_success_threshold: 2,
        }
    }
}

/// 健康计数（跨轮持久于 registry 快照）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HealthCounters {
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
}

/// 单轮健康判定结果（映射到 `RuntimeState` 的 3 个健康态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthVerdict {
    Healthy,
    Degraded,
    Failed,
}

impl HealthVerdict {
    pub fn as_runtime_state(self) -> RuntimeState {
        match self {
            HealthVerdict::Healthy => RuntimeState::Healthy,
            HealthVerdict::Degraded => RuntimeState::Degraded,
            HealthVerdict::Failed => RuntimeState::Failed,
        }
    }
}

/// 单轮三层探测报告（P4-002/003/004）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayersReport {
    /// Level 1：进程健康（registry 中 PID 存续；崩溃由 watcher 更快接管）。
    pub process_alive: bool,
    /// Level 2：`warp-cli status` connected（控制面）。
    pub control_connected: bool,
    /// Level 3：数据面探测结果；`None` = 探测失败（超时/连接拒绝/异常）。
    pub data_plane: Option<DataPlaneReport>,
}

impl LayersReport {
    /// 数据面是否满足健康判据：任一地址族探测成功且 `warp=on`
    /// （AGENTS.md：Healthy 需要真实数据面 `warp=on`，不只是 PID 存活）。
    pub fn data_plane_ok(&self) -> bool {
        self.data_plane.as_ref().is_some_and(|r| r.warp_on())
    }
}

/// 单轮判定：输入三层报告 + 既有计数 + 阈值 + 当前状态，输出新判定与下一轮计数。
///
/// 规则（DESIGN §14.5）：
/// - 进程死 → 立即 `Failed`（硬状态，不等阈值）；
/// - 全部通过（控制面 connected 且数据面 `warp=on`）→ 成功轮：
///   - 本就 Healthy 或连续成功已达恢复阈值 → Healthy；
///   - 否则（Degraded 恢复中）→ Degraded（恢复需连续成功，默认 2 次）；
/// - 其余（控制面断开 / 数据面失败 / `warp!=on`）→ 失败轮：
///   `failures+1`，`>= failure_threshold` → Failed，否则 Degraded。
pub fn assess_layers(
    report: &LayersReport,
    counters: &HealthCounters,
    config: &HealthConfig,
    current: RuntimeState,
) -> (HealthVerdict, HealthCounters) {
    assert!(
        config.failure_threshold >= 1,
        "failure_threshold must be >= 1"
    );
    assert!(
        config.recovery_success_threshold >= 1,
        "recovery_success_threshold must be >= 1"
    );

    if !report.process_alive {
        return (
            HealthVerdict::Failed,
            HealthCounters {
                consecutive_failures: counters.consecutive_failures.saturating_add(1),
                consecutive_successes: 0,
            },
        );
    }

    if report.control_connected && report.data_plane_ok() {
        let successes = counters.consecutive_successes.saturating_add(1);
        let recovered =
            successes >= config.recovery_success_threshold || current == RuntimeState::Healthy;
        return (
            if recovered {
                HealthVerdict::Healthy
            } else {
                HealthVerdict::Degraded
            },
            HealthCounters {
                consecutive_failures: 0,
                consecutive_successes: successes,
            },
        );
    }

    let failures = counters.consecutive_failures.saturating_add(1);
    let verdict = if failures >= config.failure_threshold {
        HealthVerdict::Failed
    } else {
        HealthVerdict::Degraded
    };
    (
        verdict,
        HealthCounters {
            consecutive_failures: failures,
            consecutive_successes: 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::probe::TraceResult;

    fn ok_report() -> LayersReport {
        LayersReport {
            process_alive: true,
            control_connected: true,
            data_plane: Some(DataPlaneReport {
                trace_v4: Some(TraceResult {
                    ip: Some("104.28.1.2".to_string()),
                    colo: Some("LAX".to_string()),
                    warp: Some("on".to_string()),
                }),
                trace_v6: Some(TraceResult {
                    ip: Some("2a09:bac5::1".to_string()),
                    colo: Some("LAX".to_string()),
                    warp: Some("on".to_string()),
                }),
                latency_ms: 38,
            }),
        }
    }

    fn counters(failures: u32, successes: u32) -> HealthCounters {
        HealthCounters {
            consecutive_failures: failures,
            consecutive_successes: successes,
        }
    }

    #[test]
    fn healthy_on_first_good_probe() {
        // 启动完成即 Healthy（on_started），首轮探测成功保持 Healthy。
        let (v, c) = assess_layers(
            &ok_report(),
            &counters(0, 0),
            &HealthConfig::default(),
            RuntimeState::Healthy,
        );
        assert_eq!(v, HealthVerdict::Healthy);
        assert_eq!(c, counters(0, 1));
    }

    #[test]
    fn single_failure_is_degraded() {
        let bad = LayersReport {
            data_plane: None,
            ..ok_report()
        };
        let (v, c) = assess_layers(
            &bad,
            &counters(0, 0),
            &HealthConfig::default(),
            RuntimeState::Healthy,
        );
        assert_eq!(v, HealthVerdict::Degraded);
        assert_eq!(c, counters(1, 0));
    }

    #[test]
    fn repeated_failures_reach_failed_at_threshold() {
        let bad = LayersReport {
            control_connected: false,
            ..ok_report()
        };
        let cfg = HealthConfig::default();
        let (v1, c1) = assess_layers(&bad, &counters(0, 0), &cfg, RuntimeState::Healthy);
        assert_eq!(v1, HealthVerdict::Degraded);
        let (v2, c2) = assess_layers(&bad, &c1, &cfg, RuntimeState::Degraded);
        assert_eq!(v2, HealthVerdict::Degraded);
        let (v3, c3) = assess_layers(&bad, &c2, &cfg, RuntimeState::Degraded);
        assert_eq!(v3, HealthVerdict::Failed);
        assert_eq!(c3, counters(3, 0));
    }

    #[test]
    fn recovery_requires_consecutive_successes() {
        let cfg = HealthConfig::default();
        // 两次失败 → Degraded。
        let bad = LayersReport {
            data_plane: None,
            ..ok_report()
        };
        let (_, c) = assess_layers(&bad, &counters(0, 0), &cfg, RuntimeState::Healthy);
        let (_, c) = assess_layers(&bad, &c, &cfg, RuntimeState::Degraded);

        // 第一次恢复探测：仍 Degraded（未满 2 次）。
        let (v1, c1) = assess_layers(&ok_report(), &c, &cfg, RuntimeState::Degraded);
        assert_eq!(v1, HealthVerdict::Degraded);
        // 第二次恢复探测：Healthy。
        let (v2, c2) = assess_layers(&ok_report(), &c1, &cfg, RuntimeState::Degraded);
        assert_eq!(v2, HealthVerdict::Healthy);
        assert_eq!(c2, counters(0, 2));
    }

    #[test]
    fn warp_off_counts_as_failure() {
        let off = LayersReport {
            data_plane: Some(DataPlaneReport {
                trace_v4: Some(TraceResult {
                    ip: Some("8.8.8.8".to_string()),
                    colo: Some("DFW".to_string()),
                    warp: Some("off".to_string()),
                }),
                trace_v6: Some(TraceResult {
                    ip: Some("2a09:bac5::2".to_string()),
                    colo: Some("DFW".to_string()),
                    warp: Some("off".to_string()),
                }),
                latency_ms: 5,
            }),
            ..ok_report()
        };
        let (v, c) = assess_layers(
            &off,
            &counters(0, 0),
            &HealthConfig::default(),
            RuntimeState::Healthy,
        );
        assert_eq!(v, HealthVerdict::Degraded);
        assert_eq!(c, counters(1, 0));
    }

    #[test]
    fn single_family_reports_healthy_when_other_fails() {
        // v4 正常 + v6 探测失败（None）→ 数据面仍健康（P13-001 语义）。
        let single = LayersReport {
            data_plane: Some(DataPlaneReport {
                trace_v4: Some(TraceResult {
                    ip: Some("104.28.1.2".to_string()),
                    colo: Some("LAX".to_string()),
                    warp: Some("on".to_string()),
                }),
                trace_v6: None,
                latency_ms: 20,
            }),
            ..ok_report()
        };
        let (v, c) = assess_layers(
            &single,
            &counters(0, 0),
            &HealthConfig::default(),
            RuntimeState::Healthy,
        );
        assert_eq!(v, HealthVerdict::Healthy);
        assert_eq!(c, counters(0, 1));
    }

    #[test]
    fn missing_warp_field_counts_as_failure() {
        let missing = LayersReport {
            data_plane: Some(DataPlaneReport {
                trace_v4: Some(TraceResult {
                    ip: Some("1.2.3.4".to_string()),
                    colo: None,
                    warp: None,
                }),
                trace_v6: None,
                latency_ms: 9,
            }),
            ..ok_report()
        };
        let (v, _) = assess_layers(
            &missing,
            &counters(0, 0),
            &HealthConfig::default(),
            RuntimeState::Healthy,
        );
        assert_eq!(v, HealthVerdict::Degraded);
    }

    #[test]
    fn disconnected_control_plane_is_failure_even_if_data_ok() {
        let cfg = HealthConfig::default();
        let bad = LayersReport {
            process_alive: true,
            control_connected: false,
            data_plane: None,
        };
        let (v, _) = assess_layers(&bad, &counters(0, 0), &cfg, RuntimeState::Healthy);
        assert_eq!(v, HealthVerdict::Degraded);
    }

    #[test]
    fn dead_process_is_immediate_failed() {
        let dead = LayersReport {
            process_alive: false,
            control_connected: false,
            data_plane: None,
        };
        let (v, c) = assess_layers(
            &dead,
            &counters(0, 0),
            &HealthConfig::default(),
            RuntimeState::Healthy,
        );
        assert_eq!(v, HealthVerdict::Failed);
        assert_eq!(c, counters(1, 0));
    }

    #[test]
    fn success_wipes_failure_history() {
        let cfg = HealthConfig::default();
        let bad = LayersReport {
            data_plane: None,
            ..ok_report()
        };
        let (_, c) = assess_layers(&bad, &counters(1, 2), &cfg, RuntimeState::Degraded);
        assert_eq!(c, counters(2, 0));
        // 3 次失败后 Failed；随后一成功轮清零失败计数。
        let (_, c) = assess_layers(&bad, &c, &cfg, RuntimeState::Degraded);
        let (_, c) = assess_layers(&ok_report(), &c, &cfg, RuntimeState::Degraded);
        assert_eq!(c, counters(0, 1));
    }

    #[test]
    fn healthy_instance_stays_healthy_without_recovery_lag() {
        let cfg = HealthConfig::default();
        // 持续成功的 Healthy 实例不受恢复阈值延迟影响。
        let (v, c) = assess_layers(&ok_report(), &counters(0, 5), &cfg, RuntimeState::Healthy);
        assert_eq!(v, HealthVerdict::Healthy);
        assert_eq!(c, counters(0, 6));
    }

    #[test]
    fn verifies_result_maps_to_runtime_states() {
        assert_eq!(
            HealthVerdict::Healthy.as_runtime_state(),
            RuntimeState::Healthy
        );
        assert_eq!(
            HealthVerdict::Degraded.as_runtime_state(),
            RuntimeState::Degraded
        );
        assert_eq!(
            HealthVerdict::Failed.as_runtime_state(),
            RuntimeState::Failed
        );
    }

    #[test]
    fn latency_and_ip_are_carried_in_report() {
        let r = ok_report();
        let ip_v4 = r.data_plane.as_ref().and_then(|d| d.exit_ip_v4()).unwrap();
        let ip_v6 = r.data_plane.as_ref().and_then(|d| d.exit_ip_v6()).unwrap();
        assert!(ip_v4.is_ipv4());
        assert!(ip_v6.is_ipv6());
        assert_eq!(r.data_plane.as_ref().unwrap().latency_ms, 38);
        assert_eq!(
            r.data_plane.as_ref().unwrap().colo().as_deref(),
            Some("LAX")
        );
    }
}
