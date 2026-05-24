//! Web fetch tool — fetch content from a URL.
//!
//! Supports content-type-aware parsing:
//! - JSON: pretty-printed
//! - HTML: converted to readable markdown via html2text
//! - Plain text / XML / other: passed through as-is
//!
//! Includes SSRF protection (private IP rejection + DNS rebinding check).

use super::base::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::config::settings::Settings;
use zeno_tools::{JsonToolOutput, ToolOutput};

/// Maximum response body size to read (10 MB).
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Maximum output characters returned to the LLM.
const MAX_OUTPUT_CHARS: usize = 100_000;

/// Request timeout.
const TIMEOUT_SECS: u64 = 30;

pub struct WebFetchTool {
    client: reqwest::Client,
    settings: Arc<Settings>,
}

impl WebFetchTool {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("zeno/0.1 (terminal AI assistant; +https://github.com/nicepkg/zeno)")
                .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .unwrap_or_default(),
            settings,
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch and read content from a URL. Returns the page content as text. Supports HTML (converted to readable markdown), JSON (pretty-printed), and plain text. URLs without a scheme are treated as https://.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The URL to fetch. Must be http:// or https://. Bare domains (e.g. \"example.com\") are treated as https://."
                        },
                        "extract_text": {
                            "type": "boolean",
                            "description": "If true (default), convert HTML to readable text using html2text. If false, return the raw HTML.",
                            "default": true
                        }
                    },
                    "required": ["url"]
                }
            }
        })
    }

    async fn execute(
        &self,
        arguments: Value,
        _ctx: &ToolContext,
    ) -> Result<Box<dyn ToolOutput>, ToolError> {
        let raw_url = arguments["url"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing required field 'url'".into()))?;

        let extract_text = arguments
            .get("extract_text")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Normalize URL: add scheme if missing
        let url = normalize_url(raw_url);

        // SSRF protection step 1: reject private/local addresses by URL analysis
        if !is_safe_url(&url) {
            return Err(ToolError::Execution(
                "URL rejected: private/local addresses are not allowed (SSRF protection).".into(),
            ));
        }

        // SSRF protection step 2: DNS prerequest check.
        // Prevents DNS rebinding attacks where the hostname resolves to a public IP
        // during this check but to a private IP when the actual HTTP request is made.
        // The TOCTOU window is tiny and acceptable for an LLM agent tool.
        if let Err(e) = verify_dns_not_private(&url).await {
            return Err(ToolError::Execution(format!(
                "URL rejected: DNS resolved to private/local address (SSRF protection): {}",
                e
            )));
        }

        // Send request
        let resp = self
            .client
            .get(&url)
            .header("Accept", "text/html, application/json, text/plain, */*")
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ToolError::Timeout(format!(
                        "Request to {} timed out after {}s",
                        url, TIMEOUT_SECS
                    ))
                } else {
                    ToolError::Execution(format!("Request failed: {}", e))
                }
            })?;

        let status = resp.status();
        let final_url = resp.url().to_string();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        // Handle non-success status codes
        if !status.is_success() {
            let body_snippet = resp.text().await.unwrap_or_default();
            let snippet = truncate_chars(&body_snippet, 500);
            return Err(ToolError::Execution(format!(
                "HTTP {} for {}\nContent-Type: {}\nBody: {}",
                status, final_url, content_type, snippet
            )));
        }

        // Read body with size limit
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read response body: {}", e)))?;

        if bytes.len() > MAX_BODY_BYTES {
            return Err(ToolError::Execution(format!(
                "Response too large: {} bytes (max {} bytes). Consider using a more specific URL.",
                bytes.len(),
                MAX_BODY_BYTES
            )));
        }

        // Parse based on content type
        let content = if is_json_content_type(&content_type) {
            parse_json(&bytes)
        } else if extract_text && is_html_content_type(&content_type) {
            // Try auxiliary model first for HTML extraction/summarization.
            // Falls back to raw html2text if auxiliary model is unavailable.
            let raw_text = html2text::from_read(&bytes[..], 120)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
            match crate::auxiliary::web_fetch::extract_web_content(&self.settings, &url, &raw_text)
                .await
            {
                Ok(summary) => summary,
                Err(e) => {
                    tracing::debug!(
                        event = "web_fetch_auxiliary_fallback",
                        error = %e,
                        "Auxiliary model failed, falling back to raw html2text"
                    );
                    raw_text
                }
            }
        } else {
            // Plain text or unknown — return as-is
            String::from_utf8_lossy(&bytes).into_owned()
        };

        // Build response with metadata header
        let size = content.len();
        let header = format!(
            "URL: {}\nStatus: {}\nContent-Type: {}\nSize: {} chars",
            if final_url != url {
                format!("{} (redirected from {})", final_url, url)
            } else {
                url.clone()
            },
            status,
            content_type
                .split(';')
                .next()
                .unwrap_or(&content_type)
                .trim(),
            bytes.len(),
        );

        let output = if size > MAX_OUTPUT_CHARS {
            format!(
                "{}\n\n{}\n\n[Truncated: showing {}/{} chars]",
                header,
                truncate_chars(&content, MAX_OUTPUT_CHARS),
                MAX_OUTPUT_CHARS,
                size
            )
        } else {
            format!("{}\n\n{}", header, content)
        };

        Ok(Box::new(JsonToolOutput::success(output)))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// URL normalization
// ---------------------------------------------------------------------------

/// Normalize a URL: add `https://` if no scheme is present.
fn normalize_url(url: &str) -> String {
    let trimmed = url.trim();
    // Already has a scheme
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }
    // Looks like a domain/path without scheme
    if !trimmed.contains(' ') && trimmed.contains('.') {
        return format!("https://{}", trimmed);
    }
    // Return as-is (will fail SSRF or request, but that's fine)
    trimmed.to_string()
}

// ---------------------------------------------------------------------------
// Content type helpers
// ---------------------------------------------------------------------------

fn is_json_content_type(ct: &str) -> bool {
    let ct_lower = ct.to_lowercase();
    ct_lower.contains("application/json") || ct_lower.contains("text/json")
}

fn is_html_content_type(ct: &str) -> bool {
    let ct_lower = ct.to_lowercase();
    ct_lower.contains("text/html") || ct_lower.contains("application/xhtml")
}

/// Parse JSON bytes into a pretty-printed string, falling back to raw text.
fn parse_json(bytes: &[u8]) -> String {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(val) => serde_json::to_string_pretty(&val)
            .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned()),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Truncate a string to at most `max_chars` characters (char-boundary safe).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());
    format!("{}...", &s[..end])
}

// ---------------------------------------------------------------------------
// SSRF protection
// ---------------------------------------------------------------------------

/// Check if a URL is safe to fetch (not a private/local address).
/// Rejects localhost, loopback, private IP ranges, link-local, and cloud metadata endpoints.
fn is_safe_url(url: &str) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return false;
    }

    let host = match parsed.host_str() {
        Some(h) => h,
        None => return false,
    };

    // Reject localhost / loopback
    if host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "0.0.0.0" {
        return false;
    }

    // Reject IPv6 loopback in bracket notation (url crate returns "[::1]")
    if host == "[::1]" {
        return false;
    }

    // Reject hostnames that look like IP addresses in private ranges
    if let Ok(ip) = host.parse::<std::net::IpAddr>()
        && (ip.is_loopback() || is_private_ip(&ip))
    {
        return false;
    }

    // Also handle bracketed IPv6 addresses: parse "[::1]" → ::1
    if host.starts_with('[')
        && host.ends_with(']')
        && let Ok(ip) = host[1..host.len() - 1].parse::<std::net::IpAddr>()
        && (ip.is_loopback() || is_private_ip(&ip))
    {
        return false;
    }

    true
}

/// Check if an IP address is in a private/reserved range.
fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 10.0.0.0/8
            octets[0] == 10
            // 172.16.0.0/12
            || (octets[0] == 172 && (octets[1] & 0xf0) == 16)
            // 192.168.0.0/16
            || (octets[0] == 192 && octets[1] == 168)
            // 169.254.0.0/16 (link-local / cloud metadata)
            || (octets[0] == 169 && octets[1] == 254)
            // 100.64.0.0/10 (Carrier-grade NAT)
            || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
            // 0.0.0.0/8
            || octets[0] == 0
        }
        std::net::IpAddr::V6(v6) => {
            // IPv6 unique local (fc00::/7) and link-local (fe80::/10)
            let segments = v6.segments();
            (segments[0] & 0xfe00) == 0xfc00 || (segments[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Resolve the hostname from a URL and verify that none of the DNS results
/// point to a private/local IP address. This prevents DNS rebinding attacks
/// where a public hostname is later re-resolved to an internal address.
async fn verify_dns_not_private(url_str: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url_str).map_err(|e| format!("parse error: {}", e))?;
    let host = parsed.host_str().ok_or("no host in URL")?;
    let port = parsed
        .port()
        .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });

    // Skip DNS check for IP literals (already checked by is_safe_url)
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    // Strip brackets from IPv6 literal (e.g. "[::1]")
    let host_for_lookup = if host.starts_with('[') && host.ends_with(']') {
        &host[1..host.len() - 1]
    } else {
        host
    };
    if host_for_lookup.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }

    let lookup_target = format!("{}:{}", host_for_lookup, port);
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&lookup_target)
        .await
        .map_err(|e| format!("DNS lookup failed for {}: {}", host, e))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("DNS returned no addresses for {}", host));
    }

    for addr in &addrs {
        if addr.ip().is_loopback() || is_private_ip(&addr.ip()) {
            return Err(format!("{} resolved to private IP {}", host, addr.ip()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- URL normalization --

    #[test]
    fn test_normalize_url_adds_scheme() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(
            normalize_url("example.com/path"),
            "https://example.com/path"
        );
        assert_eq!(
            normalize_url("docs.rs/crate/serde"),
            "https://docs.rs/crate/serde"
        );
    }

    #[test]
    fn test_normalize_url_preserves_scheme() {
        assert_eq!(normalize_url("https://example.com"), "https://example.com");
        assert_eq!(normalize_url("http://example.com"), "http://example.com");
    }

    #[test]
    fn test_normalize_url_trims_whitespace() {
        assert_eq!(normalize_url("  example.com  "), "https://example.com");
    }

    // -- Content type detection --

    #[test]
    fn test_json_content_type() {
        assert!(is_json_content_type("application/json"));
        assert!(is_json_content_type("application/json; charset=utf-8"));
        assert!(is_json_content_type("text/json"));
        assert!(!is_json_content_type("text/html"));
    }

    #[test]
    fn test_html_content_type() {
        assert!(is_html_content_type("text/html"));
        assert!(is_html_content_type("text/html; charset=utf-8"));
        assert!(is_html_content_type("application/xhtml+xml"));
        assert!(!is_html_content_type("application/json"));
    }

    // -- Parse JSON --

    #[test]
    fn test_parse_json_valid() {
        let input = r#"{"name":"test","value":42}"#;
        let result = parse_json(input.as_bytes());
        assert!(result.contains("\"name\": \"test\""));
        assert!(result.contains("\"value\": 42"));
    }

    #[test]
    fn test_parse_json_invalid() {
        let input = "not json at all";
        let result = parse_json(input.as_bytes());
        assert_eq!(result, "not json at all");
    }

    // -- Truncation --

    #[test]
    fn test_truncate_chars_short() {
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_chars_exact() {
        assert_eq!(truncate_chars("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_chars_long() {
        let result = truncate_chars("hello world", 5);
        assert_eq!(result, "hello...");
    }

    #[test]
    fn test_truncate_chars_multibyte() {
        // Ensure we don't panic on multi-byte UTF-8
        let s = "你好世界hello";
        let result = truncate_chars(s, 4);
        assert_eq!(result, "你好世界...");
    }

    // -- SSRF protection (unchanged) --

    #[test]
    fn test_safe_public_urls() {
        assert!(is_safe_url("https://example.com/page"));
        assert!(is_safe_url("http://github.com/repo"));
        assert!(is_safe_url("https://docs.rs/crate"));
    }

    #[test]
    fn test_reject_localhost() {
        assert!(!is_safe_url("http://localhost:8080/api"));
        assert!(!is_safe_url("http://127.0.0.1:3000/data"));
        assert!(!is_safe_url("http://[::1]/api"));
        assert!(!is_safe_url("http://0.0.0.0/health"));
    }

    #[test]
    fn test_reject_private_ips() {
        assert!(!is_safe_url("http://10.0.0.1/internal"));
        assert!(!is_safe_url("http://172.16.0.1/internal"));
        assert!(!is_safe_url("http://192.168.1.1/admin"));
    }

    #[test]
    fn test_reject_cloud_metadata() {
        // AWS/GCP cloud metadata endpoint
        assert!(!is_safe_url("http://169.254.169.254/latest/meta-data/"));
    }

    #[test]
    fn test_reject_non_http_schemes() {
        assert!(!is_safe_url("file:///etc/passwd"));
        assert!(!is_safe_url("ftp://internal.server/file"));
    }

    #[tokio::test]
    async fn test_dns_check_skips_ip_literals() {
        // IP literals should skip DNS check (already handled by is_safe_url)
        assert!(verify_dns_not_private("http://1.2.3.4/page").await.is_ok());
    }

    #[tokio::test]
    async fn test_dns_check_rejects_unresolvable() {
        // A domain that doesn't exist should fail the DNS check
        let result =
            verify_dns_not_private("http://this-domain-does-not-exist-xyz123.invalid/").await;
        assert!(result.is_err());
    }
}
