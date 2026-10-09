use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    model::{Model, ModelRouting, Profile},
    provider::{Provider, ProviderAuth, ProviderKind},
    route::{ResolvedRoute, Route, RoutePolicy, SelectionStrategy},
};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    #[serde(default)]
    pub models: BTreeMap<String, Model>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
    #[serde(default, rename = "current", skip_serializing)]
    pub legacy_current: Option<String>,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    /// Persistent client sessions (v0.5 M2). Runtime state, so top-level
    /// per the config-placement convention — telemetry stays under
    /// `[observability]`.
    #[serde(default)]
    pub clients: ClientsConfig,
}

/// `[clients]` — persistent client sessions (v0.5 M2). All fields carry
/// serde defaults, so a config.toml written before v0.5 loads unchanged
/// with persistence ON; `persist = false` restores v0.4 memory-only
/// semantics exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientsConfig {
    /// Persist scoped client targets to `$CCM_HOME/clients.toml` so they
    /// survive proxy restarts.
    pub persist: bool,
    /// Entries older than this (by `last_seen_ms`) are dropped at load.
    pub ttl_days: u64,
    /// LRU cap on the in-memory map, enforced at insert and load.
    pub max_entries: u64,
}

impl Default for ClientsConfig {
    fn default() -> Self {
        Self {
            persist: true,
            ttl_days: 7,
            max_entries: 256,
        }
    }
}

/// `[observability]` — history persistence and export knobs (v0.4 M5/M7).
/// All fields carry serde defaults, so a config.toml written before v0.4
/// loads unchanged with history and `/metrics` enabled; a saved config gains
/// the section with default values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservabilityConfig {
    pub history_enabled: bool,
    pub retention_days: u64,
    pub max_records_per_file: u64,
    pub max_bytes_per_file: u64,
    pub metrics_snapshot_interval_secs: u64,
    /// Prometheus text exposition on the proxy's one listener (v0.4 M7).
    /// `false` removes the `/metrics` route entirely — the loopback guard
    /// already covers it; this knob exists for owners who want nothing
    /// exported at all.
    pub prometheus_enabled: bool,
    /// OTLP/HTTP JSON push export (v0.5 M4). Section absent = entirely
    /// off; independent of `prometheus_enabled` (push-on + scrape-off
    /// keeps `/metrics` off). Auth headers, if ever needed, come from the
    /// `OTEL_EXPORTER_OTLP_HEADERS` env var ONLY — never this file.
    pub otlp: Option<OtlpConfig>,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            history_enabled: true,
            retention_days: 14,
            max_records_per_file: 50_000,
            max_bytes_per_file: 8 * 1024 * 1024,
            metrics_snapshot_interval_secs: 30,
            prometheus_enabled: true,
            otlp: None,
        }
    }
}

/// `[observability.otlp]` — the OTLP/HTTP JSON push receiver (v0.5 M4).
/// `endpoint` is the collector base URL (e.g. `http://localhost:4318`);
/// `/v1/metrics` is appended at push time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtlpConfig {
    pub endpoint: String,
    /// Push interval; must be > 0.
    #[serde(default = "default_otlp_interval_secs")]
    pub interval_secs: u64,
}

fn default_otlp_interval_secs() -> u64 {
    30
}

impl AppConfig {
    pub fn path() -> Result<PathBuf> {
        Ok(Self::home_dir()?.join("config.toml"))
    }

    /// CCM root directory: `$CCM_HOME` when set, otherwise `~/.ccm`.
    pub fn home_dir() -> Result<PathBuf> {
        if let Some(root) = std::env::var_os("CCM_HOME") {
            return Ok(PathBuf::from(root));
        }
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(home.join(".ccm"))
    }

    /// History store root: `<home>/history`.
    pub fn history_dir() -> Result<PathBuf> {
        Ok(Self::home_dir()?.join("history"))
    }

    pub fn starter() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".to_string(),
            Provider {
                kind: ProviderKind::Anthropic,
                base_url: "https://api.anthropic.com".to_string(),
                auth: Some(ProviderAuth::XApiKey),
            },
        );
        providers.insert(
            "zai".to_string(),
            Provider {
                kind: ProviderKind::AnthropicCompatible,
                base_url: "https://api.z.ai/api/anthropic".to_string(),
                auth: Some(ProviderAuth::XApiKey),
            },
        );

        let mut models = BTreeMap::new();
        models.insert(
            "claude".to_string(),
            Model {
                provider: "anthropic".to_string(),
                model_id: "claude-sonnet-5-5".to_string(),
                routing: ModelRouting {
                    cost_weight: 1.0,
                    quality_weight: 1.0,
                },
                // the starter ships unpriced: prices are hand-entered facts
                // about the owner's plan, never sample values (M6).
                pricing: None,
            },
        );
        models.insert(
            "glm".to_string(),
            Model {
                provider: "zai".to_string(),
                model_id: "glm-5.3".to_string(),
                routing: ModelRouting {
                    cost_weight: 0.25,
                    quality_weight: 0.85,
                },
                pricing: None,
            },
        );

        let mut profiles = BTreeMap::new();
        profiles.insert(
            "coding".to_string(),
            Profile {
                model: "claude".to_string(),
            },
        );
        profiles.insert(
            "fast".to_string(),
            Profile {
                model: "glm".to_string(),
            },
        );

        let mut routes = BTreeMap::new();
        routes.insert(
            "coding-route".to_string(),
            Route {
                primary: "claude".to_string(),
                fallback: vec!["glm".to_string()],
                policy: RoutePolicy::default(),
            },
        );
        routes.insert(
            "fast-route".to_string(),
            Route {
                primary: "glm".to_string(),
                fallback: Vec::new(),
                policy: RoutePolicy::default(),
            },
        );

        Self {
            providers,
            models,
            profiles,
            routes,
            legacy_current: None,
            observability: ObservabilityConfig::default(),
            clients: ClientsConfig::default(),
        }
    }

    pub fn init(force: bool) -> Result<PathBuf> {
        let path = Self::path()?;
        if path.exists() && !force {
            bail!(
                "config already exists at {}; pass --force to overwrite it",
                path.display()
            );
        }
        Self::starter().save()?;
        Ok(path)
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            bail!(
                "config not found at {}. Run `ccm init` first",
                path.display()
            );
        }

        let raw =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let config: AppConfig = toml::from_str(&raw).context("invalid TOML configuration")?;
        config.validate_observability()?;
        config.validate_model_pricing()?;
        config.validate_clients()?;
        Ok(config)
    }

    /// `[clients]` sanity (v0.5 M2): a zero TTL would drop every entry at
    /// load, a zero cap would evict every entry at insert — both are
    /// configuration errors rather than silent no-ops (the
    /// `validate_observability` family).
    pub fn validate_clients(&self) -> Result<()> {
        if self.clients.ttl_days == 0 {
            bail!("[clients] ttl_days must be greater than 0");
        }
        if self.clients.max_entries == 0 {
            bail!("[clients] max_entries must be greater than 0");
        }
        Ok(())
    }

    /// `[observability]` sanity: zero thresholds would disable rotation or
    /// the snapshot loop entirely, so they are configuration errors rather
    /// than silent no-ops.
    pub fn validate_observability(&self) -> Result<()> {
        let observability = &self.observability;
        if observability.retention_days == 0 {
            bail!("[observability] retention_days must be greater than 0");
        }
        if observability.max_records_per_file == 0 {
            bail!("[observability] max_records_per_file must be greater than 0");
        }
        if observability.max_bytes_per_file == 0 {
            bail!("[observability] max_bytes_per_file must be greater than 0");
        }
        if observability.metrics_snapshot_interval_secs == 0 {
            bail!("[observability] metrics_snapshot_interval_secs must be greater than 0");
        }
        if let Some(otlp) = &observability.otlp {
            if otlp.endpoint.trim().is_empty() {
                bail!("[observability.otlp] endpoint must not be empty");
            }
            if otlp.interval_secs == 0 {
                bail!("[observability.otlp] interval_secs must be greater than 0");
            }
        }
        Ok(())
    }

    /// `[models.<name>.pricing]` sanity (v0.4 M6): prices are USD per
    /// million tokens; a negative or non-finite price would produce
    /// meaningless (or NaN-poisoned) cost sums. A missing table stays
    /// legal — unpriced is a recorded state, not an error.
    pub fn validate_model_pricing(&self) -> Result<()> {
        for (name, model) in &self.models {
            let Some(pricing) = &model.pricing else {
                continue;
            };
            for (field, value) in [
                ("input", pricing.input),
                ("output", pricing.output),
                ("cache_read", pricing.cache_read),
                ("cache_write", pricing.cache_write),
            ] {
                if !value.is_finite() || value < 0.0 {
                    bail!(
                        "model `{name}` pricing.{field} must be a finite value >= 0 (USD per million tokens)"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("cannot serialize configuration")?;
        fs::write(&path, raw).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }

    pub fn add_provider(&mut self, name: String, provider: Provider) {
        self.providers.insert(name, provider);
    }

    pub fn add_model(&mut self, name: String, model: Model) -> Result<()> {
        if !self.providers.contains_key(&model.provider) {
            bail!("unknown provider `{}`", model.provider);
        }
        if model.routing.cost_weight < 0.0 {
            bail!("model routing cost_weight must be >= 0");
        }
        if model.routing.quality_weight < 0.0 {
            bail!("model routing quality_weight must be >= 0");
        }
        self.models.insert(name, model);
        Ok(())
    }

    pub fn add_route(&mut self, name: String, route: Route) -> Result<()> {
        self.validate_route(&name, &route)?;
        self.routes.insert(name, route);
        Ok(())
    }

    fn validate_route(&self, name: &str, route: &Route) -> Result<()> {
        if !self.models.contains_key(&route.primary) {
            bail!("unknown primary model `{}`", route.primary);
        }
        for fallback in &route.fallback {
            if !self.models.contains_key(fallback) {
                bail!("unknown fallback model `{fallback}`");
            }
        }
        if route.policy.max_attempts == 0 {
            bail!("route policy max_attempts must be greater than 0");
        }
        if route.policy.header_timeout_ms == 0 {
            bail!("route policy header_timeout_ms must be greater than 0");
        }

        let weights = &route.policy.weights;
        if weights.reliability < 0.0
            || weights.latency < 0.0
            || weights.cost < 0.0
            || weights.quality < 0.0
        {
            bail!("route selection weights must be >= 0");
        }
        if matches!(route.policy.selection, SelectionStrategy::Weighted)
            && weights.reliability + weights.latency + weights.cost + weights.quality <= 0.0
        {
            bail!("weighted selection requires at least one positive selection weight");
        }

        if route.policy.circuit_breaker.enabled {
            if route.policy.circuit_breaker.failure_threshold == 0 {
                bail!("circuit breaker failure_threshold must be greater than 0");
            }
            if route.policy.circuit_breaker.open_ms == 0 {
                bail!("circuit breaker open_ms must be greater than 0");
            }
        }

        self.warn_if_mixed_protocol(name, route);
        Ok(())
    }

    /// Non-blocking warning for routes whose candidates span both
    /// anthropic-protocol and openai-compatible providers. Such routes work
    /// (fallback is status-code based), but prompt caching and extended
    /// thinking are lost on the openai-compatible candidates. Printed once
    /// per route name per process because `resolve_route` revalidates every
    /// proxied request.
    fn warn_if_mixed_protocol(&self, name: &str, route: &Route) {
        let candidates = std::iter::once(&route.primary)
            .chain(route.fallback.iter())
            .filter_map(|model_name| self.candidate_kind(model_name))
            .collect::<Vec<_>>();
        let has_openai = candidates
            .iter()
            .any(|(_, kind)| matches!(kind, ProviderKind::OpenAICompatible));
        let has_anthropic = candidates
            .iter()
            .any(|(_, kind)| !matches!(kind, ProviderKind::OpenAICompatible));
        if !has_openai || !has_anthropic {
            return;
        }
        let openai_models = candidates
            .iter()
            .filter(|(_, kind)| matches!(kind, ProviderKind::OpenAICompatible))
            .map(|(model, _)| *model)
            .collect::<Vec<_>>()
            .join(", ");
        static WARNED_ROUTES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        let warned = WARNED_ROUTES.get_or_init(|| Mutex::new(HashSet::new()));
        let mut warned = warned
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if warned.insert(name.to_string()) {
            eprintln!(
                "ccm: warning: route `{name}` mixes anthropic-protocol and openai-compatible providers; prompt caching and extended thinking are lost on the openai-compatible candidates ({openai_models})"
            );
        }
    }

    /// (model alias, provider kind) for a route candidate.
    fn candidate_kind<'a>(&self, model_name: &'a str) -> Option<(&'a str, ProviderKind)> {
        self.models
            .get(model_name)
            .and_then(|model| self.providers.get(&model.provider))
            .map(|provider| (model_name, provider.kind))
    }

    pub fn resolve_target(&self, target: &str) -> Result<String> {
        if self.models.contains_key(target) {
            return Ok(target.to_string());
        }

        if let Some(profile) = self.profiles.get(target) {
            if self.models.contains_key(&profile.model) {
                return Ok(profile.model.clone());
            }
            bail!(
                "profile `{}` references unknown model `{}`",
                target,
                profile.model
            );
        }

        bail!("unknown model/profile `{}`", target)
    }

    pub fn resolve_route(&self, target: &str) -> Result<ResolvedRoute> {
        if let Some(route) = self.routes.get(target) {
            self.validate_route(target, route)?;
            return Ok(ResolvedRoute {
                target: target.to_string(),
                primary: route.primary.clone(),
                fallback: route.fallback.clone(),
                policy: route.policy.clone(),
            });
        }

        let model = self.resolve_target(target)?;
        Ok(ResolvedRoute {
            target: target.to_string(),
            primary: model,
            fallback: Vec::new(),
            policy: RoutePolicy::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelPricing;

    #[test]
    fn legacy_current_deserializes_but_is_not_serialized() {
        let raw = r#"
current = "glm"

[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
"#;
        let config: AppConfig = toml::from_str(raw).unwrap();
        assert_eq!(config.legacy_current.as_deref(), Some("glm"));

        let serialized = toml::to_string(&config).unwrap();
        assert!(!serialized.contains("current ="));
    }

    #[test]
    fn clients_defaults_and_validation() {
        // a config written before v0.5 has no [clients] section: it loads
        // with persistence ON (ttl 7 days, cap 256) — sessions surviving a
        // restart is the fix v0.5 ships, so it must not require a config edit
        let raw = r#"
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
"#;
        let config: AppConfig = toml::from_str(raw).unwrap();
        assert!(config.clients.persist);
        assert_eq!(config.clients.ttl_days, 7);
        assert_eq!(config.clients.max_entries, 256);
        config.validate_clients().unwrap();

        // the v0.4 kill-switch: memory-only, byte-for-byte
        let raw = raw.to_string() + "\n[clients]\npersist = false\n";
        let config: AppConfig = toml::from_str(&raw).unwrap();
        assert!(!config.clients.persist);
        config.validate_clients().unwrap();

        // zero ttl / cap are configuration errors, not silent no-ops
        let mut config = AppConfig::starter();
        config.clients.ttl_days = 0;
        assert!(config.validate_clients().is_err());
        let mut config = AppConfig::starter();
        config.clients.max_entries = 0;
        assert!(config.validate_clients().is_err());
    }

    #[test]
    fn observability_defaults_apply_for_pre_v04_configs() {
        // a config written before v0.4 has no [observability] section: it
        // loads with history and /metrics enabled at the default thresholds
        let raw = r#"
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
"#;
        let config: AppConfig = toml::from_str(raw).unwrap();
        assert!(config.observability.history_enabled);
        assert!(config.observability.prometheus_enabled);
        assert_eq!(config.observability.retention_days, 14);
        assert_eq!(config.observability.max_records_per_file, 50_000);
        assert_eq!(config.observability.max_bytes_per_file, 8 * 1024 * 1024);
        assert_eq!(config.observability.metrics_snapshot_interval_secs, 30);
        config.validate_observability().unwrap();
    }

    #[test]
    fn observability_validation_rejects_zero_thresholds() {
        let mut config = AppConfig::starter();
        config.observability.retention_days = 0;
        assert!(config.validate_observability().is_err());

        let mut config = AppConfig::starter();
        config.observability.max_records_per_file = 0;
        assert!(config.validate_observability().is_err());

        let mut config = AppConfig::starter();
        config.observability.metrics_snapshot_interval_secs = 0;
        assert!(config.validate_observability().is_err());

        // and an explicit disable is a valid configuration — either half
        let mut config = AppConfig::starter();
        config.observability.history_enabled = false;
        config.validate_observability().unwrap();

        let mut config = AppConfig::starter();
        config.observability.prometheus_enabled = false;
        config.validate_observability().unwrap();
    }

    #[test]
    fn pricing_defaults_and_validation() {
        // pre-M6 configs load unchanged with pricing = None
        let raw = r#"
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"

[models.claude]
provider = "anthropic"
model_id = "claude-sonnet-5-5"
"#;
        let config: AppConfig = toml::from_str(raw).unwrap();
        assert!(config.models["claude"].pricing.is_none());
        config.validate_model_pricing().unwrap();

        // a full table loads; cache rates default to 0
        let raw = r#"
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"

[models.claude]
provider = "anthropic"
model_id = "claude-sonnet-5-5"

[models.claude.pricing]
input = 3.0
output = 15.0
cache_read = 0.3
cache_write = 3.75
"#;
        let config: AppConfig = toml::from_str(raw).unwrap();
        let pricing = config.models["claude"].pricing.as_ref().unwrap();
        assert_eq!(
            (
                pricing.input,
                pricing.output,
                pricing.cache_read,
                pricing.cache_write
            ),
            (3.0, 15.0, 0.3, 3.75)
        );
        config.validate_model_pricing().unwrap();
        // 1M in + 1M out + 2M cache-read + 1M cache-write = 3 + 15 + 0.6 + 3.75
        assert!(
            (pricing.cost_usd(1_000_000, 1_000_000, 2_000_000, 1_000_000) - 22.35).abs() < 1e-9
        );

        // missing cache rates default to 0, not to the input price
        let raw = r#"
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"

[models.claude]
provider = "anthropic"
model_id = "claude-sonnet-5-5"
pricing = { input = 3.0, output = 15.0 }
"#;
        let config: AppConfig = toml::from_str(raw).unwrap();
        let pricing = config.models["claude"].pricing.as_ref().unwrap();
        assert_eq!((pricing.cache_read, pricing.cache_write), (0.0, 0.0));
        assert!((pricing.cost_usd(0, 0, 1_000_000, 1_000_000) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn pricing_validation_rejects_negative_and_non_finite() {
        for bad in [-0.01, f64::NAN, f64::INFINITY] {
            let mut config = AppConfig::starter();
            config.models.get_mut("glm").unwrap().pricing = Some(ModelPricing {
                input: bad,
                output: 1.0,
                cache_read: 0.0,
                cache_write: 0.0,
            });
            assert!(
                config.validate_model_pricing().is_err(),
                "input = {bad} must be rejected"
            );
        }
        // every field is checked, not just input
        let mut config = AppConfig::starter();
        config.models.get_mut("glm").unwrap().pricing = Some(ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_read: 0.0,
            cache_write: -1.0,
        });
        assert!(config.validate_model_pricing().is_err());
    }

    #[test]
    fn resolves_model_alias() {
        let config = AppConfig::starter();
        assert_eq!(config.resolve_target("glm").unwrap(), "glm");
    }

    #[test]
    fn resolves_profile_alias() {
        let config = AppConfig::starter();
        assert_eq!(config.resolve_target("fast").unwrap(), "glm");
    }

    #[test]
    fn resolves_named_route_without_collapsing_target() {
        let config = AppConfig::starter();
        let route = config.resolve_route("coding-route").unwrap();
        assert_eq!(route.target, "coding-route");
        assert_eq!(route.primary, "claude");
        assert_eq!(route.fallback, vec!["glm"]);
        assert_eq!(route.policy.header_timeout_ms, 30_000);
        assert!(route.policy.circuit_breaker.enabled);
    }

    #[test]
    fn rejects_model_with_unknown_provider() {
        let mut config = AppConfig::starter();
        let result = config.add_model(
            "broken".to_string(),
            Model {
                provider: "missing".to_string(),
                model_id: "x".to_string(),
                routing: ModelRouting::default(),
                pricing: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_negative_model_cost_weight() {
        let mut config = AppConfig::starter();
        let result = config.add_model(
            "broken".to_string(),
            Model {
                provider: "anthropic".to_string(),
                model_id: "x".to_string(),
                routing: ModelRouting {
                    cost_weight: -1.0,
                    quality_weight: 1.0,
                },
                pricing: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn mixed_protocol_route_validates_with_non_blocking_warning() {
        let mut config = AppConfig::starter();
        config.add_provider(
            "deepseek".to_string(),
            Provider {
                kind: ProviderKind::OpenAICompatible,
                base_url: "https://api.deepseek.com".to_string(),
                auth: None,
            },
        );
        config
            .add_model(
                "deepseek-chat".to_string(),
                Model {
                    provider: "deepseek".to_string(),
                    model_id: "deepseek-chat".to_string(),
                    routing: ModelRouting::default(),
                    pricing: None,
                },
            )
            .unwrap();
        // Mixed-protocol routes are legal: the warning is non-blocking and
        // prints once per process (stderr is not asserted here).
        config
            .add_route(
                "mixed".to_string(),
                Route {
                    primary: "claude".to_string(),
                    fallback: vec!["deepseek-chat".to_string()],
                    policy: RoutePolicy::default(),
                },
            )
            .unwrap();
        assert!(config.routes.contains_key("mixed"));
    }
}
