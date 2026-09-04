//! Provider pricing policies selected by declarations.

use ygg_ai::{Pricing, PricingTier, TokenRate};

use super::contract::{PricingProfile, ProviderDeclaration};

fn flat_pricing(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Pricing {
    Pricing {
        input: TokenRate(input),
        output: TokenRate(output),
        cache_read: TokenRate(cache_read),
        cache_write_5m: TokenRate(cache_write),
        cache_write_1h: None,
        reasoning: None,
        tiers: vec![],
    }
}

type FlatRates = (u64, u64, u64, u64);

fn pricing_with_long_context_tier(base: FlatRates, tier: Option<FlatRates>) -> Pricing {
    let mut pricing = flat_pricing(base.0, base.1, base.2, base.3);
    pricing.tiers = tier
        .map(|rates| ygg_ai::PricingTier {
            // Pi's source catalogs express this as "above 272000".
            min_input_tokens: 272_001,
            input: Some(TokenRate(rates.0)),
            output: Some(TokenRate(rates.1)),
            cache_read: Some(TokenRate(rates.2)),
            cache_write_5m: Some(TokenRate(rates.3)),
            cache_write_1h: None,
            reasoning: None,
        })
        .into_iter()
        .collect();
    pricing
}

fn openai_pricing(model_id: &str) -> Option<Pricing> {
    let (base, tier) = match model_id {
        "gpt-5" | "gpt-5-codex" | "gpt-5.1" | "gpt-5.1-codex" | "gpt-5.1-codex-max" => {
            ((1_250_000, 10_000_000, 125_000, 0), None)
        }
        "gpt-5-nano" => ((50_000, 400_000, 5_000, 0), None),
        "gpt-5.1-codex-mini" => ((250_000, 2_000_000, 25_000, 0), None),
        "gpt-5.2" | "gpt-5.2-codex" | "gpt-5.3-codex" => {
            ((1_750_000, 14_000_000, 175_000, 0), None)
        }
        "gpt-5.4" => (
            (2_500_000, 15_000_000, 250_000, 0),
            Some((5_000_000, 22_500_000, 500_000, 0)),
        ),
        "gpt-5.4-mini" => ((750_000, 4_500_000, 75_000, 0), None),
        "gpt-5.4-nano" => ((200_000, 1_250_000, 20_000, 0), None),
        "gpt-5.4-pro" | "gpt-5.5-pro" => (
            (30_000_000, 180_000_000, 0, 0),
            Some((60_000_000, 270_000_000, 0, 0)),
        ),
        "gpt-5.5" => (
            (5_000_000, 30_000_000, 500_000, 0),
            Some((10_000_000, 45_000_000, 1_000_000, 0)),
        ),
        // GPT-5.6 uses OpenAI's published standard costs (Pi 0.84.4
        // correction); the checked-in models.dev snapshot already agrees
        // with these rates.
        "gpt-5.6-luna" => (
            (200_000, 1_200_000, 20_000, 250_000),
            Some((400_000, 1_800_000, 40_000, 500_000)),
        ),
        "gpt-5.6-sol" => (
            (5_000_000, 30_000_000, 500_000, 6_250_000),
            Some((10_000_000, 45_000_000, 1_000_000, 12_500_000)),
        ),
        "gpt-5.6-terra" => (
            (2_000_000, 12_000_000, 200_000, 2_500_000),
            Some((4_000_000, 18_000_000, 400_000, 5_000_000)),
        ),
        _ => return None,
    };
    Some(pricing_with_long_context_tier(base, tier))
}

fn opencode_openai_pricing(model_id: &str) -> Option<Pricing> {
    let rates = match model_id {
        "gpt-5" | "gpt-5-codex" | "gpt-5.1" | "gpt-5.1-codex" => (1_070_000, 8_500_000, 107_000, 0),
        "gpt-5-nano" => (50_000, 400_000, 5_000, 0),
        "gpt-5.1-codex-max" => (1_250_000, 10_000_000, 125_000, 0),
        "gpt-5.1-codex-mini" => (250_000, 2_000_000, 25_000, 0),
        "gpt-5.2" | "gpt-5.2-codex" | "gpt-5.3-codex" => (1_750_000, 14_000_000, 175_000, 0),
        "gpt-5.4" => (2_500_000, 15_000_000, 250_000, 0),
        "gpt-5.4-mini" => (750_000, 4_500_000, 75_000, 0),
        "gpt-5.4-nano" => (200_000, 1_250_000, 20_000, 0),
        "gpt-5.4-pro" | "gpt-5.5-pro" => (30_000_000, 180_000_000, 30_000_000, 0),
        "gpt-5.5" => (5_000_000, 30_000_000, 500_000, 0),
        "gpt-5.6-luna" => (200_000, 1_200_000, 20_000, 250_000),
        "gpt-5.6-sol" => (5_000_000, 30_000_000, 500_000, 6_250_000),
        "gpt-5.6-terra" => (2_000_000, 12_000_000, 200_000, 2_500_000),
        _ => return None,
    };
    Some(flat_pricing(rates.0, rates.1, rates.2, rates.3))
}

fn anthropic_pricing(model_id: &str) -> Option<Pricing> {
    let rates = if model_id.starts_with("claude-fable-5") {
        (10_000_000, 50_000_000, 1_000_000, 12_500_000)
    } else if model_id.starts_with("claude-haiku-4-5") {
        (1_000_000, 5_000_000, 100_000, 1_250_000)
    } else if model_id.starts_with("claude-opus-4-1") {
        (15_000_000, 75_000_000, 1_500_000, 18_750_000)
    } else if [
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
    ]
    .iter()
    .any(|prefix| model_id.starts_with(prefix))
    {
        (5_000_000, 25_000_000, 500_000, 6_250_000)
    } else if ["claude-sonnet-4", "claude-sonnet-4-5", "claude-sonnet-4-6"]
        .iter()
        .any(|prefix| model_id.starts_with(prefix))
    {
        (3_000_000, 15_000_000, 300_000, 3_750_000)
    } else if model_id.starts_with("claude-sonnet-5") {
        (2_000_000, 10_000_000, 200_000, 2_500_000)
    } else {
        return None;
    };
    Some(flat_pricing(rates.0, rates.1, rates.2, rates.3))
}

fn mistral_pricing(model_id: &str) -> Option<Pricing> {
    let rates = match model_id {
        "codestral-latest" => (300_000, 900_000, 0, 0),
        "devstral-latest" => (400_000, 2_000_000, 0, 0),
        "magistral-medium-latest" => (2_000_000, 5_000_000, 0, 0),
        "mistral-large-latest" => (500_000, 1_500_000, 0, 0),
        "mistral-small-latest" => (150_000, 600_000, 0, 0),
        "pixtral-large-latest" => (2_000_000, 6_000_000, 0, 0),
        _ => return None,
    };
    Some(flat_pricing(rates.0, rates.1, rates.2, rates.3))
}

fn cloudflare_workers_ai_pricing(model_id: &str) -> Option<Pricing> {
    let rates = match model_id {
        "@cf/meta/llama-4-scout-17b-16e-instruct" => (270_000, 850_000, 0, 0),
        "@cf/mistralai/mistral-small-3.1-24b-instruct" => (351_000, 555_000, 0, 0),
        "@cf/moonshotai/kimi-k2.7-code" => (950_000, 4_000_000, 190_000, 0),
        "@cf/openai/gpt-oss-120b" => (350_000, 750_000, 0, 0),
        "@cf/zai-org/glm-5.2" => (1_400_000, 4_400_000, 260_000, 0),
        _ => return None,
    };
    Some(flat_pricing(rates.0, rates.1, rates.2, rates.3))
}

fn cloudflare_ai_gateway_pricing(model_id: &str) -> Option<Pricing> {
    let rates = match model_id {
        "claude-haiku-4-5" => (1_000_000, 5_000_000, 100_000, 1_250_000),
        "claude-sonnet-4-5" => (3_000_000, 15_000_000, 300_000, 3_750_000),
        "claude-opus-4-5" => (5_000_000, 25_000_000, 500_000, 6_250_000),
        "gpt-4o" => (2_500_000, 10_000_000, 1_250_000, 0),
        "gpt-4o-mini" => (150_000, 600_000, 80_000, 0),
        "gpt-5.4" => (2_500_000, 15_000_000, 250_000, 0),
        "o3" => (2_000_000, 8_000_000, 500_000, 0),
        "o4-mini" => (1_100_000, 4_400_000, 280_000, 0),
        "workers-ai/@cf/moonshotai/kimi-k2.6" => (950_000, 4_000_000, 160_000, 0),
        _ => return None,
    };
    Some(flat_pricing(rates.0, rates.1, rates.2, rates.3))
}

/// Return Ygg-owned pricing overrides for provider/model routes whose live
/// inventory APIs do not publish rates. Special cases such as long-context
/// tiers remain here; the declaration-aware [`pricing_for`] wrapper falls back to the
/// checked-in models.dev snapshot for other routes.
fn legacy_model_pricing(profile: PricingProfile, model_id: &str) -> Option<Pricing> {
    let rates = match profile {
        PricingProfile::OpenAi => return openai_pricing(model_id),
        PricingProfile::Anthropic => return anthropic_pricing(model_id),
        PricingProfile::DeepSeek => match model_id {
            "deepseek-v4-flash" => (140_000, 280_000, 2_800, 0),
            "deepseek-v4-pro" => (435_000, 870_000, 3_625, 0),
            _ => return None,
        },
        PricingProfile::MiniMax => match model_id {
            "MiniMax-M2.7" => (300_000, 1_200_000, 60_000, 375_000),
            "MiniMax-M2.7-highspeed" => (600_000, 2_400_000, 60_000, 375_000),
            "MiniMax-M3" => (300_000, 1_200_000, 60_000, 0),
            _ => return None,
        },
        PricingProfile::OpenCode => {
            if let Some(pricing) =
                opencode_openai_pricing(model_id).or_else(|| anthropic_pricing(model_id))
            {
                return Some(pricing);
            }
            match model_id {
                "big-pickle"
                | "deepseek-v4-flash-free"
                | "hy3-free"
                | "mimo-v2.5-free"
                | "nemotron-3-ultra-free"
                | "north-mini-code-free" => (0, 0, 0, 0),
                "deepseek-v4-flash" => (140_000, 280_000, 28_000, 0),
                "deepseek-v4-pro" => (1_740_000, 3_840_000, 145_000, 0),
                "glm-5" => (1_000_000, 3_200_000, 200_000, 0),
                "glm-5.1" | "glm-5.2" => (1_400_000, 4_400_000, 260_000, 0),
                "grok-4.5" => (2_000_000, 6_000_000, 500_000, 0),
                "grok-build-0.1" => (1_000_000, 2_000_000, 200_000, 0),
                "kimi-k2.5" => (600_000, 3_000_000, 80_000, 0),
                "kimi-k2.6" => (950_000, 4_000_000, 160_000, 0),
                "kimi-k2.7-code" => (950_000, 4_000_000, 190_000, 0),
                "minimax-m2.5" | "minimax-m2.7" | "minimax-m3" => (300_000, 1_200_000, 60_000, 0),
                "qwen3.5-plus" => (200_000, 1_200_000, 20_000, 250_000),
                "qwen3.6-plus" => (500_000, 3_000_000, 50_000, 625_000),
                _ => return None,
            }
        }
        PricingProfile::Mistral => return mistral_pricing(model_id),
        PricingProfile::CloudflareWorkersAi => return cloudflare_workers_ai_pricing(model_id),
        PricingProfile::CloudflareAiGateway => return cloudflare_ai_gateway_pricing(model_id),
        PricingProfile::Subscription => return subscription_pricing(model_id),
        _ => return None,
    };
    Some(flat_pricing(rates.0, rates.1, rates.2, rates.3))
}

fn subscription_pricing(model_id: &str) -> Option<Pricing> {
    let (input, output, cache_read, cache_write, tier) = match model_id {
        "gpt-5.3-codex-spark" => (1_750_000, 14_000_000, 175_000, 0, None),
        "gpt-5.4" => (
            2_500_000,
            15_000_000,
            250_000,
            0,
            Some((5_000_000, 22_500_000, 500_000, 0)),
        ),
        "gpt-5.4-mini" => (750_000, 4_500_000, 75_000, 0, None),
        "gpt-5.4-pro" | "gpt-5.5-pro" => (
            30_000_000,
            180_000_000,
            0,
            0,
            Some((60_000_000, 270_000_000, 0, 0)),
        ),
        "gpt-5.5" => (
            5_000_000,
            30_000_000,
            500_000,
            0,
            Some((10_000_000, 45_000_000, 1_000_000, 0)),
        ),
        // GPT-5.6 uses OpenAI's published standard costs, which are well below
        // the older catalog estimates (Pi 0.84.4 pinned these as authoritative).
        "gpt-5.6-luna" => (
            200_000,
            1_200_000,
            20_000,
            250_000,
            Some((400_000, 1_800_000, 40_000, 500_000)),
        ),
        "gpt-5.6-sol" => (
            5_000_000,
            30_000_000,
            500_000,
            6_250_000,
            Some((10_000_000, 45_000_000, 1_000_000, 12_500_000)),
        ),
        "gpt-5.6-terra" => (
            2_000_000,
            12_000_000,
            200_000,
            2_500_000,
            Some((4_000_000, 18_000_000, 400_000, 5_000_000)),
        ),
        _ => return None,
    };
    let tiers = tier
        .map(|(input, output, cache_read, cache_write)| PricingTier {
            // Pi's source catalog expresses this as "above 272000".
            min_input_tokens: 272_001,
            input: Some(TokenRate(input)),
            output: Some(TokenRate(output)),
            cache_read: Some(TokenRate(cache_read)),
            cache_write_5m: Some(TokenRate(cache_write)),
            cache_write_1h: None,
            reasoning: None,
        })
        .into_iter()
        .collect();
    Some(Pricing {
        input: TokenRate(input),
        output: TokenRate(output),
        cache_read: TokenRate(cache_read),
        cache_write_5m: TokenRate(cache_write),
        cache_write_1h: None,
        reasoning: None,
        tiers,
    })
}

/// Return trusted pricing for a provider/model route.
///
/// Provider-specific overrides preserve Ygg's special cases (for example
/// OpenAI long-context tiers). The checked-in models.dev snapshot fills in
/// newly released and discovered routes, so discovery can provide trusted
/// pricing without another hand-maintained model match arm.
pub(crate) fn pricing_for(provider: &ProviderDeclaration, model_id: &str) -> Option<Pricing> {
    legacy_model_pricing(provider.pricing, model_id).or_else(|| {
        let reference_provider = match provider.pricing {
            PricingProfile::Subscription => "openai",
            _ => provider.id,
        };
        ygg_ai::model_metadata::model_pricing(reference_provider, model_id)
    })
}
