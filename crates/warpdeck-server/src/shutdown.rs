//! 优雅停机信号（P1-009）。
//!
//! DESIGN §21.7：同时处理 SIGINT（Ctrl+C）与 SIGTERM（`docker stop`），
//! 缺少 SIGTERM 处理会让容器停止时残留脏的 runtime 文件。

/// 等待 SIGINT（Ctrl+C）或 SIGTERM。仅在收到信号时返回。
pub async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    // `Signal::recv()` 借用的 future 与 `Signal` 同生命周期；
    // 必须独立绑定，否则临时值在语句结束即释放 → E0716（Linux target 编译才暴露）。
    #[cfg(unix)]
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");
    #[cfg(unix)]
    let sigterm = signal.recv();
    #[cfg(not(unix))]
    let sigterm = std::future::pending::<Option<()>>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = sigterm => {},
    }
    tracing::info!("shutdown signal received, stopping");
}
