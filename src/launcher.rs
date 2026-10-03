use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::{
    config::AppConfig,
    credential,
    provider::{ProviderAuth, ProviderKind},
};

/// Why an `openai-compatible` target cannot run without the proxy. Shared by
/// the direct-launch guard (`ccm run <target>` without `--proxy`) and the
/// `ccm doctor` note for an openai-kind current target.
pub(crate) const PROXY_ONLY_REASON: &str = "openai-compatible models are proxy-only (protocol translation exists only inside the ccm proxy; a direct launch would send the Anthropic protocol to an OpenAI endpoint) — start `ccm proxy` and use `ccm run --proxy`";

pub fn run_claude(config: &AppConfig, model_name: &str) -> Result<()> {
    let model = config
        .models
        .get(model_name)
        .with_context(|| format!("unknown model `{model_name}`"))?;
    let provider = config
        .providers
        .get(&model.provider)
        .with_context(|| format!("unknown provider `{}`", model.provider))?;
    let token = credential::get(&model.provider)?;

    run_command(
        provider.base_url.trim_end_matches('/'),
        &token,
        &model.model_id,
        provider.resolved_auth(),
    )
}

pub fn run_claude_via_proxy(proxy_url: &str, client_id: &str) -> Result<()> {
    run_command_with_proxy_env(
        proxy_url.trim_end_matches('/'),
        &format!("ccm-local-{client_id}"),
        "ccm",
        ProviderAuth::Bearer,
        proxy_url,
        client_id,
    )
}

fn run_command(base_url: &str, token: &str, model: &str, auth: ProviderAuth) -> Result<()> {
    run_command_inner(base_url, token, model, auth, None, None)
}

fn run_command_with_proxy_env(
    base_url: &str,
    token: &str,
    model: &str,
    auth: ProviderAuth,
    proxy_url: &str,
    client_id: &str,
) -> Result<()> {
    run_command_inner(
        base_url,
        token,
        model,
        auth,
        Some(proxy_url),
        Some(client_id),
    )
}

pub fn claude_command() -> Command {
    // Windows: npm installs ship only claude.cmd shims and CreateProcessW resolves
    // bare names to .exe only, so route through cmd for both .cmd and .exe.
    if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.arg("/c").arg("claude");
        command
    } else {
        Command::new("claude")
    }
}

fn run_command_inner(
    base_url: &str,
    token: &str,
    model: &str,
    auth: ProviderAuth,
    proxy_url: Option<&str>,
    client_id: Option<&str>,
) -> Result<()> {
    let mut command = claude_command();
    command
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", model)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN");

    if let Some(proxy_url) = proxy_url {
        command.env("CCM_PROXY_URL", proxy_url);
    } else {
        command.env_remove("CCM_PROXY_URL");
    }

    if let Some(client_id) = client_id {
        command.env("CCM_CLIENT_ID", client_id).env(
            "ANTHROPIC_CUSTOM_HEADERS",
            merge_custom_headers(parent_custom_headers().as_deref(), client_id),
        );
    }

    match auth {
        ProviderAuth::XApiKey => {
            command.env("ANTHROPIC_API_KEY", token);
        }
        ProviderAuth::Bearer => {
            command.env("ANTHROPIC_AUTH_TOKEN", token);
        }
    }

    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context(
            "failed to launch `claude`; make sure Claude Code is installed and available on PATH",
        )?;

    if !status.success() {
        anyhow::bail!("Claude Code exited with status {status}");
    }

    Ok(())
}

/// `ANTHROPIC_CUSTOM_HEADERS` already present in ccm's own environment, if
/// any. Non-UTF-8 values are treated as unset.
fn parent_custom_headers() -> Option<String> {
    std::env::var("ANTHROPIC_CUSTOM_HEADERS").ok()
}

/// Merge the parent environment's `ANTHROPIC_CUSTOM_HEADERS` (newline-
/// separated `Name: Value` lines) with ccm's client-identity line.
///
/// Every existing line is preserved EXCEPT `x-ccm-client` lines (matched
/// case-insensitively; CRLF and stray whitespace tolerated), which are
/// replaced by exactly one line carrying `id`. The proxy resolves the client
/// id from the FIRST `x-ccm-client` header value, so leaving two lines with
/// different ids would make resolution unpredictable; keeping ccm's line
/// first and unique makes the id deterministic. Empty/whitespace-only parent
/// values behave like unset. Output uses `\n` line endings.
pub(crate) fn merge_custom_headers(parent: Option<&str>, id: &str) -> String {
    let ours = format!("x-ccm-client: {id}");
    let Some(parent) = parent else {
        return ours;
    };
    let mut lines = vec![ours];
    for line in parent.lines() {
        let line = line.trim();
        if line.is_empty() || is_client_header_line(line) {
            continue;
        }
        lines.push(line.to_string());
    }
    lines.join("\n")
}

fn is_client_header_line(line: &str) -> bool {
    line.split_once(':')
        .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("x-ccm-client"))
}

/// Short per-launch default client id: 8 lowercase hex chars derived from a
/// hash of the process id and the current unix millisecond. Valid client-id
/// charset by construction, zero dependencies, and intentionally NOT stable
/// across restarts (no cross-restart session identity in v0.4).
pub(crate) fn short_client_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    short_client_id_from(std::process::id(), millis)
}

/// Pure core of [`short_client_id`] so the format is testable without
/// reading the clock.
fn short_client_id_from(pid: u32, millis: u128) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pid.hash(&mut hasher);
    millis.hash(&mut hasher);
    format!("{:08x}", hasher.finish() & 0xffff_ffff)
}

/// True when the model's provider speaks the OpenAI chat/completions
/// protocol and the model can therefore only run through the proxy (the
/// translation is proxy-side). Unknown models/models report `false`; name
/// resolution errors are surfaced by the caller.
pub(crate) fn is_openai_compatible_model(config: &AppConfig, model_name: &str) -> bool {
    config
        .models
        .get(model_name)
        .and_then(|model| config.providers.get(&model.provider))
        .is_some_and(|provider| matches!(provider.kind, ProviderKind::OpenAICompatible))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_headers_without_parent_value_is_just_our_line() {
        assert_eq!(merge_custom_headers(None, "abc"), "x-ccm-client: abc");
        // Empty or whitespace-only parent values behave like unset.
        assert_eq!(merge_custom_headers(Some(""), "abc"), "x-ccm-client: abc");
        assert_eq!(
            merge_custom_headers(Some(" \r\n  \n"), "abc"),
            "x-ccm-client: abc"
        );
    }

    #[test]
    fn custom_headers_preserve_unrelated_parent_lines() {
        assert_eq!(
            merge_custom_headers(Some("X-Custom: 1\nX-Other: two"), "abc"),
            "x-ccm-client: abc\nX-Custom: 1\nX-Other: two"
        );
    }

    #[test]
    fn custom_headers_replace_existing_client_lines_with_exactly_one() {
        let parent = "X-Custom: 1\nX-CCM-Client: stale\nX-Other: 2\nx-ccm-client: also-stale";
        let merged = merge_custom_headers(Some(parent), "fresh");
        assert_eq!(
            merged, "x-ccm-client: fresh\nX-Custom: 1\nX-Other: 2",
            "all x-ccm-client lines are dropped and ccm's line is first"
        );
    }

    #[test]
    fn custom_headers_tolerate_crlf_line_endings() {
        let parent = "X-Custom: 1\r\nx-ccm-client: stale\r\n";
        assert_eq!(
            merge_custom_headers(Some(parent), "abc"),
            "x-ccm-client: abc\nX-Custom: 1"
        );
    }

    #[test]
    fn short_client_ids_are_8_lowercase_hex_chars() {
        for id in [
            short_client_id_from(1234, 1_700_000_000_000),
            short_client_id_from(0, 0),
            short_client_id_from(u32::MAX, u128::MAX),
        ] {
            assert_eq!(id.len(), 8, "{id}");
            assert!(
                id.chars()
                    .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()),
                "{id}"
            );
            assert!(crate::proxy::valid_client_id(&id), "{id}");
        }
    }

    #[test]
    fn short_client_ids_differ_across_launch_inputs() {
        assert_ne!(short_client_id_from(1, 100), short_client_id_from(2, 100));
        assert_ne!(short_client_id_from(1, 100), short_client_id_from(1, 101));
    }

    #[test]
    fn openai_kind_models_are_proxy_only() {
        let mut config = AppConfig::starter();
        config.add_provider(
            "deepseek".to_string(),
            crate::provider::Provider {
                kind: ProviderKind::OpenAICompatible,
                base_url: "https://api.deepseek.com".to_string(),
                auth: None,
            },
        );
        config
            .add_model(
                "deepseek-chat".to_string(),
                crate::model::Model {
                    provider: "deepseek".to_string(),
                    model_id: "deepseek-chat".to_string(),
                    routing: crate::model::ModelRouting::default(),
                },
            )
            .unwrap();

        assert!(is_openai_compatible_model(&config, "deepseek-chat"));
        // anthropic/anthropic-compatible kinds and unknown names are not.
        assert!(!is_openai_compatible_model(&config, "glm"));
        assert!(!is_openai_compatible_model(&config, "no-such-model"));
    }
}
