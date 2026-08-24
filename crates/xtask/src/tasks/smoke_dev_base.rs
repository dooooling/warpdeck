//! `cargo xtask smoke-dev-base`：dev-base 冒烟（P2-012 / §23.3），替代 smoke-dev-base.ps1。
//!
//! 两部分：
//! - A 组件齐备性（无需特权）：warp-cli/warp-svc/dbus/tini 可执行 + 出网可达；
//! - B 真实数据面（`--full`，需 tun/NET_ADMIN）：免费注册 → mode proxy → connect，
//!   socks5 trace 期望 `warp=on`（无需 WARP+ license）。

use anyhow::{ensure, Context, Result};

use crate::common;

pub struct Args {
    pub full: bool,
}

const COMPONENTS_SCRIPT: &str = "set -e\n\
  command -v warp-cli && command -v warp-svc\n\
  dbus-daemon --version\n\
  tini --version | grep -i tini\n\
  echo 'components-ok'";

// 与原 .ps1 相同的时序：轮询 IPC socket 而非 warp-cli status（后者在 daemon
// 未就绪时也 exit 0，不能作为就绪判据）；connect 后轮询 status 到 Connected。
const DATAPLANE_SCRIPT: &str = "set -e\n\
  mkdir -p /var/run/dbus\n\
  dbus-daemon --system --fork\n\
  warp-svc --accept-tos >/tmp/warp-svc.log 2>&1 &\n\
  for i in 1 2 3 4 5; do\n\
    [ -S /run/cloudflare-warp/warp_service ] && break || sleep 2\n\
  done\n\
  warp-cli --accept-tos registration new\n\
  warp-cli --accept-tos mode proxy\n\
  warp-cli --accept-tos proxy port 40000\n\
  warp-cli --accept-tos connect\n\
  for i in 1 2 3 4 5; do\n\
    warp-cli --accept-tos status | grep -q 'Connected' && break || sleep 3\n\
  done\n\
  warp-cli --accept-tos status\n\
  curl -fsSL --max-time 30 --socks5-hostname 127.0.0.1:40000 https://cloudflare.com/cdn-cgi/trace ||\n\
    { echo '=== warp-svc.log ==='; cat /tmp/warp-svc.log; exit 1; }";

pub fn run(args: &Args) -> Result<()> {
    // ---------- A. 组件齐备性 ----------
    common::run(
        "docker",
        &[
            "run".into(),
            "--rm".into(),
            "warpdeck-dev-base:1".into(),
            "bash".into(),
            "-c".into(),
            COMPONENTS_SCRIPT.into(),
        ],
    )?;
    let trace = common::capture(
        "docker",
        &[
            "run".into(),
            "--rm".into(),
            "warpdeck-dev-base:1".into(),
            "bash".into(),
            "-c".into(),
            "curl -fsSL --max-time 20 https://cloudflare.com/cdn-cgi/trace".into(),
        ],
    )?;
    ensure!(trace.contains("warp="), "missing warp= field in trace");
    println!(
        "direct trace: {}",
        trace.lines().find(|l| l.contains("warp=")).unwrap_or("")
    );
    println!("OK: dev-base components smoke passed");

    // ---------- B. 真实数据面 ----------
    if args.full {
        let out = common::capture(
            "docker",
            &[
                "run".into(),
                "--rm".into(),
                "--user".into(),
                "root".into(),
                "--device".into(),
                "/dev/net/tun".into(),
                "--cap-add".into(),
                "NET_ADMIN".into(),
                "--entrypoint".into(),
                "bash".into(),
                "warpdeck-dev-base:1".into(),
                "-c".into(),
                DATAPLANE_SCRIPT.into(),
            ],
        )
        .context("data plane smoke failed")?;
        println!("{out}");
        ensure!(out.contains("warp=on"), "expected warp=on");
        println!("OK: dev-base data plane smoke passed (warp=on)");
    }
    Ok(())
}
