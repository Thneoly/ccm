use reqwest::RequestBuilder;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProviderAuth>,
}

impl Provider {
    pub fn resolved_auth(&self) -> ProviderAuth {
        match self.auth {
            Some(auth) => auth,
            None => match self.kind {
                ProviderKind::Anthropic | ProviderKind::AnthropicCompatible => {
                    ProviderAuth::XApiKey
                }
                ProviderKind::OpenAICompatible => ProviderAuth::Bearer,
            },
        }
    }

    pub fn apply_auth(&self, builder: RequestBuilder, token: &str) -> RequestBuilder {
        match self.resolved_auth() {
            ProviderAuth::XApiKey => builder.header("x-api-key", token),
            ProviderAuth::Bearer => builder.header("authorization", format!("Bearer {token}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderAuth {
    #[default]
    XApiKey,
    Bearer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    AnthropicCompatible,
    // serde's kebab-case would render this variant as "open-ai-compatible"
    // ("Open|AI|Compatible"); the documented TOML value is "openai-compatible".
    #[serde(rename = "openai-compatible")]
    OpenAICompatible,
}

impl ProviderKind {
    /// The TOML/JSON name of this kind. Must stay in sync with the serde
    /// renames; `kind_names_match_serde_renames` asserts the sync.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::AnthropicCompatible => "anthropic-compatible",
            ProviderKind::OpenAICompatible => "openai-compatible",
        }
    }

    /// Upstream chat endpoint for this kind. This is the ONLY place the
    /// kind -> endpoint mapping is defined; forwarding and health checks
    /// share it so they can never diverge.
    pub fn upstream_path(&self) -> &'static str {
        match self {
            ProviderKind::Anthropic | ProviderKind::AnthropicCompatible => "/v1/messages",
            ProviderKind::OpenAICompatible => "/v1/chat/completions",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_provider(raw: &str) -> Provider {
        toml::from_str(raw).unwrap()
    }

    #[test]
    fn missing_auth_resolves_per_kind() {
        // anthropic kinds without an explicit auth keep the historical
        // x-api-key default (byte-compatible with the pre-Option serde default)
        for raw in [
            r#"kind = "anthropic"
               base_url = "https://example.com""#,
            r#"kind = "anthropic-compatible"
               base_url = "https://example.com""#,
        ] {
            let provider = parse_provider(raw);
            assert_eq!(provider.resolved_auth(), ProviderAuth::XApiKey, "{raw}");
        }

        // openai-compatible defaults to bearer
        let provider = parse_provider(
            r#"
kind = "openai-compatible"
base_url = "https://example.com"
"#,
        );
        assert_eq!(provider.auth, None);
        assert_eq!(provider.resolved_auth(), ProviderAuth::Bearer);

        // an explicit auth always wins over the kind default
        let provider = parse_provider(
            r#"
kind = "openai-compatible"
base_url = "https://example.com"
auth = "x-api-key"
"#,
        );
        assert_eq!(provider.resolved_auth(), ProviderAuth::XApiKey);
    }

    #[test]
    fn auth_is_omitted_from_serialized_output_when_none() {
        let provider = parse_provider(
            r#"
kind = "openai-compatible"
base_url = "https://example.com"
"#,
        );
        let serialized = toml::to_string(&provider).unwrap();
        assert!(!serialized.contains("auth"));

        let provider = parse_provider(
            r#"
kind = "anthropic"
base_url = "https://example.com"
auth = "bearer"
"#,
        );
        let serialized = toml::to_string(&provider).unwrap();
        assert!(serialized.contains(r#"auth = "bearer""#));
    }

    #[test]
    fn upstream_paths_follow_provider_kind() {
        assert_eq!(ProviderKind::Anthropic.upstream_path(), "/v1/messages");
        assert_eq!(
            ProviderKind::AnthropicCompatible.upstream_path(),
            "/v1/messages"
        );
        assert_eq!(
            ProviderKind::OpenAICompatible.upstream_path(),
            "/v1/chat/completions"
        );
    }

    #[test]
    fn kind_names_match_serde_renames() {
        for kind in [
            ProviderKind::Anthropic,
            ProviderKind::AnthropicCompatible,
            ProviderKind::OpenAICompatible,
        ] {
            let serialized = serde_json::to_value(kind).unwrap();
            assert_eq!(
                serialized,
                serde_json::Value::String(kind.as_str().to_string())
            );
            let deserialized: ProviderKind =
                serde_json::from_value(serde_json::Value::String(kind.as_str().to_string()))
                    .unwrap();
            assert_eq!(deserialized, kind);
        }
    }
}
