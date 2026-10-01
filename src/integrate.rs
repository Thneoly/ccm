use std::{fs, path::PathBuf};

use anyhow::{Context, Result};

use crate::cli::IntegrateCommand;

const CLAUDE_SWITCH_COMMAND: &str = r#"---
description: Switch the active CCM proxy route for the running Claude Code session
argument-hint: <model-or-profile>
allowed-tools: Bash(ccm switch:*), Bash(ccm current:*)
---

Switch the running CCM proxy route to `$ARGUMENTS` without changing the persisted default configuration.

Run:

!`ccm switch $ARGUMENTS`

Then report the active runtime route concisely. This requires a CCM proxy running on the default local endpoint and Claude Code started with `ccm run --proxy`.
"#;

pub fn handle(command: IntegrateCommand) -> Result<()> {
    match command {
        IntegrateCommand::Claude { remove } => {
            if remove {
                remove_claude_command()
            } else {
                install_claude_command()
            }
        }
    }
}

fn command_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".claude").join("commands").join("switch.md"))
}

fn install_claude_command() -> Result<()> {
    let path = command_path()?;
    let parent = path
        .parent()
        .context("cannot determine Claude commands directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    fs::write(&path, CLAUDE_SWITCH_COMMAND)
        .with_context(|| format!("cannot write {}", path.display()))?;
    println!("Installed Claude Code command: /switch <model-or-profile>");
    println!("Path: {}", path.display());
    println!("The command now switches CCM's in-memory runtime route.");
    Ok(())
}

fn remove_claude_command() -> Result<()> {
    let path = command_path()?;
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("cannot remove {}", path.display()))?;
        println!("Removed Claude Code /switch integration");
    } else {
        println!("Claude Code /switch integration is not installed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_contains_runtime_switch_invocation() {
        assert!(CLAUDE_SWITCH_COMMAND.contains("ccm switch $ARGUMENTS"));
        assert!(CLAUDE_SWITCH_COMMAND.contains("/switch"));
    }
}
