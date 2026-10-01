use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "ccm", version, about = "Claude Code Model Manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create ~/.ccm/config.toml with a starter configuration.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Add or update providers, models, and routes.
    Add {
        #[command(subcommand)]
        command: AddCommand,
    },
    /// Install or remove integrations for supported coding CLIs.
    Integrate {
        #[command(subcommand)]
        command: IntegrateCommand,
    },
    /// Run local diagnostics for Claude Code and the selected model.
    Doctor,
    /// Start the local Anthropic-compatible routing proxy.
    Proxy {
        #[arg(long, default_value = "127.0.0.1:13521")]
        bind: String,
    },
    /// List configured models.
    List,
    /// Show the selected persisted model alias.
    Current,
    /// Persist a model or profile alias as the default selection.
    Use { target: String },
    /// Switch the active model/profile/route of a running CCM proxy.
    Switch {
        target: String,
        #[arg(long, default_value = "http://127.0.0.1:13521")]
        proxy_url: String,
    },
    /// Launch Claude Code directly or through the local CCM proxy.
    Run {
        target: Option<String>,
        #[arg(long)]
        proxy: bool,
        #[arg(long, default_value = "http://127.0.0.1:13521")]
        proxy_url: String,
    },
    /// Manage provider credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Check whether a model's provider endpoint is reachable.
    Health { target: String },
}

#[derive(Debug, Subcommand)]
pub enum AddCommand {
    /// Add or update a provider. Missing values are prompted interactively.
    Provider {
        name: String,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        kind: Option<String>,
    },
    /// Add or update a model. Missing values are prompted interactively.
    Model {
        name: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model_id: Option<String>,
    },
    /// Add or update a route and its fallback policy.
    Route {
        name: String,
        #[arg(long)]
        primary: Option<String>,
        #[arg(long)]
        fallback: Option<String>,
        #[arg(long, default_value_t = 30_000)]
        header_timeout_ms: u64,
        #[arg(long, default_value = "429,502,503,504")]
        fallback_on: String,
        #[arg(long, default_value_t = 3)]
        max_attempts: usize,
        #[arg(long, default_value_t = 200)]
        backoff_ms: u64,
    },
}

#[derive(Debug, Subcommand)]
pub enum IntegrateCommand {
    /// Install the global /switch command for Claude Code.
    Claude {
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store a provider API key in the OS keyring.
    Set { provider: String },
    /// Delete a provider API key from the OS keyring.
    Delete { provider: String },
}
