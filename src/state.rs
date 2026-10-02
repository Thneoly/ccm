use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub current: Option<String>,
}

impl AppState {
    pub fn path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(home.join(".ccm").join("state.toml"))
    }

    pub fn starter() -> Self {
        Self {
            current: Some("claude".to_string()),
        }
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&raw).context("invalid CCM state TOML")
    }

    pub fn load_or_migrate(config: &AppConfig) -> Result<Self> {
        let mut state = Self::load()?;
        if state.current.is_none() {
            if let Some(current) = config.legacy_current.clone() {
                state.current = Some(current);
                state.save()?;
                config.save()?;
            }
        }
        Ok(state)
    }

    pub fn init(force: bool) -> Result<PathBuf> {
        let path = Self::path()?;
        if path.exists() && !force {
            return Ok(path);
        }
        Self::starter().save()?;
        Ok(path)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("cannot serialize CCM state")?;
        fs::write(&path, raw).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }
}
