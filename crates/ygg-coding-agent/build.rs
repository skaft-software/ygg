#![allow(missing_docs)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::{Builder, Header};

const TEXT_EXTENSIONS: &[&str] = &[
    "md", "toml", "py", "json", "txt", "yaml", "yml", "sha256", "sh",
];

fn should_include(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == "README.md" || name == "SHA256SUMS")
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| TEXT_EXTENSIONS.contains(&extension))
}

fn sorted_entries(path: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn should_skip_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            ".git"
                | ".catalog"
                | ".pytest_cache"
                | "__pycache__"
                | "artifacts"
                | "private"
                | "target"
        )
    ) || path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn append_path_entry(
    builder: &mut Builder<GzEncoder<File>>,
    source: &Path,
    archive_path: &Path,
    metadata: &Metadata,
) -> io::Result<()> {
    let mut header = Header::new_gnu();
    if metadata.is_dir() {
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
    } else {
        header.set_mode(if metadata.permissions().readonly() {
            0o444
        } else {
            0o644
        });
        header.set_size(metadata.len());
    }
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);

    if metadata.is_dir() {
        builder.append_data(&mut header, archive_path, io::empty())
    } else {
        let mut file = File::open(source)?;
        builder.append_data(&mut header, archive_path, &mut file)
    }
}

fn append_directory(
    builder: &mut Builder<GzEncoder<File>>,
    source: &Path,
    archive_path: &Path,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("documentation asset is a symlink: {}", source.display()),
        ));
    }
    if metadata.is_dir() {
        if should_skip_directory(source) {
            return Ok(());
        }
        append_path_entry(builder, source, archive_path, &metadata)?;

        for entry in sorted_entries(source)? {
            let child = entry.path();
            let child_archive_path = archive_path.join(entry.file_name());
            let child_metadata = fs::symlink_metadata(&child)?;
            if child_metadata.is_dir() {
                append_directory(builder, &child, &child_archive_path)?;
            } else if child_metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("documentation asset is a symlink: {}", child.display()),
                ));
            } else if child_metadata.is_file() && should_include(&child) {
                append_path_entry(builder, &child, &child_archive_path, &child_metadata)?;
            }
        }
        Ok(())
    } else if metadata.is_file() && should_include(source) {
        append_path_entry(builder, source, archive_path, &metadata)
    } else {
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderManifest {
    schema_version: u32,
    providers: Vec<ProviderSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderSpec {
    const_name: String,
    id: String,
    name: String,
    base_url: String,
    #[serde(default)]
    base_url_environment: Vec<String>,
    authentication: AuthenticationSpec,
    runtime_configuration: Option<String>,
    model_discovery: DiscoverySpec,
    discovery_capabilities: String,
    static_models: String,
    inventory_cache: String,
    routes: Vec<RouteSpec>,
    route_rules: Vec<RouteRuleSpec>,
    extra_headers: Vec<[String; 2]>,
    compatibility: String,
    pricing: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum AuthenticationSpec {
    Environment { variables: Vec<String> },
    Aws { variables: Vec<String> },
    Subscription { login: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoverySpec {
    kind: String,
    filter: Option<FilterSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilterSpec {
    kind: String,
    values: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteSpec {
    endpoint_id: String,
    #[serde(default)]
    base_path: String,
    protocol: String,
    transport: String,
    body_encoding: String,
    responses_profile: String,
    #[serde(default = "default_openai_chat_profile")]
    openai_chat_profile: String,
    auth_presentation: String,
    auth_header: Option<String>,
}

fn default_openai_chat_profile() -> String {
    "default".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteRuleSpec {
    kind: String,
    value: Option<String>,
    prefix: Option<String>,
    suffix: Option<String>,
    fragment: Option<String>,
    route: Option<usize>,
}

fn provider_manifest_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn valid_provider_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_constant_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        && value.as_bytes()[0].is_ascii_uppercase()
}

fn valid_provider_label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn valid_environment_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_base_url(url: &url::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && valid_version_query(url)
        && url.fragment().is_none()
        && url.path().ends_with('/')
}

fn valid_base_url_template(base_url: &str, environment: &[String]) -> bool {
    let mut rendered = base_url.to_owned();
    let mut seen = HashSet::new();
    for variable in environment {
        let placeholder = format!("{{{variable}}}");
        if !valid_environment_name(variable)
            || !seen.insert(variable)
            || rendered.matches(&placeholder).count() != 1
        {
            return false;
        }
        rendered = rendered.replace(&placeholder, "placeholder");
    }
    if rendered.contains('{') || rendered.contains('}') {
        return false;
    }
    url::Url::parse(&rendered).is_ok_and(|url| valid_base_url(&url))
}

fn valid_route_base_path(path: &str) -> bool {
    if path.is_empty() {
        return true;
    }
    if !path.ends_with('/') || path.starts_with('/') {
        return false;
    }
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    !segments.is_empty()
        && segments.iter().all(|segment| {
            segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

fn credential_like_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let compact: String = lower
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(char::from)
        .collect();
    lower.contains("auth")
        || compact.contains("key")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("credential")
        || lower.contains("cookie")
        || lower.contains("password")
}

fn valid_version_query(url: &url::Url) -> bool {
    let Some(query) = url.query() else {
        return true;
    };
    query.len() <= 128
        && url.query_pairs().next().is_some_and(|(name, value)| {
            name == "api-version"
                && !value.is_empty()
                && value.len() <= 96
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
        })
        && url.query_pairs().nth(1).is_none()
}

fn valid_public_header(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(is_http_token_byte)
        && value.len() <= 1024
        && value.is_ascii()
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        && !credential_like_header(name)
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn protocol_expression(value: &str) -> Option<&'static str> {
    match value {
        "openai_responses" => Some("Protocol::OpenAiResponses"),
        "openai_chat" => Some("Protocol::OpenAiChat"),
        "anthropic_messages" => Some("Protocol::AnthropicMessages"),
        "bedrock_converse" => Some("Protocol::BedrockConverse"),
        _ => None,
    }
}

fn transport_expression(value: &str) -> Option<&'static str> {
    match value {
        "http" => Some("EndpointTransport::Http"),
        "websocket_preferred" => Some("EndpointTransport::WebSocketPreferred"),
        _ => None,
    }
}

fn body_encoding_expression(value: &str) -> Option<&'static str> {
    match value {
        "identity" => Some("RequestBodyEncoding::Identity"),
        "zstd" => Some("RequestBodyEncoding::Zstd"),
        _ => None,
    }
}

fn responses_profile_expression(value: &str) -> Option<&'static str> {
    match value {
        "default" => Some("ResponsesRuntimeProfile::Default"),
        "codex" => Some("ResponsesRuntimeProfile::Codex"),
        _ => None,
    }
}

fn openai_chat_profile_expression(value: &str) -> Option<&'static str> {
    match value {
        "default" => Some("OpenAiChatRuntimeProfile::Default"),
        "mistral" => Some("OpenAiChatRuntimeProfile::Mistral"),
        _ => None,
    }
}

fn auth_presentation_expression(route: &RouteSpec) -> Option<String> {
    match route.auth_presentation.as_str() {
        "bearer" if route.auth_header.is_none() => {
            Some("EndpointAuthPresentation::Bearer".to_owned())
        }
        "api_key_header" if route.auth_header.is_none() => {
            Some("EndpointAuthPresentation::ApiKeyHeader".to_owned())
        }
        "cloudflare_ai_gateway" if route.auth_header.is_none() => {
            Some("EndpointAuthPresentation::CloudflareAiGateway".to_owned())
        }
        "header" => route
            .auth_header
            .as_deref()
            .filter(|header| !header.is_empty() && header.bytes().all(is_http_token_byte))
            .map(|header| format!("EndpointAuthPresentation::Header({})", quote(header))),
        "aws_sigv4" if route.auth_header.is_none() => {
            Some("EndpointAuthPresentation::AwsSigV4".to_owned())
        }
        "dynamic" if route.auth_header.is_none() => {
            Some("EndpointAuthPresentation::Dynamic".to_owned())
        }
        _ => None,
    }
}

fn discovery_capabilities_expression(value: &str) -> Option<&'static str> {
    match value {
        "default" => Some("DiscoveryCapabilityProfile::Default"),
        "gpt_vision_fallback" => Some("DiscoveryCapabilityProfile::GptVisionFallback"),
        "assume_image_input" => Some("DiscoveryCapabilityProfile::AssumeImageInput"),
        _ => None,
    }
}

fn static_models_expression(value: &str) -> Option<&'static str> {
    match value {
        "none" => Some("StaticModelSet::None"),
        "minimax" => Some("StaticModelSet::MiniMax"),
        "opencode" => Some("StaticModelSet::OpenCode"),
        "mistral" => Some("StaticModelSet::Mistral"),
        "cloudflare_workers_ai" => Some("StaticModelSet::CloudflareWorkersAi"),
        "cloudflare_ai_gateway" => Some("StaticModelSet::CloudflareAiGateway"),
        "bedrock" => Some("StaticModelSet::Bedrock"),
        _ => None,
    }
}

fn runtime_configuration_expression(value: Option<&str>) -> Option<&'static str> {
    match value.unwrap_or("default") {
        "default" => Some("ProviderRuntimeConfiguration::Default"),
        "aws_bedrock" => Some("ProviderRuntimeConfiguration::AwsBedrock"),
        "azure_openai" => Some("ProviderRuntimeConfiguration::AzureOpenAi"),
        _ => None,
    }
}

fn cache_mode_expression(value: &str) -> Option<&'static str> {
    match value {
        "required" => Some("InventoryCacheMode::Required"),
        "supplemental" => Some("InventoryCacheMode::Supplemental"),
        _ => None,
    }
}

fn compatibility_expression(value: &str) -> Option<&'static str> {
    match value {
        "default" => Some("CompatibilityProfile::Default"),
        "openai" => Some("CompatibilityProfile::OpenAi"),
        "openrouter" => Some("CompatibilityProfile::OpenRouter"),
        "short_retention" => Some("CompatibilityProfile::ShortRetention"),
        "fireworks" => Some("CompatibilityProfile::Fireworks"),
        "opencode" => Some("CompatibilityProfile::OpenCode"),
        "custom" => Some("CompatibilityProfile::Custom"),
        "codex" => Some("CompatibilityProfile::Codex"),
        "mistral" => Some("CompatibilityProfile::Mistral"),
        "cloudflare" => Some("CompatibilityProfile::Cloudflare"),
        _ => None,
    }
}

fn pricing_expression(value: &str) -> Option<&'static str> {
    match value {
        "reference" => Some("PricingProfile::Reference"),
        "openai" => Some("PricingProfile::OpenAi"),
        "anthropic" => Some("PricingProfile::Anthropic"),
        "deepseek" => Some("PricingProfile::DeepSeek"),
        "minimax" => Some("PricingProfile::MiniMax"),
        "opencode" => Some("PricingProfile::OpenCode"),
        "openrouter" => Some("PricingProfile::OpenRouter"),
        "custom" => Some("PricingProfile::Custom"),
        "subscription" => Some("PricingProfile::Subscription"),
        "mistral" => Some("PricingProfile::Mistral"),
        "cloudflare_workers_ai" => Some("PricingProfile::CloudflareWorkersAi"),
        "cloudflare_ai_gateway" => Some("PricingProfile::CloudflareAiGateway"),
        _ => None,
    }
}

fn quote(value: &str) -> String {
    format!("{value:?}")
}

fn discovery_expression(spec: &DiscoverySpec) -> Result<String, io::Error> {
    let expression = match spec.kind.as_str() {
        "static" => {
            if spec.filter.is_some() {
                return Err(provider_manifest_error(
                    "static discovery cannot have a filter",
                ));
            }
            "ModelDiscovery::Static".to_owned()
        }
        "openai_models" => {
            let Some(filter) = spec.filter.as_ref() else {
                return Err(provider_manifest_error(
                    "openai_models discovery requires a filter",
                ));
            };
            let filter = match filter.kind.as_str() {
                "all" if filter.values.is_none() => "ModelFilter::All".to_owned(),
                "prefix" => {
                    let Some(values) = filter.values.as_ref() else {
                        return Err(provider_manifest_error(
                            "prefix model filter requires values",
                        ));
                    };
                    if values.is_empty() || values.iter().any(|value| value.is_empty()) {
                        return Err(provider_manifest_error("prefix model filter is invalid"));
                    }
                    let values = values
                        .iter()
                        .map(|value| quote(value))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("ModelFilter::Prefix(&[{values}])")
                }
                _ => return Err(provider_manifest_error("unknown model filter")),
            };
            format!("ModelDiscovery::OpenAiModels {{ filter: {filter} }}")
        }
        "anthropic_models" => {
            let Some(filter) = spec.filter.as_ref() else {
                return Err(provider_manifest_error(
                    "anthropic_models discovery requires a filter",
                ));
            };
            let filter = match filter.kind.as_str() {
                "all" if filter.values.is_none() => "ModelFilter::All".to_owned(),
                "prefix" => {
                    let Some(values) = filter.values.as_ref() else {
                        return Err(provider_manifest_error(
                            "prefix model filter requires values",
                        ));
                    };
                    if values.is_empty() || values.iter().any(|value| value.is_empty()) {
                        return Err(provider_manifest_error("prefix model filter is invalid"));
                    }
                    let values = values
                        .iter()
                        .map(|value| quote(value))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("ModelFilter::Prefix(&[{values}])")
                }
                _ => return Err(provider_manifest_error("unknown model filter")),
            };
            format!("ModelDiscovery::AnthropicModels {{ filter: {filter} }}")
        }
        "openrouter_models" => {
            if spec.filter.is_some() {
                return Err(provider_manifest_error(
                    "openrouter discovery cannot have a filter",
                ));
            }
            "ModelDiscovery::OpenRouterModels".to_owned()
        }
        "deepseek_models" => {
            if spec.filter.is_some() {
                return Err(provider_manifest_error(
                    "deepseek discovery cannot have a filter",
                ));
            }
            "ModelDiscovery::DeepSeekModels".to_owned()
        }
        "codex_subscription" => {
            if spec.filter.is_some() {
                return Err(provider_manifest_error(
                    "subscription discovery cannot have a filter",
                ));
            }
            "ModelDiscovery::CodexSubscription".to_owned()
        }
        "none" => {
            if spec.filter.is_some() {
                return Err(provider_manifest_error(
                    "none discovery cannot have a filter",
                ));
            }
            "ModelDiscovery::None".to_owned()
        }
        _ => return Err(provider_manifest_error("unknown model discovery")),
    };
    Ok(expression)
}

fn route_rule_expression(spec: &RouteRuleSpec, routes: usize) -> Result<String, io::Error> {
    let route = |value: Option<usize>| -> Result<usize, io::Error> {
        let Some(value) = value else {
            return Err(provider_manifest_error("route rule has no route index"));
        };
        if value >= routes {
            return Err(provider_manifest_error(
                "route rule references a missing route",
            ));
        }
        Ok(value)
    };
    match spec.kind.as_str() {
        "exclude_exact" => {
            let Some(value) = spec.value.as_deref().filter(|value| !value.is_empty()) else {
                return Err(provider_manifest_error("exclude_exact rule has no value"));
            };
            if spec.route.is_some()
                || spec.prefix.is_some()
                || spec.suffix.is_some()
                || spec.fragment.is_some()
            {
                return Err(provider_manifest_error(
                    "exclude_exact rule has unexpected fields",
                ));
            }
            Ok(format!("ModelRouteRule::ExcludeExact({})", quote(value)))
        }
        "exclude_prefix" => {
            let Some(value) = spec.value.as_deref().filter(|value| !value.is_empty()) else {
                return Err(provider_manifest_error("exclude_prefix rule has no value"));
            };
            if spec.route.is_some()
                || spec.prefix.is_some()
                || spec.suffix.is_some()
                || spec.fragment.is_some()
            {
                return Err(provider_manifest_error(
                    "exclude_prefix rule has unexpected fields",
                ));
            }
            Ok(format!("ModelRouteRule::ExcludePrefix({})", quote(value)))
        }
        "select_prefix" => {
            let Some(prefix) = spec.prefix.as_deref().filter(|value| !value.is_empty()) else {
                return Err(provider_manifest_error("select_prefix rule has no prefix"));
            };
            if spec.value.is_some() || spec.suffix.is_some() || spec.fragment.is_some() {
                return Err(provider_manifest_error(
                    "select_prefix rule has unexpected fields",
                ));
            }
            Ok(format!(
                "ModelRouteRule::SelectPrefix {{ prefix: {}, route: {} }}",
                quote(prefix),
                route(spec.route)?
            ))
        }
        "select_ascii_insensitive_contains" => {
            let Some(fragment) = spec.fragment.as_deref().filter(|value| !value.is_empty()) else {
                return Err(provider_manifest_error(
                    "select_ascii_insensitive_contains rule has no fragment",
                ));
            };
            if spec.value.is_some() || spec.prefix.is_some() || spec.suffix.is_some() {
                return Err(provider_manifest_error(
                    "select_ascii_insensitive_contains rule has unexpected fields",
                ));
            }
            Ok(format!(
                "ModelRouteRule::SelectAsciiInsensitiveContains {{ fragment: {}, route: {} }}",
                quote(fragment),
                route(spec.route)?
            ))
        }
        "select_prefix_and_suffix" => {
            let Some(prefix) = spec.prefix.as_deref().filter(|value| !value.is_empty()) else {
                return Err(provider_manifest_error(
                    "select_prefix_and_suffix rule has no prefix",
                ));
            };
            let Some(suffix) = spec.suffix.as_deref().filter(|value| !value.is_empty()) else {
                return Err(provider_manifest_error(
                    "select_prefix_and_suffix rule has no suffix",
                ));
            };
            if spec.value.is_some() || spec.fragment.is_some() {
                return Err(provider_manifest_error(
                    "select_prefix_and_suffix rule has unexpected fields",
                ));
            }
            Ok(format!(
                "ModelRouteRule::SelectPrefixAndSuffix {{ prefix: {}, suffix: {}, route: {} }}",
                quote(prefix),
                quote(suffix),
                route(spec.route)?
            ))
        }
        "default" => {
            if spec.value.is_some()
                || spec.prefix.is_some()
                || spec.suffix.is_some()
                || spec.fragment.is_some()
            {
                return Err(provider_manifest_error(
                    "default route rule has unexpected fields",
                ));
            }
            Ok(format!(
                "ModelRouteRule::Default {{ route: {} }}",
                route(spec.route)?
            ))
        }
        _ => Err(provider_manifest_error("unknown provider route rule")),
    }
}

fn validate_provider(spec: &ProviderSpec) -> Result<(), io::Error> {
    if !valid_constant_name(&spec.const_name) || !valid_provider_identifier(&spec.id) {
        return Err(provider_manifest_error(
            "provider declaration has an invalid identifier",
        ));
    }
    if !valid_provider_label(&spec.name) {
        return Err(provider_manifest_error(
            "provider declaration has an invalid label",
        ));
    }
    if !valid_base_url_template(&spec.base_url, &spec.base_url_environment) {
        return Err(provider_manifest_error(
            "provider declaration has an invalid base URL",
        ));
    }
    match &spec.authentication {
        AuthenticationSpec::Environment { variables } | AuthenticationSpec::Aws { variables }
            if !variables.is_empty()
                && variables.iter().all(|value| valid_environment_name(value)) => {}
        AuthenticationSpec::Environment { .. } | AuthenticationSpec::Aws { .. } => {
            return Err(provider_manifest_error(
                "provider declaration has an invalid credential environment",
            ));
        }
        AuthenticationSpec::Subscription { login } if valid_provider_identifier(login) => {}
        AuthenticationSpec::Subscription { .. } => {
            return Err(provider_manifest_error(
                "provider declaration has an invalid subscription login",
            ));
        }
    }
    if spec.routes.is_empty() {
        return Err(provider_manifest_error(
            "provider declaration has no routes",
        ));
    }
    for route in &spec.routes {
        if !valid_provider_identifier(&route.endpoint_id)
            || !valid_route_base_path(&route.base_path)
            || protocol_expression(&route.protocol).is_none()
            || transport_expression(&route.transport).is_none()
            || body_encoding_expression(&route.body_encoding).is_none()
            || responses_profile_expression(&route.responses_profile).is_none()
            || openai_chat_profile_expression(&route.openai_chat_profile).is_none()
            || auth_presentation_expression(route).is_none()
        {
            return Err(provider_manifest_error(
                "provider declaration has an invalid route",
            ));
        }
        if route.responses_profile != "default" && route.protocol != "openai_responses" {
            return Err(provider_manifest_error(
                "provider Responses runtime profile requires a Responses route",
            ));
        }
        if route.openai_chat_profile != "default" && route.protocol != "openai_chat" {
            return Err(provider_manifest_error(
                "provider OpenAI Chat runtime profile requires a Chat route",
            ));
        }
        if route.transport == "websocket_preferred" && route.protocol != "openai_responses" {
            return Err(provider_manifest_error(
                "provider WebSocket transport requires a Responses route",
            ));
        }
    }
    for route in &spec.routes {
        let valid_presentation = matches!(
            (&spec.authentication, route.auth_presentation.as_str()),
            (
                AuthenticationSpec::Environment { .. },
                "bearer" | "api_key_header" | "cloudflare_ai_gateway" | "header"
            ) | (AuthenticationSpec::Aws { .. }, "aws_sigv4")
                | (AuthenticationSpec::Subscription { .. }, "dynamic")
        );
        if !valid_presentation {
            return Err(provider_manifest_error(
                "provider declaration has an invalid credential presentation",
            ));
        }
    }
    for (index, route) in spec.routes.iter().enumerate() {
        for previous in &spec.routes[..index] {
            if previous.endpoint_id == route.endpoint_id
                && (previous.base_path != route.base_path
                    || previous.auth_presentation != route.auth_presentation
                    || previous.transport != route.transport
                    || previous.body_encoding != route.body_encoding
                    || previous.responses_profile != route.responses_profile
                    || previous.openai_chat_profile != route.openai_chat_profile)
            {
                return Err(provider_manifest_error(
                    "provider endpoint routes disagree on runtime configuration",
                ));
            }
        }
    }
    if spec.route_rules.is_empty()
        || spec
            .route_rules
            .last()
            .is_none_or(|rule| rule.kind != "default")
        || spec
            .route_rules
            .iter()
            .filter(|rule| rule.kind == "default")
            .count()
            != 1
    {
        return Err(provider_manifest_error(
            "provider declaration requires one final default route rule",
        ));
    }
    let mut generated_rules = HashSet::new();
    for rule in &spec.route_rules {
        let generated = route_rule_expression(rule, spec.routes.len())?;
        if !generated_rules.insert(generated) {
            return Err(provider_manifest_error(
                "provider declaration contains duplicate route rules",
            ));
        }
    }
    if discovery_capabilities_expression(&spec.discovery_capabilities).is_none()
        || static_models_expression(&spec.static_models).is_none()
        || runtime_configuration_expression(spec.runtime_configuration.as_deref()).is_none()
        || cache_mode_expression(&spec.inventory_cache).is_none()
        || compatibility_expression(&spec.compatibility).is_none()
        || pricing_expression(&spec.pricing).is_none()
    {
        return Err(provider_manifest_error(
            "provider declaration has an unknown profile",
        ));
    }
    discovery_expression(&spec.model_discovery)?;
    if spec
        .extra_headers
        .iter()
        .any(|header| !valid_public_header(&header[0], &header[1]))
    {
        return Err(provider_manifest_error(
            "provider declaration contains a credential-like or invalid header",
        ));
    }
    Ok(())
}

fn authentication_expression(spec: &AuthenticationSpec) -> String {
    match spec {
        AuthenticationSpec::Environment { variables } => {
            let variables = variables
                .iter()
                .map(|value| quote(value))
                .collect::<Vec<_>>()
                .join(", ");
            format!("ProviderAuthentication::Environment {{ variables: &[{variables}] }}")
        }
        AuthenticationSpec::Aws { variables } => {
            let variables = variables
                .iter()
                .map(|value| quote(value))
                .collect::<Vec<_>>()
                .join(", ");
            format!("ProviderAuthentication::Aws {{ variables: &[{variables}] }}")
        }
        AuthenticationSpec::Subscription { login } => format!(
            "ProviderAuthentication::Subscription {{ login: {} }}",
            quote(login)
        ),
    }
}

fn generate_provider_declarations(manifest_dir: &Path, out_dir: &Path) -> io::Result<()> {
    let manifest_path = manifest_dir.join("src/providers/declarations.json");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let source = fs::read_to_string(&manifest_path)?;
    let manifest: ProviderManifest = serde_json::from_str(&source).map_err(|error| {
        provider_manifest_error(format!(
            "provider declaration manifest is invalid JSON: {error}"
        ))
    })?;
    if manifest.schema_version != 2 || manifest.providers.is_empty() {
        return Err(provider_manifest_error(
            "provider declaration manifest has an unsupported schema version",
        ));
    }
    let mut ids = HashSet::new();
    let mut constants = HashSet::new();
    let mut endpoint_ids = HashSet::new();
    for provider in &manifest.providers {
        validate_provider(provider)?;
        if !ids.insert(&provider.id) || !constants.insert(&provider.const_name) {
            return Err(provider_manifest_error(
                "provider declaration manifest contains duplicate identities",
            ));
        }
        let mut provider_endpoint_ids = HashSet::new();
        for route in &provider.routes {
            if provider_endpoint_ids.insert(&route.endpoint_id)
                && !endpoint_ids.insert(&route.endpoint_id)
            {
                return Err(provider_manifest_error(
                    "provider declaration manifest contains duplicate endpoint identities",
                ));
            }
        }
    }

    let mut generated = String::from(
        "// @generated by build.rs from src/providers/declarations.json; do not edit.\n\n",
    );
    for provider in &manifest.providers {
        writeln!(
            generated,
            "pub const {}: ProviderDeclaration = ProviderDeclaration {{",
            provider.const_name
        )
        .expect("writing to String cannot fail");
        writeln!(generated, "    id: {},", quote(&provider.id))
            .expect("writing to String cannot fail");
        writeln!(generated, "    name: {},", quote(&provider.name))
            .expect("writing to String cannot fail");
        writeln!(generated, "    base_url: {},", quote(&provider.base_url))
            .expect("writing to String cannot fail");
        let base_url_environment = provider
            .base_url_environment
            .iter()
            .map(|value| quote(value))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            generated,
            "    base_url_environment: &[{base_url_environment}],"
        )
        .expect("writing to String cannot fail");
        writeln!(
            generated,
            "    authentication: {},",
            authentication_expression(&provider.authentication)
        )
        .expect("writing to String cannot fail");
        writeln!(
            generated,
            "    runtime_configuration: {},",
            runtime_configuration_expression(provider.runtime_configuration.as_deref())
                .expect("validated runtime configuration")
        )
        .expect("writing to String cannot fail");
        writeln!(
            generated,
            "    model_discovery: {},",
            discovery_expression(&provider.model_discovery)?
        )
        .expect("writing to String cannot fail");
        writeln!(
            generated,
            "    discovery_capabilities: {},",
            discovery_capabilities_expression(&provider.discovery_capabilities)
                .expect("validated discovery capabilities")
        )
        .expect("writing to String cannot fail");
        writeln!(
            generated,
            "    static_models: {},",
            static_models_expression(&provider.static_models).expect("validated static models")
        )
        .expect("writing to String cannot fail");
        writeln!(
            generated,
            "    inventory_cache: {},",
            cache_mode_expression(&provider.inventory_cache).expect("validated cache mode")
        )
        .expect("writing to String cannot fail");
        generated.push_str("    routes: &[\n");
        for route in &provider.routes {
            writeln!(
                generated,
                "        ProviderRoute {{ endpoint_id: {}, base_path: {}, protocol: {}, auth_presentation: {}, transport: {}, runtime: RequestRuntime {{ body_encoding: {}, responses_profile: {}, openai_chat_profile: {}, lifecycle_feedback: false }} }},",
                quote(&route.endpoint_id),
                quote(&route.base_path),
                protocol_expression(&route.protocol).expect("validated protocol"),
                auth_presentation_expression(route)
                    .expect("validated auth presentation"),
                transport_expression(&route.transport).expect("validated transport"),
                body_encoding_expression(&route.body_encoding).expect("validated body encoding"),
                responses_profile_expression(&route.responses_profile)
                    .expect("validated responses profile"),
                openai_chat_profile_expression(&route.openai_chat_profile)
                    .expect("validated OpenAI Chat profile"),
            )
            .expect("writing to String cannot fail");
        }
        generated.push_str("    ],\n    route_rules: &[\n");
        for rule in &provider.route_rules {
            writeln!(
                generated,
                "        {},",
                route_rule_expression(rule, provider.routes.len())?
            )
            .expect("writing to String cannot fail");
        }
        generated.push_str("    ],\n    extra_headers: &[\n");
        for [name, value] in &provider.extra_headers {
            writeln!(generated, "        ({}, {}),", quote(name), quote(value))
                .expect("writing to String cannot fail");
        }
        generated.push_str("    ],\n");
        writeln!(
            generated,
            "    compatibility: {},",
            compatibility_expression(&provider.compatibility).expect("validated compatibility")
        )
        .expect("writing to String cannot fail");
        writeln!(
            generated,
            "    pricing: {},",
            pricing_expression(&provider.pricing).expect("validated pricing")
        )
        .expect("writing to String cannot fail");
        generated.push_str("};\n\n");
    }
    generated.push_str("/// Every generated built-in and subscription provider declaration.\n");
    generated.push_str("pub const ALL_PROVIDER_DECLARATIONS: &[ProviderDeclaration] = &[\n");
    for provider in &manifest.providers {
        writeln!(generated, "    {},", provider.const_name).expect("writing to String cannot fail");
    }
    generated.push_str("];\n\n");
    generated.push_str(
        "/// API-key built-ins; subscription routes are kept separately by auth ownership.\n",
    );
    generated.push_str("pub const BUILTIN_PROVIDER_DECLARATIONS: &[ProviderDeclaration] = &[\n");
    for provider in &manifest.providers {
        if matches!(
            provider.authentication,
            AuthenticationSpec::Environment { .. } | AuthenticationSpec::Aws { .. }
        ) {
            writeln!(generated, "    {},", provider.const_name)
                .expect("writing to String cannot fail");
        }
    }
    generated.push_str("];\n");
    fs::write(out_dir.join("provider_declarations.rs"), generated)
}

fn main() -> io::Result<()> {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("Cargo must provide CARGO_MANIFEST_DIR to the build script"),
    );
    let source_root = manifest_dir.join("../..");
    let out_dir = PathBuf::from(
        std::env::var_os("OUT_DIR").expect("Cargo must provide OUT_DIR to the build script"),
    );
    let archive_path = out_dir.join("ygg-documentation.tar.gz");

    generate_provider_declarations(&manifest_dir, &out_dir)?;

    let canonical_assets_available = ["README.md", "docs", "examples", "sdk"]
        .iter()
        .all(|path| source_root.join(path).exists());
    let roots = if canonical_assets_available {
        vec![
            (source_root.join("README.md"), PathBuf::from("README.md")),
            (source_root.join("docs"), PathBuf::from("docs")),
            (source_root.join("examples"), PathBuf::from("examples")),
            (source_root.join("sdk"), PathBuf::from("sdk")),
        ]
    } else {
        // A crates.io package cannot contain files outside its package root.
        // Keep that package buildable, while git/path installs use the
        // canonical repository assets above and get the complete bundle.
        println!(
            "cargo:warning=Ygg documentation sources are unavailable; packaged binaries will use the published documentation URL"
        );
        vec![(manifest_dir.join("README.md"), PathBuf::from("README.md"))]
    };
    for (source, _) in &roots {
        println!("cargo:rerun-if-changed={}", source.display());
    }

    let output = File::create(&archive_path)?;
    let encoder = GzEncoder::new(output, Compression::best());
    let mut builder = Builder::new(encoder);
    for (source, archive_path_root) in roots {
        append_directory(&mut builder, &source, &archive_path_root)?;
    }
    let encoder = builder.into_inner()?;
    encoder.finish()?.sync_all()?;

    println!(
        "cargo:rustc-env=YGG_EMBEDDED_DOCS_ARCHIVE={}",
        archive_path.display()
    );
    Ok(())
}
