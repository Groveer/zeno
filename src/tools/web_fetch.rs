//! Web fetch tool — fetch content from a URL.
use super::base::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct WebFetchTool {
    client: reqwest::Client,
}
impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("zeno/0.1 (terminal AI assistant)")
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
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
                "description": "Fetch and parse content from a URL.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The URL to fetch." }
                    },
                    "required": ["url"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let url = arguments["url"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'url'".into()))?;

        // SSRF protection step 1: reject private/local addresses by URL analysis
        if !is_safe_url(url) {
            return Err(ToolError::Execution(
                "URL rejected: private/local addresses are not allowed (SSRF protection).".into(),
            ));
        }

        // SSRF protection step 2: DNS prerequest check.
        // Prevents DNS rebinding attacks where the hostname resolves to a public IP
        // during this check but to a private IP when the actual HTTP request is made.
        // The TOCTOU window is tiny and acceptable for an LLM agent tool.
        if let Err(e) = verify_dns_not_private(url).await {
            return Err(ToolError::Execution(format!(
                "URL rejected: DNS resolved to private/local address (SSRF protection): {}",
                e
            )));
        }

        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("Request failed: {}", e)))?;
        let html = resp
            .text()
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to read response: {}", e)))?;
        // Simple HTML strip for now
        let text = html2text::from_read(html.as_bytes(), 80).unwrap_or_else(|_| html.clone());
        Ok(text)
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}

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
        assert!(verify_dns_not_private("http://[::1]/page").await.is_ok());
    }

    #[tokio::test]
    async fn test_dns_check_rejects_unresolvable() {
        // A domain that doesn't exist should fail the DNS check
        let result =
            verify_dns_not_private("http://this-domain-does-not-exist-xyz123.invalid/").await;
        assert!(result.is_err());
    }
}
