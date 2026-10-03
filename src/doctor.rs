use std::process::Stdio;

use anyhow::Result;

use crate::{config::AppConfig, credential, launcher, state::AppState};

/// Claude Code first supported `ANTHROPIC_CUSTOM_HEADERS` (the primary
/// client-identity channel for multi-client routing) in 2.1.227. Below this
/// version only the `ccm-local-<id>` token channel identifies a client.
const CUSTOM_HEADERS_MIN_VERSION: (u64, u64, u64) = (2, 1, 227);

pub async fn run(config: &AppConfig) -> Result<()> {
    println!("CCM doctor\n");

    check_claude();
    check_claude_settings_overrides();

    let state = AppState::load_or_migrate(config)?;
    let Some(current) = state.current.as_deref() else {
        println!("! current target: not selected");
        return Ok(());
    };
    println!("✓ current target: {current}");

    let route = match config.resolve_route(current) {
        Ok(route) => route,
        Err(error) => {
            println!("✗ target `{current}`: {error}");
            return Ok(());
        }
    };
    println!("✓ primary model: {}", route.primary);

    let Some(model) = config.models.get(&route.primary) else {
        println!("✗ model `{}` is missing from config", route.primary);
        return Ok(());
    };
    println!("✓ model id: {}", model.model_id);

    let Some(provider) = config.providers.get(&model.provider) else {
        println!("✗ provider `{}` is missing from config", model.provider);
        return Ok(());
    };
    println!("✓ provider: {}", model.provider);
    println!("✓ base URL: {}", provider.base_url);

    if launcher::is_openai_compatible_model(config, &route.primary) {
        println!("! current target: {}", launcher::PROXY_ONLY_REASON);
    }

    match credential::get(&model.provider) {
        Ok(_) => println!("✓ credential: present"),
        Err(_) => println!(
            "✗ credential: missing (set CCM_{}_API_KEY or run `ccm auth set {}`)",
            model
                .provider
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_uppercase()
                } else {
                    '_'
                })
                .collect::<String>(),
            model.provider
        ),
    }

    match reqwest::Client::new().get(&provider.base_url).send().await {
        Ok(response) => println!("✓ endpoint: reachable ({})", response.status()),
        Err(error) => println!("✗ endpoint: {error}"),
    }

    println!(
        "\nFor a full authenticated model check, run `ccm health {}`.",
        route.primary
    );
    Ok(())
}

fn check_claude() {
    let result = launcher::claude_command()
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match result {
        Ok(output) if output.status.success() => {
            match parse_claude_version(&String::from_utf8_lossy(&output.stdout)) {
                Some(version) => {
                    println!("✓ Claude Code: installed ({})", format_version(version));
                    if version_below(version) {
                        println!(
                        "! Claude Code {} predates {}: ANTHROPIC_CUSTOM_HEADERS is unsupported there, client identity falls back to the token channel only",
                        format_version(version),
                        format_version(CUSTOM_HEADERS_MIN_VERSION)
                    );
                    }
                }
                // Unparseable output keeps the plain installed line: no version,
                // no warning.
                None => println!("✓ Claude Code: installed"),
            }
        }
        _ => println!("✗ Claude Code: not found on PATH"),
    }
}

/// First `x.y.z` version token in `claude --version` output.
fn parse_claude_version(output: &str) -> Option<(u64, u64, u64)> {
    output
        .split(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .find_map(parse_version_token)
}

fn parse_version_token(token: &str) -> Option<(u64, u64, u64)> {
    let mut parts = token.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(major), Some(minor), Some(patch), None) => Some((
            major.parse().ok()?,
            minor.parse().ok()?,
            patch.parse().ok()?,
        )),
        _ => None,
    }
}

fn format_version(version: (u64, u64, u64)) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

fn version_below(version: (u64, u64, u64)) -> bool {
    version < CUSTOM_HEADERS_MIN_VERSION
}

// Claude Code applies its settings.json env block after the environment ccm
// builds, so ANTHROPIC_* keys there silently override ccm's per-launch
// injection and routing — including ANTHROPIC_CUSTOM_HEADERS, which would
// clobber the injected client-identity header.
fn check_claude_settings_overrides() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let mut found_any_file = false;
    for name in ["settings.json", "settings.local.json"] {
        let path = home.join(".claude").join(name);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        found_any_file = true;
        let raw = raw.trim_start_matches('\u{feff}');
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            println!("! {name}: invalid JSON, cannot check for ANTHROPIC_* env overrides");
            continue;
        };
        let overrides = anthropic_env_overrides(&value);
        if overrides.is_empty() {
            println!("✓ {name}: no ANTHROPIC_* env overrides");
        } else {
            println!(
                "✗ {name}: env block sets {} — applied after ccm's injection and overrides its routing; move them to user environment variables",
                overrides.join(", ")
            );
        }
    }
    if !found_any_file {
        println!("✓ Claude Code settings: not found (no env overrides)");
    }
}

fn anthropic_env_overrides(value: &serde_json::Value) -> Vec<&'static str> {
    let Some(env) = value.get("env").and_then(|env| env.as_object()) else {
        return Vec::new();
    };
    [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_CUSTOM_HEADERS",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ]
    .into_iter()
    .filter(|key| env.contains_key(*key))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_anthropic_env_overrides() {
        let settings = json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "secret",
                "ANTHROPIC_BASE_URL": "https://example.com",
                "API_TIMEOUT_MS": "3000"
            }
        });
        assert_eq!(
            anthropic_env_overrides(&settings),
            vec!["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"]
        );
    }

    #[test]
    fn custom_headers_in_settings_override_the_identity_header() {
        let settings = json!({
            "env": {
                "ANTHROPIC_CUSTOM_HEADERS": "x-ccm-client: forged"
            }
        });
        assert_eq!(
            anthropic_env_overrides(&settings),
            vec!["ANTHROPIC_CUSTOM_HEADERS"]
        );
    }

    #[test]
    fn other_env_keys_and_missing_blocks_are_not_overrides() {
        assert!(anthropic_env_overrides(&json!({"env": {"API_TIMEOUT_MS": "1"}})).is_empty());
        assert!(anthropic_env_overrides(&json!({})).is_empty());
    }

    #[test]
    fn parses_first_version_token_from_claude_output() {
        assert_eq!(
            parse_claude_version("2.1.261 (Claude Code)\n"),
            Some((2, 1, 261))
        );
        assert_eq!(parse_claude_version("1.9.0"), Some((1, 9, 0)));
        assert_eq!(parse_claude_version("garbage"), None);
        assert_eq!(parse_claude_version(""), None);
    }

    #[test]
    fn custom_headers_support_threshold_is_2_1_227() {
        assert!(version_below((1, 9, 0)));
        assert!(version_below((2, 1, 226)));
        assert!(!version_below((2, 1, 227)));
        assert!(!version_below((2, 1, 261)));
        assert!(!version_below((3, 0, 0)));
    }
}
