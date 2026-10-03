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
    History {
        #[command(subcommand)]
        command: HistoryCommand,
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
        /// Keep only the newest N matching records.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Print persisted metric snapshots as tables (default: the latest one).
    Metrics {
        /// Print the newest N snapshots instead of only the latest.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Print persisted circuit-breaker transitions (oldest first).
    Circuit {
        /// Only transitions for this model.
        #[arg(long)]
        model: Option<String>,
        /// Keep only the newest N matching transitions.
        #[arg(long)]
        limit: Option<usize>,
    },
}
