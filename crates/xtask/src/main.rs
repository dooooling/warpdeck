//! xtask：WarpDeck 构建/检查任务入口（替代 scripts/*.ps1 编排层）。
//!
//! 用法（`.cargo/config.toml` 提供 alias）：
//!
//! ```text
//! cargo xtask dev-base                  # 构建运行时开发镜像 warpdeck-dev-base:1
//! cargo xtask in-container              # 容器内编译 Linux ELF 并导出
//! cargo xtask check-linux [--test]      # Linux 侧 build+test，CI ubuntu job 本地等价
//! cargo xtask release [--tag T] [--proxy P]   # 构建发布镜像（默认 warpdeck:local）
//! ```
//!
//! GOST/WARP 依赖在镜像构建期内由 docker/fetch-deps.sh 下载并强制 SHA256 校验
//! （URL/哈希经 --build-arg 注入，单一来源 crates/xtask/src/versions.json）；
//! 中国网络下用 `--proxy socks5h://host.docker.internal:10808` 走宿主代理。

mod common;
mod tasks;

use clap::{Parser, Subcommand};

use crate::tasks::in_container::{CheckLinuxArgs, InContainerArgs};

#[derive(Parser)]
#[command(name = "xtask", about = "WarpDeck build & check tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 构建 warpdeck-dev-base:1 运行时开发镜像。
    DevBase {
        /// 构建期代理（依赖下载走此代理）；缺省直连。
        #[arg(long)]
        proxy: Option<String>,
    },
    /// 在 warpdeck-dev-rust:1 内编译 Linux ELF 并导出到 target/linux-artifacts/。
    InContainer {
        /// 先重建编译镜像。
        #[arg(long, default_value_t = false)]
        rebuild_image: bool,
        /// 清空 target 命名卷后全量重编。
        #[arg(long, default_value_t = false)]
        clean_target: bool,
        /// 重建镜像时 rustup component add 用的代理；缺省直连。
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Linux 侧 build+test——与 CI ubuntu job 等价的本地验证（离线可用）。
    CheckLinux {
        /// 连同 `cargo test --workspace` 一起跑。
        #[arg(long, default_value_t = false)]
        test: bool,
    },
    /// 构建发布镜像。
    Release {
        /// 产物 tag。
        #[arg(long, default_value = "warpdeck:local")]
        tag: String,
        /// 构建期代理（依赖下载走此代理）；缺省直连。
        #[arg(long)]
        proxy: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::DevBase { proxy } => tasks::dev_base::run(&tasks::dev_base::DevBaseArgs { proxy }),
        Command::InContainer {
            rebuild_image,
            clean_target,
            proxy,
        } => tasks::in_container::build(&InContainerArgs {
            rebuild_image,
            clean_target,
            proxy,
        }),
        Command::CheckLinux { test } => tasks::in_container::check(&CheckLinuxArgs { test }),
        Command::Release { tag, proxy } => {
            tasks::release::run(&tasks::release::ReleaseArgs { tag, proxy })
        }
    }
}
