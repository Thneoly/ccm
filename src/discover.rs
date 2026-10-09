//! Provider discovery (v0.5 M5): list a gateway's `GET /v1/models` and
//! turn selected ids into registered models — the onboarding path that
//! replaces hand-copying `model_id`s from a gateway console.
//!
//! Shape discipline (M0 live calibration): the common skeleton is
//! `data[].id`; the Anthropic list shape adds `has_more`/`first_id`/
//! `last_id` pagination and `display_name`, the OpenAI shape adds
//! `object:"list"` and `owned_by`. Every extra field is OPTIONAL — z.ai
//! omits the pagination fields entirely while minimax returns the full
//! Anthropic shape, so the parser stays lenient and pagination simply
//! ends when `has_more` is absent or false (capped at 10 pages).
//!
//! Registration contract: an already-registered `(provider, model_id)`
//! pair is SKIPPED with a notice — never overwritten (`ccm add model`
//! stays the upsert path; discovery must not clobber hand-tuned
//! weights). New models land with routing weights 1.0/1.0 and
//! `pricing = None` (prices are hand-entered TOML facts, by policy);
//! discovery prints a commented `[models.<alias>.pricing]` skeleton per
//! selected unpriced model — the discover → hand-enter prices →
//! `ccm advise` journey.
//!
//! Credentials boundary (invariant 1): the token flows into the request
//! header ONLY — listing, display, and registration text are built from
//! the parsed listing and config, which have no field that could carry
//! it. Gateway text quoted into an error is scrubbed of the sent token
//! first (a proxy echoing `x-api-key: ...` back in a diagnostic body
//! never reaches the terminal), and the discovery client follows NO
//! redirects — the credential is keyed to the configured host (reqwest
//! strips `Authorization` cross-host but not `x-api-key`). Degradation
//! paths: 404/405 → the manual onboarding message; 401/403 → the
//! health-style auth failure text; a 200 body with no `data` array →
//! "not a models list" carrying the gateway's own words (z.ai answers a
//! bad key as 200 + `{"code":401,...}` — the auth failure never
//! surfaces as a misleading "0 models"); every request is
//! timeout-bounded (10s).
//!
//! Honest boundary, documented in USAGE: the gateway's list is a
//! convenience, not a contract — listed ids can 400 at use time, and
//! working models can be unlisted. A provider whose base_url points at
//! another ccm gets the 404 path today (the proxy does not serve
//! /v1/models); if a future ccm synthesizes one, the listed names are
//! that ccm's aliases, not gateway ids.

use std::io::{self, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::{
    config::AppConfig,
    credential,
    model::{Model, ModelRouting},
    provider::{Provider, ProviderKind},
};

/// One request may take this long, end to end. A gateway that never
/// answers must not hang onboarding.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Anthropic-style pagination cap: a gateway that always answers
/// `has_more: true` ends here with a truncation notice instead of an
/// unbounded walk.
const MAX_PAGES: usize = 10;

/// Page size for follow-up pages (`?after_id=&limit=`). The first
/// request carries NO query — M0's calibration hit both real gateways
/// with the plain path.
const PAGE_LIMIT: &str = "100";

pub async fn run(config: &mut AppConfig, provider_name: Option<String>, all: bool) -> Result<()> {
    let name = match provider_name {
        Some(name) => name,
        None => prompt_provider(config)?,
    };
    let Some(provider) = config.providers.get(&name) else {
        let known: Vec<&str> = config.providers.keys().map(String::as_str).collect();
        if known.is_empty() {
            // The explicit-name path skips prompt_provider, so its
            // remediation message must live here too — a dangling
            // "configured providers: " helps nobody.
            bail!("no providers configured; run `ccm add provider <name>` first");
        }
        bail!(
            "unknown provider `{name}`; configured providers: {}",
            known.join(", ")
        );
    };
    let provider = provider.clone();
    // The existing resolution order and the env-name/auth-set message.
    let token = credential::get(&name)?;

    let listing = fetch_listing(&provider, &token).await?;
    if let Some(warning) = cross_check_warning(&listing, provider.kind, &name) {
        eprintln!("ccm: {warning}");
    }

    println!(
        "== ccm discover — provider {name} ({}) @ {}",
        provider.kind.as_str(),
        provider.base_url.trim_end_matches('/')
    );
    for (index, model) in listing.models.iter().enumerate() {
        println!("{}", render_listing_line(index, model));
    }
    let registered = listing
        .models
        .iter()
        .filter(|m| is_registered(config, &name, &m.id))
        .count();
    println!(
        "{} listed, {registered} already registered",
        listing.models.len()
    );
    if listing.truncated {
        println!(
            "note: pagination cap reached ({MAX_PAGES} pages) — the listing may be incomplete"
        );
    }
    if listing.models.is_empty() {
        println!("gateway listed 0 models; nothing to register");
        return Ok(());
    }

    let picked = if all {
        (0..listing.models.len()).collect::<Vec<_>>()
    } else {
        prompt_selection(&listing)?
    };
    if picked.is_empty() {
        println!("nothing selected; config unchanged");
        return Ok(());
    }

    let plans = plan_registration(config, &name, &listing, &picked);
    let mut report_lines = Vec::new();
    let mut saved_any = false;
    for (model, action) in &plans {
        match action {
            RegAction::Register { alias } => {
                config.add_model(
                    alias.clone(),
                    Model {
                        provider: name.clone(),
                        model_id: model.id.clone(),
                        routing: ModelRouting::default(),
                        // Prices are hand-entered TOML facts, never CLI
                        // defaults (the `ccm add model` policy).
                        pricing: None,
                    },
                )?;
                report_lines.push(format!("Saved model {alias}"));
                saved_any = true;
            }
            RegAction::Skip { existing_alias } => report_lines.push(format!(
                "Skipped {} — already registered as `{existing_alias}`",
                model.id
            )),
        }
    }
    // Persist first, report second: a failed save must not leave "Saved"
    // lines on stdout (the manage.rs no-faked-clean-close rule — `--all`
    // exists for scripts, and scripts scrape stdout).
    if saved_any {
        config.save()?;
    }
    for line in report_lines {
        println!("{line}");
    }

    // The §3.1 hand-off: skeletons for every selected model that still
    // has no pricing table (new registrations by construction, plus any
    // skipped model the owner never priced).
    let unpriced: Vec<&str> = picked
        .iter()
        .filter_map(|i| {
            let id = &listing.models[*i].id;
            let alias = config
                .models
                .iter()
                .find(|(_, m)| m.provider == name && &m.model_id == id)
                .map(|(alias, _)| alias.as_str())?;
            config
                .models
                .get(alias)
                .and_then(|m| m.pricing.as_ref())
                .is_none()
                .then_some(alias)
        })
        .collect();
    if !unpriced.is_empty() {
        println!("\n# hand-enter prices to make `ccm advise` useful — paste into config.toml:");
        for alias in unpriced {
            println!("{}", pricing_skeleton(alias));
        }
    }
    Ok(())
}

// ===========================================================================
// Listing (the IO half)
// ===========================================================================

/// Fetch the full listing, following Anthropic-style pagination when the
/// gateway actually asks for it (both M0 gateways do not).
async fn fetch_listing(provider: &Provider, token: &str) -> Result<Listing> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // Follow NO redirects: the credential is keyed to the configured
        // gateway host, and reqwest strips `Authorization` on cross-host
        // redirects but NOT `x-api-key`. A 302 ingress surfaces as its
        // own status instead of silently carrying the key elsewhere.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("cannot build the discovery HTTP client")?;
    let base = provider.base_url.trim_end_matches('/').to_string();

    let mut listing = Listing::default();
    for page in 0..MAX_PAGES {
        let mut builder = client
            .get(format!("{base}/v1/models"))
            .header("accept", "application/json");
        // Same kind-keyed request shape as the health check: anthropic
        // kinds carry the version header.
        if matches!(
            provider.kind,
            ProviderKind::Anthropic | ProviderKind::AnthropicCompatible
        ) {
            builder = builder.header("anthropic-version", "2023-06-01");
        }
        if page > 0 {
            let after = listing
                .last_id
                .as_deref()
                .context("gateway asked for another page but sent no last_id")?;
            builder = builder.query(&[("after_id", after), ("limit", PAGE_LIMIT)]);
        }

        let response = provider
            .apply_auth(builder, token)
            .send()
            .await
            .context("provider request failed")?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            bail!("provider reachable but authentication failed ({status})");
        }
        if status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED {
            bail!(
                "gateway does not expose /v1/models; add models manually (ccm add provider / ccm add model)"
            );
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "provider returned {status}: {}",
                truncate(&scrub_credential(&body, token), 300)
            );
        }
        let body = response
            .text()
            .await
            .context("cannot read the /v1/models response body")?;
        let page_listing = parse_listing(&scrub_credential(&body, token))?;
        listing.absorb(page_listing);
        if !listing.has_more {
            break;
        }
        if page + 1 == MAX_PAGES {
            listing.truncated = true;
        }
    }
    Ok(listing)
}

// ===========================================================================
// Parsing (the pure half)
// ===========================================================================

/// One listed model. Only `id` is guaranteed; the display extras are
/// shape-dependent and optional.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DiscoveredModel {
    pub id: String,
    /// Anthropic shape: a human-friendly name.
    pub display_name: Option<String>,
    /// OpenAI shape: the owning organization/account.
    pub owned_by: Option<String>,
}

/// Which protocol family the response looked like — for display and the
/// shape/kind cross-check only; parsing itself is shape-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ListShape {
    /// Full Anthropic list envelope (pagination fields and/or
    /// display_name entries).
    Anthropic,
    /// OpenAI list envelope (`object:"list"` and/or owned_by entries).
    OpenAi,
    /// The bare common skeleton — `data[].id` and nothing else (the z.ai
    /// capture). No pagination, no extras.
    #[default]
    Bare,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Listing {
    pub models: Vec<DiscoveredModel>,
    pub shape: ListShape,
    /// The last page's `has_more` (false when absent, as at z.ai).
    pub has_more: bool,
    /// The last page's `last_id` — the pagination cursor.
    pub last_id: Option<String>,
    /// The pagination cap was hit; the walk stopped early.
    pub truncated: bool,
}

impl Listing {
    /// Merge one page: extend models (deduped by id — pagination must not
    /// double-register), keep the freshest pagination cursor, and refine
    /// the shape with whatever this page revealed.
    fn absorb(&mut self, page: Listing) {
        for model in page.models {
            if !self.models.iter().any(|m| m.id == model.id) {
                self.models.push(model);
            }
        }
        if page.shape != ListShape::Bare {
            self.shape = page.shape;
        }
        self.has_more = page.has_more;
        self.last_id = page.last_id.or_else(|| self.last_id.clone());
    }
}

/// Lenient serde view over both protocol shapes; every field except the
/// `data` array itself is optional (M0: z.ai omits all pagination
/// fields). `data` is `Option` so an absent key stays distinguishable
/// from an empty list — every legitimate shape (anthropic, openai, bare)
/// carries a `data` member, so a 200 body without one is not a models
/// list at all (live calibration: z.ai answers a bad key as HTTP 200
/// with `{"code":401,"msg":"token expired or incorrect"}`).
#[derive(Deserialize)]
struct RawListing {
    #[serde(default)]
    data: Option<Vec<RawModel>>,
    #[serde(default)]
    has_more: Option<bool>,
    #[serde(default)]
    first_id: Option<String>,
    #[serde(default)]
    last_id: Option<String>,
    #[serde(default)]
    object: Option<String>,
}

#[derive(Deserialize)]
struct RawModel {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    owned_by: Option<String>,
}

/// Parse one page. Entries without a string `id` are skipped (a gateway
/// mixing shapes must not break the walk); the listing is a convenience.
/// A JSON body with NO `data` member is an error, not an empty listing:
/// the response is not a models list (an in-body auth failure, a login
/// page, a mis-pointed base_url), and the gateway's own words travel
/// with the error instead of a misleading "0 models".
pub(crate) fn parse_listing(body: &str) -> Result<Listing> {
    let raw: RawListing = serde_json::from_str(body)
        .with_context(|| "cannot parse the /v1/models response as JSON".to_string())?;
    let Some(data) = raw.data else {
        bail!(
            "the /v1/models response is not a models list (no \"data\" array): {} — some gateways answer an auth failure as HTTP 200 with an error body; verify the credential",
            truncate(body, 300)
        );
    };

    let mut anthropic_markers =
        raw.has_more.is_some() || raw.first_id.is_some() || raw.last_id.is_some();
    let mut openai_markers = raw.object.as_deref() == Some("list");
    let mut models = Vec::new();
    for entry in data {
        let Some(id) = entry.id.filter(|id| !id.is_empty()) else {
            continue;
        };
        openai_markers |= entry.owned_by.is_some();
        anthropic_markers |= entry.display_name.is_some();
        models.push(DiscoveredModel {
            id,
            display_name: entry.display_name,
            owned_by: entry.owned_by,
        });
    }
    let shape = if openai_markers {
        ListShape::OpenAi
    } else if anthropic_markers {
        ListShape::Anthropic
    } else {
        ListShape::Bare
    };
    Ok(Listing {
        models,
        shape,
        has_more: raw.has_more.unwrap_or(false),
        last_id: raw.last_id,
        truncated: false,
    })
}

/// The shape/kind cross-check (§3.4): an anthropic-looking response
/// against a declared openai-compatible kind (or vice versa) is today's
/// costliest config error caught early. A bare skeleton never warns —
/// it is shape-neutral.
pub(crate) fn cross_check_warning(
    listing: &Listing,
    kind: ProviderKind,
    provider: &str,
) -> Option<String> {
    let mismatch = matches!(
        (listing.shape, kind),
        (ListShape::Anthropic, ProviderKind::OpenAICompatible)
            | (
                ListShape::OpenAi,
                ProviderKind::Anthropic | ProviderKind::AnthropicCompatible
            )
    );
    mismatch.then(|| {
        format!(
            "provider `{provider}` is declared {} but the /v1/models response looks {} — check the kind; requests still follow the declared kind",
            kind.as_str(),
            match listing.shape {
                ListShape::Anthropic => "anthropic-shaped",
                ListShape::OpenAi => "openai-shaped",
                ListShape::Bare => "bare",
            }
        )
    })
}

// ===========================================================================
// Selection and registration (pure decisions + thin IO)
// ===========================================================================

/// Parse the interactive selection: `all`, empty/`none` (nothing), or a
/// comma list of 1-based numbers and `a-b` ranges. Indices are returned
/// deduplicated and sorted.
pub(crate) fn parse_selection(input: &str, count: usize) -> Result<Vec<usize>> {
    let input = input.trim();
    if input.is_empty() || input.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    if input.eq_ignore_ascii_case("all") {
        return Ok((0..count).collect());
    }
    let mut picked = Vec::new();
    for token in input.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let bounds = token.split_once('-');
        let indices: Vec<usize> = match bounds {
            Some((start, end)) => {
                let start: usize = start.trim().parse().map_err(|_| {
                    anyhow::anyhow!("invalid selection `{input}`: bad range `{token}`")
                })?;
                let end: usize = end.trim().parse().map_err(|_| {
                    anyhow::anyhow!("invalid selection `{input}`: bad range `{token}`")
                })?;
                if end < start {
                    bail!("invalid selection `{input}`: range `{token}` is reversed");
                }
                // Range-check BEFORE materializing: `(start..=end).collect()`
                // on a huge end would abort on capacity overflow before the
                // per-index check below could ever reject it.
                if start == 0 || end > count {
                    bail!(
                        "invalid selection `{input}`: range `{token}` is out of range 1..={count}"
                    );
                }
                (start..=end).collect()
            }
            None => {
                let index: usize = token.parse().map_err(|_| {
                    anyhow::anyhow!("invalid selection `{input}`: unknown item `{token}`")
                })?;
                vec![index]
            }
        };
        for index in indices {
            if index == 0 || index > count {
                bail!("invalid selection `{input}`: {index} is out of range 1..={count}");
            }
            if !picked.contains(&(index - 1)) {
                picked.push(index - 1);
            }
        }
    }
    if picked.is_empty() {
        bail!("invalid selection `{input}`: nothing selected; use `all`, `none`, or numbers");
    }
    picked.sort_unstable();
    Ok(picked)
}

/// Alias = normalized model_id: anything outside `[A-Za-z0-9_-]`
/// becomes `-` (TOML key and CLI safe). The mapping is char-for-char, so
/// a non-empty id (the only kind `parse_listing` admits) never
/// normalizes to empty.
pub(crate) fn normalize_alias(model_id: &str) -> String {
    model_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn is_registered(config: &AppConfig, provider: &str, model_id: &str) -> bool {
    config
        .models
        .values()
        .any(|m| m.provider == provider && m.model_id == model_id)
}

/// What registration will do with one picked model.
#[derive(Debug, PartialEq)]
pub(crate) enum RegAction {
    Register { alias: String },
    Skip { existing_alias: String },
}

/// The pure registration decision over the picked indices: skip what is
/// already registered under this provider (never overwrite), alias the
/// rest. Alias assignment sees earlier picks in the same run, so two
/// ids that normalize to the same alias do not collide.
pub(crate) fn plan_registration(
    config: &AppConfig,
    provider: &str,
    listing: &Listing,
    picked: &[usize],
) -> Vec<(DiscoveredModel, RegAction)> {
    // Seeded with EVERY name a mechanical alias could collide with:
    // model aliases (across providers) AND profile names — resolve_target
    // resolves models before profiles, so a discovered alias equal to a
    // profile name would silently capture that target.
    let mut taken: Vec<String> = config
        .models
        .keys()
        .chain(config.profiles.keys())
        .cloned()
        .collect();
    let mut plans = Vec::new();
    for index in picked {
        let model = &listing.models[*index];
        if let Some(existing) = config
            .models
            .iter()
            .find(|(_, m)| m.provider == provider && m.model_id == model.id)
        {
            plans.push((
                model.clone(),
                RegAction::Skip {
                    existing_alias: existing.0.clone(),
                },
            ));
            continue;
        }
        let base = normalize_alias(&model.id);
        let mut alias = base.clone();
        let mut suffix = 2;
        while taken.contains(&alias) {
            alias = format!("{base}-{suffix}");
            suffix += 1;
        }
        taken.push(alias.clone());
        plans.push((model.clone(), RegAction::Register { alias }));
    }
    plans
}

// ===========================================================================
// Rendering and prompts
// ===========================================================================

fn render_listing_line(index: usize, model: &DiscoveredModel) -> String {
    let extra = match (&model.display_name, &model.owned_by) {
        (Some(name), _) => format!("  {name}"),
        (None, Some(owner)) => format!("  (owned by {owner})"),
        (None, None) => String::new(),
    };
    format!("{:>4}. {}{}", index + 1, model.id, extra)
}

fn pricing_skeleton(alias: &str) -> String {
    format!(
        "# [models.{alias}.pricing]\n\
         # input = 0.0        # USD per 1M input tokens\n\
         # output = 0.0       # USD per 1M output tokens\n\
         # cache_read = 0.0   # USD per 1M cache-read tokens\n\
         # cache_write = 0.0  # USD per 1M cache-write tokens"
    )
}

/// Interactive provider pick, the `manage.rs` stdin style (no TUI
/// dependency). EOF behaves like empty input: the required() error.
fn prompt_provider(config: &AppConfig) -> Result<String> {
    if config.providers.is_empty() {
        bail!("no providers configured; run `ccm add provider <name>` first");
    }
    let known: Vec<&str> = config.providers.keys().map(String::as_str).collect();
    print!("Provider ({})? ", known.join(", "));
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim().to_string();
    if value.is_empty() {
        bail!("provider cannot be empty");
    }
    Ok(value)
}

fn prompt_selection(listing: &Listing) -> Result<Vec<usize>> {
    print!("Register which? (numbers like 1,3-5, `all`, or Enter for none): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    parse_selection(&input, listing.models.len())
}

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    input.chars().take(max).collect::<String>() + "…"
}

/// Invariant 1 hygiene for untrusted gateway text: a proxy or debug-mode
/// gateway can echo the just-sent credential back inside a body
/// ("invalid x-api-key: sk-..."), so the token is replaced before any
/// gateway text reaches an error message. Applied to EVERY body fetch
/// reads — both the non-2xx error arm and the listing parse path.
fn scrub_credential(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "***")
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::response::{IntoResponse, Json, Response};
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;

    // The proxy-suite env discipline: these tests mutate CCM_HOME and a
    // provider credential env var. They share the PROXY suite's lock —
    // every env-mutating test in the crate serializes against the same
    // mutex, so a discover run can never swap CCM_HOME out from under a
    // proxy integration test (or vice versa).
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::proxy::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ------------------------------------------------------------------
    // parse_listing: both shapes + the bare z.ai capture
    // ------------------------------------------------------------------

    #[test]
    fn parses_the_full_anthropic_list_shape() {
        // The minimax capture: pagination fields present, has_more=false.
        let body = json!({
            "data": [
                {"id": "MiniMax-M3", "type": "model", "display_name": "MiniMax M3",
                 "created_at": "2026-01-02T00:00:00Z"},
                {"id": "MiniMax-M2.5", "type": "model", "display_name": "MiniMax M2.5"}
            ],
            "has_more": false,
            "first_id": "MiniMax-M2.5",
            "last_id": "MiniMax-M3"
        })
        .to_string();
        let listing = parse_listing(&body).unwrap();
        assert_eq!(listing.shape, ListShape::Anthropic);
        assert!(!listing.has_more);
        assert_eq!(listing.last_id.as_deref(), Some("MiniMax-M3"));
        assert_eq!(listing.models.len(), 2);
        assert_eq!(
            listing.models[0],
            DiscoveredModel {
                id: "MiniMax-M3".to_string(),
                display_name: Some("MiniMax M3".to_string()),
                owned_by: None,
            }
        );
    }

    #[test]
    fn parses_the_bare_zai_shape_without_pagination_fields() {
        // The z.ai capture: data[].{id,type,display_name,created_at} and
        // NOT ONE pagination field. display_name still marks it
        // anthropic-shaped for the cross-check.
        let body = json!({
            "data": [
                {"id": "glm-4.5", "type": "model", "display_name": "GLM-4.5"},
                {"id": "glm-5.3", "type": "model", "display_name": "GLM-5.3"}
            ]
        })
        .to_string();
        let listing = parse_listing(&body).unwrap();
        assert_eq!(listing.shape, ListShape::Anthropic, "display_name marks it");
        assert!(!listing.has_more, "absent has_more ends pagination");
        assert_eq!(listing.last_id, None);
        assert_eq!(listing.models.len(), 2);
    }

    #[test]
    fn parses_the_openai_list_shape() {
        let body = json!({
            "object": "list",
            "data": [
                {"id": "deepseek-chat", "object": "model", "owned_by": "deepseek"},
                {"id": "deepseek-reasoner", "object": "model", "owned_by": "deepseek"}
            ]
        })
        .to_string();
        let listing = parse_listing(&body).unwrap();
        assert_eq!(listing.shape, ListShape::OpenAi);
        assert_eq!(listing.models[1].owned_by.as_deref(), Some("deepseek"));
        assert_eq!(listing.models[0].display_name, None);
    }

    #[test]
    fn bare_skeleton_without_markers_stays_bare() {
        let listing =
            parse_listing(&json!({"data": [{"id": "a"}, {"id": "b"}]}).to_string()).unwrap();
        assert_eq!(listing.shape, ListShape::Bare);
        assert_eq!(listing.models.len(), 2);
    }

    #[test]
    fn entries_without_an_id_are_skipped_and_bad_json_fails() {
        let listing = parse_listing(
            &json!({"data": [{"id": "ok"}, {"object": "model"}, {"id": ""}]}).to_string(),
        )
        .unwrap();
        assert_eq!(listing.models.len(), 1, "only the id-bearing entry");

        let error = parse_listing("not json").unwrap_err();
        assert!(error.to_string().contains("cannot parse"), "{error:#}");
    }

    // Live calibration (M5 wire pass): z.ai answers a BAD key as HTTP 200
    // with this exact envelope — the auth failure never reaches the 401/403
    // arm. A body with no `data` member must error with the gateway's own
    // words, never degrade to a misleading "0 models".
    #[test]
    fn a_body_without_data_is_an_error_not_an_empty_listing() {
        let zai_bad_key =
            json!({"code": 401, "msg": "token expired or incorrect", "success": false});
        let error = parse_listing(&zai_bad_key.to_string()).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("not a models list"), "{text}");
        assert!(
            text.contains("token expired or incorrect"),
            "the gateway's own words travel with the error: {text}"
        );

        // An explicitly EMPTY data array is a legitimate empty listing.
        let listing = parse_listing(&json!({"data": []}).to_string()).unwrap();
        assert!(listing.models.is_empty());
    }

    #[test]
    fn absorb_dedupes_ids_across_pages_and_keeps_the_cursor() {
        let mut listing = parse_listing(
            &json!({"data": [{"id": "a"}, {"id": "b"}], "has_more": true, "last_id": "b"})
                .to_string(),
        )
        .unwrap();
        let next = parse_listing(
            &json!({"data": [{"id": "b"}, {"id": "c"}], "has_more": false, "last_id": "c"})
                .to_string(),
        )
        .unwrap();
        listing.absorb(next);
        assert_eq!(
            listing
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"],
            "pagination must not double-register"
        );
        assert!(!listing.has_more);
        assert_eq!(listing.last_id.as_deref(), Some("c"));
    }

    // ------------------------------------------------------------------
    // cross-check, aliasing, selection
    // ------------------------------------------------------------------

    #[test]
    fn cross_check_warns_only_on_shape_kind_mismatch() {
        let anthropic = Listing {
            shape: ListShape::Anthropic,
            ..Listing::default()
        };
        let openai = Listing {
            shape: ListShape::OpenAi,
            ..Listing::default()
        };
        let bare = Listing::default();

        let warning =
            cross_check_warning(&anthropic, ProviderKind::OpenAICompatible, "mixed").unwrap();
        assert!(warning.contains("openai-compatible"), "{warning}");
        assert!(warning.contains("anthropic-shaped"), "{warning}");

        let warning = cross_check_warning(&openai, ProviderKind::AnthropicCompatible, "mixed")
            .expect("openai shape on an anthropic kind warns");
        assert!(warning.contains("openai-shaped"), "{warning}");

        assert!(cross_check_warning(&bare, ProviderKind::OpenAICompatible, "x").is_none());
        assert!(cross_check_warning(&anthropic, ProviderKind::Anthropic, "x").is_none());
        assert!(cross_check_warning(&openai, ProviderKind::OpenAICompatible, "x").is_none());
    }

    #[test]
    fn aliases_are_normalized_and_suffixed_on_collision() {
        assert_eq!(normalize_alias("glm-5.3"), "glm-5-3");
        assert_eq!(normalize_alias("a b/c"), "a-b-c");
        assert_eq!(normalize_alias("MiniMax-M3"), "MiniMax-M3");
        assert_eq!(
            normalize_alias("..."),
            "---",
            "symbols map to dashes, never removed"
        );

        // Suffix-on-collision through the real path: `glm` is taken by a
        // DIFFERENT model_id, so a new id normalizing to `glm` gets -2.
        let config = scratch_config();
        let plans = plan_registration(&config, "mockz", &listing_of(&["glm", "fresh"]), &[0, 1]);
        assert_eq!(
            plans[0].1,
            RegAction::Register {
                alias: "glm-2".to_string()
            },
            "alias collision with a different model suffixes"
        );
        assert_eq!(
            plans[1].1,
            RegAction::Register {
                alias: "fresh".to_string()
            }
        );
    }

    // Verify-pass pin: `taken` is seeded with profile names too — a
    // mechanical alias must never capture a profile target
    // (resolve_target resolves models before profiles).
    #[test]
    fn a_discovered_alias_never_shadows_a_profile_name() {
        let mut config = scratch_config();
        config.profiles.insert(
            "fast".to_string(),
            crate::model::Profile {
                model: "glm".to_string(),
            },
        );
        let plans = plan_registration(&config, "mockz", &listing_of(&["fast", "fresh"]), &[0, 1]);
        assert_eq!(
            plans[0].1,
            RegAction::Register {
                alias: "fast-2".to_string()
            },
            "a profile name is taken — the alias suffixes away from it"
        );
        assert_eq!(
            plans[1].1,
            RegAction::Register {
                alias: "fresh".to_string()
            }
        );
    }

    #[test]
    fn selection_parsing_covers_all_none_numbers_and_ranges() {
        assert!(parse_selection("", 3).unwrap().is_empty());
        assert!(parse_selection("none", 3).unwrap().is_empty());
        assert_eq!(parse_selection("all", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_selection("2", 3).unwrap(), vec![1]);
        assert_eq!(parse_selection("1,3", 3).unwrap(), vec![0, 2]);
        assert_eq!(
            parse_selection("3,1,1", 3).unwrap(),
            vec![0, 2],
            "dedup + sort"
        );
        assert_eq!(parse_selection("1-3", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_selection("2,2-3", 3).unwrap(), vec![1, 2]);

        for bad in [
            "0",
            "4",
            "1-9",
            "9-1",
            "x",
            "1,x",
            "1-",
            "all,2",
            // A huge end must ERROR, not abort on capacity overflow: the
            // range is checked before `(start..=end)` is ever materialized.
            "1-18446744073709551615",
            "0-2",
            "1-2000000000",
        ] {
            assert!(parse_selection(bad, 3).is_err(), "{bad} must not parse");
        }
    }

    // ------------------------------------------------------------------
    // registration planning: skip-not-overwrite is the contract
    // ------------------------------------------------------------------

    fn scratch_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.add_provider(
            "mockz".to_string(),
            Provider {
                kind: ProviderKind::AnthropicCompatible,
                base_url: "http://127.0.0.1:9".to_string(),
                auth: None,
            },
        );
        // A hand-tuned registration discovery must never clobber.
        config
            .add_model(
                "glm".to_string(),
                Model {
                    provider: "mockz".to_string(),
                    model_id: "glm-5.3".to_string(),
                    routing: ModelRouting {
                        cost_weight: 0.25,
                        quality_weight: 0.85,
                    },
                    pricing: None,
                },
            )
            .unwrap();
        config
    }

    fn listing_of(ids: &[&str]) -> Listing {
        Listing {
            models: ids
                .iter()
                .map(|id| DiscoveredModel {
                    id: id.to_string(),
                    display_name: None,
                    owned_by: None,
                })
                .collect(),
            ..Listing::default()
        }
    }

    #[test]
    fn plan_skips_registered_pairs_and_aliases_the_rest() {
        let config = scratch_config();
        let listing = listing_of(&["glm-5.3", "glm-5.3-air", "MiniMax/M3"]);
        let plans = plan_registration(&config, "mockz", &listing, &[0, 1, 2]);

        assert_eq!(
            plans[0].1,
            RegAction::Skip {
                existing_alias: "glm".to_string()
            },
            "same (provider, model_id) is skipped, never overwritten"
        );
        assert_eq!(
            plans[1].1,
            RegAction::Register {
                alias: "glm-5-3-air".to_string()
            }
        );
        assert_eq!(
            plans[2].1,
            RegAction::Register {
                alias: "MiniMax-M3".to_string()
            },
            "'/' normalizes to '-'"
        );
    }

    #[test]
    fn plan_suffixed_aliases_within_one_run_when_ids_collide() {
        let config = scratch_config();
        // Both ids normalize to the same alias; the second gets -2.
        let listing = listing_of(&["glm.5", "glm:5"]);
        let plans = plan_registration(&config, "mockz", &listing, &[0, 1]);
        assert_eq!(
            plans[0].1,
            RegAction::Register {
                alias: "glm-5".to_string()
            }
        );
        assert_eq!(
            plans[1].1,
            RegAction::Register {
                alias: "glm-5-2".to_string()
            }
        );
    }

    #[test]
    fn pricing_skeleton_names_the_alias_and_advise_journey() {
        let skeleton = pricing_skeleton("glm-5-3");
        assert!(
            skeleton.contains("# [models.glm-5-3.pricing]"),
            "{skeleton}"
        );
        assert!(skeleton.contains("input = 0.0"));
        assert!(skeleton.contains("cache_write = 0.0"));
        // Every line commented: pasting the block verbatim is harmless.
        assert!(skeleton.lines().all(|line| line.starts_with('#')));
    }

    // ------------------------------------------------------------------
    // The wire-level contract against live axum listeners
    // ------------------------------------------------------------------

    #[derive(Clone)]
    enum ModelsBehavior {
        Json(serde_json::Value),
        Status(StatusCode),
        /// Answers with the caller's own x-api-key echoed into the body —
        /// the credential-hygiene adversarial shape.
        EchoAuth(StatusCode),
        /// Answers 302 + Location to the given URL — the redirect shape.
        Redirect(String),
        EndlessPagination {
            page_ids: Vec<Vec<&'static str>>,
        },
    }

    #[derive(Clone, Default)]
    struct ModelsState {
        behavior: Option<ModelsBehavior>,
        seen_auth: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        seen_authz: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        seen_version: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
        pages_served: std::sync::Arc<std::sync::Mutex<usize>>,
    }

    async fn mock_models(State(state): State<ModelsState>, headers: HeaderMap) -> Response {
        *state.pages_served.lock().unwrap() += 1;
        let auth = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        state.seen_auth.lock().unwrap().push(auth.clone());
        let authz = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        state.seen_authz.lock().unwrap().push(authz);
        let version = headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        state.seen_version.lock().unwrap().push(version);
        match state.behavior.clone().expect("behavior set") {
            ModelsBehavior::Json(value) => (StatusCode::OK, Json(value)).into_response(),
            ModelsBehavior::Status(status) => {
                (status, Json(json!({"error": {"message": "nope"}}))).into_response()
            }
            ModelsBehavior::EchoAuth(status) => (
                status,
                Json(
                    json!({"error": {"message": format!("upstream rejected: x-api-key: {auth}")}}),
                ),
            )
                .into_response(),
            ModelsBehavior::Redirect(location) => {
                (StatusCode::FOUND, [("location", location)], Json(json!({}))).into_response()
            }
            ModelsBehavior::EndlessPagination { page_ids } => {
                let page = *state.pages_served.lock().unwrap() - 1;
                let page = page.min(page_ids.len() - 1);
                let ids = &page_ids[page];
                let body = json!({
                    "data": ids.iter().map(|id| json!({"id": id, "display_name": id})).collect::<Vec<_>>(),
                    "has_more": true,
                    "last_id": ids.last(),
                });
                (StatusCode::OK, Json(body)).into_response()
            }
        }
    }

    async fn spawn_models_mock(
        behavior: ModelsBehavior,
    ) -> (
        std::net::SocketAddr,
        ModelsState,
        tokio::task::JoinHandle<()>,
    ) {
        let state = ModelsState {
            behavior: Some(behavior),
            ..ModelsState::default()
        };
        let app = Router::new()
            .route("/v1/models", get(mock_models))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, state, handle)
    }

    fn scratch_home(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("ccm-discover-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// The full list-and-register contract against live listeners,
    /// including the z.ai bare shape, skip-not-overwrite, the
    /// degradation paths, and the pagination cap.
    // The env mutex guard is held across awaits on purpose (proxy.rs's
    // v0.3-test discipline, extended here): it serializes this test
    // against every other env-mutating test in the crate. Each test owns
    // its current-thread runtime, so nothing on that runtime contends
    // for the lock.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn discover_covers_listing_registration_and_degradation() {
        let guard = env_guard();
        let root = scratch_home("wire");
        std::env::set_var("CCM_HOME", &root);
        std::env::set_var("CCM_MOCKZ_API_KEY", "mockz-secret");
        std::env::remove_var("CCM_DEADZ_API_KEY");

        let mut config = scratch_config();
        config.providers.get_mut("mockz").unwrap().auth = None;

        // -- z.ai bare shape: list + --all registration -------------------
        let (addr, state, mock) = spawn_models_mock(ModelsBehavior::Json(json!({
            "data": [
                {"id": "glm-5.3", "type": "model", "display_name": "GLM-5.3"},
                {"id": "glm-5.3-air", "type": "model", "display_name": "GLM-5.3-Air"},
                {"id": "glm-4.5", "type": "model", "display_name": "GLM-4.5"}
            ]
        })))
        .await;
        config.providers.get_mut("mockz").unwrap().base_url = format!("http://{addr}");
        config.save().unwrap();

        run(&mut config, Some("mockz".to_string()), true)
            .await
            .unwrap();

        // Registration state is exactly what `ccm list` and /_ccm/models
        // serve (both read config.models).
        let glm = &config.models["glm"];
        assert_eq!(glm.model_id, "glm-5.3", "existing entry untouched");
        assert_eq!(glm.routing.cost_weight, 0.25, "hand-tuned weight survives");
        assert!(config.models.contains_key("glm-5-3-air"));
        assert_eq!(config.models["glm-5-3-air"].model_id, "glm-5.3-air");
        assert_eq!(config.models["glm-5-3-air"].provider, "mockz");
        assert_eq!(config.models["glm-4-5"].model_id, "glm-4.5", "'.' -> '-'");
        assert_eq!(
            config.models.len(),
            3,
            "glm + the two newly discovered (glm-5.3 itself was a skip)"
        );
        assert!(
            config.models.values().all(|m| m.pricing.is_none()),
            "prices are hand-entered, never CLI defaults"
        );

        // The saved config on disk matches (the ccm list of a new shell).
        let reloaded = AppConfig::load().unwrap();
        assert!(reloaded.models.contains_key("glm-4-5"));

        // Auth really happened, and only as a header.
        let seen = state.seen_auth.lock().unwrap().clone();
        assert!(
            seen.iter().all(|v| v == "mockz-secret"),
            "x-api-key applied"
        );
        // The kind-keyed version header is pinned at the wire level
        // (verify-pass fix: the stale comment used to claim a pinning no
        // test performed).
        let versions = state.seen_version.lock().unwrap().clone();
        assert!(
            versions.iter().all(|v| v.as_deref() == Some("2023-06-01")),
            "anthropic kinds carry anthropic-version: {versions:?}"
        );

        // -- re-run: everything now registered, nothing overwritten ------
        let mut before = config.clone();
        before
            .models
            .get_mut("glm-5-3-air")
            .unwrap()
            .routing
            .cost_weight = 0.5;
        before.save().unwrap();
        let mut rerun = AppConfig::load().unwrap();
        run(&mut rerun, Some("mockz".to_string()), true)
            .await
            .unwrap();
        assert_eq!(rerun.models.len(), 3, "no duplicates on re-run");
        assert_eq!(
            rerun.models["glm-5-3-air"].routing.cost_weight, 0.5,
            "skip-not-overwrite: the re-run kept the tuned weight"
        );
        mock.abort();

        // -- 404: the manual onboarding path ------------------------------
        let (addr, _state, mock) =
            spawn_models_mock(ModelsBehavior::Status(StatusCode::NOT_FOUND)).await;
        let mut config = scratch_config();
        config.providers.get_mut("mockz").unwrap().base_url = format!("http://{addr}");
        let error = run(&mut config, Some("mockz".to_string()), true)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("does not expose /v1/models"), "{text}");
        assert!(text.contains("manually"), "{text}");
        assert!(config.models.len() == 1, "nothing registered on 404");
        mock.abort();

        // -- 401: the health-style auth failure ---------------------------
        let (addr, _state, mock) =
            spawn_models_mock(ModelsBehavior::Status(StatusCode::UNAUTHORIZED)).await;
        let mut config = scratch_config();
        config.providers.get_mut("mockz").unwrap().base_url = format!("http://{addr}");
        let error = run(&mut config, Some("mockz".to_string()), true)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("authentication failed (401"), "{text}");
        mock.abort();

        // -- 200 + error body: z.ai's bad-key shape (live calibration) ---
        // The HTTP status is a success; only the missing `data` member
        // betrays that this is not a listing. The error must carry the
        // gateway's own words, and nothing may be registered.
        let (addr, _state, mock) = spawn_models_mock(ModelsBehavior::Json(json!({
            "code": 401, "msg": "token expired or incorrect", "success": false
        })))
        .await;
        let mut config = scratch_config();
        config.providers.get_mut("mockz").unwrap().base_url = format!("http://{addr}");
        let error = run(&mut config, Some("mockz".to_string()), true)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("not a models list"), "{text}");
        assert!(
            text.contains("token expired or incorrect"),
            "the gateway's message travels: {text}"
        );
        assert!(config.models.len() == 1, "nothing registered");
        mock.abort();

        // -- an explicitly empty data array is a legitimate empty listing --
        // The early return ("gateway listed 0 models; nothing to register")
        // must fire before selection — even --all skips the prompt path.
        let (addr, _state, mock) =
            spawn_models_mock(ModelsBehavior::Json(json!({"data": []}))).await;
        let mut config = scratch_config();
        config.providers.get_mut("mockz").unwrap().base_url = format!("http://{addr}");
        run(&mut config, Some("mockz".to_string()), true)
            .await
            .unwrap();
        assert_eq!(config.models.len(), 1, "an empty listing registers nothing");
        mock.abort();

        // -- credential hygiene: a gateway echoing the just-sent key back
        //    in its error body must not get it printed (invariant 1) ----
        let (addr, _state, mock) =
            spawn_models_mock(ModelsBehavior::EchoAuth(StatusCode::BAD_GATEWAY)).await;
        let mut config = scratch_config();
        config.providers.get_mut("mockz").unwrap().base_url = format!("http://{addr}");
        let error = run(&mut config, Some("mockz".to_string()), true)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("502"), "{text}");
        assert!(
            !text.contains("mockz-secret"),
            "the echoed credential is scrubbed from the error: {text}"
        );
        assert!(text.contains("***"), "{text}");
        mock.abort();

        // The same echo inside a 200 in-body error (the z.ai shape).
        let (addr, _state, mock) =
            spawn_models_mock(ModelsBehavior::EchoAuth(StatusCode::OK)).await;
        let mut config = scratch_config();
        config.providers.get_mut("mockz").unwrap().base_url = format!("http://{addr}");
        let error = run(&mut config, Some("mockz".to_string()), true)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("not a models list"), "{text}");
        assert!(!text.contains("mockz-secret"), "{text}");
        mock.abort();

        // -- a redirecting gateway: the credential is keyed to the
        //    configured host — no cross-host re-send, and the 302
        //    surfaces as its own status ---------------------------------
        let (target_addr, target_state, target_mock) = spawn_models_mock(ModelsBehavior::Json(
            json!({"data": [{"id": "redirect-bait"}]}),
        ))
        .await;
        let (addr, _state, mock) = spawn_models_mock(ModelsBehavior::Redirect(format!(
            "http://{target_addr}/v1/models"
        )))
        .await;
        let mut config = scratch_config();
        config.providers.get_mut("mockz").unwrap().base_url = format!("http://{addr}");
        let error = run(&mut config, Some("mockz".to_string()), true)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("302"),
            "the redirect surfaces as its status: {text}"
        );
        assert_eq!(
            *target_state.pages_served.lock().unwrap(),
            0,
            "no request followed the redirect — the key never left the configured host"
        );
        mock.abort();
        target_mock.abort();

        // -- persist first, report second: a failed save must propagate
        //    BEFORE any "Saved" line could be printed --------------------
        let (addr, _state, mock) = spawn_models_mock(ModelsBehavior::Json(json!({
            "data": [{"id": "unsavable"}]
        })))
        .await;
        let mut config = scratch_config();
        config.providers.get_mut("mockz").unwrap().base_url = format!("http://{addr}");
        // config.toml as a DIRECTORY: the fs::write inside save() fails.
        let config_path = root.join("config.toml");
        std::fs::remove_file(&config_path).unwrap();
        std::fs::create_dir_all(&config_path).unwrap();
        let error = run(&mut config, Some("mockz".to_string()), true)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot write"),
            "the save failure propagates: {error:#}"
        );
        mock.abort();
        let _ = std::fs::remove_dir_all(&config_path);

        // -- pagination: cap at MAX_PAGES with a truncation fact ----------
        let (addr, state, mock) = spawn_models_mock(ModelsBehavior::EndlessPagination {
            page_ids: vec![vec!["page-one-model"], vec!["page-two-model"]],
        })
        .await;
        let mut config = scratch_config();
        config.providers.get_mut("mockz").unwrap().base_url = format!("http://{addr}");
        // --all registers everything the capped walk collected.
        run(&mut config, Some("mockz".to_string()), true)
            .await
            .unwrap();
        assert_eq!(
            *state.pages_served.lock().unwrap(),
            MAX_PAGES,
            "an always-has_more gateway stops at the cap"
        );
        assert!(
            config.models.contains_key("page-two-model"),
            "follow-up pages after_id were fetched and registered"
        );
        mock.abort();

        // -- missing credential: the existing env-name + auth set message --
        let mut config = scratch_config();
        config.add_provider(
            "deadz".to_string(),
            Provider {
                kind: ProviderKind::AnthropicCompatible,
                base_url: "http://127.0.0.1:9".to_string(),
                auth: None,
            },
        );
        let error = run(&mut config, Some("deadz".to_string()), true)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("CCM_DEADZ_API_KEY"), "{text}");
        assert!(text.contains("ccm auth set deadz"), "{text}");

        // -- unknown provider names the configured set --------------------
        let error = run(&mut config, Some("nope".to_string()), true)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown provider `nope`"));

        // -- an empty providers map: no dangling "configured providers: "
        let error = run(&mut AppConfig::default(), Some("x".to_string()), true)
            .await
            .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("no providers configured"),
            "the empty-map remediation message: {text}"
        );

        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_MOCKZ_API_KEY");
        std::env::remove_var("CCM_DEADZ_API_KEY");
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[allow(clippy::await_holding_lock)] // see note on the wire test above
    #[tokio::test]
    async fn openai_shape_registers_on_an_openai_kind() {
        let guard = env_guard();
        let root = scratch_home("openai");
        std::env::set_var("CCM_HOME", &root);
        std::env::set_var("CCM_DS_API_KEY", "ds-secret");

        let (addr, state, mock) = spawn_models_mock(ModelsBehavior::Json(json!({
            "object": "list",
            "data": [
                {"id": "deepseek-chat", "object": "model", "owned_by": "deepseek"},
                {"id": "deepseek-reasoner", "object": "model", "owned_by": "deepseek"}
            ]
        })))
        .await;

        let mut config = AppConfig::default();
        config.add_provider(
            "ds".to_string(),
            Provider {
                kind: ProviderKind::OpenAICompatible,
                base_url: format!("http://{addr}"),
                auth: None,
            },
        );
        run(&mut config, Some("ds".to_string()), true)
            .await
            .unwrap();
        assert_eq!(config.models.len(), 2);
        assert_eq!(config.models["deepseek-chat"].model_id, "deepseek-chat");
        // The kind-keyed auth, pinned at the header level (verify-pass
        // fix: the old assertion was vacuous — an empty x-api-key capture
        // passes even when NO auth header at all is sent).
        let authz = state.seen_authz.lock().unwrap().clone();
        assert_eq!(
            authz,
            vec!["Bearer ds-secret".to_string()],
            "openai-compatible kind authenticates with bearer"
        );
        let seen = state.seen_auth.lock().unwrap().clone();
        assert!(
            seen.iter().all(|v| v.is_empty()),
            "bearer kind sends no x-api-key"
        );
        let versions = state.seen_version.lock().unwrap().clone();
        assert!(
            versions.iter().all(|v| v.is_none()),
            "the version header is anthropic-kinds only: {versions:?}"
        );
        mock.abort();

        std::env::remove_var("CCM_HOME");
        std::env::remove_var("CCM_DS_API_KEY");
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    // Rendering is pure display: the extras appear beside the id, and no
    // render path has a credential to leak (invariant 1). Wire-level
    // header pinning (x-api-key / bearer / anthropic-version) lives in
    // the integration tests above.
    #[test]
    fn render_listing_line_shows_extras_without_credentials() {
        let line = render_listing_line(
            0,
            &DiscoveredModel {
                id: "glm-5.3".to_string(),
                display_name: Some("GLM-5.3".to_string()),
                owned_by: None,
            },
        );
        assert_eq!(line, "   1. glm-5.3  GLM-5.3");
        let line = render_listing_line(
            2,
            &DiscoveredModel {
                id: "d".to_string(),
                display_name: None,
                owned_by: Some("deepseek".to_string()),
            },
        );
        assert_eq!(line, "   3. d  (owned by deepseek)");
        let line = render_listing_line(
            9,
            &DiscoveredModel {
                id: "x".to_string(),
                display_name: None,
                owned_by: None,
            },
        );
        assert_eq!(line, "  10. x");
    }
}
