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
        model.context_window,
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

fn run_command(
    base_url: &str,
    token: &str,
    model: &str,
    auth: ProviderAuth,
    context_window: Option<u64>,
) -> Result<()> {
    run_command_inner(base_url, token, model, auth, None, None, context_window)
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
        None,
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
    context_window: Option<u64>,
) -> Result<()> {
    let mut command = build_claude_command(
        base_url,
        token,
        model,
        auth,
        proxy_url,
        client_id,
        context_window,
    );

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

/// Assemble the `claude` child-process environment without spawning it, so
/// the injected variables are testable without touching the real process.
/// The spawn half lives in `run_command_inner`.
fn build_claude_command(
    base_url: &str,
    token: &str,
    model: &str,
    auth: ProviderAuth,
    proxy_url: Option<&str>,
    client_id: Option<&str>,
    context_window: Option<u64>,
) -> Command {
    let mut command = claude_command();
    command
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", model)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN");

    // Declared context window (direct mode): Claude Code does not know
    // third-party model ids and would assume a small default window for
    // them, auto-compacting long sessions at a fraction of the real one.
    // Setting the declared value also overrides any inherited variable;
    // an undeclared model leaves the parent environment untouched. Proxy
    // launches pass `None` (wired in `run_command_with_proxy_env`) — the
    // launch-time target would go stale under a runtime `ccm switch`.
    if let Some(window) = context_window {
        command.env("CLAUDE_CODE_MAX_CONTEXT_TOKENS", window.to_string());
    }

    if let Some(proxy_url) = proxy_url {
        command.env("CCM_PROXY_URL", proxy_url);
    } else {
        command.env_remove("CCM_PROXY_URL");
    }

    match client_id {
        Some(client_id) => {
            command.env("CCM_CLIENT_ID", client_id).env(
                "ANTHROPIC_CUSTOM_HEADERS",
                merge_custom_headers(parent_custom_headers().as_deref(), client_id),
            );
        }
        None => {
            // Direct mode carries no ccm identity. A stale CCM_CLIENT_ID or
            // inherited `x-ccm-client` line (e.g. a direct launch nested in a
            // proxy-launched session) would scope a nested `ccm switch` to
            // the DEFAULT proxy URL and leak the identity header to the real
            // upstream — there is no proxy to strip it. Unrelated custom
            // headers pass through untouched.
            command.env_remove("CCM_CLIENT_ID");
            if let Some(parent) = parent_custom_headers() {
                match strip_client_headers(&parent) {
                    Some(filtered) => {
                        command.env("ANTHROPIC_CUSTOM_HEADERS", filtered);
                    }
                    None => {
                        command.env_remove("ANTHROPIC_CUSTOM_HEADERS");
                    }
                }
            }
        }
    }

    match auth {
        ProviderAuth::XApiKey => {
            command.env("ANTHROPIC_API_KEY", token);
        }
        ProviderAuth::Bearer => {
            command.env("ANTHROPIC_AUTH_TOKEN", token);
        }
    }

    command
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

/// Direct mode drops any inherited `x-ccm-client` lines while unrelated
/// custom headers survive. Returns `None` when nothing remains, so the
/// variable is removed rather than set to an empty value.
pub(crate) fn strip_client_headers(parent: &str) -> Option<String> {
    let kept: Vec<&str> = parent
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_client_header_line(line))
        .collect();
    (!kept.is_empty()).then(|| kept.join("\n"))
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
    use std::ffi::OsStr;

    /// Value `key` is set to on the child command, or `None` when it is
    /// not set. Note the collapse: `get_envs` surfaces `env_remove`d keys
    /// as `(key, None)` pairs (stable since 1.57), which this helper
    /// flattens — "never set" and "explicitly removed" are
    /// indistinguishable here. Where the distinction is the contract, use
    /// [`env_key_untouched`] instead.
    fn env_value<'a>(command: &'a Command, key: &'a str) -> Option<&'a OsStr> {
        command
            .get_envs()
            .find(|(k, _)| *k == OsStr::new(key))
            .and_then(|(_, v)| v)
    }

    /// True when `key` is entirely absent from the command's explicit-env
    /// map — neither set nor removed, so a parent-shell value passes
    /// through to the child untouched. This is the pin for "未声明 =
    /// 不干预" (USAGE 4.2/5.3): an added `env_remove` would strip a
    /// parent value while `env_value` alone reports `None` for both
    /// cases.
    fn env_key_untouched(command: &Command, key: &str) -> bool {
        command
            .get_envs()
            .find(|(k, _)| *k == OsStr::new(key))
            .is_none()
    }

    #[test]
    fn declared_context_window_is_injected_on_direct_launches() {
        let command = build_claude_command(
            "https://api.z.ai/api/anthropic",
            "tok",
            "glm-5.3",
            ProviderAuth::XApiKey,
            None,
            None,
            Some(1_000_000),
        );
        assert_eq!(
            env_value(&command, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            Some(OsStr::new("1000000"))
        );
    }

    #[test]
    fn undeclared_context_window_leaves_the_environment_untouched() {
        // The variable is never set (and never removed): an undeclared
        // model means "let Claude Code assume its default window", and a
        // parent-shell value stays authoritative.
        let command = build_claude_command(
            "https://api.anthropic.com",
            "tok",
            "claude-sonnet-5-5",
            ProviderAuth::XApiKey,
            None,
            None,
            None,
        );
        assert!(env_key_untouched(
            &command,
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS"
        ));
    }

    #[test]
    fn proxy_shape_sets_identity_but_not_a_context_window() {
        // The proxy launch shape (proxy_url + client_id present) carries
        // identity variables and never a window key: the shape itself
        // injects none, and the production wiring `run_claude_via_proxy`
        // → `run_command_with_proxy_env` passes `None` by construction —
        // the accepted untested seam (V0.5_PLAN §11); this pins the
        // builder half, not that wiring. Absence is asserted at the
        // `get_envs` level so a future `env_remove` would also fail:
        // proxy sessions pass a parent-shell window variable through
        // untouched (USAGE 5.3).
        let command = build_claude_command(
            "http://127.0.0.1:13521",
            "ccm-local-abc",
            "ccm",
            ProviderAuth::Bearer,
            Some("http://127.0.0.1:13521"),
            Some("abc"),
            None,
        );
        assert_eq!(
            env_value(&command, "CCM_PROXY_URL"),
            Some(OsStr::new("http://127.0.0.1:13521"))
        );
        assert_eq!(
            env_value(&command, "CCM_CLIENT_ID"),
            Some(OsStr::new("abc"))
        );
        assert!(env_key_untouched(
            &command,
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS"
        ));
    }

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
    fn direct_mode_strips_inherited_client_header_lines() {
        // Unrelated lines survive; client lines (any case/spacing) drop.
        assert_eq!(
            strip_client_headers("X-Custom: 1\nx-ccm-client: stale\nX-CCM-CLIENT: also\r\n"),
            Some("X-Custom: 1".to_string())
        );
        // Only client lines / whitespace remain -> remove the variable.
        assert_eq!(strip_client_headers("x-ccm-client: stale\n\n"), None);
        assert_eq!(strip_client_headers("  "), None);
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
                    context_window: None,
                    routing: crate::model::ModelRouting::default(),
                    pricing: None,
                },
            )
            .unwrap();

        assert!(is_openai_compatible_model(&config, "deepseek-chat"));
        // anthropic/anthropic-compatible kinds and unknown names are not.
        assert!(!is_openai_compatible_model(&config, "glm"));
        assert!(!is_openai_compatible_model(&config, "no-such-model"));
    }
}
