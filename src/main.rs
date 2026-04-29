mod api;
mod cli;
mod config;
mod engine;
mod permissions;
mod tools;

use clap::Parser;
use config::settings;
use engine::messages::ConversationHistory;
use engine::query_engine::QueryEngine;
use tools::base::ToolRegistry;

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

    /// Permission mode
    #[arg(long, value_name = "MODE")]
    permission: Option<String>,
}

/// Build an API client based on the provider name.
fn build_client(
    provider_name: &str,
    provider_config: &config::settings::ProviderConfig,
) -> anyhow::Result<Box<dyn api::client::SupportsStreamingMessages>> {
    let api_key = settings::resolve_api_key(provider_config)?;
    let base_url = provider_config.base_url.clone();

    let client: Box<dyn api::client::SupportsStreamingMessages> = match provider_name {
        "anthropic" => Box::new(api::anthropic::AnthropicClient::new(api_key, base_url)),
        _ => Box::new(api::openai::OpenAIClient::new(api_key, base_url)),
    };

    Ok(client)
}

/// Build the tool registry based on settings.
fn build_registry(tool_config: &config::settings::ToolsConfig) -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    if tool_config.bash {
        registry.register(Box::new(tools::bash::BashTool::new(true)));
    }
    if tool_config.file_read {
        registry.register(Box::new(tools::file_read::FileReadTool::new()));
    }
    if tool_config.file_write {
        registry.register(Box::new(tools::file_write::FileWriteTool::new()));
    }
    if tool_config.file_edit {
        registry.register(Box::new(tools::file_edit::FileEditTool::new()));
    }
    if tool_config.glob {
        registry.register(Box::new(tools::glob::GlobTool::new()));
    }
    if tool_config.grep {
        registry.register(Box::new(tools::grep::GrepTool::new()));
    }

    // Always-available tools
    registry.register(Box::new(tools::config_tool::ConfigTool::new()));
    registry.register(Box::new(tools::ask_user::AskUserTool::new()));

    registry
}

fn resolve_permission_mode(
    cli_value: Option<&str>,
    config_value: &config::settings::PermissionMode,
) -> anyhow::Result<config::settings::PermissionMode> {
    match cli_value {
        Some(s) => s.parse().map_err(|e: String| anyhow::anyhow!(e)),
        None => Ok(config_value.clone()),
    }
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
    let permission_mode = resolve_permission_mode(cli.permission.as_deref(), &settings.permissions)?;

    let provider_config = settings.providers.get(provider_name).ok_or_else(|| {
        anyhow::anyhow!(
            "Provider '{}' not configured. Add it to ~/.config/rcode/config.yaml",
            provider_name
        )
    })?;

    let client = build_client(provider_name, provider_config)?;
    let registry = build_registry(&settings.tools);

    let tool_names: Vec<String> = registry.names().into_iter().map(String::from).collect();
    tracing::info!("Registered tools: {:?}", tool_names);

    let mut engine = QueryEngine::new(
        client,
        model.to_string(),
        String::new(), // system prompt — Phase 4
        ConversationHistory::new(),
        registry,
        cli.max_turns,
        settings.max_tokens,
        permission_mode.clone(),
    );

    match cli.prompt {
        Some(prompt) => {
            // Non-interactive: single query
            let result = engine.query(&prompt).await?;
            println!();
            if result.tool_calls > 0 {
                eprintln!(
                    "[stats] {} tool call(s), {} tokens",
                    result.tool_calls,
                    result.usage.total()
                );
            }
        }
        None => {
            // Interactive: simple readline loop (TUI comes in Phase 3)
            println!("rcode v{} — type /exit to quit", env!("CARGO_PKG_VERSION"));
            println!(
                "Provider: {} | Model: {} | Tools: {} | Permissions: {}",
                provider_name,
                model,
                tool_names.len(),
                permission_mode,
            );

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
                if input == "/tools" {
                    println!("Registered tools: {}", tool_names.join(", "));
                    continue;
                }
                if input == "/cost" {
                    // Placeholder — will be implemented with cost_tracker
                    println!("Cost tracking not yet implemented (Phase 4).");
                    continue;
                }

                let result = engine.query(input).await;
                match result {
                    Ok(r) => {
                        println!();
                        if r.tool_calls > 0 {
                            eprintln!(
                                "[stats] {} tool call(s), {} tokens",
                                r.tool_calls,
                                r.usage.total()
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("\n[error] {}", e);
                    }
                }
            }
        }
    }

    Ok(())
}
