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
    let env_name = env_key_name(provider);
    if let Ok(value) = std::env::var(&env_name) {
        if !value.trim().is_empty() {
            return Ok(value);
        }
    }

    let entry = Entry::new(SERVICE, provider)
        .with_context(|| format!("cannot open keyring entry for provider `{provider}`"))?;
    entry.get_password().with_context(|| {
        format!(
            "no credential found for provider `{provider}`; set {env_name} or run `ccm auth set {provider}`"
        )
    })
}

fn env_key_name(provider: &str) -> String {
    let normalized = provider
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("CCM_{normalized}_API_KEY")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_provider_name_for_environment_key() {
        assert_eq!(env_key_name("z-ai.test"), "CCM_Z_AI_TEST_API_KEY");
    }
}
