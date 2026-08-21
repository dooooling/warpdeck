//! `cargo xtask dev-base`：构建 warpdeck-dev-base:1 运行时开发镜像。
//!
//! 只在 docker/*.sh / OS 包 / WARP+GOST 安装逻辑变化时重建；后端普通代码变化
//! 走 `in-container`（bind mount 二进制），不经过本任务（AGENTS.md 纪律）。
//! 依赖在构建期内经 fetch-deps.sh 下载（URL/SHA256 由 versions.json 注入，
//! cache mount 持久；`--proxy` 透传给中国网络环境）。

use anyhow::Result;

use crate::common::{self, Versions};

pub struct DevBaseArgs {
    /// 构建期代理；None = 直连。
    pub proxy: Option<String>,
}

pub fn run(args: &DevBaseArgs) -> Result<()> {
    let v = Versions::load()?;
    println!(
        "deps pin: gost v{} / warp v{} (downloaded & verified in-image)",
        v.gost.version, v.warp.version
    );
    let repo = common::repo_root()?;
    common::run(
        "docker",
        &[
            "build".into(),
            "--progress=plain".into(),
            "--build-arg".into(),
            format!("GOST_TARBALL_SHA256={}", v.gost.sha256),
            "--build-arg".into(),
            format!("WARP_DEB_SHA256={}", v.warp.sha256),
            "--build-arg".into(),
            format!("GOST_TARBALL_URL={}", v.gost.url),
            "--build-arg".into(),
            format!("WARP_DEB_URL={}", v.warp.url),
            "--build-arg".into(),
            format!("GOST_VERSION_PIN={}", v.gost.version),
            "--build-arg".into(),
            format!("DL_PROXY={}", args.proxy.clone().unwrap_or_default()),
            "-t".into(),
            "warpdeck-dev-base:1".into(),
            "-f".into(),
            repo.join("docker")
                .join("Dockerfile.dev-base")
                .display()
                .to_string(),
            repo.display().to_string(),
        ],
    )?;
    println!("OK: warpdeck-dev-base:1");
    Ok(())
}
