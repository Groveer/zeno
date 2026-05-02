//! Anthropic Messages API client with streaming support.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use reqwest::Client;
use serde_json::json;

use crate::api::client::SupportsStreamingMessages;
use crate::api::sse;
use crate::api::types::{ApiError, Message, Role, StreamEvent};
use crate::config::model_context::DEFAULT_MAX_OUTPUT_TOKENS;

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicClient {
    http: Client,
    api_key: String,
    base_url: String,
}

impl AnthropicClient {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self {
            http: Client::new(),
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Convert tools from OpenAI format to Anthropic format.
    ///
    /// OpenAI: `{ "type": "function", "function": { "name": "...", "parameters": {...} } }`
    /// Anthropic: `{ "name": "...", "description": "...", "input_schema": {...} }`
    fn convert_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
        tools
            .iter()
            .filter_map(|t| {
                let func = t.get("function")?;
                let name = func.get("name")?.as_str()?;
                let description = func
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                let parameters = func.get("parameters").cloned().unwrap_or(json!({}));

                Some(json!({
                    "name": name,
                    "description": description,
                    "input_schema": parameters,
                }))
            })
            .collect()
    }

    fn build_request_body(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        max_tokens: Option<u32>,
    ) -> serde_json::Value {
        // Anthropic's Messages API requires max_tokens as a mandatory field.
        // When None (auto mode), fall back to DEFAULT_MAX_OUTPUT_TOKENS.
        let effective = max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let mut body = json!({
            "model": model,
            "max_tokens": effective,
            "stream": true,
        });

        if !system.is_empty() {
            body["system"] = json!(system);
        }

        // Anthropic accepts our ContentBlock format directly
        // (type: "text", type: "tool_use", type: "tool_result")
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                let content: Vec<serde_json::Value> = m
                    .content
                    .iter()
                    .filter_map(|block| serde_json::to_value(block).ok())
                    .collect();
                let role_str = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                json!({
                    "role": role_str,
                    "content": content,
                })
            })
            .collect();

        body["messages"] = json!(api_messages);

        // Convert tools to Anthropic format
        if !tools.is_empty() {
            body["tools"] = json!(Self::convert_tools(tools));
            // Explicitly allow the model to return multiple tool_use blocks in
            // a single response. Anthropic's default permits this, but setting
            // disable_parallel_tool_use: false makes the intent explicit and
            // prevents future API changes from breaking parallel execution.
            body["tool_choice"] = json!({
                "type": "auto",
                "disable_parallel_tool_use": false,
            });
        }

        body
    }
}

#[async_trait]
impl SupportsStreamingMessages for AnthropicClient {
    async fn stream_messages(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        max_tokens: Option<u32>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>, ApiError> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = self.build_request_body(model, system, messages, tools, max_tokens);

        let response = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(ApiError::Request)?;

        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            let status_code = status.as_u16();
            // Extract response headers before consuming the body.
            // The Retry-After header is essential for proper 429 retry backoff.
            let resp_headers = response.headers().clone();
            let body_text = response.text().await.unwrap_or_default();
            if status_code == 402 {
                return Err(ApiError::PaymentRequired);
            }
            let body = if body_text.is_empty() && status.is_server_error() {
                format!(
                    "(empty response body — server error {status_code}. \
                     Possible causes: reverse proxy cannot reach upstream, \
                     or the API endpoint URL is incorrect.)"
                )
            } else {
                body_text
            };
            // Convert reqwest headers to our HashMap for ApiError
            let headers: Option<std::collections::HashMap<String, String>> = {
                let mut map = std::collections::HashMap::new();
                for (key, value) in resp_headers.iter() {
                    if let Ok(v) = value.to_str() {
                        map.insert(key.to_string(), v.to_string());
                    }
                }
                if map.is_empty() { None } else { Some(map) }
            };
            return Err(ApiError::Http {
                status: status_code,
                body,
                headers,
            });
        }

        Ok(sse::parse_sse_stream(response))
    }
}
