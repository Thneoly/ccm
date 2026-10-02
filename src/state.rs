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
        if let Some(root) = std::env::var_os("CCM_HOME") {
            return Ok(PathBuf::from(root).join("state.toml"));
        }
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
        if let Some(current) = config.legacy_current.clone() {
            if state.current.is_none() {
                state.current = Some(current);
                state.save()?;
            }
            Self::strip_legacy_current()?;
        }
        Ok(state)
    }

    // Remove only the legacy `current` key so that unknown keys and comments in
    // config.toml survive the migration instead of a full round-trip rewrite.
    fn strip_legacy_current() -> Result<()> {
        let path = AppConfig::path()?;
        let raw =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut document = raw
            .parse::<toml_edit::DocumentMut>()
            .context("invalid TOML configuration")?;
        if document.remove("current").is_some() {
            fs::write(&path, document.to_string())
                .with_context(|| format!("cannot write {}", path.display()))?;
        }
        Ok(())
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
