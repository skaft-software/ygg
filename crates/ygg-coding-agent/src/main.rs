#![allow(missing_docs)]

mod app;
mod auth;
mod cli;
mod commands;
mod compaction;
mod config;
mod extensions;
mod hydrate;
mod modes;
mod presentation;
mod prompts;
mod providers;
mod resource_resolver;
mod resources;
mod session_commands;
mod session_store;
mod session_tree;
mod tui;

use clap::Parser;

// Keep a small multi-thread scheduler for provider/control responsiveness;
// bounded filesystem work uses Tokio's blocking pool, and TUI layout/terminal
// writes run on `ygg-tui-render`.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let top_level_command = cli.command.clone();

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
    tui::terminal::install_signal_restore()?;
    let config = cli::build_config(cli, &cwd)?;
    if let Some(cli::TopLevelCommand::Sessions { command }) = top_level_command {
        return session_commands::run(command, &config);
    }
    let mode = config.mode.clone();
    let initial_prompt = config.initial_prompt.clone();
    let capabilities = tui::terminal::TerminalCapabilities::detect(config.color, config.plain);
    let boot = app::bootstrap::bootstrap(config)?;
    let result = match mode {
        config::Mode::Interactive if capabilities.interactive => {
            modes::interactive::run_interactive(boot).await
        }
        config::Mode::Interactive => modes::plain::run_plain(boot, initial_prompt).await,
        config::Mode::Print { prompt } => modes::print::run_print(boot, prompt).await,
    };
    // Mode owners have now aborted active work and shut down their children.
    // Preserve the conventional signal status even when cleanup itself found
    // an error, rather than surfacing an unrelated anyhow exit code.
    tui::terminal::exit_if_signaled();
    result
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
        "custom" | "openai-custom" => {
            let store = auth::custom::CredentialStore::new(auth::custom::default_path());
            match command {
                AuthCommand::Login { .. } => {
                    use auth::custom::CustomCredential;
                    if store.load()?.is_some() {
                        anyhow::bail!(
                            "custom endpoint already configured at {}; use --logout custom first",
                            auth::custom::default_path().display()
                        );
                    }
                    let cred = CustomCredential {
                        base_url: "http://localhost:1234/v1/".into(),
                        api_key: String::new(),
                        api_name: "local-model".into(),
                        headers: Vec::new(),
                        models: Vec::new(),
                        auto_discover: true,
                    };
                    store.save(&cred)?;
                    println!(
                        "Custom endpoint template saved to {}.\n\
                         Edit that file with your endpoint details and restart ygg.",
                        auth::custom::default_path().display()
                    );
                    Ok(())
                }
                AuthCommand::Logout => {
                    store.delete()?;
                    println!("Custom endpoint removed.");
                    Ok(())
                }
            }
        }
        other => anyhow::bail!("unknown provider {other:?}; supported: codex, custom"),
    }
}
