mod api;
mod cli;
mod config;
mod engine;

use clap::Parser;
use config::settings;
use engine::messages::ConversationHistory;
use engine::query_engine::QueryEngine;

/// Rust AI Coding Assistant
#[derive(Parser)]
#[command(name = "rc", version, about = "Rust AI Coding Assistant")]
struct Cli {
    /// User prompt (non-interactive mode)
    prompt: Option<String>,

    /// Override provider
    #[arg(long)]
    provider: Option<String>,

    /// Override model
    #[arg(long)]
    model: Option<String>,

    /// Max turns for the tool loop
    #[arg(long, default_value_t = 8)]
    max_turns: u32,
}

/// Build an API client based on the provider name.
/// Known providers get specialized clients; others default to OpenAI-compatible.
fn build_client(
    provider_name: &str,
    provider_config: &config::settings::ProviderConfig,
) -> anyhow::Result<Box<dyn api::client::SupportsStreamingMessages>> {
    let api_key = settings::resolve_api_key(provider_config)?;
    let base_url = provider_config.base_url.clone();

    let client: Box<dyn api::client::SupportsStreamingMessages> =
        match provider_name {
            "anthropic" => Box::new(api::anthropic::AnthropicClient::new(api_key, base_url)),
            _ => Box::new(api::openai::OpenAIClient::new(api_key, base_url)),
        };

    Ok(client)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rcode=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let settings = settings::load()?;

    let provider_name = cli
        .provider
        .as_deref()
        .unwrap_or(&settings.active_provider);
    let model = cli.model.as_deref().unwrap_or(&settings.model);

    let provider_config = settings.providers.get(provider_name).ok_or_else(|| {
        anyhow::anyhow!(
            "Provider '{}' not configured. Add it to ~/.config/rcode/config.yaml",
            provider_name
        )
    })?;

    let client = build_client(provider_name, provider_config)?;

    let mut engine = QueryEngine::new(
        client,
        model.to_string(),
        String::new(), // system prompt — Phase 4
        ConversationHistory::new(),
        cli.max_turns,
        settings.max_tokens,
    );

    match cli.prompt {
        Some(prompt) => {
            // Non-interactive: single query
            engine.query(&prompt).await?;
            println!();
        }
        None => {
            // Interactive: simple readline loop (TUI comes in Phase 3)
            println!("rcode v{} — type /exit to quit", env!("CARGO_PKG_VERSION"));
            println!("Provider: {} | Model: {}", provider_name, model);

            loop {
                print!("> ");
                std::io::Write::flush(&mut std::io::stdout())?;

                let mut input = String::new();
                let bytes_read = std::io::stdin().read_line(&mut input)?;
                if bytes_read == 0 {
                    break; // EOF
                }
                let input = input.trim();
                if input.is_empty() {
                    continue;
                }
                if input == "/exit" || input == "/quit" || input == "/q" {
                    break;
                }
                if input == "/clear" {
                    engine.history.clear();
                    println!("History cleared.");
                    continue;
                }

                let _ = engine.query(input).await;
                println!();
            }
        }
    }

    Ok(())
}
