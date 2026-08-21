//! `cargo xtask release`：构建发布镜像（warpdeck:local / warpdeck:e2e）。
//!
//! 职责（2026-08-21 起，依赖改为构建期下载后大幅简化）：
//! - 从 versions.json 读 URL/SHA256/版本（单一事实来源），经 --build-arg 注入；
//! - 注入 WARPDECK_VERSION=0.1.0-<git 短 sha>（P12-012）；
//! - 可选透传 DL_PROXY（中国网络下走宿主代理；CI/海外直连留空）。
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
        "deps pin: gost v{} / warp v{} (sha256 verified in-image)",
        v.gost.version, v.warp.version
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
            args.tag.clone(),
            repo.display().to_string(),
        ],
    )?;
    println!("OK: {}", args.tag);
    Ok(())
}
