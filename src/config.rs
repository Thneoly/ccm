use std::{collections::BTreeMap, fs, path::PathBuf};

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
}

impl AppConfig {
    pub fn path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(home.join(".ccm").join("config.toml"))
    }

    pub fn starter() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".to_string(),
            Provider {
                kind: ProviderKind::Anthropic,
                base_url: "https://api.anthropic.com".to_string(),
                auth: ProviderAuth::XApiKey,
            },
        );
        providers.insert(
            "zai".to_string(),
            Provider {
                kind: ProviderKind::AnthropicCompatible,
                base_url: "https://api.z.ai/api/anthropic".to_string(),
                auth: ProviderAuth::XApiKey,
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
        toml::from_str(&raw).context("invalid TOML configuration")
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
        self.validate_route(&route)?;
        self.routes.insert(name, route);
        Ok(())
    }

    fn validate_route(&self, route: &Route) -> Result<()> {
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
        Ok(())
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
            self.validate_route(route)?;
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
            },
        );
        assert!(result.is_err());
    }
}
