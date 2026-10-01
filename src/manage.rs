use std::io::{self, Write};

use anyhow::{bail, Context, Result};

use crate::{
    cli::AddCommand,
    config::AppConfig,
    model::Model,
    provider::{Provider, ProviderKind},
    route::{Route, RoutePolicy},
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
        AddCommand::Route {
            name,
            primary,
            fallback,
            header_timeout_ms,
            fallback_on,
            max_attempts,
            backoff_ms,
        } => {
            let primary = required(primary, "Primary model")?;
            let fallback = parse_csv_strings(fallback.unwrap_or_default());
            let fallback_on = parse_status_codes(&fallback_on)?;
            let policy = RoutePolicy {
                header_timeout_ms,
                fallback_on,
                max_attempts,
                backoff_ms,
            };
            config.add_route(
                name.clone(),
                Route {
                    primary,
                    fallback,
                    policy,
                },
            )?;
            config.save()?;
            println!("Saved route {name}");
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

fn parse_csv_strings(value: String) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_status_codes(value: &str) -> Result<Vec<u16>> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<u16>()
                .with_context(|| format!("invalid HTTP status code `{value}`"))
        })
        .collect()
}

fn parse_kind(value: &str) -> Result<ProviderKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "anthropic" => Ok(ProviderKind::Anthropic),
        "anthropic-compatible" | "compatible" => Ok(ProviderKind::AnthropicCompatible),
        other => bail!("unsupported provider kind `{other}`"),
    }
}
