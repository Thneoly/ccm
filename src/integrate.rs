use std::{fs, path::PathBuf};

use anyhow::{Context, Result};

use crate::cli::IntegrateCommand;

const CLAUDE_SWITCH_SKILL: &str = r#"---
name: switch
description: Switch the active CCM model, profile, or route for the current Claude Code session when the user invokes /switch.
argument-hint: "<model-or-profile-or-route>"
disable-model-invocation: true
allowed-tools: ["Bash(ccm switch:*)"]
---

## Arguments

`$0` is the CCM model, profile, or route to activate.

Run `ccm switch "$0"` using Bash. If it succeeds, report the active target concisely. Do not change CCM's persisted default.
"#;

pub fn handle(command: IntegrateCommand) -> Result<()> {
    match command {
        IntegrateCommand::Claude { remove } => {
            if remove {
                remove_claude_integration()
            } else {
                install_claude_integration()
            }
        }
    }
}

fn skill_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home
        .join(".claude")
        .join("skills")
        .join("switch")
        .join("SKILL.md"))
}

fn legacy_command_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".claude").join("commands").join("switch.md"))
}

fn install_claude_integration() -> Result<()> {
    let path = skill_path()?;
    let parent = path
        .parent()
        .context("cannot determine Claude skill directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    fs::write(&path, CLAUDE_SWITCH_SKILL)
        .with_context(|| format!("cannot write {}", path.display()))?;

    let legacy = legacy_command_path()?;
    if legacy.exists() {
        fs::remove_file(&legacy)
            .with_context(|| format!("cannot remove legacy {}", legacy.display()))?;
    }

    println!("Installed Claude Code skill: /switch <model-or-profile-or-route>");
    println!("Path: {}", path.display());
    println!("The skill switches CCM's in-memory runtime target.");
    Ok(())
}

fn remove_claude_integration() -> Result<()> {
    let path = skill_path()?;
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("cannot remove {}", path.display()))?;
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }

    let legacy = legacy_command_path()?;
    if legacy.exists() {
        fs::remove_file(&legacy)
            .with_context(|| format!("cannot remove legacy {}", legacy.display()))?;
    }

    println!("Removed Claude Code /switch integration");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_contains_runtime_switch_invocation() {
        assert!(CLAUDE_SWITCH_SKILL.contains("name: switch"));
        assert!(CLAUDE_SWITCH_SKILL.contains("ccm switch \"$0\""));
        assert!(CLAUDE_SWITCH_SKILL.contains("argument-hint: \"<model-or-profile-or-route>\""));
        assert!(!CLAUDE_SWITCH_SKILL.contains("\\\""));
    }
}
