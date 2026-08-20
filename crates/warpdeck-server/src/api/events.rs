//! SSE 事件流（P7-009 / P10-002/003/007）。
//!
//! `GET /api/v1/events`：把内部 `EventBus`（P4-008 状态/健康帧）与
//! `LogBus`（P10-007 实时日志行）合并成一条 SSE 流推给浏览器。
//!
//! 帧契约（P10-003）——所有 payload 统一包裹，字段稳定：
//!
//! ```json
//! {
//!   "type": "instance.state_changed",
//!   "version": 1,
//!   "timestamp": "2026-08-18T11:50:11.123456Z",
//!   "resource_id": "instance:7",
//!   "data": { ... }
//! }
//! ```
//!
//! 只暴露公开字段，绝不携带 secret；日志行已过中心 redactor（P8）。
//! 事件是幂等快照/可丢行（broadcast 语义，backpressure：DESIGN §25.9 / P10-008）。

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::{select, Stream};
use futures_util::StreamExt as _;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::broadcast;

use crate::api::ApiState;
use crate::observability::RequestId;
use crate::runtime::events::HealthEvent;
use crate::runtime::logs::LogLine;
use crate::runtime::registry::RuntimeState;

/// 事件契约版本（P10-003；payload 破坏性变更时递增）。
const EVENT_VERSION: u64 = 1;

/// UTC ISO-8601 时间戳（Rfc3339）。
fn timestamp_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// 统一事件包裹层（对外契约，所有帧共用）。
fn envelope(event_type: &str, resource_id: String, data: Value) -> Value {
    json!({
        "type": event_type,
        "version": EVENT_VERSION,
        "timestamp": timestamp_now(),
        "resource_id": resource_id,
        "data": data,
    })
}

/// 状态迁移帧 data（纯函数：测试直接断言 JSON 契约，Event 无 data getter）。
fn transition_data(instance_id: i64, from: RuntimeState, to: RuntimeState, reason: &str) -> Value {
    json!({
        "instance_id": instance_id,
        "from": from.as_str(),
        "to": to.as_str(),
        "reason": reason,
    })
}

/// 出口 IP 变化帧 data（双地址族，P13-001）。
fn exit_ip_data(
    instance_id: i64,
    exit_ip_v4: Option<&str>,
    exit_ip_v6: Option<&str>,
    colo: Option<&str>,
    latency_ms: Option<u32>,
) -> Value {
    json!({
        "instance_id": instance_id,
        "exit_ip_v4": exit_ip_v4,
        "exit_ip_v6": exit_ip_v6,
        "colo": colo,
        "latency_ms": latency_ms,
    })
}

/// 帧内容纯函数：SSE 帧名 + 事件类型 + resource_id + data（测试直接断言）。
fn frame_parts(event: HealthEvent) -> (&'static str, &'static str, String, Value) {
    match event {
        HealthEvent::StateChanged(t) => (
            "state_changed",
            "instance.state_changed",
            format!("instance:{}", t.instance_id.as_i64()),
            transition_data(t.instance_id.as_i64(), t.from, t.to, &t.reason),
        ),
        HealthEvent::HealthChanged(t) => (
            "health_changed",
            "instance.health_changed",
            format!("instance:{}", t.instance_id.as_i64()),
            transition_data(t.instance_id.as_i64(), t.from, t.to, &t.reason),
        ),
        HealthEvent::ExitIpChanged {
            instance_id,
            exit_ip_v4,
            exit_ip_v6,
            colo,
            latency_ms,
        } => (
            "exit_ip_changed",
            "instance.exit_ip_changed",
            format!("instance:{}", instance_id.as_i64()),
            exit_ip_data(
                instance_id.as_i64(),
                exit_ip_v4.as_deref(),
                exit_ip_v6.as_deref(),
                colo.as_deref(),
                latency_ms,
            ),
        ),
    }
}

/// SSE 事件（帧名 + 包裹 payload）。
fn to_sse_event(event: HealthEvent) -> Event {
    let (frame, event_type, resource_id, data) = frame_parts(event);
    Event::default()
        .event(frame)
        .json_data(envelope(event_type, resource_id, data))
        .unwrap_or_default()
}

/// 实时日志行帧内容（纯函数）。
fn log_frame_parts(line: LogLine) -> (&'static str, String, Value) {
    let data = json!({
        "source": line.source.id(),
        "seq": line.seq,
        "line": line.line,
    });
    ("log.line", line.source.resource_id(), data)
}

/// 实时日志行帧。
fn to_log_sse_event(line: LogLine) -> Event {
    let (frame, resource_id, data) = log_frame_parts(line);
    Event::default()
        .event(frame)
        .json_data(envelope("log.line", resource_id, data))
        .unwrap_or_default()
}

/// 从 broadcast receiver 构造 SSE 帧流：lagged 跳过（快照/可丢行），
/// 通道关闭（总线 drop）即断开。
fn stream<Item>(rx: broadcast::Receiver<Item>) -> impl Stream<Item = Item> + Send
where
    Item: Clone + Send + 'static,
{
    futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(item) => return Some((item, rx)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

/// `GET /api/v1/events`：状态/健康帧 + 实时日志行合并流（keep-alive 15s）。
pub async fn subscribe(
    State(state): State<ApiState>,
    RequestId(_rid): RequestId,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send> {
    let health = stream(state.bus.subscribe())
        .map(Ok::<_, Infallible>)
        .map(|r| r.map(to_sse_event));
    let logs = stream(state.log_bus.subscribe())
        .map(Ok::<_, Infallible>)
        .map(|r| r.map(to_log_sse_event));
    Sse::new(select(health, logs)).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keep-alive"),
    )
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;

    use super::*;
    use crate::runtime::events::{EventBus, StateTransition};
    use crate::runtime::instance::InstanceId;
    use crate::runtime::logs::LogSource;

    fn id(v: i64) -> InstanceId {
        InstanceId::from_db(v).unwrap()
    }

    #[test]
    fn state_changed_payload_is_enveloped_view_only() {
        let json = envelope(
            "instance.state_changed",
            "instance:7".to_string(),
            transition_data(
                id(7).as_i64(),
                RuntimeState::Stopped,
                RuntimeState::Healthy,
                "start",
            ),
        );
        assert_eq!(json["type"], "instance.state_changed");
        assert_eq!(json["version"], 1);
        assert!(json["timestamp"].as_str().unwrap().ends_with("Z"));
        assert_eq!(json["resource_id"], "instance:7");
        let data = &json["data"];
        assert_eq!(data["instance_id"], 7);
        assert_eq!(data["from"], "stopped");
        assert_eq!(data["to"], "healthy");
        assert_eq!(data["reason"], "start");
        // 契约顶层字段稳定：type/version/timestamp/resource_id/data（顺序无关）。
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["data", "resource_id", "timestamp", "type", "version"]
        );
        // data 不携带 secret 等额外字段（秘密边界）。
        assert_eq!(data.as_object().unwrap().len(), 4);
    }

    #[test]
    fn exit_ip_changed_payload_has_ip_fields() {
        let event = HealthEvent::ExitIpChanged {
            instance_id: id(3),
            exit_ip_v4: Some("104.28.1.2".into()),
            exit_ip_v6: Some("2a09:bac5::1".into()),
            colo: Some("LAX".into()),
            latency_ms: Some(38),
        };
        let (frame, event_type, resource, data) = frame_parts(event);
        assert_eq!(frame, "exit_ip_changed");
        assert_eq!(event_type, "instance.exit_ip_changed");
        assert_eq!(resource, "instance:3");
        let payload = envelope(event_type, resource, data);
        assert_eq!(payload["data"]["exit_ip_v4"], "104.28.1.2");
        assert_eq!(payload["data"]["exit_ip_v6"], "2a09:bac5::1");
        assert_eq!(payload["data"]["colo"], "LAX");
        assert_eq!(payload["data"]["latency_ms"], 38);
    }

    #[tokio::test]
    async fn stream_emits_event_then_ends_on_close() {
        let bus = EventBus::default();
        let rx = bus.subscribe();
        bus.publish(HealthEvent::StateChanged(StateTransition {
            instance_id: id(1),
            from: RuntimeState::Stopped,
            to: RuntimeState::Healthy,
            reason: "start".into(),
        }));
        drop(bus);
        let mut s = Box::pin(stream(rx));
        let first = s.next().await.expect("first frame");
        assert!(matches!(first, HealthEvent::StateChanged(_)));
        assert!(s.next().await.is_none(), "stream must end after bus close");
    }

    #[tokio::test]
    async fn log_frame_carries_source_and_redacted_line() {
        let line = LogLine {
            source: LogSource::Instance(id(0)),
            seq: 42,
            line: "hello warp".into(),
        };
        let (frame, resource_id, data) = log_frame_parts(line.clone());
        let payload = envelope(frame, resource_id, data);
        assert_eq!(payload["type"], "log.line");
        assert_eq!(payload["resource_id"], "instance:0");
        assert_eq!(payload["data"]["source"], "instance:0");
        assert_eq!(payload["data"]["seq"], 42);
        assert_eq!(payload["data"]["line"], "hello warp");
        assert_eq!(line.line, "hello warp");
    }
}
