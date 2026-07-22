//! Built-in API-key provider definitions ported from Pi's provider catalog.

use ygg_ai::{
    CacheCompatibility, CacheControlFormat, Pricing, Protocol, ReasoningEffort,
    SessionAffinityFormat, TokenRate,
};

/// Declarative configuration for one built-in API-key provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderPreset {
    /// Provider id, also used as the endpoint and model namespace.
    pub id: &'static str,
    /// Human-readable provider name.
    pub name: &'static str,
    /// Versioned request base URL. Ygg requires a trailing slash.
    pub base_url: &'static str,
    /// API-key environment variables, checked in priority order.
    pub api_key_env: &'static [&'static str],
    /// Default protocol for discovered models.
    pub protocol: Protocol,
    /// How this provider's model inventory is populated.
    pub model_discovery: ModelDiscovery,
    /// Headers attached to every inference request.
    pub extra_headers: &'static [(&'static str, &'static str)],
}

/// Model inventory strategy for a provider preset.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelDiscovery {
    /// Use only the provider's embedded model list.
    Static,
    /// Query an OpenAI-compatible `GET /models` endpoint.
    OpenAiModels { filter: ModelFilter },
    /// Query Anthropic's `GET /models` endpoint.
    AnthropicModels,
    /// Query OpenRouter and retain its provider-specific pricing metadata.
    OpenRouterModels,
    /// Do not populate models automatically.
    None,
}

/// Filter applied to OpenAI-compatible model inventories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFilter {
    /// Accept every returned model id.
    All,
    /// Accept model ids beginning with any listed prefix.
    Prefix(&'static [&'static str]),
}

/// Static model metadata used by multi-protocol providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaticModelPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub protocol: Protocol,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub vision: bool,
    pub reasoning: bool,
    pub max_reasoning_effort: ReasoningEffort,
}

impl StaticModelPreset {
    #[allow(clippy::too_many_arguments)]
    const fn new(
        id: &'static str,
        name: &'static str,
        protocol: Protocol,
        context_window: u64,
        max_output_tokens: u64,
        vision: bool,
        reasoning: bool,
        max_reasoning_effort: ReasoningEffort,
    ) -> Self {
        Self {
            id,
            name,
            protocol,
            context_window,
            max_output_tokens,
            vision,
            reasoning,
            max_reasoning_effort,
        }
    }
}

const OPENAI_MODEL_PREFIXES: &[&str] = &["gpt-", "chatgpt-", "codex-", "o"];
const NVIDIA_HEADERS: &[(&str, &str)] = &[("NVCF-POLL-SECONDS", "3600")];

pub const OPENAI: ProviderPreset = ProviderPreset {
    id: "openai",
    name: "OpenAI",
    base_url: "https://api.openai.com/v1/",
    api_key_env: &["OPENAI_API_KEY"],
    protocol: Protocol::OpenAiResponses,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::Prefix(OPENAI_MODEL_PREFIXES),
    },
    extra_headers: &[],
};

pub const ANTHROPIC: ProviderPreset = ProviderPreset {
    id: "anthropic",
    name: "Anthropic",
    // Pi stores the unversioned SDK URL. Ygg codecs join the final method path,
    // so presets store the equivalent versioned base.
    base_url: "https://api.anthropic.com/v1/",
    api_key_env: &["ANTHROPIC_API_KEY"],
    protocol: Protocol::AnthropicMessages,
    model_discovery: ModelDiscovery::AnthropicModels,
    extra_headers: &[],
};

pub const DEEPSEEK: ProviderPreset = ProviderPreset {
    id: "deepseek",
    name: "DeepSeek",
    base_url: "https://api.deepseek.com/v1/",
    api_key_env: &["DEEPSEEK_API_KEY"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: &[],
};

pub const OPENROUTER: ProviderPreset = ProviderPreset {
    id: "openrouter",
    name: "OpenRouter",
    base_url: "https://openrouter.ai/api/v1/",
    api_key_env: &["OPENROUTER_API_KEY"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenRouterModels,
    extra_headers: &[],
};

pub const GROQ: ProviderPreset = ProviderPreset {
    id: "groq",
    name: "Groq",
    base_url: "https://api.groq.com/openai/v1/",
    api_key_env: &["GROQ_API_KEY"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: &[],
};

pub const CEREBRAS: ProviderPreset = ProviderPreset {
    id: "cerebras",
    name: "Cerebras",
    base_url: "https://api.cerebras.ai/v1/",
    api_key_env: &["CEREBRAS_API_KEY"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: &[],
};

pub const XAI: ProviderPreset = ProviderPreset {
    id: "xai",
    name: "xAI",
    base_url: "https://api.x.ai/v1/",
    api_key_env: &["XAI_API_KEY"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: &[],
};

pub const TOGETHER: ProviderPreset = ProviderPreset {
    id: "together",
    name: "Together AI",
    base_url: "https://api.together.ai/v1/",
    api_key_env: &["TOGETHER_API_KEY"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: &[],
};

pub const FIREWORKS: ProviderPreset = ProviderPreset {
    id: "fireworks",
    name: "Fireworks AI",
    base_url: "https://api.fireworks.ai/inference/v1/",
    api_key_env: &["FIREWORKS_API_KEY"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: &[],
};

pub const NVIDIA: ProviderPreset = ProviderPreset {
    id: "nvidia",
    name: "NVIDIA",
    base_url: "https://integrate.api.nvidia.com/v1/",
    api_key_env: &["NVIDIA_API_KEY"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: NVIDIA_HEADERS,
};

pub const HUGGINGFACE: ProviderPreset = ProviderPreset {
    id: "huggingface",
    name: "Hugging Face",
    base_url: "https://router.huggingface.co/v1/",
    api_key_env: &["HF_TOKEN"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: &[],
};

pub const MOONSHOTAI: ProviderPreset = ProviderPreset {
    id: "moonshotai",
    name: "Moonshot AI",
    base_url: "https://api.moonshot.ai/v1/",
    api_key_env: &["MOONSHOT_API_KEY"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: &[],
};

pub const XIAOMI: ProviderPreset = ProviderPreset {
    id: "xiaomi",
    name: "Xiaomi",
    base_url: "https://api.xiaomimimo.com/v1/",
    api_key_env: &["XIAOMI_API_KEY"],
    protocol: Protocol::OpenAiChat,
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: &[],
};

pub const MINIMAX: ProviderPreset = ProviderPreset {
    id: "minimax",
    name: "MiniMax",
    // Pi configures https://api.minimax.io/anthropic and its SDK appends
    // /v1/messages. This is the equivalent Ygg codec base.
    base_url: "https://api.minimax.io/anthropic/v1/",
    api_key_env: &["MINIMAX_API_KEY"],
    protocol: Protocol::AnthropicMessages,
    model_discovery: ModelDiscovery::Static,
    extra_headers: &[],
};

pub const OPENCODE: ProviderPreset = ProviderPreset {
    id: "opencode",
    name: "OpenCode Zen",
    base_url: "https://opencode.ai/zen/v1/",
    api_key_env: &["OPENCODE_API_KEY"],
    protocol: Protocol::OpenAiChat,
    // Registration seeds Pi's protocol-aware list, then supplements it from
    // the OpenAI-compatible inventory without overriding known entries.
    model_discovery: ModelDiscovery::OpenAiModels {
        filter: ModelFilter::All,
    },
    extra_headers: &[],
};

/// API-key providers whose protocols Ygg can currently encode and decode.
pub const BUILTIN_PROVIDERS: &[ProviderPreset] = &[
    OPENAI,
    ANTHROPIC,
    DEEPSEEK,
    OPENROUTER,
    GROQ,
    CEREBRAS,
    XAI,
    TOGETHER,
    FIREWORKS,
    NVIDIA,
    HUGGINGFACE,
    MOONSHOTAI,
    XIAOMI,
    MINIMAX,
    OPENCODE,
];

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
        "gpt-5.6-luna" => (
            (1_000_000, 6_000_000, 100_000, 1_250_000),
            Some((2_000_000, 9_000_000, 200_000, 2_500_000)),
        ),
        "gpt-5.6-sol" => (
            (5_000_000, 30_000_000, 500_000, 6_250_000),
            Some((10_000_000, 45_000_000, 1_000_000, 12_500_000)),
        ),
        "gpt-5.6-terra" => (
            (2_500_000, 15_000_000, 250_000, 3_125_000),
            Some((5_000_000, 22_500_000, 500_000, 6_250_000)),
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
        "gpt-5.6-luna" => (1_000_000, 6_000_000, 100_000, 1_250_000),
        "gpt-5.6-sol" => (5_000_000, 30_000_000, 500_000, 6_250_000),
        "gpt-5.6-terra" => (2_500_000, 15_000_000, 250_000, 3_125_000),
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

/// Return checked-in reference pricing for provider/model routes whose live
/// inventory APIs do not publish rates. Unknown or mutable routes remain
/// explicitly unpriced rather than borrowing another provider's prices.
pub(crate) fn model_pricing(provider_id: &str, model_id: &str) -> Option<Pricing> {
    let rates = match provider_id {
        "openai" => return openai_pricing(model_id),
        "anthropic" => return anthropic_pricing(model_id),
        "deepseek" => match model_id {
            "deepseek-v4-flash" => (140_000, 280_000, 2_800, 0),
            "deepseek-v4-pro" => (435_000, 870_000, 3_625, 0),
            _ => return None,
        },
        "minimax" => match model_id {
            "MiniMax-M2.7" => (300_000, 1_200_000, 60_000, 375_000),
            "MiniMax-M2.7-highspeed" => (600_000, 2_400_000, 60_000, 375_000),
            "MiniMax-M3" => (300_000, 1_200_000, 60_000, 0),
            _ => return None,
        },
        "opencode" => {
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
        _ => return None,
    };
    Some(flat_pricing(rates.0, rates.1, rates.2, rates.3))
}

/// Select Fireworks' wire protocol for a discovered model.
///
/// Fireworks' current catalog is predominantly Anthropic Messages despite its
/// OpenAI-compatible inventory endpoint. GLM 5.2 is the documented exception
/// and remains on Chat Completions.
pub(crate) fn discovered_protocol(
    provider_id: &str,
    model_id: &str,
    default: Protocol,
) -> Protocol {
    if provider_id != FIREWORKS.id {
        return default;
    }
    let model_id = model_id.to_ascii_lowercase();
    if model_id.contains("glm-5p2") || model_id.contains("glm-5.2") {
        Protocol::OpenAiChat
    } else {
        Protocol::AnthropicMessages
    }
}

/// Return the tested prompt-cache compatibility profile for a provider/model
/// route. Unknown routes retain the conservative protocol defaults.
pub(crate) fn cache_compatibility(
    provider_id: &str,
    model_id: &str,
    protocol: Protocol,
) -> CacheCompatibility {
    let mut cache = CacheCompatibility::default();

    match provider_id {
        "openai" => {
            cache.send_session_affinity_headers = true;
            cache.session_affinity_format = Some(SessionAffinityFormat::OpenAi);
        }
        // OpenRouter forwards Anthropic's explicit cache-control blocks only
        // for its Anthropic routes. These markers are required for prompt
        // caching there; regular OpenAI-compatible routes use their defaults.
        "openrouter" => {
            cache.send_session_affinity_headers = true;
            cache.session_affinity_format = Some(SessionAffinityFormat::OpenRouter);
            if model_id.starts_with("anthropic/") {
                cache.cache_control_format = Some(CacheControlFormat::Anthropic);
            }
        }
        // These OpenAI-compatible providers reject the 24-hour retention
        // parameter. Short retention remains enabled.
        "deepseek" | "together" | "nvidia" => {
            cache.supports_long_retention = false;
        }
        // Fireworks' Anthropic Messages routes require routing affinity and
        // accept cache controls on system/conversation blocks but reject them
        // on tool definitions.
        "fireworks" if protocol == Protocol::AnthropicMessages => {
            cache.supports_long_retention = false;
            cache.send_session_affinity_headers = true;
            cache.supports_cache_control_on_tools = false;
        }
        // Only these known OpenCode Chat routes reject long cache retention;
        // do not disable caching for the provider's unrelated models.
        "opencode"
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
        _ => {}
    }

    // OpenCode's known Responses routes use Pi's `openai-nosession` variant:
    // retain request affinity but omit the unsupported `session_id` header.
    if provider_id == "opencode"
        && protocol == Protocol::OpenAiResponses
        && (model_id.starts_with("gpt-") || model_id.starts_with("codex-"))
    {
        cache.send_session_id_header = false;
        cache.session_affinity_format = Some(SessionAffinityFormat::OpenAiNoSession);
    }

    cache
}

pub const MINIMAX_MODELS: &[StaticModelPreset] = &[
    StaticModelPreset::new(
        "MiniMax-M2.7",
        "MiniMax-M2.7",
        Protocol::AnthropicMessages,
        204_800,
        131_072,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "MiniMax-M2.7-highspeed",
        "MiniMax-M2.7-highspeed",
        Protocol::AnthropicMessages,
        204_800,
        131_072,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "MiniMax-M3",
        "MiniMax-M3",
        Protocol::AnthropicMessages,
        1_000_000,
        128_000,
        true,
        true,
        ReasoningEffort::High,
    ),
];

// Ported from @earendil-works/pi-ai 0.80.10. Models using Google's protocol
// are intentionally omitted until Ygg implements google-generative-ai.
pub const OPENCODE_MODELS: &[StaticModelPreset] = &[
    StaticModelPreset::new(
        "big-pickle",
        "Big Pickle",
        Protocol::OpenAiChat,
        200_000,
        32_000,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "claude-fable-5",
        "Claude Fable 5",
        Protocol::AnthropicMessages,
        1_000_000,
        128_000,
        true,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "claude-haiku-4-5",
        "Claude Haiku 4.5",
        Protocol::AnthropicMessages,
        200_000,
        64_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "claude-opus-4-1",
        "Claude Opus 4.1",
        Protocol::AnthropicMessages,
        200_000,
        32_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "claude-opus-4-5",
        "Claude Opus 4.5",
        Protocol::AnthropicMessages,
        200_000,
        64_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "claude-opus-4-6",
        "Claude Opus 4.6",
        Protocol::AnthropicMessages,
        1_000_000,
        128_000,
        true,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "claude-opus-4-7",
        "Claude Opus 4.7",
        Protocol::AnthropicMessages,
        1_000_000,
        128_000,
        true,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "claude-opus-4-8",
        "Claude Opus 4.8",
        Protocol::AnthropicMessages,
        1_000_000,
        128_000,
        true,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "claude-sonnet-4",
        "Claude Sonnet 4",
        Protocol::AnthropicMessages,
        200_000,
        64_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "claude-sonnet-4-5",
        "Claude Sonnet 4.5",
        Protocol::AnthropicMessages,
        200_000,
        64_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "claude-sonnet-4-6",
        "Claude Sonnet 4.6",
        Protocol::AnthropicMessages,
        1_000_000,
        64_000,
        true,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        Protocol::AnthropicMessages,
        1_000_000,
        128_000,
        true,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        Protocol::OpenAiChat,
        1_000_000,
        384_000,
        false,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "deepseek-v4-flash-free",
        "DeepSeek V4 Flash Free",
        Protocol::OpenAiChat,
        200_000,
        128_000,
        false,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        Protocol::OpenAiChat,
        1_000_000,
        384_000,
        false,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "glm-5",
        "GLM-5",
        Protocol::OpenAiChat,
        204_800,
        131_072,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "glm-5.1",
        "GLM-5.1",
        Protocol::OpenAiChat,
        204_800,
        131_072,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "glm-5.2",
        "GLM-5.2",
        Protocol::OpenAiChat,
        1_000_000,
        131_072,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "gpt-5",
        "GPT-5",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "gpt-5-codex",
        "GPT-5 Codex",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "gpt-5-nano",
        "GPT-5 Nano",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "gpt-5.1",
        "GPT-5.1",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "gpt-5.1-codex",
        "GPT-5.1 Codex",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "gpt-5.1-codex-max",
        "GPT-5.1 Codex Max",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "gpt-5.1-codex-mini",
        "GPT-5.1 Codex Mini",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "gpt-5.2",
        "GPT-5.2",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::Xhigh,
    ),
    StaticModelPreset::new(
        "gpt-5.2-codex",
        "GPT-5.2 Codex",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::Xhigh,
    ),
    StaticModelPreset::new(
        "gpt-5.3-codex",
        "GPT-5.3 Codex",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::Xhigh,
    ),
    StaticModelPreset::new(
        "gpt-5.4",
        "GPT-5.4",
        Protocol::OpenAiResponses,
        272_000,
        128_000,
        true,
        true,
        ReasoningEffort::Xhigh,
    ),
    StaticModelPreset::new(
        "gpt-5.4-mini",
        "GPT-5.4 Mini",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::Xhigh,
    ),
    StaticModelPreset::new(
        "gpt-5.4-nano",
        "GPT-5.4 Nano",
        Protocol::OpenAiResponses,
        400_000,
        128_000,
        true,
        true,
        ReasoningEffort::Xhigh,
    ),
    StaticModelPreset::new(
        "gpt-5.4-pro",
        "GPT-5.4 Pro",
        Protocol::OpenAiResponses,
        1_050_000,
        128_000,
        true,
        true,
        ReasoningEffort::Xhigh,
    ),
    StaticModelPreset::new(
        "gpt-5.5",
        "GPT-5.5",
        Protocol::OpenAiResponses,
        1_050_000,
        128_000,
        true,
        true,
        ReasoningEffort::Xhigh,
    ),
    StaticModelPreset::new(
        "gpt-5.5-pro",
        "GPT-5.5 Pro",
        Protocol::OpenAiResponses,
        1_050_000,
        128_000,
        true,
        true,
        ReasoningEffort::Xhigh,
    ),
    StaticModelPreset::new(
        "gpt-5.6-luna",
        "GPT-5.6 Luna",
        Protocol::OpenAiResponses,
        1_050_000,
        128_000,
        true,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "gpt-5.6-sol",
        "GPT-5.6 Sol",
        Protocol::OpenAiResponses,
        1_050_000,
        128_000,
        true,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "gpt-5.6-terra",
        "GPT-5.6 Terra",
        Protocol::OpenAiResponses,
        1_050_000,
        128_000,
        true,
        true,
        ReasoningEffort::Max,
    ),
    StaticModelPreset::new(
        "grok-4.5",
        "Grok 4.5",
        Protocol::OpenAiChat,
        500_000,
        500_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "grok-build-0.1",
        "Grok Build 0.1",
        Protocol::OpenAiChat,
        256_000,
        256_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "hy3-free",
        "Hy3 Free",
        Protocol::OpenAiChat,
        190_000,
        64_000,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "kimi-k2.5",
        "Kimi K2.5",
        Protocol::OpenAiChat,
        262_144,
        65_536,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "kimi-k2.6",
        "Kimi K2.6",
        Protocol::OpenAiChat,
        262_144,
        65_536,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "kimi-k2.7-code",
        "Kimi K2.7 Code",
        Protocol::OpenAiChat,
        262_144,
        262_144,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "mimo-v2.5-free",
        "MiMo V2.5 Free",
        Protocol::OpenAiChat,
        200_000,
        32_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "minimax-m2.5",
        "MiniMax-M2.5",
        Protocol::OpenAiChat,
        204_800,
        131_072,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "minimax-m2.7",
        "MiniMax-M2.7",
        Protocol::OpenAiChat,
        204_800,
        131_072,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "minimax-m3",
        "MiniMax-M3",
        Protocol::OpenAiChat,
        512_000,
        128_000,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "nemotron-3-ultra-free",
        "Nemotron 3 Ultra Free",
        Protocol::OpenAiChat,
        1_000_000,
        128_000,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "north-mini-code-free",
        "North Mini Code Free",
        Protocol::OpenAiChat,
        256_000,
        64_000,
        false,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "qwen3.5-plus",
        "Qwen3.5 Plus",
        Protocol::AnthropicMessages,
        262_144,
        65_536,
        true,
        true,
        ReasoningEffort::High,
    ),
    StaticModelPreset::new(
        "qwen3.6-plus",
        "Qwen3.6 Plus",
        Protocol::AnthropicMessages,
        262_144,
        65_536,
        true,
        true,
        ReasoningEffort::High,
    ),
];

// Deferred until the corresponding protocol/auth implementation exists:
// google (GEMINI_API_KEY), mistral (MISTRAL_API_KEY), amazon-bedrock (AWS_*),
// azure-openai-responses (AZURE_OPENAI_API_KEY), google-vertex,
// github-copilot (OAuth), cloudflare-workers-ai, and cloudflare-ai-gateway.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_provider_ids_and_urls_are_valid() {
        let mut ids = std::collections::HashSet::new();
        for preset in BUILTIN_PROVIDERS {
            assert!(ids.insert(preset.id), "duplicate provider {}", preset.id);
            assert!(
                !preset.api_key_env.is_empty(),
                "{} has no key env",
                preset.id
            );
            assert!(preset.base_url.ends_with('/'));
            let url = url::Url::parse(preset.base_url).unwrap();
            assert!(matches!(url.scheme(), "http" | "https"));
        }
    }

    #[test]
    fn expected_compatible_pi_providers_are_present() {
        let ids = BUILTIN_PROVIDERS
            .iter()
            .map(|preset| preset.id)
            .collect::<std::collections::HashSet<_>>();
        for expected in [
            "openai",
            "anthropic",
            "deepseek",
            "openrouter",
            "groq",
            "cerebras",
            "xai",
            "together",
            "fireworks",
            "nvidia",
            "huggingface",
            "moonshotai",
            "xiaomi",
            "minimax",
            "opencode",
        ] {
            assert!(ids.contains(expected), "missing provider {expected}");
        }
    }

    #[test]
    fn cache_compatibility_matches_known_provider_routes() {
        let openai = cache_compatibility(OPENAI.id, "gpt-5.4", Protocol::OpenAiResponses);
        assert!(openai.send_session_affinity_headers);
        assert_eq!(
            openai.session_affinity_format,
            Some(SessionAffinityFormat::OpenAi)
        );

        let openrouter = cache_compatibility(
            OPENROUTER.id,
            "anthropic/claude-sonnet-4-5",
            Protocol::OpenAiChat,
        );
        assert_eq!(
            openrouter.cache_control_format,
            Some(CacheControlFormat::Anthropic)
        );
        assert!(openrouter.send_session_affinity_headers);
        assert_eq!(
            openrouter.session_affinity_format,
            Some(SessionAffinityFormat::OpenRouter)
        );
        let openrouter_openai =
            cache_compatibility(OPENROUTER.id, "openai/gpt-5.4", Protocol::OpenAiChat);
        assert_eq!(openrouter_openai.cache_control_format, None);
        assert!(openrouter_openai.send_session_affinity_headers);
        assert_eq!(
            openrouter_openai.session_affinity_format,
            Some(SessionAffinityFormat::OpenRouter)
        );

        for provider in [DEEPSEEK.id, TOGETHER.id, NVIDIA.id] {
            assert!(
                !cache_compatibility(provider, "any", Protocol::OpenAiChat).supports_long_retention
            );
        }

        let fireworks = cache_compatibility(
            FIREWORKS.id,
            "accounts/fireworks/models/kimi-k2p7-code",
            Protocol::AnthropicMessages,
        );
        assert!(fireworks.send_session_affinity_headers);
        assert!(!fireworks.supports_cache_control_on_tools);
        assert!(!fireworks.supports_long_retention);
        assert_eq!(
            discovered_protocol(
                FIREWORKS.id,
                "accounts/fireworks/models/kimi-k2p7-code",
                Protocol::OpenAiChat,
            ),
            Protocol::AnthropicMessages
        );
        assert_eq!(
            discovered_protocol(
                FIREWORKS.id,
                "accounts/fireworks/models/glm-5p2",
                Protocol::OpenAiChat,
            ),
            Protocol::OpenAiChat
        );

        assert!(
            !cache_compatibility(OPENCODE.id, "deepseek-v4-pro", Protocol::OpenAiChat)
                .supports_long_retention
        );
        assert!(
            cache_compatibility(OPENCODE.id, "glm-5.2", Protocol::OpenAiChat)
                .supports_long_retention
        );
        assert!(
            !cache_compatibility(OPENCODE.id, "gpt-5.4", Protocol::OpenAiResponses)
                .send_session_id_header
        );
    }

    #[test]
    fn checked_in_static_models_have_explicit_provider_pricing() {
        for model in OPENCODE_MODELS {
            assert!(
                model_pricing(OPENCODE.id, model.id).is_some(),
                "missing OpenCode pricing for {}",
                model.id
            );
        }
        for model in MINIMAX_MODELS {
            assert!(
                model_pricing(MINIMAX.id, model.id).is_some(),
                "missing MiniMax pricing for {}",
                model.id
            );
        }

        let direct = model_pricing(DEEPSEEK.id, "deepseek-v4-pro").unwrap();
        let gateway = model_pricing(OPENCODE.id, "deepseek-v4-pro").unwrap();
        assert_eq!(direct.input, TokenRate(435_000));
        assert_eq!(gateway.input, TokenRate(1_740_000));

        let direct_gpt = model_pricing(OPENAI.id, "gpt-5").unwrap();
        let opencode_gpt = model_pricing(OPENCODE.id, "gpt-5").unwrap();
        assert_eq!(direct_gpt.input, TokenRate(1_250_000));
        assert_eq!(opencode_gpt.input, TokenRate(1_070_000));
        assert_eq!(model_pricing(OPENAI.id, "gpt-5.4").unwrap().tiers.len(), 1);
        assert!(model_pricing(OPENCODE.id, "gpt-5.4")
            .unwrap()
            .tiers
            .is_empty());
        assert!(model_pricing(OPENROUTER.id, "deepseek/deepseek-v4-pro").is_none());
    }

    #[test]
    fn opencode_omits_only_unsupported_google_models() {
        assert_eq!(OPENCODE_MODELS.len(), 51);
        assert!(OPENCODE_MODELS
            .iter()
            .all(|model| model.protocol != Protocol::AnthropicMessages
                || model.id.starts_with("claude-")
                || model.id.starts_with("qwen")));
        assert!(!OPENCODE_MODELS
            .iter()
            .any(|model| model.id.starts_with("gemini-")));
    }
}
