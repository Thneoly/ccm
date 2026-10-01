use std::process::{Command, Stdio};

use anyhow::Result;

use crate::{config::AppConfig, credential};

pub async fn run(config: &AppConfig) -> Result<()> {
    println!("CCM doctor\n");

    check_claude();

    let Some(current) = config.current.as_deref() else {
        println!("! current model: not selected");
        return Ok(());
    };
    println!("✓ current model: {current}");

    let Some(model) = config.models.get(current) else {
        println!("✗ model `{current}` is missing from config");
        return Ok(());
    };
    println!("✓ model id: {}", model.model_id);

    let Some(provider) = config.providers.get(&model.provider) else {
        println!("✗ provider `{}` is missing from config", model.provider);
        return Ok(());
    };
    println!("✓ provider: {}", model.provider);
    println!("✓ base URL: {}", provider.base_url);

    match credential::get(&model.provider) {
        Ok(_) => println!("✓ credential: present"),
        Err(_) => println!("✗ credential: missing (`ccm auth set {}`)", model.provider),
    }

    match reqwest::Client::new().get(&provider.base_url).send().await {
        Ok(response) => println!("✓ endpoint: reachable ({})", response.status()),
        Err(error) => println!("✗ endpoint: {error}"),
    }

    println!("\nFor a full authenticated model check, run `ccm health {current}`.");
    Ok(())
}

fn check_claude() {
    let result = Command::new("claude")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => println!("✓ Claude Code: installed"),
        _ => println!("✗ Claude Code: not found on PATH"),
    }
}
