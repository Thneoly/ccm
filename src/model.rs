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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub model: String,
}
