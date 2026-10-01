use std::io::{self, Write};

use anyhow::{bail, Result};

use crate::{
    cli::AddCommand,
    config::AppConfig,
    model::Model,
    provider::{Provider, ProviderKind},
};

pub fn handle(config: &mut AppConfig, command: AddCommand) -> Result<()> {
    match command {
        AddCommand::Provider {
            name,
            base_url,
            kind,
        } => {
            let base_url = required(base_url, "Base URL")?;
            let kind = required(kind, "Kind (anthropic / anthropic-compatible)")?;
            let kind = parse_kind(&kind)?;
            config.add_provider(name.clone(), Provider { kind, base_url });
            config.save()?;
            println!("Saved provider {name}");
        }
        AddCommand::Model {
            name,
            provider,
            model_id,
        } => {
            let provider = required(provider, "Provider")?;
            let model_id = required(model_id, "Model ID")?;
            config.add_model(name.clone(), Model { provider, model_id })?;
            config.save()?;
            println!("Saved model {name}");
        }
    }
    Ok(())
}

fn required(value: Option<String>, label: &str) -> Result<String> {
    if let Some(value) = value {
        if !value.trim().is_empty() {
            return Ok(value);
        }
    }

    print!("{label}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim().to_string();
    if value.is_empty() {
        bail!("{label} cannot be empty");
    }
    Ok(value)
}

fn parse_kind(value: &str) -> Result<ProviderKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "anthropic" => Ok(ProviderKind::Anthropic),
        "anthropic-compatible" | "compatible" => Ok(ProviderKind::AnthropicCompatible),
        other => bail!("unsupported provider kind `{other}`"),
    }
}
