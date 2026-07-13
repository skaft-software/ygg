#![allow(missing_docs)]

mod app;
mod cli;
mod commands;
mod compaction;
mod config;
mod hydrate;
mod modes;
mod session_store;
mod tui;

use clap::Parser;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let cwd = std::env::current_dir()?;
    tui::terminal::install_panic_hook();
    let config = cli::build_config(cli, &cwd)?;
    let mode = config.mode.clone();
    let boot = app::bootstrap::bootstrap(config)?;
    match mode {
        config::Mode::Interactive => modes::interactive::run_interactive(boot).await,
        config::Mode::Print { prompt } => modes::print::run_print(boot, prompt).await,
    }
}
