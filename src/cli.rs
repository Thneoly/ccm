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
        /// Overwrite an existing configuration file.
        #[arg(long)]
        force: bool,
    },
    /// Add or update providers and models.
    Add {
        #[command(subcommand)]
        command: AddCommand,
    },
    /// Run local diagnostics for Claude Code and the selected model.
    Doctor,
    /// List configured models.
    List,
    /// Show the selected model alias.
    Current,
    /// Select a model or profile alias.
    Use { target: String },
    /// Launch Claude Code using a model/profile alias, or the current selection.
    Run { target: Option<String> },
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
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store a provider API key in the OS keyring.
    Set { provider: String },
    /// Delete a provider API key from the OS keyring.
    Delete { provider: String },
}
