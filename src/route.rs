use serde::{Deserialize, Serialize};

fn default_header_timeout_ms() -> u64 {
    30_000
}

fn default_fallback_on() -> Vec<u16> {
    vec![429, 502, 503, 504]
}

fn default_max_attempts() -> usize {
    3
}

fn default_backoff_ms() -> u64 {
    200
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutePolicy {
    #[serde(default = "default_header_timeout_ms")]
    pub header_timeout_ms: u64,
    #[serde(default = "default_fallback_on")]
    pub fallback_on: Vec<u16>,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: usize,
    #[serde(default = "default_backoff_ms")]
    pub backoff_ms: u64,
}

impl Default for RoutePolicy {
    fn default() -> Self {
        Self {
            header_timeout_ms: default_header_timeout_ms(),
            fallback_on: default_fallback_on(),
            max_attempts: default_max_attempts(),
            backoff_ms: default_backoff_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub primary: String,
    #[serde(default)]
    pub fallback: Vec<String>,
    #[serde(default)]
    pub policy: RoutePolicy,
}

#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub target: String,
    pub primary: String,
    pub fallback: Vec<String>,
    pub policy: RoutePolicy,
}

impl ResolvedRoute {
    pub fn candidates(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.primary.as_str()).chain(self.fallback.iter().map(String::as_str))
    }
}
