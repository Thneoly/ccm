//! `ccm history` presentation (v0.4 M5): offline tables and JSONL over the
//! persisted observability records in `$CCM_HOME/history/`. Reads the JSONL
//! files directly — it works while the proxy runs (at most the writer's
//! current batch is not yet flushed) and after it exits. No lock is taken:
//! readers never contend with the single writer.
//!
//! `decisions` prints full-fidelity JSONL (pipeable to jq); `metrics` and
//! `circuit` print fixed-column tables. `run` resolves the directory from
//! config; `run_at` takes it explicitly so tests need no environment.

use std::path::Path;

use anyhow::{bail, Result};

use crate::cli::HistoryCommand;
use crate::config::AppConfig;
use crate::history::{self, CircuitTransition, DecisionQuery, MetricsSnapshot};
use crate::routing::metrics::{health_score, success_rate, ModelMetrics};

pub(crate) fn run(command: HistoryCommand) -> Result<()> {
    run_at(command, &AppConfig::history_dir()?)
}

fn run_at(command: HistoryCommand, dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        bail!(
            "no history directory at {} — the proxy writes it when [observability] history_enabled is on (the default)",
            dir.display()
        );
    }
    match command {
        HistoryCommand::Decisions {
            since,
            until,
            model,
            client,
            limit,
        } => {
            let query = DecisionQuery {
                since,
                until,
                model,
                client,
                limit,
            };
            for decision in history::read_decisions(dir, &query)? {
                println!("{}", serde_json::to_string(&decision)?);
            }
        }
        HistoryCommand::Metrics { limit } => {
            // Default: just the latest snapshot; --limit N widens the window.
            let snapshots = history::read_metrics_snapshots(dir, Some(limit.unwrap_or(1)));
            if snapshots.is_empty() {
                println!("no metric snapshots persisted (an idle proxy writes none)");
                return Ok(());
            }
            for snapshot in &snapshots {
                print_snapshot(snapshot);
            }
        }
        HistoryCommand::Circuit { model, limit } => {
            let transitions = history::read_circuit_transitions(dir, model.as_deref(), limit);
            if transitions.is_empty() {
                println!("no circuit transitions persisted");
                return Ok(());
            }
            println!(
                "{:<24} {:<16} {:<22} reason",
                "time (UTC)", "model", "transition"
            );
            for transition in &transitions {
                println!("{}", transition_line(transition));
            }
        }
        HistoryCommand::Cost { day, client } => {
            // Default: today in UTC — the same day the running proxy's
            // `/_ccm/cost` (no ?day=) would aggregate.
            let day = day.unwrap_or_else(|| crate::date::utc_day_of(crate::date::now_ms()));
            let Some(since) = crate::date::parse_utc_day(&day) else {
                bail!("invalid --day `{day}`: expected YYYY-MM-DD (UTC)");
            };
            let query = history::UsageQuery {
                since: Some(since),
                until: Some(since + 86_400_000 - 1),
                model: None,
                client,
                limit: None,
            };
            let records = history::read_usage(dir, &query)?;
            let aggregation = crate::usage::aggregate_costs(&records);
            print_cost(&day, &aggregation);
        }
    }
    Ok(())
}

/// The `ccm history cost` table. Pure so tests can pin the format.
fn print_cost(day: &str, aggregation: &crate::usage::CostAggregation) {
    println!("== {day} (UTC)");
    if aggregation.models.is_empty() {
        println!("no usage records for this day");
        return;
    }
    println!(
        "{:<20} {:>7} {:>9} {:>10} {:>10} {:>9} {:>9} {:>12}",
        "model", "reqs", "complete", "input", "output", "cache_r", "cache_w", "cost_usd"
    );
    for row in &aggregation.models {
        println!("{}", cost_row(row));
    }
    println!(
        "total: {} requests, {:.6} USD",
        aggregation.total_requests, aggregation.total_cost_usd
    );
    if aggregation.total_unpriced_requests > 0 {
        println!(
            "note: {} request(s) had no [pricing] table — their cost is unknown, not zero",
            aggregation.total_unpriced_requests
        );
    }
}

/// One `ccm history cost` table row; pure so tests can pin the format.
fn cost_row(row: &crate::usage::CostByModel) -> String {
    format!(
        "{:<20} {:>7} {:>9} {:>10} {:>10} {:>9} {:>9} {:>12.6}",
        row.model,
        row.requests,
        row.complete,
        row.input_tokens,
        row.output_tokens,
        row.cache_read_tokens,
        row.cache_write_tokens,
        row.cost_usd
    )
}

fn print_snapshot(snapshot: &MetricsSnapshot) {
    println!(
        "== snapshot {} ({})",
        utc_ms(snapshot.timestamp_ms),
        snapshot.timestamp_ms
    );
    println!(
        "{:<20} {:>8} {:>6} {:>7} {:>7} {:>9} {:>7} {:>10}",
        "model", "attempts", "ok", "rate", "health", "http_err", "t_out", "latency_ms"
    );
    for (model, metrics) in &snapshot.models {
        println!("{}", metrics_row(model, metrics));
    }
}

/// One table row of a model's counters; pure so tests can pin the format.
/// Derived rates are computed here on read (snapshots store raw counters).
fn metrics_row(model: &str, metrics: &ModelMetrics) -> String {
    format!(
        "{:<20} {:>8} {:>6} {:>6.1}% {:>7} {:>9} {:>7} {:>10}",
        model,
        metrics.attempts,
        metrics.successes,
        100.0 * success_rate(metrics),
        health_score(metrics)
            .map(|score| format!("{score:.0}"))
            .unwrap_or_else(|| "-".to_string()),
        metrics.http_errors,
        metrics.timeouts,
        metrics
            .latency_ewma_ms
            .map(|latency| format!("{latency:.0}"))
            .unwrap_or_else(|| "-".to_string()),
    )
}

fn transition_line(transition: &CircuitTransition) -> String {
    format!(
        "{:<24} {:<16} {:<22} {}",
        utc_ms(transition.timestamp_ms),
        transition.model,
        format!("{} -> {}", transition.from, transition.to),
        transition.reason
    )
}

/// UTC `YYYY-MM-DDTHH:MM:SSZ` for a unix-ms timestamp (formatting of
/// `date::utc_parts`). Presentation only — filtering and ordering always
/// use the raw `timestamp_ms` numbers.
fn utc_ms(ms: u64) -> String {
    let (year, month, day, hour, minute, second) = crate::date::utc_parts(ms);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc_ms(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_ms(951_782_400_000), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(utc_ms(1_000_000_000_000), "2001-09-09T01:46:40Z");
    }

    #[test]
    fn run_at_reports_missing_directory() {
        let missing = std::env::temp_dir().join("ccm-history-cli-missing-dir");
        let error = run_at(HistoryCommand::Metrics { limit: None }, &missing)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no history directory"), "{error}");
    }

    #[test]
    fn metrics_row_and_transition_line_show_the_essentials() {
        let metrics = ModelMetrics {
            attempts: 10,
            successes: 8,
            http_errors: 2,
            latency_ewma_ms: Some(250.0),
            ..ModelMetrics::default()
        };
        let row = metrics_row("glm", &metrics);
        assert!(row.contains("glm"), "{row}");
        assert!(row.contains("80.0%"), "{row}");
        assert!(row.contains("250"), "{row}");

        let line = transition_line(&CircuitTransition {
            timestamp_ms: 0,
            model: "glm".to_string(),
            from: "CLOSED".to_string(),
            to: "OPEN".to_string(),
            reason: "failure threshold reached".to_string(),
        });
        assert!(line.contains("1970-01-01T00:00:00Z"), "{line}");
        assert!(line.contains("CLOSED -> OPEN"), "{line}");
        assert!(line.contains("failure threshold reached"), "{line}");
    }

    #[test]
    fn cost_row_formats_the_essentials() {
        let row = cost_row(&crate::usage::CostByModel {
            model: "glm".to_string(),
            requests: 12,
            complete: 11,
            input_tokens: 123_456,
            output_tokens: 7_890,
            cache_read_tokens: 40_000,
            cache_write_tokens: 0,
            unpriced_requests: 0,
            cost_usd: 0.012345,
        });
        assert!(row.contains("glm"), "{row}");
        assert!(row.contains("12"), "{row}");
        assert!(row.contains("11"), "{row}");
        assert!(row.contains("0.012345"), "{row}");
    }

    #[test]
    fn run_at_rejects_impossible_cost_day() {
        let dir = std::env::temp_dir().join("ccm-history-cli-cost-day");
        std::fs::create_dir_all(&dir).unwrap();
        let error = run_at(
            HistoryCommand::Cost {
                day: Some("2026-02-30".to_string()),
                client: None,
            },
            &dir,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("invalid --day"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
