//! Native, versioned NDJSON host for applications that cannot link Rust.

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    ygg_sdk::host::run_stdio().await
}
