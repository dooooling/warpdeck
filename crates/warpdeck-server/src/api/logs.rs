//! 日志历史 API（P10-006）。
//!
//! - `GET /api/v1/logs/sources`：可用日志源枚举（manager / gost / 已有实例）。
//! - `GET /api/v1/logs?source=&limit=&offset=`：tail 分页读取历史行。
//!
//! 脱敏（DESIGN §25.11 / §27.2）：instance/gost 是非结构化进程输出，
//! 行经中心 redactor 整行 scrub；manager 行为结构化 tracing（字段级
//! `Sensitive` 已在日志点保证），原样返回。

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::error::ApiError;
use crate::api::{ApiResult, ApiState};
use crate::observability::RequestId;
use crate::runtime::logs::{self, LogSource};

/// 历史分页大小上限（与 tail 读取器 clamp 对齐）。
const MAX_PAGE_SIZE: usize = 500;

/// 日志源视图（sources 端点）。
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LogSourceView {
    /// 稳定 id：`manager` `gost` `instance:{id}`。
    pub source: String,
    /// 类别：`manager` / `gost` / `instance`。
    pub kind: &'static str,
    /// 实例 id（仅 kind=instance）。
    pub instance_id: Option<i64>,
    /// 该日志文件当前是否存在（fresh 系统可能尚未产生）。
    pub exists: bool,
}

/// `GET /api/v1/logs/sources`。
pub async fn sources(State(state): State<ApiState>) -> ApiResult<Json<Vec<LogSourceView>>> {
    let entries = logs::enumerate_sources(&state.data_dir);
    let views = entries
        .into_iter()
        .map(|entry| {
            let (kind, instance_id) = match &entry.source {
                LogSource::Manager => ("manager", None),
                LogSource::Gost => ("gost", None),
                LogSource::Instance(id) => ("instance", Some(id.as_i64())),
            };
            LogSourceView {
                source: entry.source.id(),
                kind,
                instance_id,
                exists: entry.path.exists(),
            }
        })
        .collect();
    Ok(Json(views))
}

/// 历史页查询参数。
#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    /// 日志源 id（`manager` / `gost` / `instance:{id}`）。
    pub source: String,
    /// 每页行数（默认 200，上限 500）。
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// 从最新向旧翻页的偏移（0 = 最新一页）。
    #[serde(default)]
    pub offset: u64,
}

fn default_limit() -> usize {
    200
}

/// 历史页响应。
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistoryResponse {
    pub source: String,
    pub offset: u64,
    pub next_offset: u64,
    pub has_more: bool,
    pub lines: Vec<String>,
}

/// 解析源 id 并定位日志文件（未知源 → Validation）。
fn resolve_source(state: &ApiState, id: &str) -> Result<std::path::PathBuf, ApiError> {
    let source = LogSource::parse(id)
        .ok_or_else(|| ApiError::Validation(format!("unknown log source: {id}")))?;
    let dir = logs::logs_dir(&state.data_dir);
    Ok(match source {
        LogSource::Manager => dir.join("manager.log"),
        LogSource::Gost => dir.join("gost.stderr.log"),
        LogSource::Instance(instance_id) => {
            dir.join(format!("instance-{}.log", instance_id.as_i64()))
        }
    })
}

/// `GET /api/v1/logs?source=&limit=&offset=`（P10-006 tail 分页）。
pub async fn history(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<Json<HistoryResponse>> {
    let limit = query.limit.clamp(1, MAX_PAGE_SIZE);
    let path =
        resolve_source(&state, &query.source).map_err(|e| e.into_response_with(&request_id))?;

    let page = logs::read_tail(&path, limit, query.offset).map_err(|e| {
        tracing::warn!(
            component = "logs_api",
            source = %query.source,
            error = %e,
            "log tail read failed"
        );
        ApiError::Internal("log read failed".to_string()).into_response_with(&request_id)
    })?;

    // 脱敏策略：非结构化进程输出整行 scrub；manager 行原样（结构化已字段级脱敏）。
    let redact = query.source != "manager";
    let lines = page
        .lines
        .into_iter()
        .map(|text| {
            if redact {
                logs::redact_line(&text)
            } else {
                text
            }
        })
        .collect();

    let next_offset = query.offset + 1;
    Ok(Json(HistoryResponse {
        source: query.source,
        offset: query.offset,
        next_offset,
        has_more: page.has_more,
        lines,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_redacts_non_manager_lines_only() {
        // 模式化脱敏（P1 审查 R2）：敏感键值替换、其余内容保留。
        assert_eq!(
            logs::redact_line("registration token abc"),
            "registration token [REDACTED]"
        );
        assert_eq!(logs::redact_line(""), "");
    }

    #[test]
    fn query_defaults() {
        let q: HistoryQuery =
            serde_json::from_value(serde_json::json!({ "source": "manager" })).unwrap();
        assert_eq!(q.limit, 200);
        assert_eq!(q.offset, 0);
    }

    #[tokio::test]
    async fn resolve_source_maps_known_ids() {
        let app = crate::app::TestApp::new_unauthenticated().await;
        let state = app.state_for_test();
        let dir = app.data_dir_for_test();
        let manager = resolve_source(&state, "manager").unwrap();
        assert_eq!(manager, dir.join("logs/manager.log"));
        let instance = resolve_source(&state, "instance:3").unwrap();
        assert_eq!(instance, dir.join("logs/instance-3.log"));
        let err = resolve_source(&state, "bogus").unwrap_err();
        assert_eq!(err.code(), "VALIDATION");
    }
}
