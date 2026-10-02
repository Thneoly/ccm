use std::process::Stdio;

use anyhow::Result;

use crate::{config::AppConfig, credential, launcher, state::AppState};

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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => println!("✓ Claude Code: installed"),
        _ => println!("✗ Claude Code: not found on PATH"),
    }
}

// Claude Code applies its settings.json env block after the environment ccm
// builds, so ANTHROPIC_* keys there silently override ccm's per-launch
// injection and routing.
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
    fn other_env_keys_and_missing_blocks_are_not_overrides() {
        assert!(anthropic_env_overrides(&json!({"env": {"API_TIMEOUT_MS": "1"}})).is_empty());
        assert!(anthropic_env_overrides(&json!({})).is_empty());
    }
}
