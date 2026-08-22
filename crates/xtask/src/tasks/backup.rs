//! `cargo xtask backup` / `restore` / `backups`：数据卷备份恢复（P12-009 / §28.3），
//! 替代 backup-restore.ps1。
//!
//! 备份 = warpdeck-data 数据卷整体快照：warpdeck.db（SQLite WAL 合并后落盘）、
//! master.key、instances/（WARP 注册态，恢复后免重新注册）。
//! **显式排除** instances/*/state/mdm.xml：该文件在 ZeroTrust 模式下含明文
//! client_secret（P0 审查 #1；服务端已改为注册后即删，排除是纵深防御——
//! 覆盖「验证未通过时文件仍在盘上」的窗口期）。恢复后的实例下次启动会从
//! 加密密文库重新生成该文件，无需备份。reg.json 保留在备份中（免重注册）。
//! 原则不变：备份/恢复期间 compose stop——MVP 允许停服务复制 DB，不做热备，
//! 杜绝 WAL/内存中未落盘数据的不一致。
//! 归档命名：warpdeck-{project}-{yyyyMMdd-HHmmss}.tar.gz。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};

use crate::common;

/// tar 排除项：ZeroTrust 明文凭据文件不进备份（P0 审查 #1 纵深防御）。
const BACKUP_EXCLUDES: &[&str] = &["./instances/*/state/mdm.xml"];

pub struct BackupArgs {
    pub project: String,
    pub backup_dir: Option<String>,
}

pub struct RestoreArgs {
    pub archive: PathBuf,
    pub project: String,
}

pub struct ListArgs {
    pub backup_dir: Option<String>,
}

/// yyyyMMdd-HHmmss（无 chrono；civil-from-days 算法）。
fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (h, m, s) = ((secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
    let z = (secs / 86400) as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}{month:02}{day:02}-{h:02}{m:02}{s:02}")
}

fn backup_dir(explicit: &Option<String>) -> Result<PathBuf> {
    let p = match explicit {
        Some(s) => PathBuf::from(s),
        None => common::repo_root()?.join("backups"),
    };
    if p.is_relative() {
        Ok(std::env::current_dir()?.join(p))
    } else {
        Ok(p)
    }
}

fn volume(project: &str) -> String {
    format!("{project}_warpdeck-data")
}

fn expect_volume(vol: &str) -> Result<()> {
    if std::process::Command::new("docker")
        .args(["volume", "inspect", vol])
        .output()?
        .status
        .success()
    {
        Ok(())
    } else {
        bail!("volume {vol} does not exist (project up?)")
    }
}

fn alpine(vol: &str, dir: &Path, script: &str) -> Result<()> {
    // 仅限**编译期常量**脚本（如固定 glob 清理）；任何含用户输入的命令必须走
    // `alpine_argv`（P1 审查 #4：sh -c 拼接 = 容器内注入）。
    debug_assert!(!script.contains('{'), "constant scripts only");
    common::run(
        "docker",
        &[
            "run".into(),
            "--rm".into(),
            "-v".into(),
            format!("{vol}:/data"),
            "-v".into(),
            format!("{}:/backup", dir.display()),
            "alpine:3.20".into(),
            "sh".into(),
            "-c".into(),
            script.into(),
        ],
    )
}

/// 在 alpine 容器内以 **argv 直调 tar**（不经 shell）：宿主 `host_dir` 挂到
/// /backup，数据卷挂到 /data。所有参数元素原样传递，无解析、无注入。
fn alpine_tar(vol: &str, host_dir: &Path, tar_args: &[&str]) -> Result<()> {
    let mut a: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "-v".into(),
        format!("{vol}:/data"),
        "-v".into(),
        format!("{}:/backup", host_dir.display()),
        "alpine:3.20".into(),
        "tar".into(),
    ];
    a.extend(tar_args.iter().map(|s| (*s).to_string()));
    common::run("docker", &a)
}

/// 同 [`alpine_tar`] 但捕获 stdout（用于列目录校验）。
fn alpine_tar_capture(vol: &str, host_dir: &Path, tar_args: &[&str]) -> Result<String> {
    let mut a: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "-v".into(),
        format!("{vol}:/data"),
        "-v".into(),
        format!("{}:/backup", host_dir.display()),
        "alpine:3.20".into(),
        "tar".into(),
    ];
    a.extend(tar_args.iter().map(|s| (*s).to_string()));
    common::capture("docker", &a)
}

fn compose(action: &str, project: &str) -> Result<()> {
    // 显式 -f 指向仓库根 compose.yml（不依赖调用方 cwd）；.env 解析随文件目录。
    let repo = common::repo_root()?;
    common::run(
        "docker",
        &[
            "compose".into(),
            "-p".into(),
            project.into(),
            "-f".into(),
            repo.join("compose.yml").display().to_string(),
            action.into(),
        ],
    )
}

pub fn backup(args: &BackupArgs) -> Result<()> {
    let vol = volume(&args.project);
    let dir = backup_dir(&args.backup_dir)?;
    std::fs::create_dir_all(&dir)?;
    expect_volume(&vol)?;
    let name = format!("warpdeck-{}-{}.tar.gz", args.project, stamp());
    println!("== backup {vol} -> {} ==", dir.join(&name).display());
    compose("stop", &args.project)?;
    let out = format!("/backup/{name}");
    let mut tar_args: Vec<&str> = vec!["czf", &out];
    for ex in BACKUP_EXCLUDES {
        tar_args.push("--exclude");
        tar_args.push(ex);
    }
    tar_args.extend(["-C", "/data", "."]);
    let res = alpine_tar(&vol, &dir, &tar_args);
    compose("start", &args.project)?;
    res?;
    println!("OK: {}", dir.join(&name).display());
    Ok(())
}

pub fn restore(args: &RestoreArgs) -> Result<()> {
    let vol = volume(&args.project);
    // P1 审查 #4：canonicalize 实际归档并挂载**其父目录**——校验的就是恢复的，
    // 杜绝「校验 A、恢复 backup_dir 下同名 B」的路径混淆；文件名只作 argv
    // 元素传入 tar（不经 sh -c），特殊字符无注入面。
    let abs = std::fs::canonicalize(&args.archive)
        .with_context(|| format!("archive not found: {}", args.archive.display()))?;
    ensure!(abs.is_file(), "not a file: {}", abs.display());
    let dir = abs
        .parent()
        .with_context(|| "archive has no parent dir")?
        .to_path_buf();
    let name = abs
        .file_name()
        .with_context(|| "no filename")?
        .to_string_lossy()
        .to_string();
    let in_backup = format!("/backup/{name}");
    let listing = alpine_tar_capture(&vol, &dir, &["tzf", &in_backup])?;
    for required in ["warpdeck.db", "master.key"] {
        let found = listing
            .lines()
            .any(|l| l.trim_start_matches("./") == required);
        ensure!(found, "archive missing '{required}' - aborting restore");
    }
    println!("== restore {} -> {vol} ==", abs.display());
    expect_volume(&vol)?;
    compose("stop", &args.project)?;
    let res = (|| -> Result<()> {
        // 常量脚本（含 glob），无用户输入，允许 sh -c。
        alpine(&vol, &dir, "rm -rf /data/* /data/.[!.]*")?;
        alpine_tar(&vol, &dir, &["xzf", &in_backup, "-C", "/data"])
    })();
    compose("start", &args.project)?;
    res?;
    println!("OK: restored {name} ({vol}), containers started");
    Ok(())
}

pub fn list(args: &ListArgs) -> Result<()> {
    let dir = backup_dir(&args.backup_dir)?;
    ensure!(dir.is_dir(), "backup dir not found: {}", dir.display());
    let mut names: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| {
                    n.to_string_lossy().starts_with("warpdeck-")
                        && n.to_string_lossy().ends_with(".tar.gz")
                })
                .unwrap_or(false)
        })
        .collect();
    names.sort();
    let mut out = std::io::stdout().lock();
    for p in names {
        let size = p.metadata().map(|m| m.len()).unwrap_or(0);
        writeln!(
            out,
            "{}  {size:>10} bytes",
            p.file_name().unwrap().to_string_lossy()
        )?;
    }
    Ok(())
}
