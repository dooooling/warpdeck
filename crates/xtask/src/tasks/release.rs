//! `cargo xtask release`：构建发布镜像（warpdeck:local / warpdeck:e2e）。
//!
//! 职责（2026-08-22 起，Dockerfile 直接消费 versions.json 后进一步简化）：
//! - 注入 WARPDECK_VERSION=0.1.0-<git 短 sha>（P12-012）；
//! - 可选透传 DL_PROXY（中国网络下走宿主代理；CI/海外直连留空）。
//! - GOST/WARP 的 URL/SHA256/版本不再经 --build-arg 传递：唯一来源
//!   crates/xtask/src/versions.json 由 Dockerfile 从 build context 直接 COPY + jq 解析。
//!
//! 镜像内 fetch-deps.sh 断点续传 + cache mount 持久 + 强制哈希校验；
//! install-gost.sh 另收 EXPECTED_GOST_SHA256 复核同源取值。
//! P11 预算不变：整个 E2E matrix 复用同一个 tag，禁止每用例重 build。

use anyhow::Result;

use crate::common::{self, Versions};

pub struct ReleaseArgs {
    pub tag: String,
    /// 构建期代理（socks5h://host.docker.internal:10808 等）；None = 直连。
    pub proxy: Option<String>,
}

pub fn run(args: &ReleaseArgs) -> Result<()> {
    let v = Versions::load()?;
    println!(
        "deps pin (consumed in-image from versions.json): gost v{} (sha256 {}) / warp v{} (sha256 {})",
        v.gost.version,
        &v.gost.sha256[..12],
        v.warp.version,
        &v.warp.sha256[..12]
    );

    // P12-012：`<app_version>-<git 短 sha>`；无 git 时回退 -dev。
    let git_sha = common::capture(
        "git",
        &["rev-parse".into(), "--short".into(), "HEAD".into()],
    )
    .unwrap_or_else(|_| "dev".to_string());
    let version = format!("{}-{}", v.app_version, git_sha);

    let repo = common::repo_root()?;
    common::run(
        "docker",
        &[
            "build".into(),
            "--progress=plain".into(),
            "--build-arg".into(),
            format!("WARPDECK_VERSION={version}"),
            "--build-arg".into(),
            format!("DL_PROXY={}", args.proxy.clone().unwrap_or_default()),
            "-t".into(),
            args.tag.clone(),
            repo.display().to_string(),
        ],
    )?;
    println!("OK: {}", args.tag);
    Ok(())
}
