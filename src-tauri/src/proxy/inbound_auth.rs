//! Per-install inbound capability token for privileged proxy actions.
//!
//! Official-account inject and hosted sidecar calls spend ChatGPT / Claude
//! OAuth. `PROXY_MANAGED` is a public placeholder written into live CLI
//! configs, so it is not a capability secret. Clients must present this
//! install token (Authorization bearer or `x-cc-switch-proxy`) before those
//! paths run. Claude Desktop's gateway token is accepted as an equivalent.

use crate::database::Database;
use crate::error::AppError;
use http::HeaderMap;
use serde_json::{json, Map, Value};
use std::net::{IpAddr, SocketAddr};

pub const LEGACY_PLACEHOLDER: &str = "PROXY_MANAGED";
pub const INBOUND_HEADER: &str = "x-cc-switch-proxy";
const SETTING_KEY: &str = "proxy_inbound_token";
const TOKEN_PREFIX: &str = "ccs-proxy-";
const CLAUDE_CUSTOM_HEADERS_ENV: &str = "ANTHROPIC_CUSTOM_HEADERS";

pub fn get_or_create_inbound_token(db: &Database) -> Result<String, AppError> {
    if let Some(token) = db.get_setting(SETTING_KEY)? {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let token = format!("{TOKEN_PREFIX}{}", uuid::Uuid::new_v4().simple());
    db.set_setting(SETTING_KEY, &token)?;
    Ok(token)
}

pub fn inbound_capability_tokens(db: &Database) -> Vec<String> {
    let mut tokens = Vec::new();
    if let Ok(token) = get_or_create_inbound_token(db) {
        tokens.push(token);
    }
    if let Some(token) = crate::claude_desktop_config::existing_gateway_token(db) {
        tokens.push(token);
    }
    tokens
}

pub fn is_legacy_placeholder(value: &str) -> bool {
    value.contains(LEGACY_PLACEHOLDER)
}

pub fn is_proxy_auth_placeholder(value: &str) -> bool {
    let token = extract_bearer(value);
    is_legacy_placeholder(token) || token.starts_with(TOKEN_PREFIX)
}

pub fn extract_bearer(value: &str) -> &str {
    value
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| value.trim().strip_prefix("bearer "))
        .unwrap_or(value)
        .trim()
}

pub fn is_public_health_path(path: &str) -> bool {
    matches!(path, "/health" | "/healthz")
}

pub fn is_status_path(path: &str) -> bool {
    path == "/status"
}

pub fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

pub fn is_loopback_peer(peer: SocketAddr) -> bool {
    is_loopback_ip(peer.ip())
}

/// `/health` is always public. `/status` always requires the inbound
/// capability token. Other paths keep the historical no-token CLI behavior
/// for loopback peers; any other peer must present the token.
pub fn inbound_peer_exempt(path: &str, peer: Option<SocketAddr>) -> bool {
    if is_public_health_path(path) {
        return true;
    }
    if is_status_path(path) {
        return false;
    }
    peer.is_some_and(is_loopback_peer)
}

pub fn inbound_request_allowed(
    path: &str,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    inbound_tokens: &[String],
) -> bool {
    inbound_peer_exempt(path, peer) || presents_inbound_secret(headers, inbound_tokens)
}

pub fn presents_inbound_secret(headers: &HeaderMap, inbound_tokens: &[String]) -> bool {
    if inbound_tokens.is_empty() {
        return false;
    }
    presented_secrets(headers).iter().any(|got| {
        inbound_tokens
            .iter()
            .any(|expected| secrets_equal(got, expected))
    })
}

fn presented_secrets(headers: &HeaderMap) -> Vec<String> {
    let mut secrets = Vec::new();
    if let Some(value) = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        let bearer = extract_bearer(value);
        if !bearer.is_empty() && !is_legacy_placeholder(bearer) {
            secrets.push(bearer.to_string());
        }
    }
    if let Some(value) = headers
        .get(INBOUND_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        secrets.push(value.to_string());
    }
    secrets
}

fn secrets_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

pub fn attach_claude_inbound_header(config: &mut Value, token: &str) {
    let token = token.trim();
    if token.is_empty() {
        return;
    }
    let Some(root) = config.as_object_mut() else {
        return;
    };
    match root.get("env") {
        Some(Value::Object(_)) => {}
        _ => {
            root.insert("env".to_string(), json!({}));
        }
    }
    let Some(env) = root.get_mut("env").and_then(Value::as_object_mut) else {
        return;
    };
    upsert_custom_header_env(env, token);
}

fn upsert_custom_header_env(env: &mut Map<String, Value>, token: &str) {
    let line = format!("{INBOUND_HEADER}: {token}");
    let existing = env
        .get(CLAUDE_CUSTOM_HEADERS_ENV)
        .and_then(Value::as_str)
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            !line
                .to_ascii_lowercase()
                .starts_with(&format!("{INBOUND_HEADER}:"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let merged = if existing.is_empty() {
        line
    } else {
        format!("{existing}\n{line}")
    };
    env.insert(CLAUDE_CUSTOM_HEADERS_ENV.to_string(), json!(merged));
}

pub fn attach_codex_inbound_http_header(
    config_text: &str,
    token: &str,
) -> Result<String, AppError> {
    let token = token.trim();
    if token.is_empty() {
        return Ok(config_text.to_string());
    }
    let mut doc = config_text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| AppError::Message(format!("Invalid Codex config.toml: {error}")))?;

    let provider_id = doc
        .get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::to_string);

    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(|item| item.as_table_mut())
    {
        let mut attached = false;
        if let Some(provider_id) = provider_id.as_deref() {
            if let Some(table) = providers
                .get_mut(provider_id)
                .and_then(|item| item.as_table_mut())
            {
                set_http_header(table, token);
                attached = true;
            }
        }
        if !attached {
            if let Some((_, item)) = providers.iter_mut().next() {
                if let Some(table) = item.as_table_mut() {
                    set_http_header(table, token);
                }
            }
        }
    }

    Ok(doc.to_string())
}

fn set_http_header(table: &mut toml_edit::Table, token: &str) {
    let headers = table
        .entry("http_headers")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    if let Some(headers) = headers.as_table_mut() {
        headers[INBOUND_HEADER] = toml_edit::value(token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use std::net::SocketAddr;

    #[test]
    fn placeholder_detection() {
        assert!(is_proxy_auth_placeholder("PROXY_MANAGED"));
        assert!(is_proxy_auth_placeholder("Bearer PROXY_MANAGED"));
        assert!(is_proxy_auth_placeholder("ccs-proxy-abc"));
        assert!(is_proxy_auth_placeholder("Bearer ccs-proxy-abc"));
        assert!(is_proxy_auth_placeholder("bearer ccs-proxy-abc"));
        assert!(!is_proxy_auth_placeholder("sk-real"));
        assert!(!is_proxy_auth_placeholder("Bearer sk-real"));
    }

    #[test]
    fn secret_must_match_exactly() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer PROXY_MANAGED"),
        );
        assert!(!presents_inbound_secret(
            &headers,
            &["ccs-proxy-test".to_string()]
        ));

        headers.insert(INBOUND_HEADER, HeaderValue::from_static("ccs-proxy-test"));
        assert!(presents_inbound_secret(
            &headers,
            &["ccs-proxy-test".to_string()]
        ));
    }

    #[test]
    fn persist_token_across_reads() {
        let db = Database::memory().unwrap();
        let first = get_or_create_inbound_token(&db).unwrap();
        let second = get_or_create_inbound_token(&db).unwrap();
        assert!(first.starts_with(TOKEN_PREFIX));
        assert_eq!(first, second);
    }

    #[test]
    fn attach_claude_creates_env() {
        let mut config = json!({});
        attach_claude_inbound_header(&mut config, "ccs-proxy-test");
        let headers = config["env"][CLAUDE_CUSTOM_HEADERS_ENV]
            .as_str()
            .expect("custom headers");
        assert!(headers.contains("x-cc-switch-proxy: ccs-proxy-test"));
    }

    #[test]
    fn non_loopback_requires_inbound_token_except_health() {
        let tokens = vec!["ccs-proxy-secret".to_string()];
        let peer: SocketAddr = "192.168.1.10:4000".parse().unwrap();
        let headers = HeaderMap::new();

        assert!(inbound_request_allowed(
            "/health",
            Some(peer),
            &headers,
            &tokens
        ));
        assert!(inbound_request_allowed(
            "/healthz",
            Some(peer),
            &headers,
            &tokens
        ));
        assert!(!inbound_request_allowed(
            "/status",
            Some(peer),
            &headers,
            &tokens
        ));
        assert!(!inbound_request_allowed(
            "/v1/messages",
            Some(peer),
            &headers,
            &tokens
        ));
        assert!(!inbound_request_allowed(
            "/v1/messages",
            None,
            &headers,
            &tokens
        ));

        let mut with_header = HeaderMap::new();
        with_header.insert(INBOUND_HEADER, HeaderValue::from_static("ccs-proxy-secret"));
        assert!(inbound_request_allowed(
            "/status",
            Some(peer),
            &with_header,
            &tokens
        ));

        let mut with_bearer = HeaderMap::new();
        with_bearer.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer ccs-proxy-secret"),
        );
        assert!(inbound_request_allowed(
            "/v1/chat/completions",
            Some(peer),
            &with_bearer,
            &tokens
        ));

        let loopback: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(
            !inbound_request_allowed("/status", Some(loopback), &headers, &tokens),
            "/status requires the inbound token even on loopback"
        );
        assert!(inbound_request_allowed(
            "/status",
            Some(loopback),
            &with_header,
            &tokens
        ));
        assert!(inbound_request_allowed(
            "/v1/messages",
            Some(loopback),
            &headers,
            &tokens
        ));
        let v6: SocketAddr = "[::1]:9".parse().unwrap();
        assert!(inbound_request_allowed(
            "/v1/messages",
            Some(v6),
            &headers,
            &tokens
        ));
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:9".parse().unwrap();
        assert!(inbound_request_allowed(
            "/v1/messages",
            Some(mapped),
            &headers,
            &tokens
        ));
    }
}
