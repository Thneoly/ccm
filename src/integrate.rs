use std::{fs, path::PathBuf};

use anyhow::{Context, Result};

use crate::cli::IntegrateCommand;

const CLAUDE_SWITCH_COMMAND: &str = r#"---
description: Switch the CCM backend model/profile used by the running Claude Code proxy session
argument-hint: <model-or-profile>
allowed-tools: Bash(ccm use:*), Bash(ccm current:*)
---

Switch the CCM backend to `$ARGUMENTS`.

Run:

!`ccm use $ARGUMENTS`

Then report the newly selected CCM model/profile concisely. This changes CCM's routing target for subsequent requests when Claude Code is running through `ccm run --proxy`.
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
    println!("Use it with a Claude Code session started by `ccm run --proxy`.");
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
    fn command_contains_switch_invocation() {
        assert!(CLAUDE_SWITCH_COMMAND.contains("ccm use $ARGUMENTS"));
        assert!(CLAUDE_SWITCH_COMMAND.contains("/switch"));
    }
}
