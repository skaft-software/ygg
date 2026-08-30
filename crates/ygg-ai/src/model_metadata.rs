//! Human-facing model metadata generated from the models.dev canonical catalog.
//!
//! The build script consumes checked-in models.dev snapshots. Runtime code
//! performs only binary searches over generated static data, so pricing remains
//! deterministic and available in offline builds.

use crate::pricing::{Pricing, TokenRate};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/models_dev_names.rs"));
    include!(concat!(env!("OUT_DIR"), "/models_dev_pricing.rs"));
}

fn lookup(table: &'static [(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table
        .binary_search_by(|(candidate, _)| candidate.cmp(&key))
        .ok()
        .map(|index| table[index].1)
}

fn lookup_key(key: &str) -> Option<&'static str> {
    lookup(generated::MODEL_NAMES, key).or_else(|| {
        let leaf = key.rsplit('/').next().unwrap_or(key);
        lookup(generated::MODEL_NAME_ALIASES, leaf)
    })
}

fn lookup_pricing(key: &str) -> Option<Pricing> {
    generated::MODEL_PRICING
        .binary_search_by(|(candidate, ..)| candidate.cmp(&key))
        .ok()
        .map(|index| {
            let (_, input, output, cache_read, cache_write_5m, reasoning) =
                generated::MODEL_PRICING[index];
            Pricing {
                input: TokenRate(input),
                output: TokenRate(output),
                cache_read: TokenRate(cache_read),
                cache_write_5m: TokenRate(cache_write_5m),
                // `cost_of` applies Anthropic's documented 1-hour cache-write
                // default (2x input) when this provider-specific field is absent.
                cache_write_1h: None,
                reasoning: reasoning.map(TokenRate),
                tiers: Vec::new(),
            }
        })
}

/// Return checked-in models.dev pricing for a provider/model route.
///
/// The key is provider-scoped because an aggregator can charge a different
/// rate for the same upstream model. Rates are represented as microdollars per
/// million tokens and are converted during the explicit snapshot refresh, not
/// at runtime.
pub fn model_pricing(provider_id: &str, model_id: &str) -> Option<Pricing> {
    let provider = provider_id.trim().to_ascii_lowercase();
    let model = model_id.trim().to_ascii_lowercase();
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    let key = format!("{provider}/{model}");
    lookup_pricing(&key)
}

/// Return the models.dev display name for a canonical or uniquely identifiable
/// model ID.
///
/// Exact canonical IDs win. Bare model names are accepted only when their leaf
/// is unique in the generated catalog. The historical `custom/` registry prefix
/// is ignored, but repository/artifact suffixes are not guessed here; callers
/// can apply a conservative fallback for models absent from models.dev.
pub fn model_display_name(id: &str) -> Option<&'static str> {
    let normalized = id.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    lookup_key(&normalized).or_else(|| normalized.strip_prefix("custom/").and_then(lookup_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_registry_resolves_canonical_and_unique_leaf_ids() {
        assert!(model_display_name("openai/gpt-4o-mini").is_some());
        assert_eq!(
            model_display_name("alibaba/qwen3.6-27b"),
            Some("Qwen3.6 27B")
        );
        assert_eq!(model_display_name("qwen3.6-27b"), Some("Qwen3.6 27B"));
    }

    #[test]
    fn generated_pricing_is_provider_scoped_and_integer_based() {
        let direct = model_pricing("openai", "gpt-5").expect("snapshot price");
        assert_eq!(direct.input, TokenRate(1_250_000));
        assert_eq!(direct.output, TokenRate(10_000_000));

        let routed = model_pricing("openrouter", "deepseek/deepseek-v4-pro")
            .expect("provider-specific snapshot price");
        assert_eq!(routed.input, TokenRate(417_252));
        assert_eq!(routed.output, TokenRate(834_504));
        assert_eq!(routed.reasoning, None);
        assert!(model_pricing("openai", "gpt-5.6").is_none());
        assert!(model_pricing("openai", "gpt-5.6-sol").is_some());
        assert!(model_pricing("unknown", "model").is_none());
    }

    #[test]
    fn generated_registry_leaves_unknown_ids_untouched_for_the_caller() {
        assert_eq!(model_display_name("acme/unknown-model-v9"), None);
    }
}
