#![allow(missing_docs)]

//! Interactive OpenAI Codex login through the hosted device-code flow.
//!
//! The previous loopback-browser flow could mint a `localhost` credential for
//! an unrecognized originator. OpenAI then routed requests through a reduced
//! model pool and returned misleading model 404s. Device authorization is the
//! same headless-safe flow used by Pi and the Codex CLI and does not depend on a
//! third-party loopback originator.

use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::store::{CredentialFile, CredentialStore, Tokens};
use super::{oauth, DEVICE_CODE_TIMEOUT_SECS, DEVICE_VERIFICATION_URI, MODELS};

/// Run OpenAI's device-code OAuth flow and persist the credential.
///
/// `headless` suppresses the best-effort browser launch. The verification URL
/// and user code are always printed, so the flow works over SSH as well.
pub async fn login(store: &CredentialStore, headless: bool) -> Result<()> {
    let http = reqwest::Client::new();
    let device = oauth::start_device_auth(&http).await?;

    println!(
        "Open this URL and enter the code shown below:\n\n  {DEVICE_VERIFICATION_URI}\n\n  Code: {}\n\nWaiting for authorization…",
        device.user_code
    );
    if !headless {
        open_browser(DEVICE_VERIFICATION_URI);
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(DEVICE_CODE_TIMEOUT_SECS);
    let mut interval = Duration::from_secs(device.interval_seconds);
    let (authorization_code, code_verifier) = loop {
        if tokio::time::Instant::now() >= deadline {
            bail!("device authorization timed out after 15 minutes");
        }
        match oauth::poll_device_auth(&http, &device).await? {
            oauth::DevicePoll::Complete {
                authorization_code,
                code_verifier,
            } => break (authorization_code, code_verifier),
            oauth::DevicePoll::Pending => {}
            oauth::DevicePoll::SlowDown => {
                interval = interval.saturating_add(Duration::from_secs(5));
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::sleep(interval.min(remaining)).await;
    };

    let tokens = oauth::exchange_device_code(&http, &authorization_code, &code_verifier)
        .await
        .context("exchanging the device authorization failed")?;
    let account_id = oauth::validate_subscription_token(&tokens.access)
        .context("OpenAI returned a credential that cannot access the ChatGPT model pool")?;

    store.save(&CredentialFile {
        tokens: Tokens {
            access_token: tokens.access,
            refresh_token: tokens.refresh,
            account_id,
        },
        expires_at: tokens.expires_at,
    })?;

    println!(
        "\nSigned in to OpenAI Codex. Available models: {}.\nSelect one with `ygg --model {}` or set `model = \"{}\"` in ~/.ygg/config.toml.",
        MODELS.join(", "),
        MODELS[0],
        MODELS[0],
    );
    Ok(())
}

/// Remove the stored credential.
pub fn logout(store: &CredentialStore) -> Result<()> {
    store.delete()?;
    println!("Signed out of OpenAI Codex.");
    Ok(())
}

/// Best-effort open of the hosted verification page.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}
