//! Effective-dated API list-price catalog and token accounting.
//!
//! Pricing here is an accounting estimate at public API list rates — never
//! invoiced subscription spend. Cached input tokens are a subset of input tokens
//! and thinking/reasoning tokens are treated as part of output tokens to prevent
//! double counting.

use contracts::providers::ModelPriceEntry;

pub const PRICE_AS_OF: &str = "2026-11-01";
pub const CACHE_WRITE_ASSUMPTION: &str = "5-minute ephemeral cache write tier";

/// Built-in list-price catalog (USD per 1M tokens).
pub fn default_catalog() -> Vec<ModelPriceEntry> {
    vec![
        entry("claude-opus-4", "Claude Opus 4.x", 15.0, 75.0, 1.5, 18.75),
        entry("claude-opus-5", "Claude Opus 5.x", 15.0, 75.0, 1.5, 18.75),
        entry("claude-sonnet-4", "Claude Sonnet 4.x", 3.0, 15.0, 0.3, 3.75),
        entry("claude-sonnet-5", "Claude Sonnet 5.x", 3.0, 15.0, 0.3, 3.75),
        entry("claude-fable", "Claude Fable", 3.0, 15.0, 0.3, 3.75),
        entry("claude-haiku", "Claude Haiku", 0.8, 4.0, 0.08, 1.0),
        entry("gpt-5", "GPT-5", 1.25, 10.0, 0.125, 1.25),
        entry("gpt-6", "GPT-6", 2.0, 12.0, 0.2, 2.0),
        entry("codex", "Codex", 1.5, 10.0, 0.15, 1.5),
        entry("o3", "o3", 2.0, 8.0, 0.5, 2.0),
        entry("o4-mini", "o4-mini", 1.1, 4.4, 0.275, 1.1),
        entry("gemini-2.5-pro", "Gemini 2.5 Pro", 1.25, 10.0, 0.3125, 1.25),
        entry("gemini-2.5-flash", "Gemini 2.5 Flash", 0.15, 0.6, 0.0375, 0.15),
        entry("gemini-3", "Gemini 3.x", 1.5, 10.0, 0.375, 1.5),
        entry("gemini-4", "Gemini 4.x", 2.0, 12.0, 0.5, 2.0),
        entry("deepseek", "DeepSeek", 0.27, 1.1, 0.07, 0.27),
    ]
}

fn entry(pattern: &str, display: &str, input: f64, output: f64, cache_read: f64, cache_write_5m: f64) -> ModelPriceEntry {
    ModelPriceEntry {
        model_pattern: pattern.to_string(),
        display_name: display.to_string(),
        effective_from: PRICE_AS_OF.to_string(),
        currency: "USD".to_string(),
        input_per_m: input,
        output_per_m: output,
        cache_read_per_m: cache_read,
        cache_write_5m_per_m: cache_write_5m,
        source_note: "API list price estimate (not subscription billing)".to_string(),
    }
}

/// Normalize a provider-prefixed model identifier and match against the catalog.
pub fn lookup_price<'a>(model_id: &str, catalog: &'a [ModelPriceEntry]) -> Option<&'a ModelPriceEntry> {
    let lower = model_id.trim().to_ascii_lowercase();
    let bare = lower.rsplit('/').next().unwrap_or(&lower);
    catalog.iter().find(|p| bare == p.model_pattern || bare.starts_with(&p.model_pattern) || bare.contains(&p.model_pattern))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCounts {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub thinking_tokens: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct PricedCost {
    pub total_usd: f64,
    pub uncached_usd: f64,
    pub cache_savings_usd: f64,
    pub thinking_cost_usd: f64,
}

/// Estimate API-equivalent USD cost.
///
/// `input_tokens` is provider-inclusive (contains `cache_read_tokens` and
/// `cache_write_tokens` when present, clamped so non-cached input never goes negative).
/// `output_tokens` is provider-inclusive (contains `thinking_tokens` when present).
pub fn estimate_cost(tokens: TokenCounts, price: &ModelPriceEntry) -> PricedCost {
    let input = tokens.input_tokens.max(0) as f64;
    let output = tokens.output_tokens.max(0) as f64;
    let cache_read = (tokens.cache_read_tokens.max(0) as f64).min(input);
    let cache_write = (tokens.cache_write_tokens.max(0) as f64).min((input - cache_read).max(0.0));
    let uncached_input = (input - cache_read - cache_write).max(0.0);
    let thinking = (tokens.thinking_tokens.max(0) as f64).min(output);

    let total_usd = (uncached_input * price.input_per_m
        + cache_read * price.cache_read_per_m
        + cache_write * price.cache_write_5m_per_m
        + output * price.output_per_m)
        / 1_000_000.0;

    let uncached_usd = (input * price.input_per_m + output * price.output_per_m) / 1_000_000.0;
    let cache_savings_usd = (uncached_usd - total_usd).max(0.0);
    let thinking_cost_usd = (thinking * price.output_per_m) / 1_000_000.0;

    PricedCost { total_usd, uncached_usd, cache_savings_usd, thinking_cost_usd }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_input_is_never_double_counted() {
        let catalog = default_catalog();
        let price = lookup_price("anthropic/claude-sonnet-5-5", &catalog).expect("sonnet price");
        let cost = estimate_cost(
            TokenCounts {
                input_tokens: 1_000_000,
                output_tokens: 100_000,
                cache_read_tokens: 800_000,
                cache_write_tokens: 100_000,
                thinking_tokens: 40_000,
            },
            price,
        );
        // 100k uncached ($0.30) + 800k cache_read ($0.24) + 100k cache_write ($0.375) + 100k output ($1.50) = $2.415
        assert!((cost.total_usd - 2.415).abs() < 1e-6, "got {}", cost.total_usd);
        // Uncached would be $3.00 + $1.50 = $4.50 -> savings $2.085
        assert!((cost.cache_savings_usd - 2.085).abs() < 1e-6, "got {}", cost.cache_savings_usd);
        // Thinking share: 40k * $15 / 1M = $0.60
        assert!((cost.thinking_cost_usd - 0.60).abs() < 1e-6, "got {}", cost.thinking_cost_usd);
    }

    #[test]
    fn unknown_model_returns_none_instead_of_zero() {
        let catalog = default_catalog();
        assert!(lookup_price("acme/unknown-model-v9", &catalog).is_none());
    }
}
