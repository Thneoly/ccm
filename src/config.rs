use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{model::{Model, Profile}, provider::Provider};

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

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            bail!(
                "config not found at {}. Copy examples/config.toml to ~/.ccm/config.toml",
                path.display()
            );
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&raw).context("invalid TOML configuration")
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("cannot serialize configuration")?;
        fs::write(&path, raw)
            .with_context(|| format!("cannot write {}", path.display()))?;
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
            bail!("profile `{}` references unknown model `{}`", target, profile.model);
        }

        bail!("unknown model/profile `{}`", target)
    }
}
