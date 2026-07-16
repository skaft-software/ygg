#![allow(missing_docs)]

mod app;
mod auth;
mod cli;
mod commands;
mod compaction;
mod config;
mod hydrate;
mod modes;
mod resources;
mod session_store;
mod tui;

use clap::Parser;

// The control loop stays local; interactive TUI layout and terminal writes run
// on `ygg-tui-render` so model streaming is not stalled by frame rendering.
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    // Subscription auth commands run and exit before any run configuration is
    // built — they need neither a workspace nor a session.
    if let Some(provider) = cli.login.as_deref() {
        return run_auth_command(
            provider,
            AuthCommand::Login {
                headless: cli.headless,
            },
        )
        .await;
    }
    if let Some(provider) = cli.logout.as_deref() {
        return run_auth_command(provider, AuthCommand::Logout).await;
    }

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

enum AuthCommand {
    Login { headless: bool },
    Logout,
}

/// Dispatch `--login`/`--logout` for a named provider.
async fn run_auth_command(provider: &str, command: AuthCommand) -> anyhow::Result<()> {
    match provider {
        "codex" | "openai-codex" | "openai" => {
            let store = auth::codex::CredentialStore::new(auth::codex::default_path());
            match command {
                AuthCommand::Login { headless } => auth::codex::login(&store, headless).await,
                AuthCommand::Logout => auth::codex::logout(&store),
            }
        }
        other => anyhow::bail!("unknown provider {other:?}; supported: codex"),
    }
}
