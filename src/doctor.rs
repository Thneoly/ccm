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
    check_history(config);

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

    // The provider lookup precedes the context-window block: its line and
    // the settings-conflict check below both promise DIRECT-LAUNCH
    // injection, which never happens when the provider is missing (`ccm
    // run` aborts at "unknown provider" before any injection) — the same
    // honesty as the proxy-only suppression further down.
    let Some(provider) = config.providers.get(&model.provider) else {
        println!("✗ provider `{}` is missing from config", model.provider);
        return Ok(());
    };
    println!("✓ provider: {}", model.provider);
    println!("✓ base URL: {}", provider.base_url);

    // Computed once: it gates the context-window line here and prints the
    // proxy-only note further down.
    let proxy_only = launcher::is_openai_compatible_model(config, &route.primary);
    if let Some(line) = context_window_line(&model.model_id, model.context_window, proxy_only) {
        println!("{line}");
    }
    if should_check_window_override(model.context_window, proxy_only) {
        check_claude_window_override();
    }

    if proxy_only {
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

    // v0.5 M5: the endpoint line probes `GET /v1/models` (unauthenticated)
    // instead of a bare GET of base_url — "does this gateway support
    // discovery at all" is now an explicit fact, not "status varies by
    // gateway". 401/403 still prove the endpoint exists. The probe
    // follows NO redirects (verify-pass fix): an SSO ingress's
    // 302-to-login must surface as its real status, not a followed 200,
    // and a bare 200 is downgraded when the content-type is not JSON —
    // catch-all servers and login pages also answer 200.
    let models_url = format!("{}/v1/models", provider.base_url.trim_end_matches('/'));
    let probe = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map(|client| client.get(models_url));
    match probe {
        Ok(request) => match request.send().await {
            Ok(response) => {
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                println!(
                    "{}",
                    discovery_verdict(response.status().as_u16(), content_type, &model.provider)
                );
            }
            Err(error) => println!("✗ endpoint: {error}"),
        },
        Err(error) => println!("✗ endpoint: {error}"),
    }

    println!(
        "\nFor a full authenticated model check, run `ccm health {}`.",
        route.primary
    );
    Ok(())
}

/// The endpoint-line verdict for one probe answer (v0.5 M5). Pure so the
/// 200-content-type and redirect-status cases stay pinned without HTTP.
/// A 200 with a JSON (or absent) content-type is a checkmark; a 200 with
/// anything else (an HTML login page, a catch-all SPA fallback) is a `!`
/// naming `ccm discover` as the confirmation — "answered" must never
/// affirm a listing the endpoint does not actually serve.
fn discovery_verdict(status: u16, content_type: &str, provider: &str) -> String {
    match status {
        200 => {
            let content_type = content_type.trim().to_ascii_lowercase();
            if content_type.contains("json") || content_type.is_empty() {
                "✓ discovery endpoint: /v1/models answered (200)".to_string()
            } else {
                format!(
                    "! discovery endpoint: /v1/models answered (200, `{content_type}`) — likely not a models listing; `ccm discover {provider}` confirms"
                )
            }
        }
        401 | 403 => format!(
            "✓ discovery endpoint: /v1/models exists ({status}, needs auth — `ccm discover {provider}`)"
        ),
        404 | 405 => format!(
            "! discovery endpoint: /v1/models not exposed ({status}) — add models manually"
        ),
        status => format!("! discovery endpoint: /v1/models answered ({status})"),
    }
}

/// The context-window line for the current model. `Some` → the declared
/// window and where it goes; `None` + a non-claude id → a `!` advisory,
/// because Claude Code does not know third-party ids and assumes a default
/// window for them — the premature auto-compaction users actually feel;
/// `None` + a claude id → no line (Claude Code knows those windows natively,
/// and the variable would be inert for them anyway, v2.1.193+).
/// Proxy-only (openai-compatible) models get NO line in either arm: the
/// line's promise is direct-launch injection, which never happens for
/// them — the proxy-only note owns that story.
fn context_window_line(
    model_id: &str,
    context_window: Option<u64>,
    proxy_only: bool,
) -> Option<String> {
    if proxy_only {
        return None;
    }
    match context_window {
        Some(window) => Some(format!(
            "✓ context window: {window} tokens (injected as CLAUDE_CODE_MAX_CONTEXT_TOKENS on direct launches)"
        )),
        None if !is_claude_model_id(model_id) => Some(format!(
            "! context window: undeclared — Claude Code does not know `{model_id}` and assumes a default window for it (the frequent premature auto-compaction); set `context_window` on the model (guide 5.3)"
        )),
        None => None,
    }
}

/// Best-effort "Claude Code knows this id's window natively" check: ids
/// carrying a `claude` spelling plausibly resolve to a model Claude Code
/// recognizes; anything else gets the undeclared advisory. The heuristic
/// errs toward false POSITIVES: official `claude-*` ids always carry the
/// spelling, but a third-party id that merely contains it (e.g.
/// `glm-5.3-claude-edition`) is classified as natively known and its
/// advisory suppressed. USAGE 3.4 documents the check as exactly this
/// spelling rule.
fn is_claude_model_id(model_id: &str) -> bool {
    model_id.to_ascii_lowercase().contains("claude")
}

// v0.4 M5: an informative look at the observability history store. Non-fatal
// by design — a missing or empty directory is normal on a fresh install or
// before the proxy's first start; a disabled store is a deliberate config.
fn check_history(config: &AppConfig) {
    if !config.observability.history_enabled {
        println!("! history: disabled ([observability] history_enabled = false)");
        return;
    }
    match AppConfig::history_dir() {
        Ok(dir) => {
            let files = count_jsonl_files(&dir);
            if files == 0 {
                println!(
                    "! history: no files yet at {} (the proxy writes them while it runs)",
                    dir.display()
                );
            } else {
                println!("✓ history: {files} file(s) at {}", dir.display());
            }
        }
        Err(err) => println!("! history: cannot resolve the history directory: {err:#}"),
    }
}

fn count_jsonl_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
                .count()
        })
        .unwrap_or(0)
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

/// Pure decision for [`check_claude_window_override`]: does this parsed
/// Claude Code settings file set `CLAUDE_CODE_MAX_CONTEXT_TOKENS`? The
/// settings env block stacks AFTER ccm's per-launch injection (the same
/// mechanism as the ANTHROPIC_* overrides), so the settings value silently
/// wins over a declared `context_window` on direct launches.
fn window_override_in(value: &serde_json::Value) -> bool {
    value
        .get("env")
        .and_then(|env| env.as_object())
        .is_some_and(|env| env.contains_key("CLAUDE_CODE_MAX_CONTEXT_TOKENS"))
}

/// Pure decision for the settings-conflict check in [`run`]: the conflict
/// only exists where ccm injects — a declared window on a
/// direct-launchable model. For undeclared or proxy sessions that variable
/// is the documented Claude-Code-side mechanism (guide 5.3), never a
/// conflict, so the check never runs there.
fn should_check_window_override(context_window: Option<u64>, proxy_only: bool) -> bool {
    context_window.is_some() && !proxy_only
}

/// Warn when Claude Code's own settings env block carries a
/// `CLAUDE_CODE_MAX_CONTEXT_TOKENS` that would override the declared
/// window. Called only against a declared, direct-launchable window — for
/// undeclared or proxy sessions that variable is the documented
/// Claude-Code-side mechanism (guide 5.3), not a conflict, so it is never
/// flagged there.
fn check_claude_window_override() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    for name in ["settings.json", "settings.local.json"] {
        let path = home.join(".claude").join(name);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let raw = raw.trim_start_matches('\u{feff}');
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        if window_override_in(&value) {
            println!(
                "! {name}: env block sets CLAUDE_CODE_MAX_CONTEXT_TOKENS — it wins over the declared context_window on direct launches; remove it (guide 5.3)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn discovery_verdict_covers_the_probe_answers() {
        // JSON or absent content-type: the checkmark (z.ai/minimax shape).
        assert_eq!(
            discovery_verdict(200, "application/json", "zai"),
            "✓ discovery endpoint: /v1/models answered (200)"
        );
        assert_eq!(
            discovery_verdict(200, "", "zai"),
            "✓ discovery endpoint: /v1/models answered (200)"
        );
        // A bare 200 proves only that SOMETHING answered: an HTML login
        // page or catch-all fallback is a `!`, with discover as the
        // confirmation.
        let html = discovery_verdict(200, "text/html; charset=utf-8", "zai");
        assert!(html.starts_with('!'), "{html}");
        assert!(html.contains("text/html"), "{html}");
        assert!(html.contains("ccm discover zai"), "{html}");
        // Redirects are not followed — an SSO 302 lands in the catch-all
        // `!` arm with its real status.
        assert!(discovery_verdict(302, "text/html", "zai").starts_with('!'));
        assert!(discovery_verdict(302, "text/html", "zai").contains("302"));
        assert!(discovery_verdict(401, "", "zai").contains("needs auth"));
        assert!(discovery_verdict(404, "", "zai").contains("not exposed"));
    }

    #[test]
    fn context_window_line_covers_declared_undeclared_and_claude_ids() {
        // declared: the fact and where it goes
        assert_eq!(
            context_window_line("glm-5.3", Some(1_000_000), false).as_deref(),
            Some(
                "✓ context window: 1000000 tokens (injected as CLAUDE_CODE_MAX_CONTEXT_TOKENS on direct launches)"
            )
        );
        // undeclared third-party id: the advisory names the model and the fix
        let advisory = context_window_line("MiniMax-M3", None, false).unwrap();
        assert!(advisory.starts_with('!'), "{advisory}");
        assert!(advisory.contains("MiniMax-M3"), "{advisory}");
        assert!(advisory.contains("context_window"), "{advisory}");
        // claude spellings stay silent: Claude Code knows those windows
        assert_eq!(context_window_line("claude-sonnet-5-5", None, false), None);
        // ...including non-bare spellings it resolves and other casings
        assert_eq!(
            context_window_line("anthropic/claude-opus-4-8", None, false),
            None
        );
        assert_eq!(context_window_line("Claude-Sonnet-5-5", None, false), None);
        // a declared window is stated even for claude ids (the owner said so)
        assert!(context_window_line("claude-sonnet-5-5", Some(200_000), false).is_some());
        // proxy-only models never direct-launch, so the line's
        // injection promise would be false in both arms — suppressed
        // entirely (the proxy-only note owns that story).
        assert_eq!(
            context_window_line("deepseek-chat", Some(128_000), true),
            None
        );
        assert_eq!(context_window_line("deepseek-chat", None, true), None);
    }

    #[test]
    fn window_override_check_requires_a_declared_direct_launchable_window() {
        // The doc-promised gate (USAGE 3.4/5.3): the settings variable is
        // only a conflict where ccm injects the declared window. Undeclared
        // or proxy-only models treat it as the documented Claude-Code-side
        // mechanism — never flagged.
        assert!(should_check_window_override(Some(1_000_000), false));
        assert!(!should_check_window_override(None, false));
        assert!(!should_check_window_override(Some(1_000_000), true));
        assert!(!should_check_window_override(None, true));
    }

    #[test]
    fn window_override_detection_and_its_precision() {
        // The conflicting key alone is enough...
        assert!(window_override_in(&json!({
            "env": {"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "200000"}
        })));
        // ...unrelated env keys are not it...
        assert!(!window_override_in(&json!({
            "env": {"ANTHROPIC_BASE_URL": "https://example.com"}
        })));
        // ...and neither are other blocks or missing env.
        assert!(!window_override_in(
            &json!({"other": {"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "1"}})
        ));
        assert!(!window_override_in(&json!({})));
    }

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

    #[test]
    fn counts_jsonl_files_and_ignores_others() {
        let dir = std::env::temp_dir().join(format!("ccm-doctor-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("decisions.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join("circuit-123.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join(".lock"), "1\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        assert_eq!(count_jsonl_files(&dir), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_history_directory_counts_zero() {
        let dir = std::env::temp_dir().join("ccm-doctor-history-missing-dir");
        assert_eq!(count_jsonl_files(&dir), 0);
    }
}
