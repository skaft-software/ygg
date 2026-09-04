//! Prompt-cache compatibility selected by provider declarations.

use ygg_ai::{CacheCompatibility, CacheControlFormat, Protocol, SessionAffinityFormat};

use super::contract::CompatibilityProfile;

/// Return the tested prompt-cache compatibility for a declaration-selected
/// provider route. Existing codecs receive only this data-derived policy; they
/// do not branch on a provider identifier.
pub(crate) fn cache_compatibility(
    profile: CompatibilityProfile,
    model_id: &str,
    protocol: Protocol,
) -> CacheCompatibility {
    let mut cache = CacheCompatibility::default();

    match profile {
        CompatibilityProfile::OpenAi => {
            cache.send_session_affinity_headers = true;
            cache.session_affinity_format = Some(SessionAffinityFormat::OpenAi);
        }
        // OpenRouter forwards Anthropic's explicit cache-control blocks only
        // for its Anthropic routes. These markers are required for prompt
        // caching there; regular OpenAI-compatible routes use their defaults.
        CompatibilityProfile::OpenRouter => {
            cache.send_session_affinity_headers = true;
            cache.session_affinity_format = Some(SessionAffinityFormat::OpenRouter);
            if model_id.starts_with("anthropic/") {
                cache.cache_control_format = Some(CacheControlFormat::Anthropic);
            }
        }
        // These OpenAI-compatible providers reject the 24-hour retention
        // parameter. Short retention remains enabled.
        CompatibilityProfile::ShortRetention => {
            cache.supports_long_retention = false;
        }
        // Fireworks' Anthropic Messages routes require routing affinity and
        // accept cache controls on system/conversation blocks but reject them
        // on tool definitions.
        CompatibilityProfile::Fireworks if protocol == Protocol::AnthropicMessages => {
            cache.supports_long_retention = false;
            cache.send_session_affinity_headers = true;
            cache.supports_cache_control_on_tools = false;
        }
        // Only these known OpenCode Chat routes reject long cache retention;
        // do not disable caching for the provider's unrelated models.
        CompatibilityProfile::OpenCode
            if matches!(
                model_id,
                "deepseek-v4-flash"
                    | "deepseek-v4-pro"
                    | "kimi-k2.5"
                    | "kimi-k2.6"
                    | "minimax-m2.7"
            ) =>
        {
            cache.supports_long_retention = false;
        }
        CompatibilityProfile::Codex => {
            cache.supports_long_retention = false;
            cache.send_session_id_header = false;
            cache.send_session_affinity_headers = true;
            cache.session_affinity_format = Some(SessionAffinityFormat::Codex);
        }
        CompatibilityProfile::Default
        | CompatibilityProfile::Fireworks
        | CompatibilityProfile::OpenCode
        | CompatibilityProfile::Custom => {}
    }

    // OpenCode's known Responses routes use Pi's `openai-nosession` variant:
    // retain request affinity but omit the unsupported `session_id` header.
    if profile == CompatibilityProfile::OpenCode
        && protocol == Protocol::OpenAiResponses
        && (model_id.starts_with("gpt-") || model_id.starts_with("codex-"))
    {
        cache.send_session_id_header = false;
        cache.session_affinity_format = Some(SessionAffinityFormat::OpenAiNoSession);
    }

    cache
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::contract::{FIREWORKS, OPENAI, OPENCODE, OPENROUTER};

    #[test]
    fn generated_profiles_preserve_known_route_behavior() {
        let openai =
            cache_compatibility(OPENAI.compatibility, "gpt-5.4", Protocol::OpenAiResponses);
        assert_eq!(
            openai.session_affinity_format,
            Some(SessionAffinityFormat::OpenAi)
        );

        let openrouter = cache_compatibility(
            OPENROUTER.compatibility,
            "anthropic/claude-sonnet-4-5",
            Protocol::OpenAiChat,
        );
        assert_eq!(
            openrouter.cache_control_format,
            Some(CacheControlFormat::Anthropic)
        );

        let fireworks = cache_compatibility(
            FIREWORKS.compatibility,
            "accounts/fireworks/models/kimi-k2p7-code",
            Protocol::AnthropicMessages,
        );
        assert!(!fireworks.supports_cache_control_on_tools);

        let opencode =
            cache_compatibility(OPENCODE.compatibility, "gpt-5.4", Protocol::OpenAiResponses);
        assert!(!opencode.send_session_id_header);
    }
}
