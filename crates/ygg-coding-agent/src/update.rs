//! Explicit, bounded release update check.

use std::time::Duration;

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/skaft-software/ygg/releases/latest";
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, serde::Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateStatus {
    Current {
        version: semver::Version,
    },
    Available {
        current: semver::Version,
        latest: semver::Version,
        url: String,
    },
}

impl std::fmt::Display for UpdateStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Current { version } => write!(formatter, "Ygg {version} is up to date."),
            Self::Available {
                current,
                latest,
                url,
            } => write!(
                formatter,
                "Ygg {latest} is available (current: {current}).\n{url}"
            ),
        }
    }
}

pub(crate) async fn check() -> anyhow::Result<UpdateStatus> {
    check_url(LATEST_RELEASE_URL, env!("CARGO_PKG_VERSION")).await
}

async fn check_url(url: &str, current: &str) -> anyhow::Result<UpdateStatus> {
    let current = semver::Version::parse(current)?;
    let client = reqwest::Client::builder()
        .connect_timeout(CHECK_TIMEOUT)
        .timeout(CHECK_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(2))
        .build()?;
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, format!("ygg/{current}"))
        .send()
        .await?
        .error_for_status()?;
    let release = response.json::<LatestRelease>().await?;
    let latest = semver::Version::parse(release.tag_name.trim().trim_start_matches('v'))?;
    if latest > current {
        Ok(UpdateStatus::Available {
            current,
            latest,
            url: release.html_url.unwrap_or_else(|| {
                format!(
                    "https://github.com/skaft-software/ygg/releases/tag/{}",
                    release.tag_name
                )
            }),
        })
    } else {
        Ok(UpdateStatus::Current { version: current })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn reports_newer_release_without_treating_older_tags_as_updates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .and(header("user-agent", "ygg/0.1.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v0.2.0",
                "html_url": "https://example.test/ygg/v0.2.0"
            })))
            .mount(&server)
            .await;
        assert!(matches!(
            check_url(&format!("{}/latest", server.uri()), "0.1.1")
                .await
                .unwrap(),
            UpdateStatus::Available { latest, .. } if latest == semver::Version::new(0, 2, 0)
        ));

        let old = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v0.1.0-alpha",
                "html_url": null
            })))
            .mount(&old)
            .await;
        assert!(matches!(
            check_url(&format!("{}/latest", old.uri()), "0.1.1")
                .await
                .unwrap(),
            UpdateStatus::Current { .. }
        ));
    }

    #[tokio::test]
    async fn rejects_malformed_release_metadata() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "not a version"
            })))
            .mount(&server)
            .await;
        assert!(check_url(&server.uri(), "0.1.1").await.is_err());
    }
}
