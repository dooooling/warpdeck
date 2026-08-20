//! 实时日志 tail watcher（P10-007）。
//!
//! 对每个日志源（manager / gost / 现存实例）独立任务：
//! - 从文件**末尾**开始跟随（只推送新行，历史走 History API）；
//! - 文件被截断（进程重启 `File::create`）→ 从头重跟；
//! - 文件被删除 → 内循环退出、按发现周期重试（实例日志随实例创建出现；
//!   监督任务按周期重新枚举源并补缺）；
//! - 每行先过中心 redactor（instance/gost 整行 scrub；manager 原样，
//!   结构化字段级已由 `Sensitive` 保证，DESIGN §27.2）；
//! - 发布到 `LogBus`（broadcast 1024，慢订阅者 lagged 丢行，P10-008）。
//!
//! `disconnect cleanup`：SSE 断线即 drop receiver（broadcast 语义自动清理）。

use std::collections::HashMap;
use std::io::{Read, Seek as _, SeekFrom};
use std::path::Path;
use std::time::Duration;

use tokio::task::JoinHandle;

use super::logs::{enumerate_sources, redact_line, LogBus, LogLine, LogSource, LogSourceFile};

/// 文件身份（Windows delete-pending 句柄仍可访问旧文件，需按身份检测重建）。
#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.ino()
}

/// Windows：file_index 不稳定，用 creation_time（重建文件必然不同）。
#[cfg(windows)]
fn file_identity(meta: &std::fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt as _;
    meta.creation_time()
}

/// 轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// 新源码发现间隔（实例日志文件随实例启动出现）。
const DISCOVER_INTERVAL: Duration = Duration::from_secs(5);
/// 单轮最多读取字节（防大块读阻塞其它任务）。
const READ_BATCH_BYTES: u64 = 64 * 1024;
/// 无换行的 pending 缓冲上限（异常长行保护）。
const PENDING_MAX_BYTES: usize = 1024 * 1024;

/// 为 data_dir 下所有已知日志源启动 watcher（监督任务，进程生命周期）。
/// 返回监督任务句柄（drop 不 abort；与 main 同进退）。
pub fn spawn_tail_watchers(data_dir: &Path, bus: LogBus) -> JoinHandle<()> {
    spawn_tail_watchers_with_interval(data_dir, bus, DISCOVER_INTERVAL)
}

/// 可注入发现周期的版本（测试用短周期验证新源补缺）。
pub fn spawn_tail_watchers_with_interval(
    data_dir: &Path,
    bus: LogBus,
    discover_interval: Duration,
) -> JoinHandle<()> {
    let data_dir = data_dir.to_path_buf();
    tokio::spawn(async move {
        let mut workers: HashMap<String, JoinHandle<()>> = HashMap::new();
        loop {
            let sources = enumerate_sources(&data_dir);
            for entry in sources {
                workers.entry(entry.source.id()).or_insert_with(|| {
                    let bus = bus.clone();
                    tokio::spawn(tail_watcher_loop(entry, bus))
                });
            }
            tokio::time::sleep(discover_interval).await;
        }
    })
}

/// 单个源的跟随循环。
async fn tail_watcher_loop(entry: LogSourceFile, bus: LogBus) {
    let redact = !matches!(entry.source, LogSource::Manager);
    tracing::debug!(source = %entry.source.id(), "tail watcher started");
    // 首次 open：文件已存在则只跟尾部（历史不推，走 History API）。
    let mut first_open = true;
    loop {
        let mut file = match std::fs::OpenOptions::new().read(true).open(&entry.path) {
            Ok(file) => file,
            Err(err) => {
                tracing::debug!(source = %entry.source.id(), %err, "tail open retry");
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        tracing::debug!(source = %entry.source.id(), first_open, "tail file opened");
        // 重新 open（文件曾被删除/重建）＝新纪元：从 0 推全部新文件内容；
        // 仅首次 open 跳过既有内容。
        let mut pos = if first_open {
            first_open = false;
            file.metadata().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        let _ = file.seek(SeekFrom::Start(pos));
        // 上批读区间的末字节签名：截断重写（truncate+write 同窗口）检测。
        let mut prev_tail_byte: Option<u8> = None;
        // 当前句柄的文件身份（delete-pending / 重建检测）。
        let file_id = file.metadata().map(|m| file_identity(&m)).ok();
        let mut pending: Vec<u8> = Vec::new();
        let mut seq: u64 = 0;
        // Windows：被删除文件的句柄仍可用（delete-pending），旧句柄读旧数据；
        // 周期按路径复查身份，发现重建即重开（rescue check）。
        let mut rescue_ticks: u32 = 0;

        // 内循环：跟随存活文件。
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let Ok(meta) = file.metadata() else {
                // 文件被删除 → 外循环重试打开（新纪元）。
                tracing::debug!(source = %entry.source.id(), "tail file gone");
                break;
            };
            if let Some(prev_id) = file_id {
                // 文件身份变化（删除重建 / 替换）→ 重开新句柄（新纪元）。
                if file_identity(&meta) != prev_id {
                    tracing::debug!(source = %entry.source.id(), "tail file replaced");
                    break;
                }
                // 路径级复查：句柄指向的文件可能已被删除/替换而句柄不报错
                // （Windows delete-pending + file tunneling 会继承创建时间）。
                rescue_ticks += 1;
                if rescue_ticks >= 10 {
                    rescue_ticks = 0;
                    let path_ok = match std::fs::metadata(&entry.path) {
                        // 身份与长度双校验：tunneling 复现创建时间但内容/长度不同。
                        Ok(pm) => file_identity(&pm) == prev_id && pm.len() == meta.len(),
                        Err(_) => false,
                    };
                    if !path_ok {
                        tracing::debug!(source = %entry.source.id(), "tail rescue reopen");
                        break;
                    }
                }
            }
            let len = meta.len();
            if len < pos {
                // 截断（进程重启 / manager 10MB 保护）→ 从头重跟，seq 新纪元。
                tracing::debug!(source = %entry.source.id(), %len, %pos, "tail truncate detected");
                let _ = file.seek(SeekFrom::Start(0));
                pending.clear();
                seq = 0;
                prev_tail_byte = None;
                pos = 0;
                continue;
            }
            // 截断重写检测：探测上批**末字节**（pos-1）是否仍与原值一致。
            // pos 是**新数据首字节**：纯 append 时该字节必然是先前读过的不同值
            // （批尾 `\n`），探测 pos 会把每次跨轮询追加误判为 rewrite → 回卷重读
            // → SSE 实时流重复刷屏（review 发现）。pos-1 在纯 append 下不变；
            // 截断重写（长度不减小）时内容已换、字节必然不同。
            if let Some(prev) = prev_tail_byte {
                if pos > 0 {
                    let mut probe = [0u8; 1];
                    let ok = file
                        .seek(SeekFrom::Start(pos - 1))
                        .is_ok_and(|_| file.read_exact(&mut probe).is_ok());
                    if ok && probe[0] != prev {
                        // truncate + 写入发生在两次轮询之间（len 未变小）：新纪元。
                        tracing::debug!(source = %entry.source.id(), %pos, "tail rewrite detected");
                        let _ = file.seek(SeekFrom::Start(0));
                        pending.clear();
                        seq = 0;
                        prev_tail_byte = None;
                        pos = 0;
                        continue;
                    }
                }
            }
            if len == pos {
                continue;
            }
            let end = len.min(pos + READ_BATCH_BYTES);
            let mut buf = vec![0u8; (end - pos) as usize];
            if file.read_exact(&mut buf).is_err() {
                break;
            }
            pos = end;
            prev_tail_byte = buf.last().copied();
            pending.extend_from_slice(&buf);
            if pending.len() > PENDING_MAX_BYTES {
                // 异常无换行长行：丢弃保护（上限后按行边界裁剪）。
                if let Some(keep_from) = pending.iter().rposition(|b| *b == b'\n') {
                    pending.drain(..=keep_from);
                } else {
                    pending.clear();
                }
            }
            // 分裂完整行发布。
            while let Some(newline) = pending.iter().position(|b| *b == b'\n') {
                let raw: Vec<u8> = pending.drain(..=newline).collect();
                let text = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
                if text.trim().is_empty() {
                    continue;
                }
                seq += 1;
                let line = if redact { redact_line(&text) } else { text };
                tracing::debug!(source = %entry.source.id(), seq, "tail publish");
                bus.publish(LogLine {
                    source: entry.source.clone(),
                    seq,
                    line,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use tokio::sync::broadcast;

    use super::*;
    use crate::runtime::instance::InstanceId;

    /// 等待下一条 log line（带超时）。
    async fn next_line(rx: &mut broadcast::Receiver<LogLine>) -> Option<LogLine> {
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(line)) => Some(line),
            _ => None,
        }
    }

    /// 等待 worker 完成首轮打开（文件已存在 → pos=len 就位），
    /// 避免 append 竞态被当作历史跳过。
    async fn settle() {
        tokio::time::sleep(POLL_INTERVAL * 3).await;
    }

    #[tokio::test]
    async fn watcher_follows_appended_lines_and_redacts_process_sources() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();

        // instance 文件在 watcher 启动前存在（跟随尾部，历史不推）。
        let inst = logs.join("instance-0.log");
        std::fs::write(&inst, "existing before watch\n").unwrap();

        let bus = LogBus::default();
        let mut rx = bus.subscribe();
        let handle = spawn_tail_watchers(dir.path(), bus);
        settle().await;

        // 追加两行明文。
        std::fs::OpenOptions::new()
            .append(true)
            .open(&inst)
            .unwrap()
            .write_all(b"registration token abc\nwarp connected\n")
            .unwrap();

        let line1 = next_line(&mut rx).await.expect("first line");
        assert_eq!(
            line1.source,
            LogSource::Instance(InstanceId::from_db(0).unwrap())
        );
        assert_eq!(line1.seq, 1);
        // 进程行整行 scrub。
        assert_eq!(line1.line, "[REDACTED]");
        let line2 = next_line(&mut rx).await.expect("second line");
        assert_eq!(line2.seq, 2);
        assert_eq!(line2.line, "[REDACTED]");

        // manager 文件：原样（结构化字段级已脱敏）。
        // 文件先于 watcher 存在（真实场景 manager.log 随进程启动即建）。
        let mgr = logs.join("manager.log");
        std::fs::write(&mgr, "boot line\n").unwrap();
        tokio::time::sleep(POLL_INTERVAL * 3).await;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&mgr)
            .unwrap()
            .write_all(b"first\n")
            .unwrap();
        let line3 = next_line(&mut rx).await.expect("manager line");
        assert_eq!(line3.source, LogSource::Manager);
        assert_eq!(line3.line, "first");

        handle.abort();
    }

    #[tokio::test]
    async fn watcher_recovers_after_file_truncate_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let path = logs.join("instance-1.log");
        // 文件先于 watcher 存在。
        std::fs::write(&path, "seed\n").unwrap();

        let bus = LogBus::default();
        let mut rx = bus.subscribe();
        let handle = spawn_tail_watchers(dir.path(), bus);
        settle().await;

        // 初始 seed 不推（跟随尾部）；追加触发首行。
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"a\n")
            .unwrap();
        let l1 = next_line(&mut rx).await.expect("line before truncate");
        assert_eq!(l1.source.id(), "instance:1");
        assert_eq!(l1.line, "[REDACTED]");

        // 截断 → 从头重跟。
        std::fs::write(&path, "fresh start\n").unwrap();
        let l2 = next_line(&mut rx).await.expect("line after truncate");
        assert_eq!(l2.seq, 1, "seq restarts after truncate");
        assert_eq!(l2.line, "[REDACTED]");

        // 删除 → 重建 → 恢复跟随（Windows file tunneling 会继承创建时间，
        // rescue 用身份+长度双校验识别重建）。
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "reborn\n").unwrap();
        let l3 = next_line(&mut rx).await.expect("line after recreate");
        assert_eq!(l3.line, "[REDACTED]");

        handle.abort();
    }

    #[tokio::test]
    async fn watcher_skips_pre_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        // 文件在 watcher 启动前已存在大量内容：不应推送历史行。
        let path = logs.join("manager.log");
        let content: String = (0..50).map(|i| format!("old line {i}\n")).collect();
        std::fs::write(&path, &content).unwrap();

        let bus = LogBus::default();
        let mut rx = bus.subscribe();
        let handle = spawn_tail_watchers(dir.path(), bus);

        tokio::time::sleep(POLL_INTERVAL * 3).await;
        assert!(
            rx.try_recv().is_err(),
            "no historical lines should be pushed"
        );

        // 追加后开始推送。
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"new line\n")
            .unwrap();
        let l = next_line(&mut rx).await.expect("new line only");
        assert_eq!(l.line, "new line");

        handle.abort();
    }

    #[tokio::test]
    async fn watcher_discovers_instance_file_created_after_start() {
        // 实例日志在 watcher 启动后随实例出现：发现周期补缺（P10-007）。
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();

        let bus = LogBus::default();
        let mut rx = bus.subscribe();
        // 短发现周期（生产默认 5s）。
        let handle = spawn_tail_watchers_with_interval(dir.path(), bus, Duration::from_millis(100));

        // watcher 已启动但文件尚未创建。
        std::fs::write(logs.join("instance-2.log"), "boot\n").unwrap();
        // 等监督任务发现（100ms）且 worker 完成首轮打开。
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::OpenOptions::new()
            .append(true)
            .open(logs.join("instance-2.log"))
            .unwrap()
            .write_all(b"warp connected\n")
            .unwrap();

        let l = next_line(&mut rx).await.expect("discovered instance line");
        assert_eq!(l.source.id(), "instance:2");
        assert_eq!(l.line, "[REDACTED]");
        assert_eq!(l.seq, 1, "boot 行在发现前已存在，只推新行");

        handle.abort();
    }

    #[tokio::test]
    async fn watcher_cross_poll_appends_do_not_replay_previous_lines() {
        // 回归（review）：rewrite 探测曾读 `pos`（新数据首字节）对比上批末字节，
        // 纯 append 跨轮询必然误判为 truncate+rewrite → 全量回卷重读重发，
        // SSE 实时流重复刷屏。修复后探测 `pos-1`（上批末字节）——append 不变、
        // 截断重写才变。
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        // manager 源不做行级脱敏，可用行文判别是否发生重放。
        let path = logs.join("manager.log");
        std::fs::write(&path, "seed line\n").unwrap();

        let bus = LogBus::default();
        let mut rx = bus.subscribe();
        let handle = spawn_tail_watchers(dir.path(), bus);
        settle().await;

        let append = |text: &str| {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        };

        // 第一批 → seq 1。
        append("alpha line\n");
        let l1 = next_line(&mut rx).await.expect("first appended line");
        assert_eq!(l1.seq, 1);
        assert_eq!(l1.line, "alpha line");

        // 跨轮询追加第二批：buggy 探测在此时误判 rewrite 并重放 alpha。
        tokio::time::sleep(POLL_INTERVAL * 3).await;
        append("beta line\n");
        let l2 = next_line(&mut rx).await.expect("second appended line");
        assert_eq!(
            l2.seq, 2,
            "纯追加不得回卷重放（buggy 行为会把 alpha 重发为 seq 1）"
        );
        assert_eq!(l2.line, "beta line");

        // 第三批同样跨轮询：继续稳定递增。
        tokio::time::sleep(POLL_INTERVAL * 3).await;
        append("gamma line\n");
        let l3 = next_line(&mut rx).await.expect("third appended line");
        assert_eq!(l3.seq, 3);
        assert_eq!(l3.line, "gamma line");

        // 后续轮询不应再有任何行（重放会在 drain 期再次出现）。
        tokio::time::sleep(POLL_INTERVAL * 4).await;
        assert!(
            rx.try_recv().is_err(),
            "cross-poll appends must not produce duplicate lines"
        );

        handle.abort();
    }
}
