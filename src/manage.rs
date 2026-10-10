use std::io::{self, Write};

use anyhow::{bail, Context, Result};

use crate::{
    cli::AddCommand,
    config::AppConfig,
    model::{Model, ModelRouting},
    provider::{Provider, ProviderAuth, ProviderKind},
    route::{CircuitBreakerPolicy, Route, RoutePolicy, SelectionStrategy, SelectionWeights},
};

pub fn handle(config: &mut AppConfig, command: AddCommand) -> Result<()> {
    match command {
        AddCommand::Provider {
            name,
            base_url,
            kind,
            auth,
        } => {
            let base_url = required(base_url, "Base URL")?;
            let kind = required(
                kind,
                "Kind (anthropic / anthropic-compatible / openai-compatible)",
            )?;
            let kind = parse_kind(&kind)?;
            let auth = match auth {
                Some(value) => Some(parse_auth(&value)?),
                None => None,
            };
            config.add_provider(
                name.clone(),
                Provider {
                    kind,
                    base_url,
                    auth,
                },
            );
            config.save()?;
            println!("Saved provider {name}");
        }
        AddCommand::Model {
            name,
            provider,
            model_id,
            cost_weight,
            quality_weight,
            context_window,
        } => {
            let provider = required(provider, "Provider")?;
            let model_id = required(model_id, "Model ID")?;
            config.add_model(
                name.clone(),
                Model {
                    provider,
                    model_id,
                    context_window,
                    routing: ModelRouting {
                        cost_weight,
                        quality_weight,
                    },
                    // `ccm add model` has no pricing flags (M6): prices are
                    // hand-edited TOML facts, not CLI defaults.
                    pricing: None,
                },
            )?;
            config.save()?;
            println!("Saved model {name}");
        }
        AddCommand::Route {
            name,
            primary,
            fallback,
            selection,
            reliability_weight,
            latency_weight,
            cost_weight,
            quality_weight,
            header_timeout_ms,
            fallback_on,
            max_attempts,
            backoff_ms,
            circuit_enabled,
            failure_threshold,
            circuit_open_ms,
        } => {
            let primary = required(primary, "Primary model")?;
            let fallback = parse_csv_strings(fallback.unwrap_or_default());
            let fallback_on = parse_status_codes(&fallback_on)?;
            let selection = parse_selection(&selection)?;
            let policy = RoutePolicy {
                selection,
                weights: SelectionWeights {
                    reliability: reliability_weight,
                    latency: latency_weight,
                    cost: cost_weight,
                    quality: quality_weight,
                },
                header_timeout_ms,
                fallback_on,
                max_attempts,
                backoff_ms,
                circuit_breaker: CircuitBreakerPolicy {
                    enabled: circuit_enabled,
                    failure_threshold,
                    open_ms: circuit_open_ms,
                },
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

fn parse_selection(value: &str) -> Result<SelectionStrategy> {
    match value.trim().to_ascii_lowercase().as_str() {
        "ordered" => Ok(SelectionStrategy::Ordered),
        "healthiest" => Ok(SelectionStrategy::Healthiest),
        "lowest-latency" | "lowest_latency" | "latency" => {
            Ok(SelectionStrategy::LowestLatency)
        }
        "lowest-cost" | "lowest_cost" | "cost" => Ok(SelectionStrategy::LowestCost),
        "weighted" => Ok(SelectionStrategy::Weighted),
        other => bail!(
            "unsupported selection strategy `{other}`; expected ordered, healthiest, lowest-latency, lowest-cost, or weighted"
        ),
    }
}

fn parse_auth(value: &str) -> Result<ProviderAuth> {
    match value.trim().to_ascii_lowercase().as_str() {
        "x-api-key" | "x_api_key" | "apikey" | "api-key" => Ok(ProviderAuth::XApiKey),
        "bearer" | "authorization" => Ok(ProviderAuth::Bearer),
        other => bail!("unsupported provider auth `{other}`; expected x-api-key or bearer"),
    }
}

fn parse_kind(value: &str) -> Result<ProviderKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "anthropic" => Ok(ProviderKind::Anthropic),
        "anthropic-compatible" | "compatible" => Ok(ProviderKind::AnthropicCompatible),
        "openai-compatible" | "openai" => Ok(ProviderKind::OpenAICompatible),
        other => bail!("unsupported provider kind `{other}`"),
    }
}
