//! Interactive, print, and RPC Ygg terminal application.

// Keep a small multi-thread scheduler for provider/control responsiveness;
// bounded filesystem work uses Tokio's blocking pool, and TUI layout/terminal
// writes run on `ygg-tui-render`.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    ygg_sdk::run_cli().await
}
