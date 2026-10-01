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

fn default_circuit_enabled() -> bool {
    true
}

fn default_failure_threshold() -> usize {
    3
}

fn default_open_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionStrategy {
    Ordered,
    Healthiest,
    LowestLatency,
}

impl Default for SelectionStrategy {
    fn default() -> Self {
        Self::Ordered
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerPolicy {
    #[serde(default = "default_circuit_enabled")]
    pub enabled: bool,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: usize,
    #[serde(default = "default_open_ms")]
    pub open_ms: u64,
}

impl Default for CircuitBreakerPolicy {
    fn default() -> Self {
        Self {
            enabled: default_circuit_enabled(),
            failure_threshold: default_failure_threshold(),
            open_ms: default_open_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutePolicy {
    #[serde(default)]
    pub selection: SelectionStrategy,
    #[serde(default = "default_header_timeout_ms")]
    pub header_timeout_ms: u64,
    #[serde(default = "default_fallback_on")]
    pub fallback_on: Vec<u16>,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: usize,
    #[serde(default = "default_backoff_ms")]
    pub backoff_ms: u64,
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerPolicy,
}

impl Default for RoutePolicy {
    fn default() -> Self {
        Self {
            selection: SelectionStrategy::default(),
            header_timeout_ms: default_header_timeout_ms(),
            fallback_on: default_fallback_on(),
            max_attempts: default_max_attempts(),
            backoff_ms: default_backoff_ms(),
            circuit_breaker: CircuitBreakerPolicy::default(),
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
