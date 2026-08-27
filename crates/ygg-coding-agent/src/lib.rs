#![allow(missing_docs)]

//! Product runtime shared by the `ygg` terminal frontend and native host.

mod app;
mod auth;
mod cli;
mod commands;
mod compaction;
mod config;
mod extension_bundle;
mod extension_package;
mod extensions;
/// Versioned NDJSON process boundary for non-Rust consumers.
pub mod host;
mod hydrate;
mod migrate;
mod modes;
mod output;
mod pi;
mod presentation;
mod prompts;
mod providers;
mod resource_resolver;
mod resources;
mod session_catalog;
mod session_commands;
mod session_store;
mod session_tree;
mod tui;
mod update;

use clap::Parser;

/// Run the terminal frontend with the same diagnostics and exit status as the `ygg` binary.
pub async fn run_cli() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // `Result`'s default `Termination` implementation writes errors
            // directly to stderr. Keep even early startup failures behind the
            // same control-safe presentation boundary as command output.
            crate::output::stderr_line(format!("Error: {error:#}"));
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
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

    if let Some(cli::TopLevelCommand::Pi { command }) = top_level_command.clone() {
        return pi::run(command, &std::env::current_dir()?);
    }
    if let Some(cli::TopLevelCommand::Migrate { command }) = top_level_command.clone() {
        return migrate::run(command, &std::env::current_dir()?);
    }
    if let Some(cli::TopLevelCommand::Extension { command }) = top_level_command.clone() {
        return extension_package::run(command).await;
    }
    if let Some(cli::TopLevelCommand::Update { check }) = top_level_command.clone() {
        return update::run(check).await;
    }
    #[cfg(not(feature = "serve"))]
    if let Some(cli::TopLevelCommand::Serve {
        no_open,
        port,
        web_root,
    }) = top_level_command.clone()
    {
        return extension_package::run_serve(no_open, port, web_root);
    }

    let cwd = std::env::current_dir()?;
    #[cfg(feature = "serve")]
    let is_serve = matches!(&top_level_command, Some(cli::TopLevelCommand::Serve { .. }));
    #[cfg(not(feature = "serve"))]
    let is_serve = false;
    if !is_serve {
        // Preserve the original startup/error boundary for every terminal and
        // non-Serve invocation.
        tui::terminal::install_panic_hook();
        tui::terminal::install_signal_restore()?;
    }
    let config = cli::build_config(cli, &cwd)?;
    if let Some(cli::TopLevelCommand::Sessions { command }) = top_level_command.clone() {
        return session_commands::run(command, &config);
    }
    #[cfg(feature = "serve")]
    if let Some(cli::TopLevelCommand::Serve {
        no_open,
        port,
        web_root,
    }) = top_level_command
    {
        return extensions::serve::run(config, port, no_open, web_root).await;
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
        config::Mode::Rpc => modes::rpc::run_rpc(boot).await,
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
                AuthCommand::Logout => auth::codex::logout(&store).await,
            }
        }
        "custom" | "openai-custom" => {
            let store = auth::custom::CredentialStore::new(auth::custom::default_path());
            match command {
                AuthCommand::Login { .. } => {
                    use auth::custom::{
                        CustomAuthConfig, CustomCredential, CustomProvider, CustomRegistry,
                    };
                    if store.load_registry()?.is_some() {
                        anyhow::bail!(
                            "custom provider registry already configured at {}; use --logout custom first",
                            auth::custom::default_path().display()
                        );
                    }
                    let provider = CustomProvider {
                        label: "Local endpoint".into(),
                        credential: CustomCredential {
                            base_url: "http://localhost:1234/v1/".into(),
                            api_key: String::new(),
                            api_name: "local-model".into(),
                            headers: Vec::new(),
                            models: Vec::new(),
                            auto_discover: true,
                        },
                        auth: Some(CustomAuthConfig::None),
                        api_key_env: None,
                        cache: None,
                        startup_timeout_secs: None,
                    };
                    store.save_registry(&CustomRegistry::single("local", provider))?;
                    crate::output::stdout_multiline(format!(
                        "Custom provider registry template saved to {}.\n\
                         Edit that file with your provider details and restart ygg.",
                        auth::custom::default_path().display()
                    ));
                    Ok(())
                }
                AuthCommand::Logout => {
                    store.delete()?;
                    crate::output::stdout_line("Custom provider registry removed.");
                    Ok(())
                }
            }
        }
        other => anyhow::bail!("unknown provider {other:?}; supported: codex, custom"),
    }
}
