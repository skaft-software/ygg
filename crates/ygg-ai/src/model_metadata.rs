//! Human-facing model metadata generated from the models.dev canonical catalog.
//!
//! The build script refreshes this table while compiling `ygg-ai` and falls
//! back to a checked-in snapshot when the network is unavailable. Runtime code
//! performs only a binary search over generated static data.

mod generated {
    include!(concat!(env!("OUT_DIR"), "/models_dev_names.rs"));
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
    fn generated_registry_leaves_unknown_ids_untouched_for_the_caller() {
        assert_eq!(model_display_name("acme/unknown-model-v9"), None);
    }
}
