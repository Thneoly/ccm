//! `ccm advise` (v0.5 M1): advise-only `cost_weight` derivation from real
//! usage (V0.5_PLAN §3.1).
//!
//! The gap it closes: `cost_weight` is a hand-entered prior, and v0.4 M6
//! already persists per-request four-way tokens — but nothing told the
//! owner how far their weights drifted from realized spend, or what to
//! change them to. This module recomputes per-model realized USD over a
//! usage window at CURRENT prices — not the embedded record snapshots
//! `aggregate_costs` sums, which would mix price regions across a
//! price-table edit — and normalizes: the most-expensive analyzed
//! candidate maps to 1.0, every other candidate scales by realized
//! USD/request relative to it (the starter convention, where claude = 1.0
//! is the priciest entry).
//!
//! Honesty rails — everything printed, nothing written:
//! - NEVER writes config; the TOML fragment is paste-only.
//! - unpriced models: `cost unknown — never guessed`, with the row naming
//!   the `[models.<alias>.pricing]` edit to make them priceable (v0.5 M5;
//!   `ccm discover` prints the skeleton for exactly this journey).
//! - below `--min-samples`: `insufficient — unchanged` (a thin sample is
//!   not evidence, however expensive it looked).
//! - incomplete records (client disconnects, transport errors, upstream
//!   error frames) are counted but excluded from the fold — their token
//!   counts may be partial.
//! - per-token columns mix each provider's own tokenizer; selection bias
//!   (usage records only what was routed) and not-bill-truth are stated in
//!   every report.
//!
//! The derivation is a pure function (records + current pricing →
//! suggestions) so a future runtime variant can reuse it verbatim.

use std::{collections::BTreeMap, path::Path};

use anyhow::{bail, Context, Result};

use crate::config::AppConfig;
use crate::history::{self, UsageQuery};
use crate::usage::UsageRecord;

/// Registered defaults (V0.5_PLAN §8.2): window 7 days; min-samples 20 —
/// cost variance is far larger than success rate's, so the floor is higher
/// than the health rank's 3 (`select.rs` HEALTH_MIN_SAMPLES).
pub(crate) const DEFAULT_WINDOW_DAYS: u64 = 7;
pub(crate) const DEFAULT_MIN_SAMPLES: u64 = 20;

const MS_PER_DAY: u64 = 86_400_000;

pub(crate) struct AdviseOptions {
    pub(crate) window_days: u64,
    pub(crate) min_samples: u64,
    pub(crate) model: Option<String>,
}

pub(crate) fn run(options: &AdviseOptions) -> Result<()> {
    let config = AppConfig::load().context("failed to load CCM config")?;
    let dir = AppConfig::history_dir()?;
    let report = run_at(options, &config, &dir, crate::date::now_ms())?;
    print!("{}", render_report(&report));
    Ok(())
}

/// Resolve the window, read the records, derive the report. Split from
/// `run` so tests drive it with an explicit config, directory, and clock
/// (the `history_cli::run_at` shape).
pub(crate) fn run_at(
    options: &AdviseOptions,
    config: &AppConfig,
    dir: &Path,
    now: u64,
) -> Result<AdviseReport> {
    if !dir.is_dir() {
        bail!(
            "no history directory at {} — the proxy writes it when [observability] history_enabled is on (the default)",
            dir.display()
        );
    }
    if let Some(model) = &options.model {
        if !config.models.contains_key(model) {
            bail!("unknown model `{model}` — advise analyzes configured model aliases");
        }
    }
    let window_ms = options
        .window_days
        .checked_mul(MS_PER_DAY)
        .context("analysis window too large")?;
    let since = now.saturating_sub(window_ms);
    let query = UsageQuery {
        since: Some(since),
        until: None,
        model: options.model.clone(),
        client: None,
        // No limit: a suggestion must see the whole window, not the newest
        // slice of it (the `/_ccm/cost` precedent).
        limit: None,
    };
    let records = history::read_usage(dir, &query)?;
    Ok(analyze(
        &records,
        config,
        options.min_samples,
        since,
        options.window_days,
    ))
}

// ===========================================================================
// Pure derivation
// ===========================================================================

/// One model's row: counts plus the folded totals and the verdict.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AdviseRow {
    pub(crate) model: String,
    /// All records in the window for this model.
    pub(crate) requests: u64,
    /// Records excluded from the fold: not `complete` (client disconnect,
    /// transport error, upstream error frame) — token counts may be
    /// partial. Counted, never folded.
    pub(crate) incomplete: u64,
    /// Complete records folded at CURRENT prices. Equals
    /// `requests - incomplete` when the model has a pricing table, 0
    /// otherwise.
    pub(crate) analyzed: u64,
    /// Realized USD over the `analyzed` records at current prices.
    pub(crate) total_usd: f64,
    /// input + output + cache_read + cache_write over the `analyzed`
    /// records.
    pub(crate) total_tokens: u64,
    /// The model's current static `cost_weight`; `None` when the model is
    /// no longer in config.toml.
    pub(crate) current_weight: Option<f64>,
    /// The model has records but no config entry anymore — its pricing
    /// cannot even be looked up.
    pub(crate) missing_from_config: bool,
    pub(crate) verdict: Verdict,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Verdict {
    /// Analyzed with enough samples. `suggestion` is the rounded suggested
    /// cost_weight (anchor = 1.0); `score` is the 1/(1+w) cost score that
    /// suggestion implies (computed from the ROUNDED value — the number
    /// the owner would actually enter).
    Analyzed { suggestion: f64, score: f64 },
    /// Priced, but fewer than `min_samples` analyzed records.
    Insufficient,
    /// No current pricing table (or no config entry): cost is unknown,
    /// never guessed.
    Unpriced,
}

/// The anchor: the most-expensive analyzed candidate (≥ min-samples).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Anchor {
    pub(crate) model: String,
    pub(crate) usd_per_request: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct AdviseReport {
    /// One row per model with at least one record in the window, sorted by
    /// model name (the `aggregate_costs` convention). Configured models
    /// with no traffic are absent — the selection-bias caveat covers them.
    pub(crate) rows: Vec<AdviseRow>,
    pub(crate) since_ms: u64,
    pub(crate) window_days: u64,
    pub(crate) min_samples: u64,
    /// `[observability] retention_days` — the honest horizon of the
    /// analysis: rotated history files older than this are already
    /// deleted, so a window wider than it folds only what is still on
    /// disk. Rendered as a note when `window_days` exceeds it.
    pub(crate) retention_days: u64,
    pub(crate) anchor: Option<Anchor>,
}

/// Per-model accumulation during the fold.
#[derive(Default)]
struct Fold {
    requests: u64,
    incomplete: u64,
    analyzed: u64,
    total_usd: f64,
    total_tokens: u64,
}

/// The pure derivation: records + current pricing → suggestions. Complete
/// records of priced models fold at the CURRENT `[models.<name>.pricing]`
/// table; the embedded `pricing`/`cost_usd` snapshots on the records are
/// ignored (they reflect the prices in force when each request was served).
pub(crate) fn analyze(
    records: &[UsageRecord],
    config: &AppConfig,
    min_samples: u64,
    since_ms: u64,
    window_days: u64,
) -> AdviseReport {
    let mut folds: BTreeMap<String, Fold> = BTreeMap::new();
    for record in records {
        let fold = folds.entry(record.model.clone()).or_default();
        fold.requests += 1;
        if !record.complete {
            fold.incomplete += 1;
            continue;
        }
        let Some(pricing) = config
            .models
            .get(&record.model)
            .and_then(|model| model.pricing.as_ref())
        else {
            // Unpriced (or no longer configured): counted, never folded —
            // never a guessed price.
            continue;
        };
        fold.analyzed += 1;
        fold.total_usd += pricing.cost_usd(
            record.input_tokens,
            record.output_tokens,
            record.cache_read_tokens,
            record.cache_write_tokens,
        );
        fold.total_tokens += record.input_tokens
            + record.output_tokens
            + record.cache_read_tokens
            + record.cache_write_tokens;
    }

    // Anchor first: the highest realized USD/request among candidates with
    // enough analyzed samples. An expensive-but-thin model is not evidence,
    // so it cannot anchor (pinned by test). Ties keep the first model in
    // name order — deterministic.
    let floor = min_samples.max(1);
    let mut anchor: Option<Anchor> = None;
    for (model, fold) in &folds {
        if fold.analyzed >= floor {
            let rate = fold.total_usd / fold.analyzed as f64;
            let better = match &anchor {
                Some(current) => rate > current.usd_per_request,
                None => true,
            };
            if better {
                anchor = Some(Anchor {
                    model: model.clone(),
                    usd_per_request: rate,
                });
            }
        }
    }

    let rows = folds
        .into_iter()
        .map(|(model, fold)| {
            let configured = config.models.get(&model);
            let priced = configured.is_some_and(|model| model.pricing.is_some());
            let qualifies = fold.analyzed >= floor;
            let verdict = if qualifies {
                // `anchor` is Some whenever any fold qualified, and this
                // fold just did.
                let anchor_rate = anchor
                    .as_ref()
                    .expect("anchor exists whenever a row qualifies")
                    .usd_per_request;
                let rate = fold.total_usd / fold.analyzed as f64;
                let suggestion = if anchor_rate > 0.0 {
                    round3((rate / anchor_rate).max(0.0))
                } else {
                    // Every candidate realized $0.000000/request at
                    // current prices — equally free, so every suggestion
                    // is 0.0 (the max cost score); stated in the report.
                    0.0
                };
                Verdict::Analyzed {
                    suggestion,
                    score: 1.0 / (1.0 + suggestion),
                }
            } else if priced {
                Verdict::Insufficient
            } else {
                Verdict::Unpriced
            };
            AdviseRow {
                current_weight: configured.map(|model| model.routing.cost_weight),
                missing_from_config: configured.is_none(),
                model,
                requests: fold.requests,
                incomplete: fold.incomplete,
                analyzed: fold.analyzed,
                total_usd: fold.total_usd,
                total_tokens: fold.total_tokens,
                verdict,
            }
        })
        .collect();

    AdviseReport {
        rows,
        since_ms,
        window_days,
        min_samples,
        retention_days: config.observability.retention_days,
        anchor,
    }
}

/// Round to 3 decimals — the precision the table, the TOML fragment, and
/// the implied score all agree on.
fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

// ===========================================================================
// Rendering
// ===========================================================================

/// The full report as printed text. Pure so tests pin every honesty rail.
pub(crate) fn render_report(report: &AdviseReport) -> String {
    let mut out = String::new();
    out.push_str("== ccm advise — cost_weight suggestions from realized spend\n");
    out.push_str(&format!(
        "   window: last {} day(s) (since {}), min-samples: {}, prices: current [models.<name>.pricing] tables\n",
        report.window_days,
        crate::date::utc_instant(report.since_ms),
        report.min_samples,
    ));
    if report.window_days > report.retention_days {
        out.push_str(&format!(
            "note: requested window ({} day(s)) exceeds history retention ({} day(s)) — rotated files older than retention are already deleted; the fold sees only what is still on disk\n",
            report.window_days, report.retention_days
        ));
    }
    if report.rows.is_empty() {
        out.push_str(&format!(
            "no usage records in the last {} day(s) — nothing to analyze\n",
            report.window_days
        ));
        // The caveats ride on EVERY report — an empty table is exactly
        // when the selection-bias caveat explains why configured models
        // show no data (verified-pass finding; pinned by test).
        out.push_str(CAVEATS_JOIN);
        for caveat in caveats() {
            out.push_str(&format!("- {caveat}\n"));
        }
        return out;
    }
    out.push_str(&format!(
        "{:<20} {:>5} {:>10} {:>8} {:>10} {:>9} {:>6} {:>7} {:>6}  {}\n",
        "model",
        "reqs",
        "incomplete",
        "analyzed",
        "usd/req",
        "usd/1ktok",
        "weight",
        "suggest",
        "score",
        "status"
    ));
    for row in &report.rows {
        out.push_str(table_row(row, report.min_samples).trim_end());
        out.push('\n');
    }

    let analyzed_count = report
        .rows
        .iter()
        .filter(|row| matches!(row.verdict, Verdict::Analyzed { .. }))
        .count();
    match &report.anchor {
        Some(anchor) if anchor.usd_per_request > 0.0 => {
            out.push_str(&format!(
                "anchor: {} — highest realized usd/req among analyzed candidates, maps to 1.0\n",
                anchor.model
            ));
        }
        Some(_) => {
            out.push_str(
                "note: every analyzed model realized $0.000000/request at current prices — all suggestions are 0.0 (equally free)\n",
            );
        }
        None => {
            out.push_str(
                "no analyzed candidates — every model in the window is unpriced or below min-samples; nothing to suggest\n",
            );
        }
    }
    if analyzed_count == 1 {
        out.push_str(
            "note: one analyzed model — its suggestion is trivially 1.0 (the anchor); relative weights need >= 2 analyzed candidates\n",
        );
    }
    out.push_str(&toml_fragment(&report.rows));
    out.push_str(CAVEATS_JOIN);
    for caveat in caveats() {
        out.push_str(&format!("- {caveat}\n"));
    }
    out
}

/// One table row. Pure so tests pin the three-way status language.
fn table_row(row: &AdviseRow, min_samples: u64) -> String {
    let usd_req = if row.analyzed > 0 {
        format!("{:.6}", row.total_usd / row.analyzed as f64)
    } else {
        "-".to_string()
    };
    let usd_ktok = if row.total_tokens > 0 {
        format!("{:.6}", row.total_usd / row.total_tokens as f64 * 1000.0)
    } else {
        "-".to_string()
    };
    let weight = row
        .current_weight
        .map(|weight| format!("{weight:.3}"))
        .unwrap_or_else(|| "-".to_string());
    let (suggest, score, status) = match &row.verdict {
        Verdict::Analyzed { suggestion, score } => (
            format!("{suggestion:.3}"),
            format!("{score:.3}"),
            String::new(),
        ),
        Verdict::Insufficient => (
            "-".to_string(),
            "-".to_string(),
            format!(
                "insufficient — unchanged ({} < {})",
                row.analyzed, min_samples
            ),
        ),
        Verdict::Unpriced => (
            "-".to_string(),
            "-".to_string(),
            if row.missing_from_config {
                "cost unknown — never guessed (model not in config.toml)".to_string()
            } else {
                // Names the config edit (v0.5 M5): no pricing CLI flag
                // exists by policy, so the message points at the TOML
                // table the owner must hand-enter.
                format!(
                    "cost unknown — never guessed (add a [models.{}.pricing] table)",
                    row.model
                )
            },
        ),
    };
    format!(
        "{:<20} {:>5} {:>10} {:>8} {:>10} {:>9} {:>6} {:>7} {:>6}  {status}",
        row.model,
        row.requests,
        row.incomplete,
        row.analyzed,
        usd_req,
        usd_ktok,
        weight,
        suggest,
        score
    )
}

/// The paste-only TOML fragment for analyzed models. Empty when nothing
/// was analyzed.
fn toml_fragment(rows: &[AdviseRow]) -> String {
    let analyzed = rows
        .iter()
        .filter(|row| matches!(row.verdict, Verdict::Analyzed { .. }));
    let mut out = String::new();
    let mut any = false;
    for row in analyzed {
        if !any {
            out.push_str(
                "# suggested cost_weight values — paste into config.toml (ccm advise never writes config)\n",
            );
            any = true;
        }
        let Verdict::Analyzed { suggestion, .. } = row.verdict else {
            continue;
        };
        // Bare keys for the common alias charset; quoted for anything else
        // (a dotted alias must not split into nested tables).
        let key = if row
            .model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            format!("[models.{}.routing]", row.model)
        } else {
            format!("[models.\"{}\".routing]", row.model)
        };
        out.push_str(&format!("\n{key}\ncost_weight = {suggestion:.3}\n"));
    }
    out
}

/// The honesty rails printed with every report (V0.5_PLAN §3.1: selection
/// bias and not-bill-truth are stated in USAGE — and once per report).
fn caveats() -> &'static [&'static str] {
    &[
        "token counts come from each provider's own usage frames — per-token numbers are not comparable across protocol families",
        "usage records only what was routed: models the router never picked have no data here (selection bias)",
        "realized cost is recomputed at your hand-entered prices — proxy-side accounting, not bill truth",
        "quality_weight stays manual — no honest derivation exists from proxy-visible signals",
    ]
}

const CAVEATS_JOIN: &str = "caveats:\n";

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelPricing;

    fn pricing(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input,
            output,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }

    fn record(model: &str, input: u64, output: u64, complete: bool) -> UsageRecord {
        UsageRecord {
            decision_id: 1,
            timestamp_ms: 0,
            model: model.to_string(),
            client: None,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            complete,
            pricing: None,
            cost_usd: None,
        }
    }

    /// starter config with claude/glm priced and a third model added.
    fn config_with_prices() -> AppConfig {
        let mut config = AppConfig::starter();
        config.models.get_mut("claude").unwrap().pricing = Some(pricing(3.0, 15.0));
        config.models.get_mut("glm").unwrap().pricing = Some(pricing(3.0, 15.0));
        config
    }

    fn row_of<'a>(report: &'a AdviseReport, model: &str) -> &'a AdviseRow {
        report
            .rows
            .iter()
            .find(|row| row.model == model)
            .unwrap_or_else(|| panic!("no row for {model}"))
    }

    // 1. the recompute fold: CURRENT prices win over the embedded record
    //    snapshots (the price-region-mixing trap in `aggregate_costs`)
    #[test]
    fn fold_recomputes_at_current_prices_not_embedded_snapshots() {
        let config = config_with_prices();
        // claude: 20 records of 100k in / 1k out at current 3.0/15.0
        //   → per request (100000×3 + 1000×15)/1e6 = 0.315 USD
        // glm: 20 records of 10k in / 200 out
        //   → per request (10000×3 + 200×15)/1e6 = 0.033 USD
        let mut records: Vec<UsageRecord> = Vec::new();
        for _ in 0..20 {
            let mut claude = record("claude", 100_000, 1_000, true);
            // Embedded snapshot from an OLD, doubled price table — the
            // recompute fold must ignore it (embedded would say 0.63).
            claude.pricing = Some(pricing(6.0, 30.0));
            claude.cost_usd = Some(0.63);
            records.push(claude);
            records.push(record("glm", 10_000, 200, true));
        }
        let report = analyze(&records, &config, 20, 0, 7);
        let claude = row_of(&report, "claude");
        assert_eq!(claude.analyzed, 20);
        assert!(
            (claude.total_usd - 20.0 * 0.315).abs() < 1e-9,
            "current prices"
        );
        let glm = row_of(&report, "glm");
        assert!((glm.total_usd - 20.0 * 0.033).abs() < 1e-9);
        // anchor = claude (most expensive analyzed) → 1.0;
        // glm suggestion = 0.033/0.315 = 0.104762… → rounded 0.105
        assert_eq!(report.anchor.as_ref().unwrap().model, "claude");
        match &glm.verdict {
            Verdict::Analyzed { suggestion, score } => {
                assert!((suggestion - 0.105).abs() < 1e-12);
                // the score is computed from the ROUNDED suggestion — the
                // number the owner would actually enter
                assert!((score - 1.0 / 1.105).abs() < 1e-12);
            }
            other => panic!("glm should be analyzed: {other:?}"),
        }
        match &claude.verdict {
            Verdict::Analyzed { suggestion, score } => {
                assert!((suggestion - 1.0).abs() < 1e-12);
                assert!((score - 0.5).abs() < 1e-12);
            }
            other => panic!("claude should be analyzed: {other:?}"),
        }
    }

    // 2. an expensive-but-thin model cannot anchor: below min-samples it
    //    gets `insufficient — unchanged`, not the 1.0 anchor slot
    #[test]
    fn expensive_but_thin_model_is_insufficient_and_cannot_anchor() {
        let config = config_with_prices();
        let mut records: Vec<UsageRecord> = Vec::new();
        for _ in 0..20 {
            records.push(record("claude", 10_000, 100, true)); // cheap, plenty
        }
        for _ in 0..3 {
            records.push(record("glm", 1_000_000, 10_000, true)); // far pricier, thin
        }
        let report = analyze(&records, &config, 20, 0, 7);
        assert_eq!(report.anchor.as_ref().unwrap().model, "claude");
        let glm = row_of(&report, "glm");
        assert_eq!(glm.verdict, Verdict::Insufficient);
        // its realized numbers still show — honest, just not a suggestion
        assert_eq!(glm.analyzed, 3);
    }

    // 3. unpriced and no-longer-configured models: counted, never folded,
    //    never guessed
    #[test]
    fn unpriced_and_missing_models_are_counted_never_folded() {
        let mut config = config_with_prices();
        config.models.get_mut("glm").unwrap().pricing = None; // unpriced
        let mut records: Vec<UsageRecord> = Vec::new();
        for _ in 0..30 {
            records.push(record("glm", 100_000, 1_000, true));
            records.push(record("ghost", 5_000, 5_000, true)); // not in config
        }
        let report = analyze(&records, &config, 20, 0, 7);
        let glm = row_of(&report, "glm");
        assert_eq!(glm.verdict, Verdict::Unpriced);
        assert_eq!(glm.analyzed, 0);
        assert_eq!(glm.total_usd, 0.0);
        assert!(!glm.missing_from_config);
        let ghost = row_of(&report, "ghost");
        assert_eq!(ghost.verdict, Verdict::Unpriced);
        assert!(ghost.missing_from_config);
        assert_eq!(ghost.current_weight, None);
        // no priced candidate → no anchor at all
        assert!(report.anchor.is_none());
    }

    // 4. incomplete records are counted but excluded from the fold
    #[test]
    fn incomplete_records_are_counted_not_folded() {
        let config = config_with_prices();
        let mut records: Vec<UsageRecord> = Vec::new();
        for _ in 0..20 {
            records.push(record("claude", 1_000, 100, true));
        }
        for _ in 0..5 {
            records.push(record("claude", 999_999, 999_999, false)); // partial tokens
        }
        let report = analyze(&records, &config, 20, 0, 7);
        let claude = row_of(&report, "claude");
        assert_eq!(claude.requests, 25);
        assert_eq!(claude.incomplete, 5);
        assert_eq!(claude.analyzed, 20);
        // 20 × (1000×3 + 100×15)/1e6 = 20 × 0.0045
        assert!((claude.total_usd - 20.0 * 0.0045).abs() < 1e-9);
    }

    // 5. all-zero spend at current prices: every suggestion is 0.0
    //    (equally free), never 0/0
    #[test]
    fn zero_spend_suggests_zero_not_nan() {
        let mut config = config_with_prices();
        for name in ["claude", "glm"] {
            config.models.get_mut(name).unwrap().pricing = Some(pricing(0.0, 0.0));
        }
        let records: Vec<UsageRecord> = (0..20)
            .flat_map(|_| {
                vec![
                    record("claude", 1_000, 100, true),
                    record("glm", 500, 50, true),
                ]
            })
            .collect();
        let report = analyze(&records, &config, 20, 0, 7);
        let anchor = report.anchor.as_ref().unwrap();
        assert_eq!(anchor.usd_per_request, 0.0);
        for row in &report.rows {
            match row.verdict {
                Verdict::Analyzed { suggestion, score } => {
                    assert_eq!(suggestion, 0.0);
                    assert_eq!(score, 1.0);
                }
                other => panic!("{} should be analyzed: {other:?}", row.model),
            }
        }
        let text = render_report(&report);
        assert!(
            text.contains("all suggestions are 0.0 (equally free)"),
            "{text}"
        );
    }

    // 6. the three-way status language and every caveat render
    #[test]
    fn render_carries_the_three_statuses_and_all_caveats() {
        let mut config = config_with_prices();
        config.models.get_mut("glm").unwrap().pricing = None;
        let mut records: Vec<UsageRecord> = Vec::new();
        for _ in 0..25 {
            records.push(record("claude", 1_000, 100, true));
        }
        for _ in 0..4 {
            records.push(record("glm", 1_000, 100, true)); // unpriced
            records.push(record("tiny", 1_000, 100, true)); // not in config
        }
        for _ in 0..19 {
            records.push(record("m3", 1_000, 100, true)); // priced? not in config…
        }
        // make m3 priced-but-thin instead: add it to config
        config.models.insert(
            "m3".to_string(),
            crate::model::Model {
                provider: "zai".to_string(),
                model_id: "MiniMax-M3".to_string(),
                context_window: None,
                routing: crate::model::ModelRouting::default(),
                pricing: Some(pricing(1.0, 2.0)),
            },
        );
        let report = analyze(&records, &config, 20, 0, 7);
        let text = render_report(&report);
        assert!(text.contains("cost unknown — never guessed"), "{text}");
        assert!(text.contains("model not in config.toml"), "{text}");
        // v0.5 M5: the unpriced message names the config edit (no pricing
        // CLI flag exists, by policy).
        assert!(text.contains("add a [models.glm.pricing] table"), "{text}");
        assert!(
            text.contains("insufficient — unchanged (19 < 20)"),
            "{text}"
        );
        assert!(text.contains("anchor: claude"), "{text}");
        assert!(text.contains("[models.claude.routing]"), "{text}");
        assert!(text.contains("cost_weight = 1.000"), "{text}");
        assert!(text.contains("never writes config"), "{text}");
        for caveat in caveats() {
            assert!(text.contains(caveat), "missing caveat: {caveat}\n{text}");
        }
        // the tokenizer caveat specifically (the plan's required wording)
        assert!(
            text.contains("not comparable across protocol families"),
            "{text}"
        );
    }

    // 7. empty window: an honest nothing-to-analyze, no table, no
    //    fragment — but the caveats still ride along (an empty table is
    //    exactly when the selection-bias caveat explains why configured
    //    models show no data)
    #[test]
    fn render_empty_window() {
        let report = analyze(&[], &config_with_prices(), 20, 0, 7);
        let text = render_report(&report);
        assert!(text.contains("no usage records"), "{text}");
        assert!(!text.contains("cost_weight = "), "{text}");
        assert!(report.anchor.is_none());
        assert!(text.contains("caveats:"), "{text}");
        assert!(
            text.contains("selection bias"),
            "empty report still carries the caveats: {text}"
        );
    }

    // 7b. a window wider than history retention is called out — the fold
    //     cannot see deleted rotated files, and the header must not imply
    //     otherwise
    #[test]
    fn window_wider_than_retention_is_flagged() {
        let config = config_with_prices(); // retention_days default: 14
        assert_eq!(config.observability.retention_days, 14);
        let report = analyze(&[], &config, 20, 0, 30);
        let text = render_report(&report);
        assert!(
            text.contains("exceeds history retention (14 day(s))"),
            "{text}"
        );
        // within retention: no note
        let quiet = analyze(&[], &config, 20, 0, 14);
        assert!(!render_report(&quiet).contains("retention"));
    }

    // 8. a dotted alias must not split into nested TOML tables
    #[test]
    fn toml_fragment_quotes_non_bare_aliases() {
        let row = |model: &str| AdviseRow {
            model: model.to_string(),
            requests: 20,
            incomplete: 0,
            analyzed: 20,
            total_usd: 1.0,
            total_tokens: 1000,
            current_weight: Some(1.0),
            missing_from_config: false,
            verdict: Verdict::Analyzed {
                suggestion: 0.25,
                score: 0.8,
            },
        };
        let text = toml_fragment(&[row("glm"), row("a.b")]);
        assert!(text.contains("[models.glm.routing]"), "{text}");
        assert!(text.contains("[models.\"a.b\".routing]"), "{text}");
        assert!(text.contains("cost_weight = 0.250"), "{text}");
        // nothing analyzed → no fragment at all
        assert!(toml_fragment(&[]).is_empty());
    }

    // 9. run_at: real files on disk, the since window, and the two bails
    #[test]
    fn run_at_reads_files_and_validates() {
        let config = config_with_prices();
        let dir = std::env::temp_dir().join("ccm-advise-run-at");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = 10 * MS_PER_DAY;
        let mut lines = String::new();
        for i in 0..20 {
            // inside the 7-day window (i ms ago)
            let mut r = record("claude", 1_000, 100, true);
            r.timestamp_ms = now - i - 1;
            lines.push_str(&serde_json::to_string(&r).unwrap());
            lines.push('\n');
        }
        // outside the window (10 days old) — must not count
        let mut old = record("claude", 999_999, 999_999, true);
        old.timestamp_ms = 0;
        lines.push_str(&serde_json::to_string(&old).unwrap());
        lines.push('\n');
        std::fs::write(dir.join("usage.jsonl"), lines).unwrap();

        let options = AdviseOptions {
            window_days: 7,
            min_samples: 20,
            model: None,
        };
        let report = run_at(&options, &config, &dir, now).unwrap();
        let claude = row_of(&report, "claude");
        assert_eq!(claude.analyzed, 20, "the 10-day-old record is excluded");
        assert_eq!(
            claude.verdict,
            Verdict::Analyzed {
                suggestion: 1.0,
                score: 0.5
            }
        );

        // unknown --model is a config error, not an empty report
        let mut bad = AdviseOptions {
            window_days: 7,
            min_samples: 20,
            model: Some("nope".to_string()),
        };
        let error = run_at(&bad, &config, &dir, now).unwrap_err().to_string();
        assert!(error.contains("unknown model `nope`"), "{error}");

        bad.model = Some("claude".to_string());
        let report = run_at(&bad, &config, &dir, now).unwrap();
        assert_eq!(report.rows.len(), 1);
        let text = render_report(&report);
        assert!(
            text.contains("its suggestion is trivially 1.0"),
            "single-candidate note: {text}"
        );

        // missing directory: the history_cli message shape
        let missing = std::env::temp_dir().join("ccm-advise-missing-dir");
        let error = run_at(&options, &config, &missing, now)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no history directory"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
