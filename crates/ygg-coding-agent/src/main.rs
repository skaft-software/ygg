#![allow(missing_docs)]

mod app;
mod cli;
mod config;
mod session_store;
mod tui;

use clap::Parser;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let cwd = std::env::current_dir()?;
    let _config = cli::build_config(cli, &cwd)?;
    println!("ygg configuration loaded");
    Ok(())
}
