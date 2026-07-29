//! Durable per-inference usage records and bounded aggregate projections.
//!
//! The append-only request log is authoritative. Lifetime, period, and daily
//! activity metrics are rebuilt from that log at startup so one durable write
//! records both the request and every aggregate derived from it.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const STORE_DIRECTORY: &str = "usage-v1";
const STORE_FILE: &str = "inference-requests.jsonl";
const STORE_VERSION: u16 = 1;
const MAX_STORE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 4 * 1024;
const MAX_RECORDS: usize = 1_000_000;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_PROVIDER_BYTES: usize = 128;
const MAX_MODEL_BYTES: usize = 256;
const DAY_MILLISECONDS: u64 = 86_400_000;
const MAX_TIMESTAMP_MILLISECONDS: u64 = 253_402_300_799_999;
/// Number of weeks returned for the activity heatmap.
pub const USAGE_ACTIVITY_WEEKS: u64 = 53;

/// Token usage reported for one completed provider operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceRequest {
    /// Stable owning session identity.
    pub session_id: String,
    /// Zero-based position in the session's durable usage-record sequence.
    pub request_ordinal: u64,
    /// Endpoint/provider route used for this operation.
    pub provider: String,
    /// Canonical selected model used for this operation.
    pub model: String,
    /// Completion wall-clock time in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// Prompt tokens billed at the standard input rate.
    pub prompt_tokens: u64,
    /// Generated output tokens, including any reasoning subset.
    pub completion_tokens: u64,
    /// Prompt tokens read from a provider cache.
    pub cache_read_tokens: u64,
    /// Prompt tokens written to a provider cache.
    pub cache_write_tokens: u64,
    /// One-hour cache writes, a subset of `cache_write_tokens`.
    pub cache_write_1h_tokens: u64,
    /// Reasoning tokens, a subset of `completion_tokens`.
    pub reasoning_tokens: u64,
    /// Provider-reported total tokens processed.
    pub total_tokens: u64,
}

impl InferenceRequest {
    fn validate(&self) -> Result<(), UsageStoreError> {
        if !valid_label(&self.session_id, MAX_SESSION_ID_BYTES)
            || !valid_label(&self.provider, MAX_PROVIDER_BYTES)
            || !valid_label(&self.model, MAX_MODEL_BYTES)
            || self.timestamp_ms > MAX_TIMESTAMP_MILLISECONDS
            || self.cache_write_1h_tokens > self.cache_write_tokens
            || self.reasoning_tokens > self.completion_tokens
        {
            return Err(UsageStoreError::InvalidRecord);
        }
        Ok(())
    }

    fn key(&self) -> (String, u64) {
        (self.session_id.clone(), self.request_ordinal)
    }
}

/// Time span accepted by the usage stats endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsagePeriod {
    /// The current UTC calendar day.
    Daily,
    /// The trailing seven UTC calendar days, including today.
    Weekly,
}

/// Flat token and request totals for a selected period.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageStats {
    /// Selected aggregation period.
    pub period: UsagePeriod,
    /// Standard-rate prompt tokens.
    pub prompt_tokens: u64,
    /// Generated output tokens.
    pub completion_tokens: u64,
    /// Prompt tokens read from a provider cache.
    pub cache_read_tokens: u64,
    /// Prompt tokens written to a provider cache.
    pub cache_write_tokens: u64,
    /// One-hour cache writes, a subset of `cache_write_tokens`.
    pub cache_write_1h_tokens: u64,
    /// Reasoning subset of generated output.
    pub reasoning_tokens: u64,
    /// Provider-reported total tokens.
    pub total_tokens: u64,
    /// Completed provider operations.
    pub request_count: u64,
}

impl Default for UsagePeriod {
    fn default() -> Self {
        Self::Daily
    }
}

/// Lifetime token totals retained by the serve host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifetimeUsage {
    /// Standard-rate prompt tokens.
    pub prompt_tokens: u64,
    /// Generated output tokens.
    pub completion_tokens: u64,
    /// Prompt tokens read from a provider cache.
    pub cache_read_tokens: u64,
    /// Prompt tokens written to a provider cache.
    pub cache_write_tokens: u64,
    /// One-hour cache writes, a subset of `cache_write_tokens`.
    pub cache_write_1h_tokens: u64,
    /// Reasoning subset of generated output.
    pub reasoning_tokens: u64,
    /// Provider-reported total tokens.
    pub total_tokens: u64,
    /// Completed provider operations.
    pub request_count: u64,
    /// Earliest retained request timestamp.
    pub first_request_at_ms: Option<u64>,
    /// Latest retained request timestamp.
    pub last_request_at_ms: Option<u64>,
}

/// Usage for one active UTC calendar day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageActivityDay {
    /// ISO-8601 UTC date (`YYYY-MM-DD`).
    pub date: String,
    /// Provider-reported total tokens on this date.
    pub tokens: u64,
    /// Completed provider operations on this date.
    pub request_count: u64,
}

/// Recent activity cells and lifetime streak facts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageActivity {
    /// Active days in the trailing 53 weeks, oldest first.
    pub days: Vec<UsageActivityDay>,
    /// Consecutive active days ending today or yesterday.
    pub current_streak: u64,
    /// Longest consecutive run of active days through today.
    pub longest_streak: u64,
}

/// Durable usage-store validation, integrity, quota, or persistence failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum UsageStoreError {
    /// A request violates the bounded usage-record contract.
    #[error("the inference usage record is invalid")]
    InvalidRecord,
    /// A stable session/ordinal key was reused for different usage.
    #[error("the inference usage record conflicts with an existing record")]
    Conflict,
    /// The bounded durable usage log reached its retention limit.
    #[error("the inference usage store quota was reached")]
    QuotaExceeded,
    /// Existing durable usage data is malformed or violates its invariants.
    #[error("the inference usage store is corrupt")]
    Corrupt,
    /// Private storage could not be opened or durably updated.
    #[error("the inference usage store is unavailable")]
    Storage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRequest {
    version: u16,
    request: InferenceRequest,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Aggregate {
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    cache_write_1h_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    request_count: u64,
}

impl Aggregate {
    fn add_request(&mut self, request: &InferenceRequest) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(request.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(request.completion_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(request.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(request.cache_write_tokens);
        self.cache_write_1h_tokens = self
            .cache_write_1h_tokens
            .saturating_add(request.cache_write_1h_tokens);
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(request.reasoning_tokens);
        self.total_tokens = self.total_tokens.saturating_add(request.total_tokens);
        self.request_count = self.request_count.saturating_add(1);
    }

    fn merge(&mut self, other: Self) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.cache_write_1h_tokens = self
            .cache_write_1h_tokens
            .saturating_add(other.cache_write_1h_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.request_count = self.request_count.saturating_add(other.request_count);
    }
}

/// In-memory lifetime and daily metrics rebuilt from durable request records.
#[derive(Clone, Debug, Default)]
pub struct LifetimeMetricsStore {
    lifetime: Aggregate,
    daily: BTreeMap<u64, Aggregate>,
    first_request_at_ms: Option<u64>,
    last_request_at_ms: Option<u64>,
}

impl LifetimeMetricsStore {
    fn record(&mut self, request: &InferenceRequest) {
        self.lifetime.add_request(request);
        self.daily
            .entry(request.timestamp_ms / DAY_MILLISECONDS)
            .or_default()
            .add_request(request);
        self.first_request_at_ms = Some(
            self.first_request_at_ms
                .map_or(request.timestamp_ms, |current| {
                    current.min(request.timestamp_ms)
                }),
        );
        self.last_request_at_ms = Some(
            self.last_request_at_ms
                .map_or(request.timestamp_ms, |current| {
                    current.max(request.timestamp_ms)
                }),
        );
    }

    /// Returns all retained token and request totals.
    pub fn lifetime(&self) -> LifetimeUsage {
        LifetimeUsage {
            prompt_tokens: self.lifetime.prompt_tokens,
            completion_tokens: self.lifetime.completion_tokens,
            cache_read_tokens: self.lifetime.cache_read_tokens,
            cache_write_tokens: self.lifetime.cache_write_tokens,
            cache_write_1h_tokens: self.lifetime.cache_write_1h_tokens,
            reasoning_tokens: self.lifetime.reasoning_tokens,
            total_tokens: self.lifetime.total_tokens,
            request_count: self.lifetime.request_count,
            first_request_at_ms: self.first_request_at_ms,
            last_request_at_ms: self.last_request_at_ms,
        }
    }

    /// Aggregates the selected period relative to a supplied clock.
    pub fn stats_at(&self, period: UsagePeriod, now_ms: u64) -> UsageStats {
        let today = now_ms / DAY_MILLISECONDS;
        let first_day = match period {
            UsagePeriod::Daily => today,
            UsagePeriod::Weekly => today.saturating_sub(6),
        };
        let mut total = Aggregate::default();
        for (_, day) in self.daily.range(first_day..=today) {
            total.merge(*day);
        }
        UsageStats {
            period,
            prompt_tokens: total.prompt_tokens,
            completion_tokens: total.completion_tokens,
            cache_read_tokens: total.cache_read_tokens,
            cache_write_tokens: total.cache_write_tokens,
            cache_write_1h_tokens: total.cache_write_1h_tokens,
            reasoning_tokens: total.reasoning_tokens,
            total_tokens: total.total_tokens,
            request_count: total.request_count,
        }
    }

    /// Returns recent activity and streaks relative to a supplied clock.
    pub fn activity_at(&self, now_ms: u64) -> UsageActivity {
        let today = now_ms / DAY_MILLISECONDS;
        let first_day = today.saturating_sub(USAGE_ACTIVITY_WEEKS * 7 - 1);
        let days = self
            .daily
            .range(first_day..=today)
            .map(|(day, aggregate)| UsageActivityDay {
                date: format_utc_day(*day),
                tokens: aggregate.total_tokens,
                request_count: aggregate.request_count,
            })
            .collect();
        let active_days = self.daily.range(..=today).map(|(day, _)| *day);
        let (current_streak, longest_streak) = streaks(active_days, today);
        UsageActivity {
            days,
            current_streak,
            longest_streak,
        }
    }
}

/// Append-only durable inference-request store.
pub struct InferenceRequestStore {
    file: File,
    file_bytes: u64,
    requests: HashMap<(String, u64), InferenceRequest>,
    metrics: LifetimeMetricsStore,
}

impl fmt::Debug for InferenceRequestStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InferenceRequestStore")
            .field("request_count", &self.requests.len())
            .field("file_bytes", &self.file_bytes)
            .finish()
    }
}

impl InferenceRequestStore {
    /// Opens or creates the owner-private versioned store.
    pub fn open(serve_state_directory: &Path) -> Result<Self, UsageStoreError> {
        let root = serve_state_directory.join(STORE_DIRECTORY);
        ensure_private_directory(&root)?;
        let path = root.join(STORE_FILE);
        let mut file = open_private_log(&path)?;
        let metadata = file.metadata().map_err(|_| UsageStoreError::Storage)?;
        if metadata.len() > MAX_STORE_BYTES {
            return Err(UsageStoreError::QuotaExceeded);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)
            .map_err(|_| UsageStoreError::Storage)?;

        let complete_bytes = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1);
        if complete_bytes < bytes.len() {
            file.set_len(complete_bytes as u64)
                .map_err(|_| UsageStoreError::Storage)?;
            file.sync_data().map_err(|_| UsageStoreError::Storage)?;
            bytes.truncate(complete_bytes);
        }

        let mut requests = HashMap::new();
        let mut metrics = LifetimeMetricsStore::default();
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_RECORD_BYTES {
                return Err(UsageStoreError::Corrupt);
            }
            let stored: StoredRequest =
                serde_json::from_slice(line).map_err(|_| UsageStoreError::Corrupt)?;
            if stored.version != STORE_VERSION || stored.request.validate().is_err() {
                return Err(UsageStoreError::Corrupt);
            }
            let key = stored.request.key();
            match requests.get(&key) {
                Some(existing) if existing == &stored.request => continue,
                Some(_) => return Err(UsageStoreError::Corrupt),
                None => {
                    metrics.record(&stored.request);
                    requests.insert(key, stored.request);
                }
            }
            if requests.len() > MAX_RECORDS {
                return Err(UsageStoreError::QuotaExceeded);
            }
        }

        Ok(Self {
            file,
            file_bytes: complete_bytes as u64,
            requests,
            metrics,
        })
    }

    /// Appends one request unless its stable session/ordinal key already exists.
    ///
    /// Returns `true` when a new durable record was written and `false` for an
    /// idempotent retry of an identical record.
    pub fn record(&mut self, request: InferenceRequest) -> Result<bool, UsageStoreError> {
        Ok(self.record_all(std::iter::once(request))? != 0)
    }

    /// Appends a batch with one durability barrier and returns the inserted count.
    pub fn record_all(
        &mut self,
        requests: impl IntoIterator<Item = InferenceRequest>,
    ) -> Result<usize, UsageStoreError> {
        let mut pending = Vec::new();
        let mut pending_keys = HashMap::new();
        for request in requests {
            request.validate()?;
            let key = request.key();
            if let Some(existing) = self.requests.get(&key) {
                if existing != &request {
                    return Err(UsageStoreError::Conflict);
                }
                continue;
            }
            if let Some(existing) = pending_keys.get(&key) {
                if existing != &request {
                    return Err(UsageStoreError::Conflict);
                }
                continue;
            }
            pending_keys.insert(key, request.clone());
            pending.push(request);
        }
        if pending.is_empty() {
            return Ok(0);
        }
        if self.requests.len().saturating_add(pending.len()) > MAX_RECORDS {
            return Err(UsageStoreError::QuotaExceeded);
        }

        let mut encoded = Vec::new();
        for request in &pending {
            let stored = StoredRequest {
                version: STORE_VERSION,
                request: request.clone(),
            };
            let line = serde_json::to_vec(&stored).map_err(|_| UsageStoreError::InvalidRecord)?;
            if line.len() > MAX_RECORD_BYTES {
                return Err(UsageStoreError::InvalidRecord);
            }
            encoded.extend_from_slice(&line);
            encoded.push(b'\n');
        }
        let encoded_bytes =
            u64::try_from(encoded.len()).map_err(|_| UsageStoreError::QuotaExceeded)?;
        if self.file_bytes.saturating_add(encoded_bytes) > MAX_STORE_BYTES {
            return Err(UsageStoreError::QuotaExceeded);
        }

        self.file
            .write_all(&encoded)
            .and_then(|()| self.file.sync_data())
            .map_err(|_| UsageStoreError::Storage)?;
        self.file_bytes += encoded_bytes;
        let inserted = pending.len();
        for request in pending {
            self.metrics.record(&request);
            self.requests.insert(request.key(), request);
        }
        Ok(inserted)
    }

    /// Returns all retained token and request totals.
    pub fn lifetime(&self) -> LifetimeUsage {
        self.metrics.lifetime()
    }

    /// Returns current UTC daily or trailing-seven-day totals.
    pub fn stats(&self, period: UsagePeriod) -> UsageStats {
        self.metrics.stats_at(period, unix_time_ms())
    }

    /// Returns recent UTC activity and lifetime streaks.
    pub fn activity(&self) -> UsageActivity {
        self.metrics.activity_at(unix_time_ms())
    }

    /// Number of unique durable inference requests.
    pub fn len(&self) -> usize {
        self.requests.len()
    }

    /// Whether no inference requests have been retained.
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }
}

fn valid_label(value: &str, maximum_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= maximum_bytes && !value.chars().any(char::is_control)
}

fn streaks(days: impl Iterator<Item = u64>, today: u64) -> (u64, u64) {
    let mut current_run = 0u64;
    let mut longest = 0u64;
    let mut previous: Option<u64> = None;
    let mut run_ending_at = BTreeMap::new();
    for day in days {
        current_run = if previous.is_some_and(|previous| previous.saturating_add(1) == day) {
            current_run.saturating_add(1)
        } else {
            1
        };
        longest = longest.max(current_run);
        run_ending_at.insert(day, current_run);
        previous = Some(day);
    }
    let current = run_ending_at
        .get(&today)
        .copied()
        .or_else(|| {
            today
                .checked_sub(1)
                .and_then(|day| run_ending_at.get(&day).copied())
        })
        .unwrap_or_default();
    (current, longest)
}

fn format_utc_day(day: u64) -> String {
    // Howard Hinnant's civil-from-days algorithm, with day zero at 1970-01-01.
    let shifted = day as i64 + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn ensure_private_directory(path: &Path) -> Result<(), UsageStoreError> {
    match path.symlink_metadata() {
        Ok(metadata) => {
            if !metadata.file_type().is_dir()
                || metadata.file_type().is_symlink()
                || !owner_private_directory(&metadata)
            {
                return Err(UsageStoreError::Storage);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(UsageStoreError::Storage)?;
            let parent_metadata = parent
                .symlink_metadata()
                .map_err(|_| UsageStoreError::Storage)?;
            if !parent_metadata.file_type().is_dir() || parent_metadata.file_type().is_symlink() {
                return Err(UsageStoreError::Storage);
            }
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            builder.create(path).map_err(|_| UsageStoreError::Storage)?;
        }
        Err(_) => return Err(UsageStoreError::Storage),
    }
    Ok(())
}

fn open_private_log(path: &Path) -> Result<File, UsageStoreError> {
    if let Ok(metadata) = path.symlink_metadata() {
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || !owner_private_file(&metadata)
        {
            return Err(UsageStoreError::Storage);
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|_| UsageStoreError::Storage)?;
    let metadata = file.metadata().map_err(|_| UsageStoreError::Storage)?;
    if !metadata.file_type().is_file() || !owner_private_file(&metadata) {
        return Err(UsageStoreError::Storage);
    }
    Ok(file)
}

fn owner_private_directory(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o777 == 0o700
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

fn owner_private_file(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o777 == 0o600
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(day: u64, ordinal: u64, total_tokens: u64) -> InferenceRequest {
        InferenceRequest {
            session_id: "session-usage-test".into(),
            request_ordinal: ordinal,
            provider: "anthropic".into(),
            model: "claude-test".into(),
            timestamp_ms: day * DAY_MILLISECONDS,
            prompt_tokens: total_tokens / 2,
            completion_tokens: total_tokens / 4,
            cache_read_tokens: 3,
            cache_write_tokens: 2,
            cache_write_1h_tokens: 1,
            reasoning_tokens: total_tokens / 8,
            total_tokens,
        }
    }

    #[test]
    fn durable_store_reopens_and_deduplicates_session_ordinals() {
        let directory = tempfile::tempdir().unwrap();
        let first = request(10, 0, 100);
        {
            let mut store = InferenceRequestStore::open(directory.path()).unwrap();
            assert!(store.record(first.clone()).unwrap());
            assert!(!store.record(first.clone()).unwrap());
            assert_eq!(store.len(), 1);
            let mut conflict = first.clone();
            conflict.total_tokens += 1;
            assert_eq!(store.record(conflict), Err(UsageStoreError::Conflict));
        }

        let store = InferenceRequestStore::open(directory.path()).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.lifetime().total_tokens, 100);
        assert_eq!(store.lifetime().request_count, 1);
    }

    #[test]
    fn incomplete_final_write_is_discarded_on_reopen() {
        let directory = tempfile::tempdir().unwrap();
        {
            let mut store = InferenceRequestStore::open(directory.path()).unwrap();
            store.record(request(0, 0, 20)).unwrap();
        }
        let path = directory.path().join(STORE_DIRECTORY).join(STORE_FILE);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"version":1"#)
            .unwrap();

        let store = InferenceRequestStore::open(directory.path()).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.lifetime().total_tokens, 20);
        assert!(std::fs::read(path).unwrap().ends_with(b"\n"));
    }

    #[test]
    fn metrics_cover_period_totals_activity_and_streaks() {
        let mut metrics = LifetimeMetricsStore::default();
        metrics.record(&request(0, 0, 80));
        metrics.record(&request(1, 1, 120));
        metrics.record(&request(3, 2, 160));

        let daily = metrics.stats_at(UsagePeriod::Daily, 3 * DAY_MILLISECONDS);
        assert_eq!(daily.total_tokens, 160);
        assert_eq!(daily.request_count, 1);
        assert_eq!(daily.cache_read_tokens, 3);
        assert_eq!(daily.cache_write_tokens, 2);
        assert_eq!(daily.cache_write_1h_tokens, 1);

        let weekly = metrics.stats_at(UsagePeriod::Weekly, 3 * DAY_MILLISECONDS);
        assert_eq!(weekly.total_tokens, 360);
        assert_eq!(weekly.request_count, 3);
        assert_eq!(weekly.cache_read_tokens, 9);
        assert_eq!(weekly.cache_write_tokens, 6);
        assert_eq!(weekly.cache_write_1h_tokens, 3);

        let activity = metrics.activity_at(3 * DAY_MILLISECONDS);
        assert_eq!(activity.current_streak, 1);
        assert_eq!(activity.longest_streak, 2);
        assert_eq!(activity.days.len(), 3);
        assert_eq!(activity.days[0].date, "1970-01-01");
        assert_eq!(activity.days[2].date, "1970-01-04");
        assert_eq!(metrics.lifetime().first_request_at_ms, Some(0));
        assert_eq!(
            metrics.lifetime().last_request_at_ms,
            Some(3 * DAY_MILLISECONDS)
        );
    }
}
