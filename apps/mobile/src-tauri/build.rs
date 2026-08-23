//! Build-time Tauri configuration for the experimental Ygg companion.

use std::path::Path;

/// The web bundle lives outside this crate, so Cargo does not fingerprint it on
/// its own. Without these directives a changed frontend silently reuses a stale
/// embedded bundle, whose digests then fail `AssetBundle::verified` at startup.
fn track_frontend_dist(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            track_frontend_dist(&path);
        } else {
            println!("cargo::rerun-if-changed={}", path.display());
        }
    }
}

fn main() {
    println!("cargo::rerun-if-changed=tauri.conf.json");

    let config = std::fs::read_to_string("tauri.conf.json").expect("tauri.conf.json is readable");
    let config: serde_json::Value =
        serde_json::from_str(&config).expect("tauri.conf.json is valid JSON");

    // `tauri-codegen` reparses and re-serializes every embedded HTML file when a
    // CSP is configured, which changes the bytes (doctype case, attribute
    // quoting, whitespace) without changing the meaning. The native proxy
    // verifies embedded assets byte-for-byte against SHA256SUMS, so enabling
    // this setting makes `index.html` fail verification and aborts startup.
    // The proxy already sends the equivalent CSP as a response header.
    assert!(
        config
            .pointer("/app/security/csp")
            .is_none_or(serde_json::Value::is_null),
        "app.security.csp must stay unset: Tauri's CSP injection rewrites embedded HTML and \
         breaks the byte-exact SHA256SUMS check in AssetBundle::verified. The loopback proxy \
         already sends this CSP as a real header via secure_headers()."
    );

    if let Some(dist) = config
        .pointer("/build/frontendDist")
        .and_then(serde_json::Value::as_str)
    {
        track_frontend_dist(Path::new(dist));
    }

    tauri_build::build()
}
