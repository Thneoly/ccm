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
                "{:<24} {:<16} {:<22} {}",
                "time (UTC)", "model", "transition", "reason"
            );
            for transition in &transitions {
                println!("{}", transition_line(transition));
            }
        }
    }
    Ok(())
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

/// UTC `YYYY-MM-DDTHH:MM:SSZ` for a unix-ms timestamp (civil-from-days,
/// Howard Hinnant's algorithm). Presentation only — filtering and ordering
/// always use the raw `timestamp_ms` numbers.
fn utc_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
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
        let mut metrics = ModelMetrics::default();
        metrics.attempts = 10;
        metrics.successes = 8;
        metrics.http_errors = 2;
        metrics.latency_ewma_ms = Some(250.0);
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
}
