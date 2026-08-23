//! 日志源与实时日志总线（P10-005/007）。
//!
//! 三类日志源（DESIGN §8.1 `{data_dir}/logs/`）：
//! - `manager.log`：manager 自身 tracing 输出（`observability::RedactFileLayer`）
//! - `instance-{id}.log`：warp-svc 进程 stdout/stderr（`SpawnCommand` 重定向）
//! - `gost.stderr.log`：GOST 进程 stderr（重定向；文件名随 P5 现状保留）
//!
//! 实时行经 `LogBus`（broadcast）推给 SSE；发布与历史读取都过中心
//! redactor（P8，绝不泄漏 secret）。慢订阅者 lagged 丢行可接受（历史在文件）。

use std::path::{Path, PathBuf};

use tokio::sync::broadcast;

use crate::observability::redactor;

use super::instance::InstanceId;

/// 一条实时日志行（日志总线负载）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// 所属日志源。
    pub source: LogSource,
    /// 数字序号（同一 source 内递增；UI 去重/排序）。
    pub seq: u64,
    /// 已经中心 redactor 过滤后的行文本。
    pub line: String,
}

/// 日志源标识（对外字符串契约：`manager` `gost` `instance:{id}`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogSource {
    Manager,
    Gost,
    Instance(InstanceId),
}

impl LogSource {
    /// 对外稳定 id。
    pub fn id(&self) -> String {
        match self {
            LogSource::Manager => "manager".to_string(),
            LogSource::Gost => "gost".to_string(),
            LogSource::Instance(id) => format!("instance:{}", id.as_i64()),
        }
    }

    /// SSE `resource_id`（与 id 相同；独立函数保持语义对称）。
    pub fn resource_id(&self) -> String {
        self.id()
    }

    /// 日志文件名（DESIGN §8.1 布局）。
    pub fn file_name(&self) -> &'static str {
        match self {
            LogSource::Manager => "manager.log",
            LogSource::Gost => "gost.stderr.log",
            LogSource::Instance(_) => "instance-{id}.log",
        }
    }

    /// 从文件路径推断源（用于扫描 logs 目录）——返回 None 表示不认识该文件。
    pub fn from_file_name(name: &str) -> Option<LogSource> {
        match name {
            "manager.log" => Some(LogSource::Manager),
            "gost.stderr.log" | "gost.log" => Some(LogSource::Gost),
            other => {
                let rest = other.strip_prefix("instance-")?.strip_suffix(".log")?;
                let id = rest.parse::<i64>().ok()?;
                InstanceId::from_db(id).ok().map(LogSource::Instance)
            }
        }
    }

    /// 解析对外字符串 id（sources API / 过滤参数）。
    pub fn parse(id: &str) -> Option<LogSource> {
        match id {
            "manager" => Some(LogSource::Manager),
            "gost" => Some(LogSource::Gost),
            other => {
                let rest = other.strip_prefix("instance:")?;
                let id = rest.parse::<i64>().ok()?;
                InstanceId::from_db(id).ok().map(LogSource::Instance)
            }
        }
    }
}

/// `{data_dir}/logs` 目录。
pub fn logs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

/// 解析后的源+文件路径对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSourceFile {
    pub source: LogSource,
    pub path: PathBuf,
}

impl LogSourceFile {
    pub fn file_name_for(&self) -> String {
        match &self.source {
            LogSource::Instance(id) => format!("instance-{}.log", id.as_i64()),
            other => other.file_name().to_string(),
        }
    }
}

/// 枚举可用日志源：固定 manager/gost + 磁盘上已存在的 instance-*.log。
pub fn enumerate_sources(data_dir: &Path) -> Vec<LogSourceFile> {
    let dir = logs_dir(data_dir);
    let mut out = vec![
        LogSourceFile {
            source: LogSource::Manager,
            path: dir.join("manager.log"),
        },
        LogSourceFile {
            source: LogSource::Gost,
            path: dir.join("gost.stderr.log"),
        },
    ];
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut instances: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let source = LogSource::from_file_name(&name)?;
                match source {
                    LogSource::Instance(_) => Some((source, e.path())),
                    _ => None,
                }
            })
            .collect();
        instances.sort_by_key(|(source, _)| source.id());
        out.extend(
            instances
                .into_iter()
                .map(|(source, path)| LogSourceFile { source, path }),
        );
    }
    out
}

/// 实时日志总线：`publish` 已 redact（中心 redactor，P8）。
#[derive(Debug, Clone)]
pub struct LogBus {
    tx: broadcast::Sender<LogLine>,
}

impl Default for LogBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl LogBus {
    /// 端口缓冲 1024 行：慢订阅者最多掉一屏，历史仍可从文件读。
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// 发布已 redact 的行；lagged 静默丢弃（实时视图可丢）。
    pub fn publish(&self, line: LogLine) {
        if let Err(err) = self.tx.send(line) {
            tracing::debug!(component = "log_bus", error = %err, "log line dropped");
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }
}

/// 单条日志行经中心 redactor 过滤（历史 API 与实时发布共同入口）。
/// 模式化片段脱敏（P1 审查 R2：整行 [REDACTED] 无诊断价值，已废弃）。
pub fn redact_line(line: &str) -> String {
    redactor::scrub_line(line)
}

/// 一页 tail 读取结果（行文本按文件顺序，旧→新）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailPage {
    pub lines: Vec<String>,
    /// 是否还有更早的行（向后翻页）。
    pub has_more: bool,
}

/// 从文件尾部反向读取最多 `limit` 行，跳过之前的 `offset*limit` 行（分页）。
///
/// P10-006：大文件只读尾部区块（`TAIL_CHUNK_BYTES` 粒度），避免一次读完整文件；
/// 行内容保持文件顺序（旧→新）。offset=0 为最新一页；文件不存在返回空页。
pub fn read_tail(path: &std::path::Path, limit: usize, offset: u64) -> std::io::Result<TailPage> {
    use std::io::ErrorKind;

    match read_tail_once(path, limit, offset) {
        // 并发截断（实例重启/日志轮转恰在读取窗口内）：read_exact 报 UnexpectedEof。
        // 文件已在变短——重试一次拿到当时快照；仍失败则按已消失处理（空页），
        // 不得让分页 API 以 500 炸出（review）。
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => {
            match read_tail_once(path, limit, offset) {
                Ok(page) => Ok(page),
                Err(_) => Ok(TailPage {
                    lines: vec![],
                    has_more: false,
                }),
            }
        }
        result => result,
    }
}

fn read_tail_once(path: &std::path::Path, limit: usize, offset: u64) -> std::io::Result<TailPage> {
    use std::io::{Read, Seek as _, SeekFrom};

    let limit = limit.clamp(1, 500);
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TailPage {
                lines: vec![],
                has_more: false,
            });
        }
        Err(err) => return Err(err),
    };
    let mut len = file.metadata()?.len();
    let skip = offset.saturating_mul(limit as u64) as usize;
    // 需要从尾部收集行数：当前页 limit 行 + 之前跳过的 skip 行，另 +1 判定 has_more。
    let collect = skip + limit + 1;

    // 从末尾向前分块读；raw 保持文件顺序（旧→新），只保留完整行。
    let mut raw = Vec::new();
    loop {
        if len == 0 {
            break;
        }
        let read_from = len.saturating_sub(TAIL_CHUNK_BYTES);
        let mut chunk = vec![0u8; (len - read_from) as usize];
        file.seek(SeekFrom::Start(read_from))?;
        file.read_exact(&mut chunk)?;
        raw.splice(0..0, chunk.iter().copied());
        // 非文件头位置：丢弃块开头的半行，保证 raw 从完整行开始。
        if read_from > 0 {
            if let Some(pos) = raw.iter().position(|b| *b == b'\n') {
                raw.drain(0..=pos);
            }
        }
        if read_from == 0 || raw.iter().filter(|b| **b == b'\n').count() as u64 >= collect as u64 {
            break;
        }
        len = read_from;
    }

    let lines: Vec<String> = raw
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect();
    let total = lines.len();
    // 当前页 = 文件末尾第 skip 行起、往前 limit 行（旧→新序）。
    let end = total.saturating_sub(skip);
    let start = end.saturating_sub(limit);
    Ok(TailPage {
        lines: lines[start..end].to_vec(),
        // 窗口起点 > 0 ⟺ 还有更早的行可向后翻。
        has_more: start > 0,
    })
}

/// tail 读取的尾部块大小（P10-006：大文件不整读）。
pub const TAIL_CHUNK_BYTES: u64 = 32 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_source_ids_are_stable() {
        assert_eq!(LogSource::Manager.id(), "manager");
        assert_eq!(LogSource::Gost.id(), "gost");
        assert_eq!(
            LogSource::Instance(InstanceId::from_db(0).unwrap()).id(),
            "instance:0"
        );
    }

    #[test]
    fn log_source_parse_roundtrip() {
        for s in [
            LogSource::Manager,
            LogSource::Gost,
            LogSource::Instance(InstanceId::from_db(12).unwrap()),
        ] {
            assert_eq!(LogSource::parse(&s.id()), Some(s));
        }
        assert_eq!(LogSource::parse("instance:-1"), None);
        assert_eq!(LogSource::parse("nope"), None);
    }

    #[test]
    fn from_file_name_maps_instance_files() {
        assert_eq!(
            LogSource::from_file_name("instance-3.log"),
            Some(LogSource::Instance(InstanceId::from_db(3).unwrap()))
        );
        assert_eq!(LogSource::from_file_name("instance-3.log.txt"), None);
        assert_eq!(
            LogSource::from_file_name("manager.log"),
            Some(LogSource::Manager)
        );
    }

    #[test]
    fn enumerate_sources_finds_existing_instance_files() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("instance-1.log"), "x").unwrap();
        std::fs::write(logs.join("instance-10.log"), "x").unwrap();
        let sources = enumerate_sources(dir.path());
        let ids: Vec<_> = sources.iter().map(|s| s.source.id()).collect();
        assert_eq!(ids, vec!["manager", "gost", "instance:1", "instance:10"]);
        assert_eq!(sources[2].path, logs.join("instance-1.log"));
    }

    #[test]
    fn log_bus_delivers_redacted_pub_sub() {
        let bus = LogBus::default();
        let mut rx = bus.subscribe();
        bus.publish(LogLine {
            source: LogSource::Gost,
            seq: 1,
            line: "hi".into(),
        });
        let got = rx.try_recv().unwrap();
        assert_eq!(got.source, LogSource::Gost);
        assert_eq!(got.seq, 1);
        assert_eq!(got.line, "hi");
    }

    fn write_lines(dir: &tempfile::TempDir, n: usize) -> std::path::PathBuf {
        let p = dir.path().join("sample.log");
        let content: String = (1..=n).map(|i| format!("line-{i}\n")).collect();
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn read_tail_small_file_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_lines(&dir, 5);
        // 最新页（limit 2）：line-4, line-5。
        let page0 = read_tail(&p, 2, 0).unwrap();
        assert_eq!(page0.lines, vec!["line-4", "line-5"]);
        assert!(page0.has_more);
        // 上一页。
        let page1 = read_tail(&p, 2, 1).unwrap();
        assert_eq!(page1.lines, vec!["line-2", "line-3"]);
        assert!(page1.has_more);
        // 最早一页。
        let page2 = read_tail(&p, 2, 2).unwrap();
        assert_eq!(page2.lines, vec!["line-1"]);
        assert!(!page2.has_more);
    }

    #[test]
    fn read_tail_handles_empty_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.log");
        std::fs::write(&empty, "").unwrap();
        let page = read_tail(&empty, 10, 0).unwrap();
        assert!(page.lines.is_empty());
        assert!(!page.has_more);

        // 文件不存在 → 空页（上游 API 对新鲜系统返回空集合）。
        let missing = read_tail(&dir.path().join("nope.log"), 10, 0).unwrap();
        assert!(missing.lines.is_empty());
        assert!(!missing.has_more);
    }

    #[test]
    fn read_tail_no_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("no-nl.log");
        std::fs::write(&p, "a\nb\nc").unwrap();
        let page = read_tail(&p, 10, 0).unwrap();
        assert_eq!(page.lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn read_tail_large_file_reads_only_tail_blocks() {
        // 多块文件：验证尾部页正确且不整读（内部由块大小保证）。
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.log");
        let mut content = String::new();
        for i in 0..(TAIL_CHUNK_BYTES * 4) {
            content.push_str(&format!("{i:08}\n"));
        }
        std::fs::write(&p, content).unwrap();
        let page = read_tail(&p, 3, 0).unwrap();
        assert_eq!(page.lines.len(), 3);
        assert!(page.has_more);
        assert!(
            page.lines[2].starts_with("3")
                || page.lines[2] == format!("{:08}", TAIL_CHUNK_BYTES * 4 - 1)
        );
    }
}
