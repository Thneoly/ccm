use reqwest::RequestBuilder;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default)]
    pub auth: ProviderAuth,
}

impl Provider {
    pub fn apply_auth(&self, builder: RequestBuilder, token: &str) -> RequestBuilder {
        match self.auth {
            ProviderAuth::XApiKey => builder.header("x-api-key", token),
            ProviderAuth::Bearer => builder.header("authorization", format!("Bearer {token}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderAuth {
    XApiKey,
    Bearer,
}

impl Default for ProviderAuth {
    fn default() -> Self {
        Self::XApiKey
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    AnthropicCompatible,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_auth_defaults_to_x_api_key() {
        let provider: Provider = toml::from_str(
            r#"
kind = "anthropic-compatible"
base_url = "https://example.com"
"#,
        )
        .unwrap();
        assert!(matches!(provider.auth, ProviderAuth::XApiKey));
    }
}
