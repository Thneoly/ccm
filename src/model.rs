use serde::{Deserialize, Serialize};

fn default_cost_weight() -> f64 {
    1.0
}

fn default_quality_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRouting {
    #[serde(default = "default_cost_weight")]
    pub cost_weight: f64,
    #[serde(default = "default_quality_weight")]
    pub quality_weight: f64,
}

impl Default for ModelRouting {
    fn default() -> Self {
        Self {
            cost_weight: default_cost_weight(),
            quality_weight: default_quality_weight(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub provider: String,
    pub model_id: String,
    #[serde(default)]
    pub routing: ModelRouting,
    /// Hand-entered per-MTok prices (v0.4 M6). `None` = unpriced: usage
    /// records for this model carry no cost and no pricing snapshot —
    /// never a guessed price.
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
}

/// USD per million tokens for one model (v0.4 M6). Hand-entered by the
/// owner; `input` and `output` are required, the cache rates default to 0
/// (cache tokens costed at nothing until a price is entered). Unrelated to
/// `ModelRouting::cost_weight`, which stays a relative routing weight.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelPricing {
    /// USD per million input tokens.
    pub input: f64,
    /// USD per million output tokens.
    pub output: f64,
    /// USD per million cache-read tokens (Anthropic `cache_read_input_tokens`,
    /// translated OpenAI `cached_tokens`).
    #[serde(default)]
    pub cache_read: f64,
    /// USD per million cache-write tokens (Anthropic
    /// `cache_creation_input_tokens`; no OpenAI counterpart).
    #[serde(default)]
    pub cache_write: f64,
}

impl ModelPricing {
    /// USD for one request's tokens: `tokens / 1e6 × price`, summed over
    /// the four kinds. Trivial math, but it lives next to the schema so
    /// the record writer and the tests agree on it.
    pub fn cost_usd(&self, input: u64, output: u64, cache_read: u64, cache_write: u64) -> f64 {
        (input as f64 * self.input
            + output as f64 * self.output
            + cache_read as f64 * self.cache_read
            + cache_write as f64 * self.cache_write)
            / 1_000_000.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub model: String,
}
