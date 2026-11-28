pub use crate::config::{McpToolProfile, WebMcpExecutionMode};
use crate::security::{hash_oauth_token, secure_token_eq};
use crate::state::AppState;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::Ordering;

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    pub(super) token: Option<String>,
    pub(super) workspace: Option<String>,
    pub(super) profile: Option<String>,
    pub(super) mode: Option<String>,
}

pub(super) fn extract_tool_profile(
    headers: &HeaderMap,
    query: &TokenQuery,
    default_profile: &str,
) -> McpToolProfile {
    if let Some(q_prof) = query.profile.as_deref().and_then(McpToolProfile::parse) {
        return q_prof;
    }
    if let Some(h_prof) = headers
        .get("x-airlock-profile")
        .and_then(|v| v.to_str().ok())
        .and_then(McpToolProfile::parse)
    {
        return h_prof;
    }
    McpToolProfile::parse(default_profile).unwrap_or(McpToolProfile::Compact)
}

pub(super) fn extract_execution_mode(
    headers: &HeaderMap,
    query: &TokenQuery,
    default_mode: &str,
) -> WebMcpExecutionMode {
    if let Some(q_mode) = query.mode.as_deref().and_then(WebMcpExecutionMode::parse) {
        return q_mode;
    }
    if let Some(h_mode) = headers
        .get("x-airlock-mode")
        .and_then(|v| v.to_str().ok())
        .and_then(WebMcpExecutionMode::parse)
    {
        return h_mode;
    }
    WebMcpExecutionMode::parse(default_mode).unwrap_or(WebMcpExecutionMode::Chat)
}

pub(super) fn extract_pinned_workspace(headers: &HeaderMap, query: &TokenQuery) -> Option<String> {
    query
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("x-airlock-workspace")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
}

pub(crate) async fn compute_base_url(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(url) = state.tunnel_url.read().await.as_ref() {
        return url.trim_end_matches('/').to_string();
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let fallback_host = format!("127.0.0.1:{}", state.server_port.load(Ordering::SeqCst));
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&fallback_host);
    format!("{proto}://{host}")
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut count = 0usize;
    let mut end_byte = s.len();
    for (idx, _) in s.char_indices() {
        if count == max_chars {
            end_byte = idx;
            break;
        }
        count += 1;
    }
    if end_byte < s.len() {
        format!("{}… [truncated]", &s[..end_byte])
    } else {
        s.to_string()
    }
}

pub(super) fn sanitize_preview(raw: &str, auth_token: &str, max_chars: usize) -> String {
    let redacted = if !auth_token.is_empty() {
        raw.replace(auth_token, "<redacted>")
    } else {
        raw.to_string()
    };
    truncate_chars(&redacted, max_chars)
}

pub(super) async fn check_auth(
    state: &AppState,
    headers: &HeaderMap,
    query: &TokenQuery,
    path_token: Option<&str>,
) -> bool {
    let expected = state.auth_token.read().await;
    if let Some(pt) = path_token {
        if secure_token_eq(pt.trim(), expected.as_str()) {
            return true;
        }
    }
    let mut bearer_candidate: Option<String> = None;
    if let Some(auth_val) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        let token_part = auth_val
            .strip_prefix("Bearer ")
            .or_else(|| auth_val.strip_prefix("bearer "))
            .unwrap_or(auth_val)
            .trim();
        if secure_token_eq(token_part, expected.as_str()) {
            return true;
        }
        if !token_part.is_empty() {
            bearer_candidate = Some(token_part.to_string());
        }
    }
    drop(expected);

    if let Some(token) = bearer_candidate {
        let now = AppState::now_epoch_secs();
        let token_hash = hash_oauth_token(&token);
        let matched_cid = {
            let tokens = state.oauth_access_tokens.read().await;
            tokens
                .get(&token_hash)
                .filter(|rec| rec.expires_at_epoch.map(|exp| exp > now).unwrap_or(true))
                .map(|rec| rec.client_id.clone())
        };
        if let Some(cid) = matched_cid {
            if !cid.is_empty() {
                let mut clients = state.oauth_clients.write().await;
                if let Some(client) = clients.get_mut(&cid) {
                    client.last_used_epoch = Some(now);
                }
            }
            return true;
        }
    }

    if let Some(q) = &query.token {
        let expected = state.auth_token.read().await;
        if secure_token_eq(q.trim(), expected.as_str()) {
            return true;
        }
    }
    false
}

fn build_safe_resource_query(query: &TokenQuery) -> Option<String> {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    let mut count = 0usize;
    if let Some(p) = query.profile.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        ser.append_pair("profile", p);
        count += 1;
    }
    if let Some(w) = query.workspace.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        ser.append_pair("workspace", w);
        count += 1;
    }
    if let Some(m) = query.mode.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        ser.append_pair("mode", m);
        count += 1;
    }
    if count > 0 {
        Some(ser.finish())
    } else {
        None
    }
}

pub(super) fn unauthorized_response(base: &str, query: Option<&TokenQuery>) -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized", "message": "Invalid or missing Bearer token" })),
    )
        .into_response();
    let meta_url = match query.and_then(build_safe_resource_query) {
        Some(qs) => format!("{base}/.well-known/oauth-protected-resource/mcp?{qs}"),
        None => format!("{base}/.well-known/oauth-protected-resource"),
    };
    let header_val = format!("Bearer resource_metadata=\"{meta_url}\"");
    if let Ok(val) = HeaderValue::from_str(&header_val) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, val);
    }
    resp
}
