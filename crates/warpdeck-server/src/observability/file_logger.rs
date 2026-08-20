//! manager 日志文件层（P10-005）。
//!
//! `{data_dir}/logs/manager.log`：manager 自身 tracing 事件落盘（与 stderr 并行）。
//!
//! 脱敏策略（DESIGN §27.2 / §25.11）：结构化 tracing 字段的 secret 由
//! `Sensitive<T>` 包装在**日志点**保证（主防线），本层不整行二次 scrub——
//! 否则 manager 日志失去排障价值。CLI stderr 类非结构化行（instance/gost）
//! 在发布/读取路径走 `redactor::redact`（§25.11 scrub 要求）。
//!
//! 大小保护（MVP 取舍）：启动时若文件 > `MANAGER_LOG_MAX_BYTES` 则截断重写；
//! 运行期不轮转（P12 再做正式 rotation）。

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// manager.log 上限：超过则下次启动截断（防无限增长）。
pub const MANAGER_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// 追加写 manager.log 的句柄（进程内 Mutex 串行化写入）。
#[derive(Debug)]
pub struct ManagerLogFile {
    file: Mutex<File>,
}

impl ManagerLogFile {
    /// 打开（append）；目录不存在则创建；超过上限则截断。
    pub fn open(data_dir: &Path) -> std::io::Result<Self> {
        let dir = data_dir.join("logs");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("manager.log");
        if path.metadata().map(|m| m.len()).unwrap_or(0) > MANAGER_LOG_MAX_BYTES {
            // 超限截断（只截断一次，运行期不再检查）。
            std::fs::write(&path, b"")?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// 追加一行（tracing 层已格式化、字段级已脱敏）。
    pub fn append_line(&self, line: &str) {
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{line}");
        }
    }

    fn write_buf(&self, buf: &[u8]) -> io::Result<usize> {
        match self.file.lock() {
            Ok(mut file) => file.write(buf),
            Err(_) => Err(io::Error::other("manager log mutex poisoned")),
        }
    }

    fn flush_buf(&self) -> io::Result<()> {
        match self.file.lock() {
            Ok(mut file) => file.flush(),
            Err(_) => Err(io::Error::other("manager log mutex poisoned")),
        }
    }
}

/// 共享写句柄（tracing fmt writer 单行写入）。
#[derive(Debug, Clone)]
pub struct SharedLogWriter(Arc<ManagerLogFile>);

impl io::Write for SharedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_buf(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush_buf()
    }
}

/// 让 tracing fmt 层把事件行写到 manager.log（P10-005）。
#[derive(Debug, Clone)]
pub struct ManagerLogLayer {
    inner: Arc<ManagerLogFile>,
}

impl ManagerLogLayer {
    pub fn new(data_dir: &Path) -> std::io::Result<Self> {
        Ok(Self {
            inner: Arc::new(ManagerLogFile::open(data_dir)?),
        })
    }
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for ManagerLogLayer {
    type Writer = SharedLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedLogWriter(self.inner.clone())
    }
}

/// 并行写句柄：一行同时进 `manager.log` 与 stderr（`docker logs` 可见）。
#[derive(Debug)]
pub struct DualLogWriter {
    file: SharedLogWriter,
    stderr: io::Stderr,
}

impl io::Write for DualLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let file_result = self.file.write(buf);
        // stderr 尽力而为：断开/关闭不应中断文件日志管线。
        let _ = self.stderr.write(buf);
        file_result
    }

    fn flush(&mut self) -> io::Result<()> {
        let file_result = self.file.flush();
        let _ = self.stderr.flush();
        file_result
    }
}

/// 让 tracing fmt 层同时写 manager.log 与 stderr（P10-005「与 stderr 并行」落地）。
#[derive(Debug, Clone)]
pub struct DualLogLayer {
    inner: Arc<ManagerLogFile>,
}

impl DualLogLayer {
    pub fn new(data_dir: &Path) -> std::io::Result<Self> {
        Ok(Self {
            inner: Arc::new(ManagerLogFile::open(data_dir)?),
        })
    }
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for DualLogLayer {
    type Writer = DualLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        DualLogWriter {
            file: SharedLogWriter(self.inner.clone()),
            stderr: io::stderr(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_logs_dir_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let logger = ManagerLogFile::open(dir.path()).unwrap();
        logger.append_line("first line");
        logger.append_line("second line");
        let content = std::fs::read_to_string(dir.path().join("logs/manager.log")).unwrap();
        assert!(content.contains("first line"));
        assert!(content.contains("second line"));
    }

    #[test]
    fn oversized_log_is_truncated_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        // 超上限的大文件。
        std::fs::write(logs.join("manager.log"), vec![b'x'; 10 * 1024 * 1024 + 1]).unwrap();
        let logger = ManagerLogFile::open(dir.path()).unwrap();
        logger.append_line("fresh start");
        let content = std::fs::read_to_string(logs.join("manager.log")).unwrap();
        assert_eq!(content, "fresh start\n");
    }

    #[test]
    fn dual_writer_writes_file_and_stderr() {
        use std::io::Write as _;
        use tracing_subscriber::fmt::MakeWriter;
        let dir = tempfile::tempdir().unwrap();
        let layer = DualLogLayer::new(dir.path()).unwrap();
        let mut writer = layer.make_writer();
        writer.write_all(b"dual line\n").unwrap();
        writer.flush().unwrap();
        let content = std::fs::read_to_string(dir.path().join("logs/manager.log")).unwrap();
        assert_eq!(content, "dual line\n");
    }
}
