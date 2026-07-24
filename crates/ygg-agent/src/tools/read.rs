//! Bounded text and multimodal file reading.

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use serde::Deserialize;
use ygg_ai::{AudioFormat, Media, Mime, ToolDef};

use crate::secure_fs::{read_regular_file_bounded, SecureFileError};
use crate::tool::{content_hash, ReplaySafety, Tool, ToolContext, ToolError, ToolOutput};
use crate::tools::{clip_line, parse_args, MAX_FILE_BYTES};
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
                          URL, or an HTTPS image/audio URL. Text returns numbered lines plus a whole-file \
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
                        "description": "Local file path, file:// URL, or HTTPS image/audio URL."
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

    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Safe
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let args: ReadArgs = parse_args(args)?;
        if let Some(url) = parse_url_source(&args.path)? {
            return match url.scheme() {
                "http" | "https" => read_remote_media(url, ctx).await,
                "file" => {
                    let path = local_path_from_url(&url)?;
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
    let byte_limit = hinted_kind
        .as_ref()
        .map_or(MAX_FILE_BYTES, MediaKind::byte_limit);
    let read_path = target.clone();
    let bytes =
        tokio::task::spawn_blocking(move || read_regular_file_bounded(&read_path, byte_limit))
            .await
            .map_err(|error| {
                ToolError::new(format!("{display_path}: read worker failed: {error}"))
            })?
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

async fn validated_remote_endpoint(
    url: &reqwest::Url,
    display: &str,
) -> Result<(String, std::net::SocketAddr), ToolError> {
    let host = url
        .host_str()
        .ok_or_else(|| ToolError::new(format!("{display}: URL has no host")))?
        .trim_end_matches('.')
        .to_ascii_lowercase();
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
    let test_loopback_http = cfg!(test)
        && url.scheme() == "http"
        && host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
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

    let addresses = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|error| ToolError::new(format!("{display}: DNS lookup failed: {error}")))?
        .collect::<Vec<_>>();
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
    Ok((host, addresses[0]))
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
            let segments = ip.segments();
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || segments[0] & 0xfe00 == 0xfc00
                || segments[0] & 0xffc0 == 0xfe80
                || segments[0] & 0xffc0 == 0xfec0
                || (segments[0] == 0x2001 && segments[1] == 0x0db8))
        }
    }
}

async fn read_remote_media(
    url: reqwest::Url,
    ctx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolError> {
    let requested_display = display_remote_url(&url);
    let (host, endpoint) = validated_remote_endpoint(&url, &requested_display).await?;
    let mut client_builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(REMOTE_CONNECT_TIMEOUT)
        .timeout(REMOTE_READ_TIMEOUT)
        .user_agent(concat!("ygg/", env!("CARGO_PKG_VERSION")))
        .no_proxy()
        .resolve(&host, endpoint);
    if url.scheme() == "https" {
        client_builder = client_builder.https_only(true);
    }
    let client = client_builder
        .build()
        .map_err(|error| ToolError::new(format!("remote media client failed: {error}")))?;
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
        if ctx.cancellation.is_cancelled() {
            return Err(ToolError::new(format!(
                "{response_display}: read cancelled"
            )));
        }
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
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();

    let offset = args.offset.unwrap_or(1).max(1);
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);

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

    // Reserve some budget for the header/footer lines.
    let byte_budget = ctx.sandbox.max_output_bytes.saturating_sub(256).max(1024);
    let requested_end = offset.saturating_add(limit.saturating_sub(1)).min(total);

    let mut body = String::new();
    let mut end = offset - 1; // last included line
    let mut truncated = false;
    for (i, line) in lines
        .iter()
        .enumerate()
        .take(requested_end)
        .skip(offset - 1)
    {
        let rendered = format!("{}: {}\n", i + 1, clip_line(line, MAX_LINE_CHARS));
        if !body.is_empty() && body.len() + rendered.len() > byte_budget {
            truncated = true;
            break;
        }
        body.push_str(&rendered);
        end = i + 1;
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

    impl Fixture {
        fn ctx(&self) -> ToolContext<'_> {
            ToolContext {
                workspace: &self.workspace,
                sandbox: &self.sandbox,
                execution_scope: "read-test",
                active_skills: &[],
                registered_tools: &[],
                progress: ToolProgressSink::null(),
                cancellation: Default::default(),
            }
        }
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
    async fn file_url_uses_the_same_local_media_pipeline() {
        let f = fixture();
        let path = f.workspace.join("screen shot.png");
        std::fs::write(&path, PNG_BYTES).unwrap();
        let url = reqwest::Url::from_file_path(&path).unwrap();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_external_paths = true;
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "read-test",
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
        };

        let output = ReadTool
            .execute(json!({"path": url.as_str()}), &ctx)
            .await
            .unwrap();
        assert_eq!(output.media_kinds(), &[crate::ToolOutputMediaKind::Image]);
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
        let f = fixture();
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
        let f = fixture();
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
        let f = fixture();
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
        let f = fixture();
        let error = ReadTool
            .execute(
                json!({"path": "https://169.254.169.254/latest/meta-data"}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("non-public"), "{error}");
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
