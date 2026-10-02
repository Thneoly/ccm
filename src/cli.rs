use clap::{ArgAction, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "ccm", version, about = "Claude Code Model Manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Init { #[arg(long)] force: bool },
    Add { #[command(subcommand)] command: AddCommand },
    Integrate { #[command(subcommand)] command: IntegrateCommand },
    Doctor,
    Proxy {
        #[arg(long, default_value = "127.0.0.1:13521")]
        bind: String,
    },
    List,
    Current,
    Use { target: String },
    Switch {
        target: String,
        #[arg(long)]
        proxy_url: Option<String>,
    },
    Run {
        target: Option<String>,
        #[arg(long)]
        proxy: bool,
        #[arg(long, default_value = "http://127.0.0.1:13521")]
        proxy_url: String,
    },
    Auth { #[command(subcommand)] command: AuthCommand },
    Health { target: String },
}

#[derive(Debug, Subcommand)]
pub enum AddCommand {
    Provider {
        name: String,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value = "x-api-key")]
        auth: String,
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
    Claude { #[arg(long)] remove: bool },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    Set { provider: String },
    Delete { provider: String },
}
