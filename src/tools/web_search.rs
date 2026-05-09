//! Web search tool — search the web via SearXNG, DuckDuckGo, Brave, or Tavily.
//!
//! Users can customize the search provider via `zn.web_search({...})` in init.lua:
//!
//! ```lua
//! zn.web_search({ provider = "brave", api_key_env = "BRAVE_API_KEY" })
//! zn.web_search({ provider = "tavily", api_key_env = "TAVILY_API_KEY" })
//! zn.web_search({ provider = "searxng", url = "http://localhost:8888" })
//! ```

use super::base::{Tool, ToolContext, ToolError};
use crate::config::settings::WebSearchConfig;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};

const DEFAULT_SEARXNG_URL: &str = "https://searx.be";

pub struct WebSearchTool {
    client: Client,
    config: WebSearchConfig,
}

impl WebSearchTool {
    /// Create with user-supplied config from init.lua.
    pub fn with_config(config: WebSearchConfig) -> Self {
        Self {
            client: Self::build_client(),
            config,
        }
    }

    fn build_client() -> Client {
        Client::builder()
            .user_agent("zeno/0.1 (terminal AI assistant)")
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web for information. Returns a list of results with titles, URLs, and snippets.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of results (default: 5, max: 10).",
                            "default": 5
                        }
                    },
                    "required": ["query"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let query = arguments["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'query'".into()))?;
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .min(10) as usize;

        match self.config.provider.as_str() {
            "brave" => self.search_brave(query, limit).await,
            "tavily" => self.search_tavily(query, limit).await,
            "duckduckgo" => self.search_duckduckgo(query, limit).await,
            // "searxng" or any unknown value: try SearXNG with DuckDuckGo fallback
            _ => match self.search_searxng(query, limit).await {
                Ok(results) if !results.is_empty() => Ok(results),
                Ok(_) => {
                    tracing::debug!(
                        event = "search_fallback",
                        reason = "no_results",
                        "SearXNG returned no results, trying DuckDuckGo"
                    );
                    self.search_duckduckgo(query, limit).await
                }
                Err(e) => {
                    tracing::debug!(
                        event = "search_fallback",
                        reason = "error",
                        error = %e,
                        "SearXNG search failed, trying DuckDuckGo"
                    );
                    self.search_duckduckgo(query, limit).await
                }
            },
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Provider implementations
// ---------------------------------------------------------------------------

impl WebSearchTool {
    // -- SearXNG (default) --

    async fn search_searxng(&self, query: &str, limit: usize) -> Result<String, ToolError> {
        let base = if self.config.url.is_empty() {
            DEFAULT_SEARXNG_URL
        } else {
            &self.config.url
        };
        let url = format!(
            "{}/search?q={}&format=json",
            base,
            urlencoding::encode(query)
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("SearXNG request failed: {}", e)))?;
        if !resp.status().is_success() {
            return Err(ToolError::Execution(format!(
                "SearXNG returned HTTP {}",
                resp.status()
            )));
        }
        let data: Value = resp.json().await.map_err(|e| {
            ToolError::Execution(format!("Failed to parse SearXNG response: {}", e))
        })?;

        let results = data["results"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .take(limit)
            .filter_map(|item| {
                let title = item["title"].as_str().unwrap_or("");
                let url = item["url"].as_str().unwrap_or("");
                let snippet = item["content"].as_str().unwrap_or("");
                if title.is_empty() && url.is_empty() {
                    None
                } else {
                    Some(format!("**{}**\n{}\n{}", title, url, snippet))
                }
            })
            .collect::<Vec<_>>();

        if results.is_empty() {
            Ok(String::new())
        } else {
            Ok(format!(
                "Found {} result(s):\n\n{}",
                results.len(),
                results.join("\n\n")
            ))
        }
    }

    // -- DuckDuckGo Lite --

    async fn search_duckduckgo(&self, query: &str, limit: usize) -> Result<String, ToolError> {
        let url = format!(
            "https://lite.duckduckgo.com/lite?q={}",
            urlencoding::encode(query)
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("DuckDuckGo request failed: {}", e)))?;
        if !resp.status().is_success() {
            return Err(ToolError::Execution(format!(
                "DuckDuckGo returned HTTP {}",
                resp.status()
            )));
        }
        let html = resp.text().await.map_err(|e| {
            ToolError::Execution(format!("Failed to read DuckDuckGo response: {}", e))
        })?;
        let results = parse_ddg_lite(&html, limit);
        if results.is_empty() {
            Ok(format!("No results found for '{}'.", query))
        } else {
            Ok(format!(
                "Found {} result(s):\n\n{}",
                results.len(),
                results.join("\n\n")
            ))
        }
    }

    // -- Brave Search API --

    async fn search_brave(&self, query: &str, limit: usize) -> Result<String, ToolError> {
        let api_key = self.config.resolve_api_key().ok_or_else(|| {
            ToolError::Execution(
                "Brave Search requires an API key. Set api_key_env or api_key in zn.web_search()."
                    .into(),
            )
        })?;

        let url = format!(
            "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
            urlencoding::encode(query),
            limit
        );
        let resp = self
            .client
            .get(&url)
            .header("X-Subscription-Token", &api_key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("Brave Search request failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::Execution(format!(
                "Brave Search returned HTTP {}: {}",
                status, body
            )));
        }

        let data: Value = resp.json().await.map_err(|e| {
            ToolError::Execution(format!("Failed to parse Brave Search response: {}", e))
        })?;

        let results = data["web"]["results"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .take(limit)
            .filter_map(|item| {
                let title = item["title"].as_str().unwrap_or("");
                let url = item["url"].as_str().unwrap_or("");
                let snippet = item["description"].as_str().unwrap_or("");
                if title.is_empty() && url.is_empty() {
                    None
                } else {
                    Some(format!("**{}**\n{}\n{}", title, url, snippet))
                }
            })
            .collect::<Vec<_>>();

        if results.is_empty() {
            Ok(format!("No results found for '{}'.", query))
        } else {
            Ok(format!(
                "Found {} result(s):\n\n{}",
                results.len(),
                results.join("\n\n")
            ))
        }
    }

    // -- Tavily Search API --

    async fn search_tavily(&self, query: &str, limit: usize) -> Result<String, ToolError> {
        let api_key = self.config.resolve_api_key().ok_or_else(|| {
            ToolError::Execution(
                "Tavily Search requires an API key. Set api_key_env or api_key in zn.web_search()."
                    .into(),
            )
        })?;

        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .json(&json!({
                "api_key": api_key,
                "query": query,
                "max_results": limit,
            }))
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("Tavily Search request failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::Execution(format!(
                "Tavily Search returned HTTP {}: {}",
                status, body
            )));
        }

        let data: Value = resp.json().await.map_err(|e| {
            ToolError::Execution(format!("Failed to parse Tavily Search response: {}", e))
        })?;

        let results = data["results"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .take(limit)
            .filter_map(|item| {
                let title = item["title"].as_str().unwrap_or("");
                let url = item["url"].as_str().unwrap_or("");
                let snippet = item["content"].as_str().unwrap_or("");
                if title.is_empty() && url.is_empty() {
                    None
                } else {
                    Some(format!("**{}**\n{}\n{}", title, url, snippet))
                }
            })
            .collect::<Vec<_>>();

        if results.is_empty() {
            Ok(format!("No results found for '{}'.", query))
        } else {
            Ok(format!(
                "Found {} result(s):\n\n{}",
                results.len(),
                results.join("\n\n")
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// DuckDuckGo HTML parsing helpers
// ---------------------------------------------------------------------------

fn parse_ddg_lite(html: &str, limit: usize) -> Vec<String> {
    let mut results = Vec::new();
    let mut seen_urls = std::collections::HashSet::new();
    for line in html.lines() {
        if results.len() >= limit {
            break;
        }
        if line.contains("rel=\"nofollow\"") {
            let url = extract_href(line).unwrap_or_default();
            if url.is_empty() || url.starts_with('/') || seen_urls.contains(&url) {
                continue;
            }
            seen_urls.insert(url.clone());
            let title = extract_tag_content(line).unwrap_or_else(|| url.clone());
            results.push(format!("**{}**\n{}", title, url));
        }
    }
    results
}

fn extract_href(line: &str) -> Option<String> {
    let start = line.find("href=\"")? + 6;
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_tag_content(line: &str) -> Option<String> {
    let start = line.find('>')? + 1;
    let rest = &line[start..];
    let end = rest.find('<').unwrap_or(rest.len());
    let text = rest[..end].trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(
            text.replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\""),
        )
    }
}
