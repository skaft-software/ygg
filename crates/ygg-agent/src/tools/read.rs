//! Bounded text and multimodal file reading.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use serde::Deserialize;
use ygg_ai::{AudioFormat, Media, Mime, ToolDef};

use crate::effect::ToolEffect;
use crate::secure_fs::{read_regular_file_bounded_by, SecureFileError};
use crate::tool::{
    content_hash, ReplaySafety, Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput,
};
use crate::tools::{
    clip_line, parse_args, validate_effect_path, MAX_FILE_BYTES, MAX_TOOL_PATH_BYTES,
};
/// Display cap for a single line.
const MAX_LINE_CHARS: usize = 2000;
/// Default number of lines returned when `limit` is omitted.
const DEFAULT_LIMIT: usize = 500;
/// Conservative inline-image cap shared across supported provider paths.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Conservative inline-audio cap. Gemini's inline request limit is 20 MB.
const MAX_AUDIO_BYTES: usize = 20 * 1024 * 1024;
const REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_READ_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_DNS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq)]
enum MediaKind {
    Image(Mime),
    Audio(AudioFormat),
}

impl MediaKind {
    fn byte_limit(&self) -> usize {
        match self {
            Self::Image(_) => MAX_IMAGE_BYTES,
            Self::Audio(_) => MAX_AUDIO_BYTES,
        }
    }

    fn media_type(&self) -> &'static str {
        match self {
            Self::Image(mime) if mime.essence_str() == "image/png" => "image/png",
            Self::Image(mime) if mime.essence_str() == "image/jpeg" => "image/jpeg",
            Self::Image(mime) if mime.essence_str() == "image/gif" => "image/gif",
            Self::Image(mime) if mime.essence_str() == "image/webp" => "image/webp",
            Self::Image(_) => "image",
            Self::Audio(AudioFormat::Wav) => "audio/wav",
            Self::Audio(AudioFormat::Aac) => "audio/aac",
            Self::Audio(AudioFormat::Mp3) => "audio/mpeg",
            Self::Audio(AudioFormat::Flac) => "audio/flac",
            Self::Audio(AudioFormat::Opus) => "audio/opus",
            Self::Audio(AudioFormat::Pcm16) => "audio/pcm",
        }
    }

    fn into_media(self, bytes: Vec<u8>) -> Media {
        match self {
            Self::Image(mime) => Media::image_bytes(Bytes::from(bytes), mime),
            Self::Audio(format) => Media::audio_bytes(Bytes::from(bytes), format),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

/// The built-in `read` tool: bounded text, image, and audio reads.
pub struct ReadTool;

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read".to_string(),
            description: "Read text, images, or audio. `path` may be a workspace-relative local path, \
                          an absolute/~/ path when trusted-local access is enabled, a local `file://` \
                          URL, or an HTTPS image/audio URL when remote reads are explicitly enabled. \
                          Text returns numbered lines plus a whole-file \
                          hash and continuation metadata. Image/audio returns bounded structured media \
                          for protocol-aware ingestion and a payload-free summary for the TUI; the active \
                          model may reject a recognized audio format it cannot accept. Existing bracketed \
                          [Image #N]/[Audio #N] attachments are already included in the prompt and must \
                          not be read again."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Local file path, file:// URL, or (when explicitly enabled) an HTTPS image/audio URL."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-indexed line to start from (default 1)."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum lines to return (default 500)."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(
        &self,
        arguments: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolEffect, ToolError> {
        let arguments = arguments
            .as_object()
            .ok_or_else(|| ToolError::new("invalid arguments: expected an object"))?;
        if arguments.len() > 3
            || arguments
                .keys()
                .any(|key| !matches!(key.as_str(), "path" | "offset" | "limit"))
        {
            return Err(ToolError::new("invalid arguments: unknown property"));
        }
        let path = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::new("invalid arguments: `path` must be a string"))?;
        if path.is_empty() {
            return Err(ToolError::new(
                "invalid arguments: `path` must be non-empty",
            ));
        }
        if path.len() > MAX_TOOL_PATH_BYTES {
            return Err(ToolError::new(format!(
                "invalid arguments: `path` is {} bytes (limit {MAX_TOOL_PATH_BYTES})",
                path.len()
            )));
        }
        if path.contains('\0') {
            return Err(ToolError::new(
                "invalid arguments: `path` must not contain NUL",
            ));
        }
        for name in ["offset", "limit"] {
            if arguments.get(name).is_some_and(|value| {
                value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .is_none_or(|value| value == 0)
            }) {
                return Err(ToolError::new(format!(
                    "invalid arguments: `{name}` must be a positive integer"
                )));
            }
        }
        if let Some(url) = parse_url_source(path)? {
            let test_loopback_http = cfg!(test)
                && url.scheme() == "http"
                && url.host().is_some_and(|host| match host {
                    url::Host::Ipv4(address) => address.is_loopback(),
                    url::Host::Ipv6(address) => address.is_loopback(),
                    url::Host::Domain(_) => false,
                });
            match url.scheme() {
                "http" if test_loopback_http && !ctx.sandbox.allow_remote_read => {
                    return Err(ToolError::new(
                        "remote URL reads are disabled; enable `allow_remote_read` \
                         (or pass `--allow-remote-read`) to permit public HTTPS image/audio fetches",
                    ));
                }
                "http" if test_loopback_http => return Ok(ToolEffect::Network),
                "http" => {
                    return Err(ToolError::new("remote media URLs must use HTTPS"));
                }
                "https" if !ctx.sandbox.allow_remote_read => {
                    return Err(ToolError::new(
                        "remote URL reads are disabled; enable `allow_remote_read` \
                         (or pass `--allow-remote-read`) to permit public HTTPS image/audio fetches",
                    ));
                }
                "https" => return Ok(ToolEffect::Network),
                "file" => {
                    // Convert URL syntax lexically without resolving or probing
                    // the model-selected host path before policy admission.
                    let local = local_path_from_url(&url)?;
                    let local = workspace_relative_file_url_path(local, ctx);
                    validate_effect_path(&local, ctx.sandbox.allow_external_paths)?;
                    return Ok(if ctx.sandbox.allow_external_paths {
                        ToolEffect::HostRead
                    } else {
                        ToolEffect::WorkspaceRead
                    });
                }
                scheme => {
                    return Err(ToolError::new(format!(
                        "unsupported read URL scheme `{scheme}`; use file or https"
                    )))
                }
            }
        }
        validate_effect_path(path, ctx.sandbox.allow_external_paths)?;
        Ok(if ctx.sandbox.allow_external_paths {
            // Resolution follows symlinks and can disclose host-path existence.
            // Classify every local request by maximum ambient authority before
            // touching the filesystem.
            ToolEffect::HostRead
        } else {
            ToolEffect::WorkspaceRead
        })
    }

    fn replay_safety(&self) -> ReplaySafety {
        // The agent's reference monitor additionally requires the exact call to
        // classify as WorkspaceRead before honoring this static capability.
        ReplaySafety::Safe
    }

    fn concurrency(&self) -> ToolConcurrency {
        // Likewise, HostRead and Network calls are forced back to sequential
        // execution after per-call effect classification.
        ToolConcurrency::Parallel
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        self.effect(&args, ctx)?;
        let args: ReadArgs = parse_args(args)?;
        if let Some(url) = parse_url_source(&args.path)? {
            return match url.scheme() {
                "http" | "https" if ctx.sandbox.allow_remote_read => {
                    let cancellation = ctx.cancellation.clone();
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {
                            Err(ToolError::new("remote media read cancelled"))
                        }
                        result = read_remote_media(url) => result,
                    }
                }
                "http" | "https" => Err(ToolError::new(
                    "remote URL reads are disabled; enable `allow_remote_read` \
                     (or pass `--allow-remote-read`) to permit public HTTPS image/audio fetches",
                )),
                "file" => {
                    let path = local_path_from_url(&url)?;
                    let path = workspace_relative_file_url_path(path, ctx);
                    read_local(&args, &path, ctx).await
                }
                scheme => Err(ToolError::new(format!(
                    "unsupported read URL scheme `{scheme}`; use file or https"
                ))),
            };
        }
        read_local(&args, &args.path, ctx).await
    }
}

async fn read_local(
    args: &ReadArgs,
    requested_path: &str,
    ctx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolError> {
    let display_path = ctx.display_path(requested_path);
    let target = ctx.resolve_existing(requested_path)?;
    let hinted_kind = media_kind_for_name(requested_path);
    let sniff_hint = hinted_kind.clone();
    let read_path = target.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        read_regular_file_bounded_by(&read_path, MAX_FILE_BYTES, |prefix| {
            let detected = sniff_media_kind(prefix, sniff_hint.as_ref());
            detected
                .iter()
                .chain(sniff_hint.iter())
                .map(MediaKind::byte_limit)
                .min()
                .unwrap_or(MAX_FILE_BYTES)
        })
    })
    .await
    .map_err(|error| ToolError::new(format!("{display_path}: read worker failed: {error}")))?
    .map_err(|error| match error {
        SecureFileError::NotRegular => ToolError::new(format!(
            "{display_path}: is not a regular file (a directory or special file is rejected)"
        )),
        other => ToolError::new(format!("{display_path}: {other}")),
    })?;
    match validated_media_kind(&bytes, hinted_kind.as_ref(), &display_path)? {
        Some(kind) => media_output(display_path, bytes, kind),
        None => text_output(args, ctx, display_path, &bytes),
    }
}

fn parse_url_source(value: &str) -> Result<Option<reqwest::Url>, ToolError> {
    let looks_like_url = value.contains("://") || value.starts_with("file:");
    if !looks_like_url {
        return Ok(None);
    }
    let url = reqwest::Url::parse(value)
        .map_err(|error| ToolError::new(format!("invalid read URL: {error}")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ToolError::new(
            "read URLs must not contain embedded credentials",
        ));
    }
    if url.fragment().is_some() {
        return Err(ToolError::new("read URLs must not contain fragments"));
    }
    Ok(Some(url))
}

fn local_path_from_url(url: &reqwest::Url) -> Result<String, ToolError> {
    let mut local = url.clone();
    match local.host_str() {
        None | Some("") => {}
        Some("localhost") => {
            local
                .set_host(None)
                .map_err(|_| ToolError::new("invalid localhost file URL"))?;
        }
        Some(host) => {
            return Err(ToolError::new(format!(
                "file URL host `{host}` is not local"
            )));
        }
    }
    local
        .to_file_path()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|_| ToolError::new("file URL does not contain a valid local path"))
}

fn workspace_relative_file_url_path(path: String, ctx: &ToolContext<'_>) -> String {
    if ctx.sandbox.allow_external_paths {
        return path;
    }
    Path::new(&path)
        .strip_prefix(ctx.workspace)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map_or(path.clone(), |relative| {
            relative.to_string_lossy().into_owned()
        })
}

async fn validated_remote_endpoint(
    url: &reqwest::Url,
    display: &str,
) -> Result<(String, Option<std::net::IpAddr>, Vec<std::net::SocketAddr>), ToolError> {
    let (host, literal_ip) = match url
        .host()
        .ok_or_else(|| ToolError::new(format!("{display}: URL has no host")))?
    {
        url::Host::Domain(domain) => (domain.trim_end_matches('.').to_ascii_lowercase(), None),
        url::Host::Ipv4(address) => {
            let address = std::net::IpAddr::V4(address);
            (address.to_string(), Some(address))
        }
        url::Host::Ipv6(address) => {
            let address = std::net::IpAddr::V6(address);
            (address.to_string(), Some(address))
        }
    };
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host == "metadata.google.internal"
    {
        return Err(ToolError::new(format!(
            "{display}: local or metadata hosts are not allowed"
        )));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ToolError::new(format!("{display}: URL has no known port")))?;
    let test_loopback_http =
        cfg!(test) && url.scheme() == "http" && literal_ip.is_some_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" if port == 443 => {}
        "https" => {
            return Err(ToolError::new(format!(
                "{display}: HTTPS media URLs must use port 443"
            )));
        }
        "http" if test_loopback_http => {}
        "http" => {
            return Err(ToolError::new(format!(
                "{display}: remote media URLs must use HTTPS"
            )));
        }
        scheme => {
            return Err(ToolError::new(format!(
                "{display}: unsupported remote URL scheme `{scheme}`"
            )));
        }
    }

    let addresses = if let Some(address) = literal_ip {
        vec![std::net::SocketAddr::new(address, port)]
    } else {
        tokio::time::timeout(
            REMOTE_DNS_TIMEOUT,
            tokio::net::lookup_host((host.as_str(), port)),
        )
        .await
        .map_err(|_| ToolError::new(format!("{display}: DNS lookup timed out")))?
        .map_err(|error| ToolError::new(format!("{display}: DNS lookup failed: {error}")))?
        .collect::<Vec<_>>()
    };
    if addresses.is_empty() {
        return Err(ToolError::new(format!(
            "{display}: DNS lookup returned no addresses"
        )));
    }
    if test_loopback_http {
        if addresses.iter().any(|address| !address.ip().is_loopback()) {
            return Err(ToolError::new(format!(
                "{display}: test HTTP URLs must resolve only to loopback"
            )));
        }
    } else if addresses
        .iter()
        .any(|address| !is_public_remote_ip(address.ip()))
    {
        return Err(ToolError::new(format!(
            "{display}: private, link-local, metadata, and non-public targets are not allowed"
        )));
    }
    Ok((host, literal_ip, addresses))
}

fn is_public_remote_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224
                || (a == 255 && b == 255 && c == 255 && d == 255))
        }
        std::net::IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4() {
                return is_public_remote_ip(std::net::IpAddr::V4(mapped));
            }
            let octets = ip.octets();
            // RFC 6052's well-known NAT64 prefix embeds an IPv4 address in the
            // final 32 bits. Apply the IPv4 denylist to the translated target,
            // while rejecting the local-use translation prefix outright.
            if octets[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0] {
                return is_public_remote_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                    octets[12], octets[13], octets[14], octets[15],
                )));
            }
            if octets[..6] == [0x00, 0x64, 0xff, 0x9b, 0x00, 0x01] {
                return false;
            }
            // 6to4 exposes its embedded IPv4 relay target directly. Teredo
            // carries multiple obfuscated IPv4 fields; reject it entirely
            // rather than attempt a permissive partial decode.
            if octets[..2] == [0x20, 0x02] {
                return is_public_remote_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                    octets[2], octets[3], octets[4], octets[5],
                )));
            }
            if octets[..4] == [0x20, 0x01, 0x00, 0x00] {
                return false;
            }
            let segments = ip.segments();
            // Remote reads are public-only. Start from the global-unicast
            // allocation (2000::/3), then subtract special-purpose space
            // within it; everything else is fail-closed.
            segments[0] & 0xe000 == 0x2000
                && !(ip.is_unspecified()
                    || ip.is_loopback()
                    || ip.is_multicast()
                    || segments[0] & 0xfe00 == 0xfc00
                    || segments[0] & 0xffc0 == 0xfe80
                    || segments[0] & 0xffc0 == 0xfec0
                    || (segments[0] == 0x2001 && segments[1] <= 0x01ff)
                    || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                    || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0))
        }
    }
}

const REMOTE_CLIENT_CACHE_CAPACITY: usize = 16;

#[derive(Clone, PartialEq, Eq)]
struct RemoteClientKey {
    host: String,
    endpoints: Vec<std::net::SocketAddr>,
    https_only: bool,
}

fn remote_client(
    host: &str,
    endpoints: &[std::net::SocketAddr],
    https_only: bool,
) -> Result<reqwest::Client, ToolError> {
    static CLIENTS: OnceLock<Mutex<VecDeque<(RemoteClientKey, reqwest::Client)>>> = OnceLock::new();
    let clients = CLIENTS.get_or_init(|| Mutex::new(VecDeque::new()));
    let key = RemoteClientKey {
        host: host.to_owned(),
        endpoints: endpoints.to_vec(),
        https_only,
    };

    {
        let mut cache = clients.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(index) = cache.iter().position(|(cached, _)| cached == &key) {
            let entry = cache.remove(index).expect("matching client cache entry");
            let client = entry.1.clone();
            cache.push_back(entry);
            return Ok(client);
        }
    }

    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(REMOTE_CONNECT_TIMEOUT)
        .timeout(REMOTE_READ_TIMEOUT)
        .user_agent(concat!("ygg/", env!("CARGO_PKG_VERSION")))
        .no_proxy()
        .resolve_to_addrs(host, endpoints);
    if https_only {
        builder = builder.https_only(true);
    }
    let client = builder
        .build()
        .map_err(|error| ToolError::new(format!("remote media client failed: {error}")))?;

    let mut cache = clients.lock().unwrap_or_else(|error| error.into_inner());
    if let Some((_, cached)) = cache.iter().find(|(cached, _)| cached == &key) {
        return Ok(cached.clone());
    }
    if cache.len() == REMOTE_CLIENT_CACHE_CAPACITY {
        cache.pop_front();
    }
    cache.push_back((key, client.clone()));
    Ok(client)
}

async fn read_remote_media(mut url: reqwest::Url) -> Result<ToolOutput, ToolError> {
    let requested_display = display_remote_url(&url);
    let (host, literal_ip, endpoints) = validated_remote_endpoint(&url, &requested_display).await?;
    // `ClientBuilder::resolve` keys exact host spellings. Normalize the URL to
    // the validated spelling too, otherwise a trailing dot can miss the pinned
    // override and trigger a second, unvalidated DNS lookup at connect time.
    if let Some(address) = literal_ip {
        url.set_ip_host(address)
            .map_err(|_| ToolError::new(format!("{requested_display}: invalid normalized host")))?;
    } else {
        url.set_host(Some(&host))
            .map_err(|_| ToolError::new(format!("{requested_display}: invalid normalized host")))?;
    }
    // Cache only clients with the same validated hostname-to-address pin. A
    // DNS change creates a different key rather than reusing an unvalidated
    // connection pool.
    let client = remote_client(&host, &endpoints, url.scheme() == "https")?;
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "image/*, audio/*")
        .send()
        .await
        .map_err(|error| {
            ToolError::new(format!(
                "{requested_display}: request failed: {}",
                error.without_url()
            ))
        })?;
    let response_display = display_remote_url(response.url());
    if !response.status().is_success() {
        return Err(ToolError::new(format!(
            "{response_display}: HTTP {}",
            response.status()
        )));
    }

    let extension_hint = media_kind_for_name(response.url().path());
    let content_type_hint = response_media_kind(response.headers(), &response_display)?;
    if let Some(extension) = &extension_hint {
        if extension != &content_type_hint {
            return Err(ToolError::new(format!(
                "{response_display}: URL extension indicates {} but Content-Type is {}",
                extension.media_type(),
                content_type_hint.media_type()
            )));
        }
    }
    let hinted_kind = content_type_hint;
    let byte_limit = hinted_kind.byte_limit();
    if response
        .content_length()
        .is_some_and(|length| length > byte_limit as u64)
    {
        return Err(media_too_large_error(
            &response_display,
            byte_limit,
            response.content_length(),
        ));
    }

    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(byte_limit as u64) as usize,
    );
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        ToolError::new(format!(
            "{response_display}: response read failed: {}",
            error.without_url()
        ))
    })? {
        if bytes.len().saturating_add(chunk.len()) > byte_limit {
            return Err(media_too_large_error(
                &response_display,
                byte_limit,
                Some(bytes.len().saturating_add(chunk.len()) as u64),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }

    let kind =
        validated_media_kind(&bytes, Some(&hinted_kind), &response_display)?.ok_or_else(|| {
            ToolError::new(format!(
                "{response_display}: remote reads accept supported image or audio content only"
            ))
        })?;
    media_output(response_display, bytes, kind)
}

fn response_media_kind(
    headers: &reqwest::header::HeaderMap,
    display: &str,
) -> Result<MediaKind, ToolError> {
    let Some(value) = headers.get(reqwest::header::CONTENT_TYPE) else {
        return Err(ToolError::new(format!(
            "{display}: remote media response is missing Content-Type"
        )));
    };
    let value = value
        .to_str()
        .map_err(|_| ToolError::new(format!("{display}: invalid Content-Type header")))?;
    let mime = value
        .parse::<Mime>()
        .map_err(|_| ToolError::new(format!("{display}: invalid Content-Type `{value}`")))?;
    media_kind_for_mime(&mime).ok_or_else(|| {
        ToolError::new(format!(
            "{display}: unsupported remote Content-Type `{}`",
            mime.essence_str()
        ))
    })
}

fn display_remote_url(url: &reqwest::Url) -> String {
    let had_query = url.query().is_some();
    let mut display = url.clone();
    display.set_query(None);
    display.set_fragment(None);
    let mut value = display.to_string();
    if had_query {
        value.push_str("?…");
    }
    value
}

fn media_too_large_error(display: &str, byte_limit: usize, actual: Option<u64>) -> ToolError {
    let actual = actual.map_or_else(String::new, |actual| format!(" ({actual} bytes)"));
    ToolError::new(format!(
        "{display}: media exceeds the {} MB limit{actual}",
        byte_limit / (1024 * 1024)
    ))
}

fn media_kind_for_name(value: &str) -> Option<MediaKind> {
    let extension = Path::new(value).extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some(MediaKind::Image(image_mime("image/png"))),
        "jpg" | "jpeg" => Some(MediaKind::Image(image_mime("image/jpeg"))),
        "gif" => Some(MediaKind::Image(image_mime("image/gif"))),
        "webp" => Some(MediaKind::Image(image_mime("image/webp"))),
        "wav" => Some(MediaKind::Audio(AudioFormat::Wav)),
        "mp3" => Some(MediaKind::Audio(AudioFormat::Mp3)),
        "flac" => Some(MediaKind::Audio(AudioFormat::Flac)),
        "opus" | "ogg" => Some(MediaKind::Audio(AudioFormat::Opus)),
        "aac" | "m4a" => Some(MediaKind::Audio(AudioFormat::Aac)),
        _ => None,
    }
}

fn media_kind_for_mime(mime: &Mime) -> Option<MediaKind> {
    match mime.essence_str() {
        "image/png" => Some(MediaKind::Image(image_mime("image/png"))),
        "image/jpeg" => Some(MediaKind::Image(image_mime("image/jpeg"))),
        "image/gif" => Some(MediaKind::Image(image_mime("image/gif"))),
        "image/webp" => Some(MediaKind::Image(image_mime("image/webp"))),
        "audio/wav" | "audio/wave" | "audio/x-wav" => Some(MediaKind::Audio(AudioFormat::Wav)),
        "audio/mpeg" | "audio/mp3" => Some(MediaKind::Audio(AudioFormat::Mp3)),
        "audio/flac" | "audio/x-flac" => Some(MediaKind::Audio(AudioFormat::Flac)),
        "audio/opus" | "audio/ogg" => Some(MediaKind::Audio(AudioFormat::Opus)),
        "audio/aac" | "audio/mp4" | "audio/x-m4a" => Some(MediaKind::Audio(AudioFormat::Aac)),
        _ => None,
    }
}

fn image_mime(value: &'static str) -> Mime {
    value.parse().expect("static image MIME is valid")
}

fn validated_media_kind(
    bytes: &[u8],
    hint: Option<&MediaKind>,
    display: &str,
) -> Result<Option<MediaKind>, ToolError> {
    let detected = sniff_media_kind(bytes, hint);
    match (hint, detected) {
        (Some(expected), Some(actual)) if expected != &actual => Err(ToolError::new(format!(
            "{display}: declared {} does not match detected {} content",
            expected.media_type(),
            actual.media_type()
        ))),
        (Some(expected), None) => Err(ToolError::new(format!(
            "{display}: content does not match declared {} media",
            expected.media_type()
        ))),
        (_, detected) => Ok(detected),
    }
}

fn sniff_media_kind(bytes: &[u8], hint: Option<&MediaKind>) -> Option<MediaKind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(MediaKind::Image(image_mime("image/png")));
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(MediaKind::Image(image_mime("image/jpeg")));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(MediaKind::Image(image_mime("image/gif")));
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(MediaKind::Image(image_mime("image/webp")));
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        return Some(MediaKind::Audio(AudioFormat::Wav));
    }
    if bytes.starts_with(b"fLaC") {
        return Some(MediaKind::Audio(AudioFormat::Flac));
    }
    if bytes.starts_with(b"OggS")
        && bytes
            .windows(b"OpusHead".len())
            .take(128)
            .any(|window| window == b"OpusHead")
    {
        return Some(MediaKind::Audio(AudioFormat::Opus));
    }
    if bytes.starts_with(b"ID3")
        || bytes.get(..2).is_some_and(|prefix| {
            prefix[0] == 0xff && prefix[1] & 0xe0 == 0xe0 && prefix[1] & 0x06 != 0
        })
    {
        return Some(MediaKind::Audio(AudioFormat::Mp3));
    }
    if bytes
        .get(..2)
        .is_some_and(|prefix| prefix[0] == 0xff && prefix[1] & 0xf6 == 0xf0)
    {
        return Some(MediaKind::Audio(AudioFormat::Aac));
    }
    if matches!(hint, Some(MediaKind::Audio(AudioFormat::Aac)))
        && bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
    {
        return Some(MediaKind::Audio(AudioFormat::Aac));
    }
    None
}

fn media_output(
    display_path: String,
    bytes: Vec<u8>,
    kind: MediaKind,
) -> Result<ToolOutput, ToolError> {
    if bytes.len() > kind.byte_limit() {
        return Err(media_too_large_error(
            &display_path,
            kind.byte_limit(),
            Some(bytes.len() as u64),
        ));
    }
    let hash = content_hash(&bytes);
    let byte_len = bytes.len();
    let media_type = kind.media_type();
    let read_kind = match &kind {
        MediaKind::Image(_) => "vision",
        MediaKind::Audio(_) => "audio",
    };
    let media = kind.into_media(bytes);
    Ok(ToolOutput::new(format!(
        "{display_path}: media={media_type} bytes={byte_len} hash={hash}\nread={read_kind}"
    ))
    .with_media(media))
}

fn text_output(
    args: &ReadArgs,
    ctx: &ToolContext<'_>,
    display_path: String,
    bytes: &[u8],
) -> Result<ToolOutput, ToolError> {
    let hash = content_hash(bytes);
    let text = String::from_utf8_lossy(bytes);
    let offset = args.offset.unwrap_or(1).max(1);
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);

    // Reserve some budget for the header/footer lines. Count and render in one
    // pass so newline-dense files do not allocate one fat reference per line.
    let byte_budget = ctx.sandbox.max_output_bytes.saturating_sub(256).max(1024);
    let requested_end = offset.saturating_add(limit.saturating_sub(1));
    let mut body = String::new();
    let mut total = 0usize;
    let mut end = offset - 1; // last included line
    let mut truncated = false;
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        total = line_number;
        if line_number < offset || line_number > requested_end || truncated {
            continue;
        }
        let rendered = format!("{line_number}: {}\n", clip_line(line, MAX_LINE_CHARS));
        if !body.is_empty() && body.len() + rendered.len() > byte_budget {
            truncated = true;
            continue;
        }
        body.push_str(&rendered);
        end = line_number;
    }

    if total == 0 {
        return Ok(ToolOutput::new(format!(
            "{display_path}:0-0/0 hash={hash}\n(empty file)\ntruncated=false"
        )));
    }
    if offset > total {
        return Err(ToolError::new(format!(
            "{display_path}: offset {offset} is beyond the end of the file ({total} lines)"
        )));
    }

    let header = format!("{display_path}:{offset}-{end}/{total} hash={hash}");
    let footer = if end < total {
        format!("next_offset={} truncated={truncated}", end + 1)
    } else {
        format!("truncated={truncated}")
    };
    Ok(ToolOutput::new(format!("{header}\n{body}{footer}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxConfig;
    use crate::ToolProgressSink;
    use serde_json::json;
    use std::path::PathBuf;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\npayload";
    const WAV_BYTES: &[u8] = b"RIFF\x04\x00\x00\x00WAVEpayload";
    const AAC_BYTES: &[u8] = b"\xff\xf1\x50\x80payload";

    struct Fixture {
        _dir: tempfile::TempDir,
        workspace: PathBuf,
        sandbox: SandboxConfig,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let sandbox = SandboxConfig::new(&workspace);
        Fixture {
            _dir: dir,
            workspace,
            sandbox,
        }
    }

    fn remote_fixture() -> Fixture {
        let mut fixture = fixture();
        fixture.sandbox.allow_remote_read = true;
        fixture
    }

    impl Fixture {
        fn ctx(&self) -> ToolContext<'_> {
            ToolContext {
                workspace: &self.workspace,
                sandbox: &self.sandbox,
                execution_scope: "read-test",
                resource_owner: "read-test",
                active_skills: &[],
                registered_tools: &[],
                progress: ToolProgressSink::null(),
                cancellation: Default::default(),
            }
        }
    }

    #[test]
    fn effect_classification_is_conservative_and_does_not_resolve_local_paths() {
        let mut fixture = fixture();
        assert_eq!(
            ReadTool
                .effect(&json!({"path": "missing.txt"}), &fixture.ctx())
                .unwrap(),
            ToolEffect::WorkspaceRead
        );
        assert_eq!(ReadTool.replay_safety(), ReplaySafety::Safe);
        assert_eq!(ReadTool.concurrency(), ToolConcurrency::Parallel);

        fixture.sandbox.allow_external_paths = true;
        for path in ["missing.txt", "/definitely/not/a/real/ygg-effect-path"] {
            assert_eq!(
                ReadTool
                    .effect(&json!({"path": path}), &fixture.ctx())
                    .unwrap(),
                ToolEffect::HostRead
            );
        }
        fixture.sandbox.allow_remote_read = true;
        assert_eq!(
            ReadTool
                .effect(
                    &json!({"path": "https://example.com/media.png"}),
                    &fixture.ctx(),
                )
                .unwrap(),
            ToolEffect::Network
        );
    }

    #[tokio::test]
    async fn reads_with_line_numbers_and_hash() {
        let f = fixture();
        std::fs::write(f.workspace.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();

        let out = ReadTool
            .execute(json!({"path": "a.txt"}), &f.ctx())
            .await
            .unwrap();
        let expected_hash = content_hash(b"alpha\nbeta\ngamma\n");
        assert_eq!(
            out.text,
            format!(
                "a.txt:1-3/3 hash={expected_hash}\n1: alpha\n2: beta\n3: gamma\ntruncated=false"
            )
        );
    }

    #[tokio::test]
    async fn local_image_and_audio_reads_return_structured_media() {
        let f = fixture();
        std::fs::write(f.workspace.join("capture.bin"), PNG_BYTES).unwrap();
        std::fs::write(f.workspace.join("memo.wav"), WAV_BYTES).unwrap();

        let image = ReadTool
            .execute(json!({"path": "capture.bin"}), &f.ctx())
            .await
            .unwrap();
        assert_eq!(image.media_kinds(), &[crate::ToolOutputMediaKind::Image]);
        assert_eq!(image.media().len(), 1);
        assert!(image.text.contains("read=vision"), "{}", image.text);
        assert!(!image.text.contains("payload"), "{}", image.text);

        let audio = ReadTool
            .execute(json!({"path": "memo.wav"}), &f.ctx())
            .await
            .unwrap();
        assert_eq!(audio.media_kinds(), &[crate::ToolOutputMediaKind::Audio]);
        assert_eq!(audio.media().len(), 1);
        assert!(audio.text.contains("read=audio"), "{}", audio.text);
        assert!(!audio.text.contains("payload"), "{}", audio.text);
    }

    #[tokio::test]
    async fn recognized_audio_format_is_preserved_for_protocol_lowering() {
        let f = fixture();
        std::fs::write(f.workspace.join("clip.aac"), AAC_BYTES).unwrap();

        let audio = ReadTool
            .execute(json!({"path": "clip.aac"}), &f.ctx())
            .await
            .unwrap();
        assert_eq!(audio.media_kinds(), &[crate::ToolOutputMediaKind::Audio]);
        assert!(audio.text.contains("media=audio/aac"), "{}", audio.text);
        let Media::Audio(audio) = &audio.media()[0] else {
            panic!("expected audio media");
        };
        assert_eq!(audio.format, AudioFormat::Aac);
    }

    #[tokio::test]
    async fn local_media_extension_must_match_magic_bytes() {
        let f = fixture();
        std::fs::write(f.workspace.join("not-really.png"), b"<html>nope</html>").unwrap();
        let error = ReadTool
            .execute(json!({"path": "not-really.png"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(error.message.contains("does not match"), "{error}");
    }

    #[tokio::test]
    async fn local_media_size_is_rejected_before_buffering() {
        let f = fixture();
        let path = f.workspace.join("oversized.png");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        let error = ReadTool
            .execute(json!({"path": "oversized.png"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(error.message.contains("too large"), "{error}");
    }

    #[tokio::test]
    async fn extensionless_media_uses_its_content_cap_before_buffering() {
        let f = fixture();
        let path = f.workspace.join("oversized.bin");
        std::fs::write(&path, PNG_BYTES).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();

        let error = ReadTool
            .execute(json!({"path": "oversized.bin"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(
            error.message.contains(&format!("limit {MAX_IMAGE_BYTES}")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn workspace_file_url_uses_the_same_local_media_pipeline() {
        let f = fixture();
        let path = f.workspace.join("screen shot.png");
        std::fs::write(&path, PNG_BYTES).unwrap();
        let url = reqwest::Url::from_file_path(&path).unwrap();

        let output = ReadTool
            .execute(json!({"path": url.as_str()}), &f.ctx())
            .await
            .unwrap();
        assert_eq!(output.media_kinds(), &[crate::ToolOutputMediaKind::Image]);
    }

    #[tokio::test]
    async fn workspace_mode_rejects_external_file_urls() {
        let f = fixture();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let url = reqwest::Url::from_file_path(outside.path()).unwrap();
        let error = ReadTool
            .execute(json!({"path": url.as_str()}), &f.ctx())
            .await
            .unwrap_err();
        assert!(
            error.message.contains("absolute paths are not allowed"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn remote_reads_are_default_off_and_rejected_before_network_access() {
        let server = MockServer::start().await;
        let f = fixture();

        let error = ReadTool
            .execute(
                json!({"path": format!("{}/capture?local-secret", server.uri())}),
                &f.ctx(),
            )
            .await
            .unwrap_err();

        assert!(
            error.message.contains("remote URL reads are disabled"),
            "{error}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn remote_read_cancellation_interrupts_header_waits() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(PNG_BYTES)
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        let f = remote_fixture();
        let cancellation = crate::CancellationToken::default();
        let cancel = cancellation.clone();
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &f.sandbox,
            execution_scope: "read-test",
            resource_owner: "read-test",
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation,
        };
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            cancel.cancel();
        });

        let started = std::time::Instant::now();
        let error = ReadTool
            .execute(json!({"path": format!("{}/slow.png", server.uri())}), &ctx)
            .await
            .unwrap_err();
        assert!(error.message.contains("cancelled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn remote_link_requires_matching_mime_and_magic_and_redacts_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/capture"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(PNG_BYTES),
            )
            .mount(&server)
            .await;
        let f = remote_fixture();
        let url = format!("{}/capture?token=secret", server.uri());

        let output = ReadTool
            .execute(json!({"path": url}), &f.ctx())
            .await
            .unwrap();
        assert_eq!(output.media_kinds(), &[crate::ToolOutputMediaKind::Image]);
        assert!(output.text.contains("?…"), "{}", output.text);
        assert!(!output.text.contains("secret"), "{}", output.text);
    }

    #[tokio::test]
    async fn remote_link_rejects_mime_magic_mismatch() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/fake.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(WAV_BYTES),
            )
            .mount(&server)
            .await;
        let f = remote_fixture();
        let error = ReadTool
            .execute(
                json!({"path": format!("{}/fake.png", server.uri())}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("does not match"), "{error}");
    }

    #[tokio::test]
    async fn remote_link_requires_a_supported_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/capture"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG_BYTES))
            .mount(&server)
            .await;
        let f = remote_fixture();
        let error = ReadTool
            .execute(
                json!({"path": format!("{}/capture", server.uri())}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("missing Content-Type"), "{error}");
    }

    #[tokio::test]
    async fn remote_link_rejects_private_https_targets_before_connecting() {
        let f = remote_fixture();
        let error = ReadTool
            .execute(
                json!({"path": "https://169.254.169.254/latest/meta-data"}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("non-public"), "{error}");
    }

    #[test]
    fn transition_ipv6_cannot_embed_a_nonpublic_ipv4_target() {
        for address in [
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::808:808",
            "2002:a9fe:a9fe::",
            "2001:0000::1",
        ] {
            let address = address.parse::<std::net::IpAddr>().unwrap();
            assert!(
                !is_public_remote_ip(address),
                "accepted transition address {address}"
            );
        }
        assert!(is_public_remote_ip(
            "64:ff9b::808:808".parse::<std::net::IpAddr>().unwrap()
        ));
    }

    #[test]
    fn reserved_address_space_is_not_treated_as_public() {
        for address in [
            "192.88.99.2",
            "100::1",
            "2001:2::1",
            "3fff::1",
            "5f00::1",
            "4000::1",
        ] {
            let address = address.parse::<std::net::IpAddr>().unwrap();
            assert!(!is_public_remote_ip(address), "accepted {address}");
        }
        assert!(is_public_remote_ip(
            "2606:4700:4700::1111".parse::<std::net::IpAddr>().unwrap()
        ));
    }

    #[tokio::test]
    async fn public_ipv6_literal_is_normalized_without_dns_or_brackets() {
        let url = reqwest::Url::parse("https://[2606:4700:4700::1111]/image.png").unwrap();
        let (host, literal, addresses) = validated_remote_endpoint(&url, "public IPv6 literal")
            .await
            .unwrap();
        let expected = "2606:4700:4700::1111".parse::<std::net::IpAddr>().unwrap();
        assert_eq!(host, "2606:4700:4700::1111");
        assert_eq!(literal, Some(expected));
        assert_eq!(addresses, vec![std::net::SocketAddr::new(expected, 443)]);
    }

    #[tokio::test]
    async fn trailing_dot_loopback_url_uses_the_normalized_pinned_host() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/capture.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(PNG_BYTES),
            )
            .expect(1)
            .mount(&server)
            .await;
        let port = reqwest::Url::parse(&server.uri()).unwrap().port().unwrap();
        let f = remote_fixture();
        let output = ReadTool
            .execute(
                json!({"path": format!("http://127.0.0.1.:{port}/capture.png")}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(output.text.contains("read=vision"), "{}", output.text);
        assert!(output.text.contains("http://127.0.0.1:"), "{}", output.text);
        assert!(!output.text.contains("127.0.0.1.:"), "{}", output.text);
    }

    #[tokio::test]
    async fn offset_and_limit_report_continuation() {
        let f = fixture();
        let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(f.workspace.join("b.txt"), &content).unwrap();

        let out = ReadTool
            .execute(json!({"path": "b.txt", "offset": 3, "limit": 4}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.starts_with("b.txt:3-6/10 hash="), "{}", out.text);
        assert!(out.text.contains("3: line3\n"));
        assert!(out.text.contains("6: line6\n"));
        assert!(!out.text.contains("7: line7"));
        assert!(out.text.ends_with("next_offset=7 truncated=false"));
    }

    #[tokio::test]
    async fn extreme_limit_is_safely_clamped_to_the_file() {
        let f = fixture();
        std::fs::write(f.workspace.join("bounded.txt"), "one\ntwo\n").unwrap();

        let out = ReadTool
            .execute(
                json!({"path": "bounded.txt", "offset": 2, "limit": usize::MAX}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.contains("bounded.txt:2-2/2"), "{}", out.text);
        assert!(out.text.contains("2: two"), "{}", out.text);
    }

    #[tokio::test]
    async fn byte_budget_truncates_with_marker() {
        let f = fixture();
        let content: String = (1..=2000).map(|i| format!("line number {i}\n")).collect();
        std::fs::write(f.workspace.join("big.txt"), &content).unwrap();
        let mut sandbox = f.sandbox.clone();
        sandbox.max_output_bytes = 2048;
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "read-test",
            resource_owner: "read-test",
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
        };

        let out = ReadTool
            .execute(json!({"path": "big.txt"}), &ctx)
            .await
            .unwrap();
        assert!(out.text.len() < 4096);
        assert!(out.text.contains("truncated=true"), "{}", out.text);
        assert!(out.text.contains("next_offset="), "{}", out.text);
    }

    #[tokio::test]
    async fn directory_missing_and_escaping_paths_fail() {
        let f = fixture();
        std::fs::create_dir(f.workspace.join("sub")).unwrap();

        let err = ReadTool
            .execute(json!({"path": "sub"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains("directory"), "{err}");

        let err = ReadTool
            .execute(json!({"path": "missing.txt"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains("missing.txt"), "{err}");

        let err = ReadTool
            .execute(json!({"path": "../outside.txt"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains(".."), "{err}");
    }

    #[tokio::test]
    async fn trusted_local_mode_reads_an_absolute_path() {
        let f = fixture();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside\n").unwrap();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_external_paths = true;
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "read-test",
            resource_owner: "read-test",
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
        };

        let out = ReadTool
            .execute(json!({"path": outside.path().to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert!(out.text.contains("1: outside"), "{}", out.text);
    }

    #[tokio::test]
    async fn offset_beyond_end_is_an_error() {
        let f = fixture();
        std::fs::write(f.workspace.join("s.txt"), "only\n").unwrap();
        let err = ReadTool
            .execute(json!({"path": "s.txt", "offset": 5}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains("beyond the end"), "{err}");
    }

    #[tokio::test]
    async fn empty_file_reads_cleanly() {
        let f = fixture();
        std::fs::write(f.workspace.join("e.txt"), "").unwrap();
        let out = ReadTool
            .execute(json!({"path": "e.txt"}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.contains("e.txt:0-0/0 hash="), "{}", out.text);
        assert!(out.text.contains("(empty file)"));
    }

    #[tokio::test]
    async fn invalid_args_are_a_tool_error() {
        let f = fixture();
        let err = ReadTool
            .execute(json!({"offset": 1}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains("invalid arguments"), "{err}");
    }
}
