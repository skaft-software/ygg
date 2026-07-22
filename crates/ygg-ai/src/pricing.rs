//! Integer-microdollar cost accounting.

#![deny(clippy::float_arithmetic)]

use crate::error::PricingError;
use crate::types::Usage;
use serde::{Deserialize, Serialize};

/// Number of picodollars in one microdollar. Cost totals retain a remainder in
/// this finer unit so repeated small requests accumulate without float drift.
pub const PICODOLLARS_PER_MICRODOLLAR: u32 = 1_000_000;

/// Rate for tokens, represented in microdollars per 1,000,000 tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TokenRate(pub u64);

/// Pricing rules for a model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pricing {
    /// Rate for prompt input tokens.
    pub input: TokenRate,
    /// Rate for generated output tokens.
    pub output: TokenRate,
    /// Rate for cached input tokens that were read.
    pub cache_read: TokenRate,
    /// Rate for input tokens that caused a cache write (5m duration).
    pub cache_write_5m: TokenRate,
    /// Rate for input tokens that caused a cache write (1h duration).
    pub cache_write_1h: Option<TokenRate>,
    /// Rate for reasoning tokens.
    pub reasoning: Option<TokenRate>,
    /// Tiered pricing rules.
    pub tiers: Vec<PricingTier>,
}

/// A pricing tier with overrides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingTier {
    /// Minimum input tokens required to qualify for this tier.
    pub min_input_tokens: u64,
    /// Override rate for prompt input tokens.
    pub input: Option<TokenRate>,
    /// Override rate for generated output tokens.
    pub output: Option<TokenRate>,
    /// Override rate for cached input tokens that were read.
    pub cache_read: Option<TokenRate>,
    /// Override rate for input tokens that caused a cache write (5m duration).
    pub cache_write_5m: Option<TokenRate>,
    /// Override rate for input tokens that caused a cache write (1h duration).
    pub cache_write_1h: Option<TokenRate>,
    /// Override rate for reasoning tokens.
    pub reasoning: Option<TokenRate>,
}

/// Cost calculation result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    /// Cost for prompt input tokens.
    pub input: u64,
    /// Cost for generated output tokens excluding reasoning tokens.
    pub output: u64,
    /// Cost for generated reasoning tokens. This is a subset of output tokens.
    #[serde(default)]
    pub reasoning: u64,
    /// Cost for cached input tokens that were read.
    pub cache_read: u64,
    /// Cost for input tokens that caused a cache write.
    pub cache_write: u64,
    /// Whole-microdollar portion of the total request cost. It is extracted
    /// only after summing exact category numerators, so it can exceed the sum
    /// of individually floored category fields by a few microdollars.
    pub total: u64,
    /// Remaining picodollars after extracting `total`. Always less than one
    /// microdollar; callers can carry it across requests without using floats.
    #[serde(default)]
    pub total_picodollars_remainder: u32,
}

struct ActiveRates {
    input: TokenRate,
    output: TokenRate,
    cache_read: TokenRate,
    cache_write_5m: TokenRate,
    cache_write_1h: Option<TokenRate>,
    reasoning: Option<TokenRate>,
}

fn apply_tier(tier: &PricingTier, input_bucket: u64, rates: &mut ActiveRates) {
    if input_bucket < tier.min_input_tokens {
        return;
    }
    if let Some(rate) = tier.input {
        rates.input = rate;
    }
    if let Some(rate) = tier.output {
        rates.output = rate;
    }
    if let Some(rate) = tier.cache_read {
        rates.cache_read = rate;
    }
    if let Some(rate) = tier.cache_write_5m {
        rates.cache_write_5m = rate;
    }
    if let Some(rate) = tier.cache_write_1h {
        rates.cache_write_1h = Some(rate);
    }
    if let Some(rate) = tier.reasoning {
        rates.reasoning = Some(rate);
    }
}

/// Calculates the cost of a request based on pricing rules and usage counters.
///
/// Catalog-validated pricing tiers take the hot path without cloning or
/// sorting. Public callers can still pass an unsorted [`Pricing`]; that slow
/// fallback preserves the original order-independent behavior.
pub fn cost_of(pricing: &Pricing, usage: &Usage) -> Result<Cost, PricingError> {
    // 1. Check subset invariants
    if usage.cache_write_1h_tokens > usage.cache_write_tokens {
        return Err(PricingError::InvalidUsageBuckets);
    }
    if usage.reasoning_tokens > usage.output_tokens {
        return Err(PricingError::InvalidUsageBuckets);
    }

    // 2. Select active rates based on input bucket
    let input_bucket = usage
        .input_tokens
        .checked_add(usage.cache_read_tokens)
        .ok_or(PricingError::ArithmeticOverflow)?
        .checked_add(usage.cache_write_tokens)
        .ok_or(PricingError::ArithmeticOverflow)?;

    let mut rates = ActiveRates {
        input: pricing.input,
        output: pricing.output,
        cache_read: pricing.cache_read,
        cache_write_5m: pricing.cache_write_5m,
        cache_write_1h: pricing.cache_write_1h,
        reasoning: pricing.reasoning,
    };

    if pricing
        .tiers
        .windows(2)
        .all(|pair| pair[0].min_input_tokens <= pair[1].min_input_tokens)
    {
        for tier in &pricing.tiers {
            apply_tier(tier, input_bucket, &mut rates);
        }
    } else {
        let mut sorted_tiers = pricing.tiers.clone();
        sorted_tiers.sort_unstable_by_key(|tier| tier.min_input_tokens);
        for tier in &sorted_tiers {
            apply_tier(tier, input_bucket, &mut rates);
        }
    }

    // 3. Compute exact category numerators in millionths of a microdollar.
    // Individual category fields remain whole microdollars, but `total` and
    // its remainder are split only after these numerators are summed. Flooring
    // every category first systematically undercounts small requests.
    const RATE_DENOMINATOR: u128 = PICODOLLARS_PER_MICRODOLLAR as u128;
    let input_numerator = usage.input_tokens as u128 * rates.input.0 as u128;
    let cache_read_numerator = usage.cache_read_tokens as u128 * rates.cache_read.0 as u128;

    // Cache write cost: 5m and 1h separate.
    let cache_write_5m_tokens = usage.cache_write_tokens - usage.cache_write_1h_tokens;
    let cache_write_5m_numerator = cache_write_5m_tokens as u128 * rates.cache_write_5m.0 as u128;

    // Anthropic's documented one-hour write price is 2x normal input, not
    // the 5-minute write price (1.25x). Catalogs that know a provider-specific
    // rate still override this default explicitly.
    let cache_write_1h_numerator = if usage.cache_write_1h_tokens == 0 {
        0
    } else {
        let active_rate = match rates.cache_write_1h {
            Some(rate) => rate,
            None => TokenRate(
                rates
                    .input
                    .0
                    .checked_mul(2)
                    .ok_or(PricingError::ArithmeticOverflow)?,
            ),
        };
        usage.cache_write_1h_tokens as u128 * active_rate.0 as u128
    };
    let cache_write_numerator = cache_write_5m_numerator
        .checked_add(cache_write_1h_numerator)
        .ok_or(PricingError::ArithmeticOverflow)?;

    // Output cost: reasoning and regular output separate.
    let output_standard_tokens = usage.output_tokens - usage.reasoning_tokens;
    let output_standard_numerator = output_standard_tokens as u128 * rates.output.0 as u128;
    let active_reasoning_rate = rates.reasoning.unwrap_or(rates.output);
    let reasoning_numerator = usage.reasoning_tokens as u128 * active_reasoning_rate.0 as u128;
    let output_numerator = output_standard_numerator
        .checked_add(reasoning_numerator)
        .ok_or(PricingError::ArithmeticOverflow)?;

    let total_numerator = input_numerator
        .checked_add(cache_read_numerator)
        .and_then(|value| value.checked_add(cache_write_numerator))
        .and_then(|value| value.checked_add(output_numerator))
        .ok_or(PricingError::ArithmeticOverflow)?;

    // 4. Split the exact request total into whole microdollars plus a
    // picodollar remainder, then safely downcast all fields.
    let input = u64::try_from(input_numerator / RATE_DENOMINATOR)
        .map_err(|_| PricingError::ArithmeticOverflow)?;
    let cache_read = u64::try_from(cache_read_numerator / RATE_DENOMINATOR)
        .map_err(|_| PricingError::ArithmeticOverflow)?;
    let cache_write = u64::try_from(cache_write_numerator / RATE_DENOMINATOR)
        .map_err(|_| PricingError::ArithmeticOverflow)?;
    let output = u64::try_from(output_standard_numerator / RATE_DENOMINATOR)
        .map_err(|_| PricingError::ArithmeticOverflow)?;
    let reasoning = u64::try_from(reasoning_numerator / RATE_DENOMINATOR)
        .map_err(|_| PricingError::ArithmeticOverflow)?;
    let total = u64::try_from(total_numerator / RATE_DENOMINATOR)
        .map_err(|_| PricingError::ArithmeticOverflow)?;
    let total_picodollars_remainder = u32::try_from(total_numerator % RATE_DENOMINATOR)
        .map_err(|_| PricingError::ArithmeticOverflow)?;

    Ok(Cost {
        input,
        output,
        cache_read,
        cache_write,
        reasoning,
        total,
        total_picodollars_remainder,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cost_of_simple() {
        let pricing = Pricing {
            input: TokenRate(10), // $10 per 1M tokens ($1e-5 per token)
            output: TokenRate(20),
            cache_read: TokenRate(5),
            cache_write_5m: TokenRate(15),
            cache_write_1h: None,
            reasoning: None,
            tiers: vec![],
        };

        let usage = Usage {
            input_tokens: 100_000,
            cache_read_tokens: 50_000,
            cache_write_tokens: 10_000,
            cache_write_1h_tokens: 0,
            output_tokens: 200_000,
            reasoning_tokens: 0,
            total_tokens: 360_000,
        };

        let cost = cost_of(&pricing, &usage).unwrap();
        // input: (100,000 * 10) / 1,000,000 = 1 microdollar
        // cache_read: (50,000 * 5) / 1,000,000 = 0 microdollars (floor-rounded)
        // cache_write: (10,000 * 15) / 1,000,000 = 0 microdollars
        // output: (200,000 * 20) / 1,000,000 = 4 microdollars
        // total: 1 + 0 + 0 + 4 = 5 microdollars
        assert_eq!(cost.input, 1);
        assert_eq!(cost.cache_read, 0);
        assert_eq!(cost.cache_write, 0);
        assert_eq!(cost.output, 4);
        assert_eq!(cost.total, 5);
    }

    #[test]
    fn request_total_preserves_the_exact_sum_of_category_costs() {
        let pricing = Pricing {
            input: TokenRate(600_000),
            output: TokenRate(600_000),
            cache_read: TokenRate(600_000),
            cache_write_5m: TokenRate(600_000),
            cache_write_1h: None,
            reasoning: None,
            tiers: vec![],
        };
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 1,
            total_tokens: 2,
            ..Usage::default()
        };

        let cost = cost_of(&pricing, &usage).unwrap();
        assert_eq!(cost.input, 0);
        assert_eq!(cost.output, 0);
        assert_eq!(cost.total, 1);
        assert_eq!(cost.total_picodollars_remainder, 200_000);
    }

    #[test]
    fn test_cost_of_tier_boundary() {
        let pricing = Pricing {
            input: TokenRate(100),
            output: TokenRate(200),
            cache_read: TokenRate(50),
            cache_write_5m: TokenRate(150),
            cache_write_1h: None,
            reasoning: None,
            tiers: vec![PricingTier {
                min_input_tokens: 100_000,
                input: Some(TokenRate(80)),
                output: None,
                cache_read: None,
                cache_write_5m: None,
                cache_write_1h: None,
                reasoning: None,
            }],
        };

        // 1. Just below tier boundary (99,999 input bucket tokens)
        let usage_below = Usage {
            input_tokens: 99_999,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_1h_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 99_999,
        };
        let cost_below = cost_of(&pricing, &usage_below).unwrap();
        // input cost: (99,999 * 100) / 1,000_000 = 9 microdollars
        assert_eq!(cost_below.input, 9);

        // 2. Exactly at/above tier boundary (100,000 input bucket tokens)
        let usage_above = Usage {
            input_tokens: 100_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_1h_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 100_000,
        };
        let cost_above = cost_of(&pricing, &usage_above).unwrap();
        // input cost: (100,000 * 80) / 1,000_000 = 8 microdollars (using tier override)
        assert_eq!(cost_above.input, 8);
    }

    #[test]
    fn one_hour_cache_write_defaults_to_twice_input_rate() {
        let pricing = Pricing {
            input: TokenRate(3_000_000),
            output: TokenRate(15_000_000),
            cache_read: TokenRate(300_000),
            cache_write_5m: TokenRate(3_750_000),
            cache_write_1h: None,
            reasoning: None,
            tiers: vec![],
        };
        let usage = Usage {
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 1_000_000,
            cache_write_1h_tokens: 1_000_000,
            output_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 1_000_000,
        };

        let cost = cost_of(&pricing, &usage).unwrap();
        assert_eq!(cost.cache_write, 6_000_000);
        assert_eq!(cost.total, 6_000_000);
    }

    #[test]
    fn test_cost_inconsistent_subsets() {
        let pricing = Pricing {
            input: TokenRate(10),
            output: TokenRate(20),
            cache_read: TokenRate(5),
            cache_write_5m: TokenRate(15),
            cache_write_1h: None,
            reasoning: None,
            tiers: vec![],
        };

        // cache_write_1h_tokens > cache_write_tokens
        let usage_bad_cache = Usage {
            input_tokens: 10_000,
            cache_read_tokens: 0,
            cache_write_tokens: 100,
            cache_write_1h_tokens: 200,
            output_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 10_300,
        };
        assert!(matches!(
            cost_of(&pricing, &usage_bad_cache),
            Err(PricingError::InvalidUsageBuckets)
        ));

        // reasoning_tokens > output_tokens
        let usage_bad_reasoning = Usage {
            input_tokens: 10_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_1h_tokens: 0,
            output_tokens: 100,
            reasoning_tokens: 200,
            total_tokens: 10_300,
        };
        assert!(matches!(
            cost_of(&pricing, &usage_bad_reasoning),
            Err(PricingError::InvalidUsageBuckets)
        ));
    }
}
