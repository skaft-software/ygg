//! Lightweight local setup diagnostics.
//!
//! `doctor` is intentionally a read-mostly command. It performs the same
//! provider catalog construction as startup (including optional discovery), but
//! does not create an Agent, open a model session, or start extensions.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::process::Command;

use crate::app::bootstrap::model_catalog_with_offline;
use crate::config::Config;

/// Print local prerequisites and provider/model visibility.
pub fn run(config: &Config) -> anyhow::Result<()> {
    let catalog = model_catalog_with_offline(config.offline)?;
    let mut lines = vec![
        format!("Ygg doctor {}", env!("CARGO_PKG_VERSION")),
        format!("workspace: {}", config.workspace.display()),
        format!("session directory: {}", config.session_dir.display()),
        format!(
            "provider discovery: {}",
            if config.offline {
                "disabled (--offline)"
            } else {
                "enabled"
            }
        ),
        format!(
            "telemetry: {}",
            config
                .telemetry
                .as_ref()
                .map_or("disabled".to_owned(), |path| path.display().to_string())
        ),
    ];

    match rg_version() {
        Some(version) => lines.push(format!("ripgrep: ok ({version})")),
        None => lines.push("ripgrep: MISSING (install rg before starting Ygg)".to_owned()),
    }

    let mut endpoints: BTreeMap<String, EndpointSummary> = BTreeMap::new();
    for spec in catalog.models() {
        let Ok(model) = catalog.resolve(&spec.id) else {
            continue;
        };
        let key = model.endpoint.id.0.clone();
        let summary = endpoints.entry(key).or_insert_with(|| EndpointSummary {
            display: endpoint_display(&model.endpoint.base_url),
            local: is_local_endpoint(&model.endpoint.base_url),
            models: 0,
        });
        summary.models += 1;
    }

    let model_count = catalog.models().count();
    let local_model_count = catalog
        .models()
        .filter_map(|spec| catalog.resolve(&spec.id).ok())
        .filter(|model| is_local_endpoint(&model.endpoint.base_url))
        .count();
    lines.push(format!(
        "models visible: {model_count} ({local_model_count} local endpoint models)"
    ));
    if endpoints.is_empty() {
        lines.push("providers: none".to_owned());
    } else {
        lines.push("providers:".to_owned());
        for (id, summary) in endpoints {
            lines.push(format!(
                "  {id}: {} · {} model{}",
                summary.display,
                summary.models,
                if summary.models == 1 { "" } else { "s" }
            ));
            if summary.local {
                lines.push("    local/private candidate: no cloud account is implied".to_owned());
            }
        }
    }

    let mut issues = Vec::new();
    if rg_version().is_none() {
        issues.push("ripgrep is unavailable".to_owned());
    }
    if model_count == 0 {
        issues.push(
            "no usable models are configured; set a provider credential or run `ygg --login custom`"
                .to_owned(),
        );
    }
    if let Some(model) = config.model.as_ref() {
        match catalog.resolve(model) {
            Ok(resolved) => lines.push(format!(
                "selected model: {} via {}",
                resolved.spec.id.0, resolved.endpoint.id.0
            )),
            Err(_) => issues.push(format!(
                "configured model `{}` is not visible; check credentials, offline mode, and model metadata",
                model.0
            )),
        }
    } else {
        lines.push("selected model: none (pass --model or configure model=...)".to_owned());
    }

    if config.session_dir.exists() {
        lines.push("session directory: present".to_owned());
    } else {
        lines.push("session directory: not created yet (normal before first run)".to_owned());
    }

    if issues.is_empty() {
        lines.push("result: PASS".to_owned());
    } else {
        lines.push(format!("result: FAIL · {}", issues.join("; ")));
    }
    crate::output::stdout_multiline(lines.join("\n"));
    if issues.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("doctor found {} issue(s)", issues.len())
    }
}

struct EndpointSummary {
    display: String,
    local: bool,
    models: usize,
}

fn rg_version() -> Option<String> {
    let output = Command::new("rg").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
}

fn endpoint_display(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or("<unknown-host>");
    let port = url.port().map_or(String::new(), |port| format!(":{port}"));
    format!("{}://{host}{port}", url.scheme())
}

fn is_local_endpoint(url: &url::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        return true;
    }
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_display_never_includes_path_or_query() {
        let url: url::Url = "http://localhost:8000/v1/?secret=hidden".parse().unwrap();
        assert_eq!(endpoint_display(&url), "http://localhost:8000");
    }

    #[test]
    fn local_endpoint_detection_is_conservative_and_explicit() {
        assert!(is_local_endpoint(
            &"http://127.0.0.1:8000/".parse().unwrap()
        ));
        assert!(is_local_endpoint(&"http://localhost/".parse().unwrap()));
        assert!(is_local_endpoint(&"http://server.local/".parse().unwrap()));
        assert!(!is_local_endpoint(
            &"https://example.invalid/".parse().unwrap()
        ));
    }
}
