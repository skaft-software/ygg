//! Durable prompt-cache waste analysis.
//!
//! A new request can reuse at most the preceding request's prompt prefix—not
//! its newly generated output. The detector mirrors Pi's conservative rules:
//! it ignores first turns, resets after a real context compaction, does not
//! label providers that have never reported cache activity, and suppresses
//! misses of at most 1,024 tokens as breakpoint granularity noise.

use std::collections::HashMap;
use std::time::Duration;

use crate::session::{EntryId, EntryValue, Session, UsageRecord, UsageRecordKind};
use ygg_ai::{Cost, EndpointId, ModelId, Usage};

/// Reusable-prefix misses of this size or smaller are intentionally ignored.
pub const CACHE_MISS_NOISE_TOKENS: u64 = 1_024;
/// Anthropic's default short prompt-cache retention is five minutes.
pub const SHORT_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// A material cache miss attached to one durable assistant response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheMiss {
    /// Assistant entry whose provider request missed the reusable prefix.
    pub assistant: EntryId,
    /// Reusable tokens expected from the preceding request's prompt.
    pub expected_reusable_tokens: u64,
    /// Tokens the provider reported reading from cache.
    pub cache_read_tokens: u64,
    /// Reusable tokens that appear to have been billed again.
    pub missed_reusable_tokens: u64,
    /// Estimated extra cost relative to a cache read, when both paid and read
    /// per-token costs can be inferred from this response's breakdown.
    pub missed_cost_microdollars: Option<u64>,
    /// Whether the endpoint/model route changed since the preceding request.
    pub model_changed: bool,
    /// Time since the preceding request, when both records have timestamps.
    pub idle: Option<Duration>,
    /// Whether `idle` exceeded the normal five-minute short-cache TTL.
    pub idle_past_short_ttl: bool,
}

/// Aggregate cache effectiveness for assistant turns on the active branch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Number of durable assistant provider turns inspected.
    pub assistant_turns: u64,
    /// Sum of provider-reported cache reads across those turns.
    pub cache_read_tokens: u64,
    /// Sum of provider-reported cache writes across those turns.
    pub cache_write_tokens: u64,
    /// Total prompt tokens from the most recently inspected assistant turn.
    pub latest_prompt_tokens: u64,
    /// Provider-reported cache reads from the most recently inspected turn.
    pub latest_cache_read_tokens: u64,
    /// Reusable prior-prompt tokens considered after cache support was seen.
    pub reusable_prefix_tokens: u64,
    /// Material reusable-prefix misses (noise excluded).
    pub missed_reusable_tokens: u64,
    /// Number of material cache misses.
    pub miss_count: u64,
    /// Sum of miss-cost estimates that could be derived.
    pub missed_cost_microdollars: u64,
    /// Number of misses included in `missed_cost_microdollars`.
    pub priced_miss_count: u64,
}

impl CacheStats {
    /// Raw provider-reported cache-read rate for the latest assistant turn, in
    /// basis points.
    ///
    /// This is the footer metric: `cache_read / (input + cache_read +
    /// cache_write)` for the most recent request, matching Pi. It intentionally
    /// does not infer reusable-prefix overlap or apply the miss noise floor.
    /// `None` means the active branch has not reported any cache activity, or
    /// its latest assistant turn has no prompt-token usage.
    pub fn latest_raw_hit_rate_basis_points(self) -> Option<u16> {
        if self.latest_prompt_tokens == 0
            || (self.cache_read_tokens == 0 && self.cache_write_tokens == 0)
        {
            return None;
        }
        Some(
            ((u128::from(self.latest_cache_read_tokens) * 10_000)
                / u128::from(self.latest_prompt_tokens)) as u16,
        )
    }

    /// Effective hit rate over the reusable previous-prompt denominator, in
    /// basis points. Noise-floor misses are treated as hits, matching the
    /// detector's alert semantics. Returns `None` before cache activity is
    /// observable.
    pub fn hit_rate_basis_points(self) -> Option<u16> {
        if self.reusable_prefix_tokens == 0 {
            return None;
        }
        let hit = self
            .reusable_prefix_tokens
            .saturating_sub(self.missed_reusable_tokens)
            .min(self.reusable_prefix_tokens);
        Some(((u128::from(hit) * 10_000) / u128::from(self.reusable_prefix_tokens)) as u16)
    }
}

#[derive(Clone, Debug)]
struct PreviousRequest {
    prompt_tokens: u64,
    endpoint: Option<EndpointId>,
    model: Option<ModelId>,
    completed_at_unix_ms: Option<u64>,
    reported_cache: bool,
    branch_index: usize,
}

fn prompt_tokens(usage: Usage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens)
}

fn route_changed(previous: &PreviousRequest, current: &UsageRecord) -> bool {
    match (
        previous.endpoint.as_ref(),
        previous.model.as_ref(),
        current.endpoint.as_ref(),
        current.model.as_ref(),
    ) {
        (Some(old_endpoint), Some(old_model), Some(endpoint), Some(model)) => {
            old_endpoint != endpoint || old_model != model
        }
        _ => false,
    }
}

fn idle_duration(previous: &PreviousRequest, current: &UsageRecord) -> Option<Duration> {
    let previous = previous.completed_at_unix_ms?;
    let current = current.completed_at_unix_ms?;
    Some(Duration::from_millis(current.saturating_sub(previous)))
}

fn estimate_missed_cost(cost: Option<Cost>, usage: Usage, missed_tokens: u64) -> Option<u64> {
    let cost = cost?;
    let paid_tokens = usage.input_tokens.saturating_add(usage.cache_write_tokens);
    // A full miss has no observed cache-read rate. Without persisting an
    // entire mutable pricing catalog, token waste remains known but cost is
    // deliberately reported as unavailable rather than guessed.
    if paid_tokens == 0 || usage.cache_read_tokens == 0 {
        return None;
    }
    let paid_cost = cost.input.saturating_add(cost.cache_write);
    let paid_for_miss =
        u128::from(missed_tokens).checked_mul(u128::from(paid_cost))? / u128::from(paid_tokens);
    let read_for_miss = u128::from(missed_tokens).checked_mul(u128::from(cost.cache_read))?
        / u128::from(usage.cache_read_tokens);
    u64::try_from(paid_for_miss.saturating_sub(read_for_miss)).ok()
}

fn active_branch(session: &Session) -> Vec<&EntryId> {
    let mut reverse = Vec::new();
    let mut cursor = session.head_ref();
    while let Some(id) = cursor {
        let Some(entry) = session.entry(id) else {
            break;
        };
        reverse.push(&entry.id);
        cursor = entry.parent.as_ref();
    }
    reverse.reverse();
    reverse
}

/// Analyze durable assistant usage on the session's active branch.
///
/// Usage from abandoned branches is excluded. A persisted compaction entry
/// resets the reusable-prefix baseline; a failed compaction provider call does
/// not, because it did not alter model-visible context.
fn analyze_session_cache_impl(
    session: &Session,
    collect_misses: bool,
) -> (CacheStats, Vec<CacheMiss>) {
    let branch = active_branch(session);
    let positions: HashMap<&str, usize> = branch
        .iter()
        .enumerate()
        .map(|(index, id)| (id.0.as_str(), index))
        .collect();
    let turns = session
        .usage_records()
        .iter()
        .filter_map(|record| match &record.kind {
            UsageRecordKind::AssistantTurn { assistant } => positions
                .get(assistant.0.as_str())
                .copied()
                .map(|index| (index, assistant, record)),
            UsageRecordKind::Compaction => None,
        })
        .collect::<Vec<_>>();
    // Usage records are append-only after their assistant entry. Filtering
    // abandoned branches therefore preserves active-branch chronology even
    // after checkout: a newly created branch suffix is always appended after
    // its retained prefix. Avoid an unnecessary O(turns log turns) sort on
    // every footer/status refresh.

    let mut stats = CacheStats::default();
    let mut misses = Vec::new();
    let mut previous: Option<PreviousRequest> = None;

    for (branch_index, assistant, record) in turns {
        stats.assistant_turns = stats.assistant_turns.saturating_add(1);
        stats.cache_read_tokens = stats
            .cache_read_tokens
            .saturating_add(record.usage.cache_read_tokens);
        stats.cache_write_tokens = stats
            .cache_write_tokens
            .saturating_add(record.usage.cache_write_tokens);

        if previous.as_ref().is_some_and(|previous| {
            branch
                .get(previous.branch_index.saturating_add(1)..branch_index)
                .is_some_and(|between| {
                    between
                        .iter()
                        .filter_map(|id| session.entry(id))
                        .any(|entry| matches!(&entry.value, EntryValue::Compaction { .. }))
                })
        }) {
            previous = None;
        }

        let current_prompt_tokens = prompt_tokens(record.usage);
        stats.latest_prompt_tokens = current_prompt_tokens;
        stats.latest_cache_read_tokens = record.usage.cache_read_tokens;
        if let Some(prior) = previous.as_ref() {
            let current_reports_cache =
                record.usage.cache_read_tokens > 0 || record.usage.cache_write_tokens > 0;
            if current_prompt_tokens > 0 && (current_reports_cache || prior.reported_cache) {
                let expected = prior.prompt_tokens.min(current_prompt_tokens);
                stats.reusable_prefix_tokens =
                    stats.reusable_prefix_tokens.saturating_add(expected);
                let missed = expected.saturating_sub(record.usage.cache_read_tokens);
                if missed > CACHE_MISS_NOISE_TOKENS {
                    let idle = idle_duration(prior, record);
                    let missed_cost = estimate_missed_cost(record.cost, record.usage, missed);
                    stats.missed_reusable_tokens =
                        stats.missed_reusable_tokens.saturating_add(missed);
                    stats.miss_count = stats.miss_count.saturating_add(1);
                    if let Some(cost) = missed_cost {
                        stats.missed_cost_microdollars =
                            stats.missed_cost_microdollars.saturating_add(cost);
                        stats.priced_miss_count = stats.priced_miss_count.saturating_add(1);
                    }
                    if collect_misses {
                        misses.push(CacheMiss {
                            assistant: assistant.clone(),
                            expected_reusable_tokens: expected,
                            cache_read_tokens: record.usage.cache_read_tokens,
                            missed_reusable_tokens: missed,
                            missed_cost_microdollars: missed_cost,
                            model_changed: route_changed(prior, record),
                            idle,
                            idle_past_short_ttl: idle.is_some_and(|idle| idle > SHORT_CACHE_TTL),
                        });
                    }
                }
            }
        }

        if current_prompt_tokens > 0 {
            let reported_cache = previous
                .as_ref()
                .is_some_and(|previous| previous.reported_cache)
                || record.usage.cache_read_tokens > 0
                || record.usage.cache_write_tokens > 0;
            previous = Some(PreviousRequest {
                prompt_tokens: current_prompt_tokens,
                endpoint: record.endpoint.clone(),
                model: record.model.clone(),
                completed_at_unix_ms: record.completed_at_unix_ms,
                reported_cache,
                branch_index,
            });
        }
    }

    (stats, misses)
}

/// Analyze cache effectiveness and return material miss details.
pub fn analyze_session_cache(session: &Session) -> (CacheStats, Vec<CacheMiss>) {
    analyze_session_cache_impl(session, true)
}

/// Analyze only aggregate cache statistics without allocating per-miss
/// diagnostic records. This is the hot path for resume/footer telemetry.
pub fn analyze_session_cache_stats(session: &Session) -> CacheStats {
    analyze_session_cache_impl(session, false).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::EntryValue;
    use ygg_ai::{AssistantMessage, AssistantPart, Message, Protocol, UserMessage, UserPart};

    fn user(text: &str) -> EntryValue {
        EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::Text(text.to_string())],
        }))
    }

    fn assistant(model: &str, text: &str) -> EntryValue {
        EntryValue::Message(Message::Assistant(AssistantMessage {
            content: vec![AssistantPart::Text(text.to_string())],
            model: ModelId(model.to_string()),
            protocol: Protocol::OpenAiChat,
        }))
    }

    fn usage(prompt: u64, read: u64, write: u64) -> Usage {
        Usage {
            input_tokens: prompt.saturating_sub(read).saturating_sub(write),
            cache_read_tokens: read,
            cache_write_tokens: write,
            total_tokens: prompt,
            ..Usage::default()
        }
    }

    fn record_turn(
        session: &mut Session,
        model: &str,
        prompt: u64,
        read: u64,
        write: u64,
    ) -> EntryId {
        session.append(user("next")).unwrap();
        let entry = session.append(assistant(model, "answer")).unwrap();
        session
            .record_assistant_usage(
                entry.clone(),
                EndpointId("provider".to_string()),
                ModelId(model.to_string()),
                usage(prompt, read, write),
                None,
            )
            .unwrap();
        entry
    }

    #[test]
    fn ignores_noise_and_reports_material_reusable_prefix_misses() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        record_turn(&mut session, "a", 10_000, 0, 10_000);
        record_turn(&mut session, "a", 11_000, 9_100, 0); // 900-token noise
        let missed_entry = record_turn(&mut session, "b", 12_000, 2_000, 0);

        let (stats, misses) = analyze_session_cache(&session);
        assert_eq!(analyze_session_cache_stats(&session), stats);
        assert_eq!(misses.len(), 1);
        assert_eq!(misses[0].assistant, missed_entry);
        assert_eq!(misses[0].missed_reusable_tokens, 9_000);
        assert!(misses[0].model_changed);
        assert!(!misses[0].idle_past_short_ttl);
        assert_eq!(stats.reusable_prefix_tokens, 21_000);
        assert_eq!(stats.missed_reusable_tokens, 9_000);
        assert_eq!(stats.hit_rate_basis_points(), Some(5_714));
        // The footer follows Pi: it shows the latest request's raw provider
        // ratio, not this cumulative material-miss rate.
        assert_eq!(stats.latest_raw_hit_rate_basis_points(), Some(1_666));
    }

    #[test]
    fn latest_raw_hit_rate_keeps_a_complete_miss_visible_after_cache_activity() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("latest-miss.jsonl")).unwrap();
        record_turn(&mut session, "a", 10_000, 0, 10_000);
        record_turn(&mut session, "a", 11_000, 0, 0);

        let (stats, _) = analyze_session_cache(&session);
        assert_eq!(stats.latest_raw_hit_rate_basis_points(), Some(0));
    }

    #[test]
    fn does_not_flag_non_cache_providers_and_resets_after_compaction() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("no-cache.jsonl")).unwrap();
        record_turn(&mut session, "a", 10_000, 0, 0);
        record_turn(&mut session, "a", 11_000, 0, 0);
        let (stats, misses) = analyze_session_cache(&session);
        assert_eq!(stats.reusable_prefix_tokens, 0);
        assert_eq!(stats.latest_raw_hit_rate_basis_points(), None);
        assert!(misses.is_empty());

        let mut session = Session::create(directory.path().join("compacted.jsonl")).unwrap();
        let first_kept = record_turn(&mut session, "a", 12_000, 0, 12_000);
        // Alter the model-visible context. The next request starts a fresh
        // baseline rather than becoming a miss.
        session.compact("summary", first_kept).unwrap();
        record_turn(&mut session, "a", 13_000, 0, 0);
        let (_, misses) = analyze_session_cache(&session);
        assert!(misses.is_empty());
    }
}
