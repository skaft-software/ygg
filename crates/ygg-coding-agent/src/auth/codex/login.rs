#![allow(missing_docs)]

//! Interactive login: browser flow via a loopback callback server, with a
//! headless / paste fallback.

use std::io::Write;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::store::{CredentialFile, CredentialStore, Tokens};
use super::{oauth, CALLBACK_PORT, MODELS};

/// Run the OpenAI Codex login flow and persist the credential.
///
/// `headless` skips the browser/loopback server and prompts for a pasted code
/// or redirect URL — for SSH/remote sessions.
pub async fn login(store: &CredentialStore, headless: bool) -> Result<()> {
    let pkce = oauth::generate_pkce();
    let state = oauth::random_state();
    let auth_url = oauth::authorize_url(&pkce.challenge, &state);
    let http = reqwest::Client::new();

    let code = if headless {
        prompt_for_code(&auth_url, &state)?
    } else {
        match browser_login(&auth_url, &state).await {
            Ok(code) => code,
            Err(err) => {
                eprintln!("Browser login unavailable ({err}); falling back to manual entry.");
                prompt_for_code(&auth_url, &state)?
            }
        }
    };

    let tokens = oauth::exchange_code(&http, &code, &pkce.verifier)
        .await
        .context("exchanging the authorization code failed")?;
    let account_id = oauth::decode_account_id(&tokens.access)
        .context("the access token did not carry a chatgpt_account_id claim")?;

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

/// Start the loopback server, open the browser, and await the callback.
async fn browser_login(auth_url: &str, state: &str) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .with_context(|| format!("binding 127.0.0.1:{CALLBACK_PORT}"))?;

    open_browser(auth_url);
    println!("Opening your browser to sign in. If it doesn't open, visit:\n\n{auth_url}\n\nWaiting for the callback…");

    tokio::time::timeout(Duration::from_secs(300), accept_callback(&listener, state))
        .await
        .map_err(|_| anyhow!("timed out waiting for the OAuth callback"))?
}

/// Accept connections until a valid `/auth/callback?code=&state=` arrives.
async fn accept_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut buf = [0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");

        let outcome = interpret_callback(target, expected_state);
        let (status, body) = match &outcome {
            CallbackOutcome::Ok(_) => (
                "200 OK",
                "<h2>OpenAI sign-in complete.</h2><p>You can close this window and return to ygg.</p>",
            ),
            CallbackOutcome::NotCallback => ("404 Not Found", "<h2>Not found.</h2>"),
            CallbackOutcome::Error(msg) => ("400 Bad Request", msg.as_str()),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;

        match outcome {
            CallbackOutcome::Ok(code) => return Ok(code),
            CallbackOutcome::NotCallback => continue, // favicon, etc.
            CallbackOutcome::Error(msg) => bail!(msg),
        }
    }
}

/// Result of inspecting one inbound HTTP request target.
#[derive(Debug, PartialEq, Eq)]
enum CallbackOutcome {
    Ok(String),
    NotCallback,
    Error(String),
}

/// Pure: classify a request target line (`/auth/callback?code=…&state=…`).
fn interpret_callback(target: &str, expected_state: &str) -> CallbackOutcome {
    let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else {
        return CallbackOutcome::NotCallback;
    };
    if url.path() != "/auth/callback" {
        return CallbackOutcome::NotCallback;
    }
    let mut code = None;
    let mut state = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }
    match (code, state) {
        (_, Some(s)) if s != expected_state => {
            CallbackOutcome::Error("<h2>State mismatch.</h2>".into())
        }
        (Some(code), _) => CallbackOutcome::Ok(code),
        (None, _) => CallbackOutcome::Error("<h2>Missing authorization code.</h2>".into()),
    }
}

/// Blocking prompt used by headless mode / browser fallback.
fn prompt_for_code(auth_url: &str, expected_state: &str) -> Result<String> {
    println!("Open this URL in a browser, authorize, then paste the code or the full redirect URL:\n\n{auth_url}\n");
    print!("Authorization code / redirect URL: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading the pasted code")?;
    let parsed = oauth::parse_authorization_input(&line);
    if let Some(state) = parsed.state {
        if state != expected_state {
            bail!("state mismatch in the pasted redirect URL");
        }
    }
    parsed
        .code
        .filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow!("no authorization code found in the input"))
}

/// Best-effort open of the system browser.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpret_callback_success() {
        assert_eq!(
            interpret_callback("/auth/callback?code=abc&state=s1", "s1"),
            CallbackOutcome::Ok("abc".into())
        );
    }

    #[test]
    fn interpret_callback_rejects_state_mismatch() {
        assert!(matches!(
            interpret_callback("/auth/callback?code=abc&state=bad", "s1"),
            CallbackOutcome::Error(_)
        ));
    }

    #[test]
    fn interpret_callback_ignores_other_paths() {
        assert_eq!(
            interpret_callback("/favicon.ico", "s1"),
            CallbackOutcome::NotCallback
        );
    }

    #[test]
    fn interpret_callback_missing_code_is_error() {
        assert!(matches!(
            interpret_callback("/auth/callback?state=s1", "s1"),
            CallbackOutcome::Error(_)
        ));
    }
}
