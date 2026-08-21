//! `cargo xtask in-container` / `cargo xtask check-linux`：Linux 侧编译与检查，替代
//! build-in-container.ps1 并补上「Windows 全绿 ≠ Linux 绿」的缺口。
//!
//! 设计（AGENTS.md「禁止反复 docker build 当开发循环」的配套）：
//! - 编译镜像 warpdeck-dev-rust:1 固定 toolchain；源码只读 bind-mount；
//! - cargo registry 缓存命名卷（跨 run 持久，命中后零下载）；target 命名卷增量编译；
//! - rustc/cargo 走 toolchain 绝对路径，完全绕过 rustup shim（其 channel 同步在
//!   国内网络会卡死）——toolchain 目录改为**容器内探测**，镜像升级不再打断脚本；
//! - 产物导出到宿主 target/linux-artifacts/。
//!
//! check-linux 复用同一挂载跑 `cargo clippy -- -D warnings`（`--test` 加跑测试），
//! 与 CI Linux job 等价；registry 命中后通常只需增量编译数分钟。

use anyhow::{Context, Result};

use crate::common;

const IMAGE: &str = "warpdeck-dev-rust:1";
const TARGET_VOL: &str = "warpdeck-target";
const ARTIFACT: &str = "warpdeck-server";

/// 容器内 toolchain bin 目录探测（取第一个匹配项）。
fn discover_toolchain_bin() -> Result<String> {
    let out = common::capture(
        "docker",
        &[
            "run".into(),
            "--rm".into(),
            IMAGE.into(),
            "sh".into(),
            "-c".into(),
            "ls -d /usr/local/rustup/toolchains/*/bin 2>/dev/null | head -n1".into(),
        ],
    )
    .context("probe toolchain inside dev-rust image")?;
    anyhow::ensure!(!out.is_empty(), "no toolchain found in {IMAGE}");
    Ok(out)
}

fn base_run_args(repo: &std::path::Path, home: &std::path::Path) -> Vec<String> {
    vec![
        "run".into(),
        "--rm".into(),
        "-v".into(),
        format!("{}:/src:ro", repo.display()),
        // 宿主 ~/.cargo/registry 直接挂到 CARGO_HOME 下（.crate 平台无关缓存 +
        // aliyun sparse 索引，命中后零下载）。注意 CARGO_HOME=/usr/local/cargo。
        "-v".into(),
        format!(
            "{}/.cargo/registry:/usr/local/cargo/registry",
            home.display()
        ),
        "-v".into(),
        format!("{TARGET_VOL}:/target"),
        "-e".into(),
        // 对 crates-io 无效（见 docker/Dockerfile.dev-rust 注释），保留仅为显式覆盖。
        "CARGO_REGISTRIES_CRATES_IO_INDEX=sparse+https://mirrors.aliyun.com/crates.io-index/"
            .into(),
        IMAGE.into(),
    ]
}

/// 在容器内执行一条 cargo 命令。
///
/// 工具链解析策略（离线硬保证）：
/// - PATH 前置 toolchain bin → `cargo`/`rustc` 直用固定版本；
/// - `RUSTUP_TOOLCHAIN=<pinned>` 兜底——即使有进程绕到 rustup shim，也只允许解析
///   本地已装工具链，绝不触发 channel 同步（国内直连 static.rust-lang.org 卡死，
///   2026-08-21 实测两次踩坑）。
fn cargo_in_container(
    repo: &std::path::Path,
    home: &std::path::Path,
    cargo_args: &str,
) -> Result<()> {
    let tc_bin = discover_toolchain_bin()?;
    let tc_name = tc_bin
        .trim_end_matches('/')
        .rsplit('/')
        .nth(1)
        .with_context(|| format!("parse toolchain name from {tc_bin}"))?
        .to_string();
    // -e 必须位于 `run` 之后（base_run_args 首元素即 "run"，跳过它再拼接）。
    let mut args = base_run_args(repo, home);
    args.splice(1..1, ["-e".into(), format!("RUSTUP_TOOLCHAIN={tc_name}")]);
    args.push("sh".into());
    args.push("-c".into());
    args.push(format!(
        "cd /src && CARGO_TARGET_DIR=/target RUSTC={tc_bin}/rustc PATH={tc_bin}:$PATH cargo {cargo_args}"
    ));
    common::run("docker", &args)
}

/// `in-container`：debug 编译 warpdeck-server 并导出 ELF 到宿主。
pub struct InContainerArgs {
    pub rebuild_image: bool,
    pub clean_target: bool,
    /// 重建镜像时给 rustup component add 用的代理；None = 直连。
    pub proxy: Option<String>,
}

fn rebuild_image(repo: &std::path::Path, proxy: Option<&str>) -> Result<()> {
    let mut args: Vec<String> = [
        "build".into(),
        "-t".into(),
        IMAGE.into(),
        "-f".into(),
        repo.join("docker")
            .join("Dockerfile.dev-rust")
            .display()
            .to_string(),
        "--build-arg".into(),
        format!("RUSTUP_PROXY={}", proxy.unwrap_or_default()),
    ]
    .into();
    args.push(repo.display().to_string());
    common::run("docker", &args)
}

/// `in-container`：debug 编译 warpdeck-server 并导出 ELF 到宿主。
pub fn build(args: &InContainerArgs) -> Result<()> {
    let repo = common::repo_root()?;
    let home = common::home_dir()?;
    if args.rebuild_image {
        rebuild_image(&repo, args.proxy.as_deref())?;
    }
    if args.clean_target {
        let _ = std::process::Command::new("docker")
            .args(["volume", "rm", TARGET_VOL])
            .status();
    }
    let _ = std::fs::create_dir_all(repo.join("target").join("linux-artifacts"));
    // 先建卷（旧脚本语义：volume create 幂等）。
    std::process::Command::new("docker")
        .args(["volume", "create", TARGET_VOL])
        .output()?;

    cargo_in_container(
        &repo,
        &home,
        "build -p warpdeck-server --bin warpdeck-server",
    )?;
    export_artifact(&repo)?;
    Ok(())
}

fn export_artifact(repo: &std::path::Path) -> Result<()> {
    common::run(
        "docker",
        &[
            "run".into(),
            "--rm".into(),
            "-v".into(),
            format!("{TARGET_VOL}:/target"),
            "-v".into(),
            format!("{}:/host-target", repo.join("target").display()),
            IMAGE.into(),
            "sh".into(),
            "-c".into(),
            format!(
                "mkdir -p /host-target/linux-artifacts && cp /target/debug/{ARTIFACT} \
                 /host-target/linux-artifacts/{ARTIFACT} && chmod +x /host-target/linux-artifacts/{ARTIFACT}"
            ),
        ],
    )?;
    println!("OK: target/linux-artifacts/{ARTIFACT}");
    Ok(())
}

/// `check-linux`：Linux 侧 clippy（--test 时加跑全量测试），CI ubuntu job 的本地等价物。
pub struct CheckLinuxArgs {
    pub test: bool,
}

pub fn check(args: &CheckLinuxArgs) -> Result<()> {
    let repo = common::repo_root()?;
    let home = common::home_dir()?;
    // 前置探测：clippy 组件缺失时给出可执行的修复指引，而不是让 rustup 报晦涩错误
    // （官方 rust 镜像 minimal profile 不含 clippy，需重建镜像预装）。
    let has_clippy = common::capture(
        "docker",
        &[
            "run".into(),
            "--rm".into(),
            IMAGE.into(),
            "sh".into(),
            "-c".into(),
            "test -f /usr/local/rustup/toolchains/*/bin/cargo-clippy && echo yes || echo no".into(),
        ],
    )?;
    anyhow::ensure!(
        has_clippy.trim() == "yes",
        "dev-rust 镜像缺少 clippy 组件；先重建：cargo xtask in-container --rebuild-image \
         --proxy socks5h://host.docker.internal:10808（国内网络）或 --proxy 直连留空参数"
    );
    let _ = std::fs::create_dir_all(repo.join("target").join("linux-artifacts"));
    std::process::Command::new("docker")
        .args(["volume", "create", TARGET_VOL])
        .output()?;
    cargo_in_container(
        &repo,
        &home,
        "clippy --workspace --all-targets --all-features -- -D warnings",
    )?;
    if args.test {
        cargo_in_container(&repo, &home, "test --workspace")?;
    }
    println!("OK: linux checks passed");
    Ok(())
}
