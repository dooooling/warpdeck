//! 跨平台公共工具：home 目录、docker/git 命令封装、versions.json 加载。
//!
//! 平台约定：
//! - 路径一律 [`PathBuf`]，docker 挂载参数直接传宿主原生路径（Command 不经 shell，
//!   无引号问题；Windows 盘符路径由 Docker Desktop 自行解析）；
//! - home 目录探测顺序 HOME → USERPROFILE，兼容 Linux/macOS/Windows。

use std::path::PathBuf;
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// versions.json 的反序列化模型（单一事实来源）。
#[derive(Debug, Clone, Deserialize)]
pub struct Versions {
    #[serde(rename = "app_version")]
    pub app_version: String,
    pub gost: Artifact,
    pub warp: Artifact,
}

/// 单个大文件依赖（GOST tarball / WARP deb）的版本与校验信息。
/// 注意：`url` 字段刻意不反序列化——它的唯一消费者是 Dockerfile
/// （build context COPY + jq），Rust 侧只关心版本号与哈希展示。
#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    pub version: String,
    /// 小写十六进制 SHA256（镜像内 fetch-deps.sh 与 install-gost.sh 双重复核）。
    pub sha256: String,
}

pub const VERSIONS_JSON: &str = include_str!("versions.json");

impl Versions {
    pub fn load() -> Result<Self> {
        serde_json::from_str(VERSIONS_JSON).context("parse embedded versions.json")
    }
}

/// 宿主 home 目录（HOME → USERPROFILE）。
pub fn home_dir() -> Result<PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(p) = std::env::var(key) {
            if !p.is_empty() {
                return Ok(PathBuf::from(p));
            }
        }
    }
    bail!("cannot determine home directory (HOME/USERPROFILE both unset)")
}

fn print_command(program: &str, args: &[String]) {
    println!("+ {program} {}", args.join(" "));
}

/// 同步运行外部命令；非零退出码即报错（stdout/stderr 直通继承，保持实时输出）。
pub fn run(program: &str, args: &[String]) -> Result<()> {
    print_command(program, args);
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("spawn {program}"))?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    Ok(())
}

/// 运行命令并捕获 stdout（UTF-8，trim 尾部换行）；用于 git rev-parse、容器内探测等。
pub fn capture(program: &str, args: &[String]) -> Result<String> {
    print_command(program, args);
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("spawn {program}"))?;
    if !status.success() {
        let err = String::from_utf8_lossy(&stderr);
        bail!("{program} exited with {status}: {err}");
    }
    Ok(String::from_utf8_lossy(&stdout).trim().to_string())
}

/// 仓库根目录（Cargo 清单所在处；xtask 从任意 cwd 调用均可）。
pub fn repo_root() -> Result<PathBuf> {
    let manifest = capture(
        "cargo",
        &[
            "locate-project".into(),
            "--workspace".into(),
            "--message-format".into(),
            "plain".into(),
        ],
    )?;
    let path = PathBuf::from(manifest.trim());
    path.parent()
        .map(|p| p.to_path_buf())
        .context("workspace root has no parent")
}
