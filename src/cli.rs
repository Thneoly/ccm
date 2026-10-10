use clap::{ArgAction, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "ccm", version, about = "Claude Code Model Manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Init {
        #[arg(long)]
        force: bool,
    },
    Add {
        #[command(subcommand)]
        command: AddCommand,
    },
    Integrate {
        #[command(subcommand)]
        command: IntegrateCommand,
    },
    Doctor,
    Proxy {
        #[arg(long, default_value = "127.0.0.1:13521")]
        bind: String,
    },
    List,
    Current,
    Use {
        target: String,
    },
    Switch {
        target: String,
        #[arg(long)]
        proxy_url: Option<String>,
        /// Scope the switch to one client id instead of the global target.
        #[arg(long)]
        client: Option<String>,
        /// Force a global switch even when CCM_CLIENT_ID is set.
        #[arg(long, conflicts_with = "client")]
        global: bool,
    },
    Clients {
        #[arg(long)]
        proxy_url: Option<String>,
    },
    Run {
        target: Option<String>,
        #[arg(long)]
        proxy: bool,
        #[arg(long, default_value = "http://127.0.0.1:13521")]
        proxy_url: String,
        /// Client id for this session (default: CCM_CLIENT_ID env, else a
        /// short random id). In proxy mode the pre-switch stays scoped to it.
        #[arg(long)]
        client: Option<String>,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Health {
        target: String,
    },
    /// List a gateway's models via GET /v1/models and register selected
    /// ones (v0.5 M5). Read-only until you confirm the selection;
    /// already-registered (provider, model_id) pairs are skipped, never
    /// overwritten.
    Discover {
        provider: Option<String>,
        /// Register every listed model without prompting (the script path).
        #[arg(long)]
        all: bool,
    },
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    /// Suggest cost_weight values from realized spend (v0.5 M1). Read-only:
    /// prints a table, a paste-only TOML fragment, and the honesty
    /// caveats — never writes config.
    Advise {
        /// Analysis window in days.
        #[arg(long, default_value_t = crate::advise::DEFAULT_WINDOW_DAYS, value_parser = clap::builder::RangedU64ValueParser::<u64>::new().range(1..))]
        window: u64,
        /// Minimum analyzed (complete + priced) requests before a
        /// suggestion is offered for a model.
        #[arg(long, default_value_t = crate::advise::DEFAULT_MIN_SAMPLES, value_parser = clap::builder::RangedU64ValueParser::<u64>::new().range(1..))]
        min_samples: u64,
        /// Narrow the analysis to one configured model alias.
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AddCommand {
    Provider {
        name: String,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        // No CLI default: omitting --auth stores None so the per-kind default
        // applies (x-api-key for anthropic kinds, bearer for openai-compatible).
        #[arg(long)]
        auth: Option<String>,
    },
    Model {
        name: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model_id: Option<String>,
        #[arg(long, default_value_t = 1.0)]
        cost_weight: f64,
        #[arg(long, default_value_t = 1.0)]
        quality_weight: f64,
        /// Declare the model's real context window in tokens. Direct-mode
        /// launches inject it as CLAUDE_CODE_MAX_CONTEXT_TOKENS so Claude
        /// Code's auto-compaction runs at the true threshold instead of its
        /// assumed default for unknown model ids. The upper bound is TOML's
        /// integer ceiling (i64::MAX): a value that parses but cannot
        /// persist in config.toml must fail here at the flag, not later at
        /// config save with a confusing write error.
        #[arg(
            long,
            value_parser = clap::builder::RangedU64ValueParser::<u64>::new().range(1..=i64::MAX as u64)
        )]
        context_window: Option<u64>,
    },
    Route {
        name: String,
        #[arg(long)]
        primary: Option<String>,
        #[arg(long)]
        fallback: Option<String>,
        #[arg(long, default_value = "ordered")]
        selection: String,
        #[arg(long, default_value_t = 0.4)]
        reliability_weight: f64,
        #[arg(long, default_value_t = 0.2)]
        latency_weight: f64,
        #[arg(long, default_value_t = 0.2)]
        cost_weight: f64,
        #[arg(long, default_value_t = 0.2)]
        quality_weight: f64,
        #[arg(long, default_value_t = 30_000)]
        header_timeout_ms: u64,
        #[arg(long, default_value = "429,502,503,504")]
        fallback_on: String,
        #[arg(long, default_value_t = 3)]
        max_attempts: usize,
        #[arg(long, default_value_t = 200)]
        backoff_ms: u64,
        #[arg(long, default_value_t = true, action = ArgAction::Set)]
        circuit_enabled: bool,
        #[arg(long, default_value_t = 3)]
        failure_threshold: usize,
        #[arg(long, default_value_t = 30_000)]
        circuit_open_ms: u64,
    },
}

#[derive(Debug, Subcommand)]
pub enum IntegrateCommand {
    Claude {
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    Set { provider: String },
    Delete { provider: String },
}

/// `ccm history` — offline views over `$CCM_HOME/history/` (v0.4 M5). Reads
/// the JSONL files directly, so it works while the proxy runs and after it
/// exits; no lock is taken.
#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    /// Print persisted routing decisions as JSONL (oldest first).
    Decisions {
        /// Inclusive unix-ms lower bound on timestamp_ms.
        #[arg(long)]
        since: Option<u64>,
        /// Inclusive unix-ms upper bound on timestamp_ms.
        #[arg(long)]
        until: Option<u64>,
        /// Only decisions whose selected or attempted model matches.
        #[arg(long)]
        model: Option<String>,
        /// Only decisions tagged with exactly this client id.
        #[arg(long)]
        client: Option<String>,
        /// Keep only the newest N matching records (N >= 1; 0 selects
        /// nothing, so it is rejected here rather than silently emptying
        /// the output).
        #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        limit: Option<usize>,
    },
    /// Print persisted metric snapshots as tables (default: the latest one).
    Metrics {
        /// Print the newest N snapshots instead of only the latest
        /// (N >= 1; see `decisions --limit` for why 0 is rejected).
        #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        limit: Option<usize>,
    },
    /// Print persisted circuit-breaker transitions (oldest first).
    Circuit {
        /// Only transitions for this model.
        #[arg(long)]
        model: Option<String>,
        /// Keep only the newest N matching transitions (N >= 1; see
        /// `decisions --limit` for why 0 is rejected).
        #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        limit: Option<usize>,
    },
    /// Aggregate usage and cost by model for one UTC day (v0.4 M6).
    Cost {
        /// UTC day to aggregate, `YYYY-MM-DD` (default: today).
        #[arg(long)]
        day: Option<String>,
        /// Only records tagged with exactly this client id.
        #[arg(long)]
        client: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    /// `--limit 0` is rejected at parse time on every `ccm history`
    /// subcommand that takes the flag: an empty selection is almost always
    /// a mistake (unset shell variable, or assuming 0 means unlimited), and
    /// the reader layer deliberately treats it as "select nothing" — so the
    /// CLI says so instead of printing an empty view that looks like the
    /// store were empty.
    #[test]
    fn history_limit_zero_is_rejected_at_parse_time() {
        for sub in ["decisions", "metrics", "circuit"] {
            let error = Cli::try_parse_from(["ccm", "history", sub, "--limit", "0"])
                .expect_err("--limit 0 must not parse");
            let message = error.to_string();
            assert!(message.contains("invalid value"), "{sub}: {message}");
            assert!(message.contains('0'), "{sub}: {message}");

            let parsed = Cli::try_parse_from(["ccm", "history", sub, "--limit", "1"]);
            assert!(parsed.is_ok(), "{sub}: --limit 1 must still parse");
        }
    }

    /// `--context-window 0` and values above TOML's integer ceiling are
    /// rejected at parse time, same reasoning as `--limit 0`: zero tokens
    /// is not a window, and an unpersistable value must die at the flag,
    /// not at config save. Hand-edited TOML gets the same zero rejection
    /// at config load with its own wording ("must be greater than 0",
    /// where clap says "invalid value"); the hand-edited ceiling is
    /// bounded by TOML's own parser.
    #[test]
    fn context_window_zero_is_rejected_at_parse_time() {
        let error = Cli::try_parse_from([
            "ccm",
            "add",
            "model",
            "glm",
            "--provider",
            "zai",
            "--model-id",
            "glm-5.3",
            "--context-window",
            "0",
        ])
        .expect_err("--context-window 0 must not parse");
        let message = error.to_string();
        assert!(message.contains("invalid value"), "{message}");
        assert!(message.contains('0'), "{message}");

        // i64::MAX + 1 parses as u64 but cannot persist in TOML.
        let error = Cli::try_parse_from([
            "ccm",
            "add",
            "model",
            "glm",
            "--provider",
            "zai",
            "--model-id",
            "glm-5.3",
            "--context-window",
            "9223372036854775808",
        ])
        .expect_err("--context-window above i64::MAX must not parse");
        assert!(error.to_string().contains("invalid value"), "{error}");

        let parsed = Cli::try_parse_from([
            "ccm",
            "add",
            "model",
            "glm",
            "--provider",
            "zai",
            "--model-id",
            "glm-5.3",
            "--context-window",
            "1000000",
        ]);
        assert!(parsed.is_ok(), "--context-window 1000000 must parse");
        let parsed = Cli::try_parse_from([
            "ccm",
            "add",
            "model",
            "glm",
            "--provider",
            "zai",
            "--model-id",
            "glm-5.3",
            "--context-window",
            "9223372036854775807",
        ]);
        assert!(parsed.is_ok(), "--context-window i64::MAX must parse");
    }
}
