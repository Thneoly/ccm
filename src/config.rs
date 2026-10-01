use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    model::{Model, Profile},
    provider::{Provider, ProviderKind},
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
    pub current: Option<String>,
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
            },
        );
        providers.insert(
            "zai".to_string(),
            Provider {
                kind: ProviderKind::AnthropicCompatible,
                base_url: "https://api.z.ai/api/anthropic".to_string(),
            },
        );

        let mut models = BTreeMap::new();
        models.insert(
            "claude".to_string(),
            Model {
                provider: "anthropic".to_string(),
                model_id: "claude-sonnet-4-5".to_string(),
            },
        );
        models.insert(
            "glm".to_string(),
            Model {
                provider: "zai".to_string(),
                model_id: "glm-5".to_string(),
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

        Self {
            providers,
            models,
            profiles,
            current: Some("claude".to_string()),
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
        self.models.insert(name, model);
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn rejects_unknown_target() {
        let config = AppConfig::starter();
        assert!(config.resolve_target("missing").is_err());
    }

    #[test]
    fn rejects_model_with_unknown_provider() {
        let mut config = AppConfig::starter();
        let result = config.add_model(
            "broken".to_string(),
            Model {
                provider: "missing".to_string(),
                model_id: "x".to_string(),
            },
        );
        assert!(result.is_err());
    }
}
