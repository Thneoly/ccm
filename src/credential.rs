use anyhow::{Context, Result};
use keyring::Entry;

use crate::cli::AuthCommand;

const SERVICE: &str = "ccm";

pub fn handle(command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Set { provider } => set(&provider),
        AuthCommand::Delete { provider } => delete(&provider),
    }
}

pub fn get(provider: &str) -> Result<String> {
    let entry = Entry::new(SERVICE, provider)
        .with_context(|| format!("cannot open keyring entry for provider `{provider}`"))?;
    entry
        .get_password()
        .with_context(|| format!("no credential found for provider `{provider}`; run `ccm auth set {provider}`"))
}

fn set(provider: &str) -> Result<()> {
    let password = rpassword::prompt_password(format!("API key for {provider}: "))?;
    let entry = Entry::new(SERVICE, provider)
        .with_context(|| format!("cannot open keyring entry for provider `{provider}`"))?;
    entry
        .set_password(&password)
        .with_context(|| format!("cannot store credential for provider `{provider}`"))?;
    println!("Stored credential for {provider}");
    Ok(())
}

fn delete(provider: &str) -> Result<()> {
    let entry = Entry::new(SERVICE, provider)
        .with_context(|| format!("cannot open keyring entry for provider `{provider}`"))?;
    entry
        .delete_credential()
        .with_context(|| format!("cannot delete credential for provider `{provider}`"))?;
    println!("Deleted credential for {provider}");
    Ok(())
}
