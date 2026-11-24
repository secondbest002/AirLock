use super::auth::{
    check_auth, compute_base_url, extract_pinned_workspace, sanitize_preview,
    unauthorized_response, TokenQuery,
};
use super::rpc::handle_single_rpc_with_context;
use super::total_tools_count;
use crate::state::{AppState, HttpTrafficEntry};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

async fn process_mcp_get(
    state: Arc<AppState>,
    headers: HeaderMap,
    query: TokenQuery,
    path_token: Option<String>,
) -> Response {
    let started = Instant::now();
    let req_path = if path_token.is_some() { "/mcp/<token>" } else { "/mcp" };
    if !check_auth(&state, &headers, &query, path_token.as_deref()).await {
        let base = compute_base_url(&state, &headers).await;
        state
            .record_http_traffic(HttpTrafficEntry {
                timestamp: AppState::now_timestamp(),
                method: "GET".to_string(),
                path: req_path.to_string(),
                rpc_method: None,
                status: 401,
                duration_ms: started.elapsed().as_millis() as u64,
                note: "Unauthorized probe (sent OAuth metadata)".to_string(),
                request_preview: None,
                response_preview: None,
            })
            .await;
        return unauthorized_response(&base);
    }

    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("text/event-stream") {
        state
            .record_http_traffic(HttpTrafficEntry {
                timestamp: AppState::now_timestamp(),
                method: "GET".to_string(),
                path: req_path.to_string(),
                rpc_method: Some("sse_probe".to_string()),
                status: 405,
                duration_ms: started.elapsed().as_millis() as u64,
                note: "405 Method Not Allowed (stateless POST mode)".to_string(),
                request_preview: None,
                response_preview: None,
            })
            .await;
        let mut resp = (
            StatusCode::METHOD_NOT_ALLOWED,
            Json(json!({ "error": "SSE stream not offered; use stateless Streamable HTTP POST" })),
        )
            .into_response();
        resp.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE, OPTIONS"));
        return resp;
    }

    state
        .record_http_traffic(HttpTrafficEntry {
            timestamp: AppState::now_timestamp(),
            method: "GET".to_string(),
            path: req_path.to_string(),
            rpc_method: Some("health_probe".to_string()),
            status: 200,
            duration_ms: started.elapsed().as_millis() as u64,
            note: "Ready".to_string(),
            request_preview: None,
            response_preview: None,
        })
        .await;

    let tools_count = total_tools_count(&state).await;
    (
        StatusCode::OK,
        Json(json!({
            "status": "ready",
            "protocol": "mcp-streamable-http",
            "protocolVersion": "2025-03-26",
            "tools": tools_count
        })),
    )
        .into_response()
}

pub(super) async fn handle_mcp_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Response {
    process_mcp_get(state, headers, query, None).await
}

pub(super) async fn handle_mcp_get_with_token(
    State(state): State<Arc<AppState>>,
    Path(path_token): Path<String>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Response {
    process_mcp_get(state, headers, query, Some(path_token)).await
}

async fn process_mcp_post(
    state: Arc<AppState>,
    headers: HeaderMap,
    query: TokenQuery,
    path_token: Option<String>,
    payload: Value,
) -> Response {
    let started = Instant::now();
    let req_path = if path_token.is_some() { "/mcp/<token>" } else { "/mcp" };
    let auth_token = state.auth_token.read().await.clone();
    let req_preview = serde_json::to_string_pretty(&payload)
        .ok()
        .map(|raw| sanitize_preview(&raw, &auth_token, 3000));

    if !check_auth(&state, &headers, &query, path_token.as_deref()).await {
        let base = compute_base_url(&state, &headers).await;
        state
            .record_http_traffic(HttpTrafficEntry {
                timestamp: AppState::now_timestamp(),
                method: "POST".to_string(),
                path: req_path.to_string(),
                rpc_method: None,
                status: 401,
                duration_ms: started.elapsed().as_millis() as u64,
                note: "Unauthorized POST".to_string(),
                request_preview: req_preview,
                response_preview: None,
            })
            .await;
        return unauthorized_response(&base);
    }

    let is_batch = payload.is_array();
    let messages = match payload {
        Value::Array(arr) => arr,
        other => vec![other],
    };

    let rpc_summary = messages
        .iter()
        .filter_map(|m| {
            let method = m.get("method").and_then(|v| v.as_str())?;
            if method == "tools/call" {
                let tname = m
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                Some(format!("tools/call({tname})"))
            } else {
                Some(method.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    let is_init = messages
        .iter()
        .any(|m| m.get("method").and_then(|v| v.as_str()) == Some("initialize"));

    let session_id = if is_init {
        Some(AppState::generate_token())
    } else {
        headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };

    let pinned_ws = extract_pinned_workspace(&headers, &query);
    let mut outs = Vec::new();
    for msg in &messages {
        if let Some(out) = handle_single_rpc_with_context(&state, msg, pinned_ws.as_deref()).await {
            outs.push(out);
        }
    }

    let mcp_method_hdr = headers
        .get("mcp-method")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mcp_name_hdr = headers
        .get("mcp-name")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut note_parts = Vec::new();
    if let Some(ref pin) = pinned_ws {
        let clean = pin.strip_prefix('@').unwrap_or(pin);
        note_parts.push(format!("ws: @{clean}"));
    }
    if let Some(ref m) = mcp_method_hdr {
        note_parts.push(format!("Mcp-Method: {m}"));
    }
    if let Some(ref n) = mcp_name_hdr {
        note_parts.push(format!("Mcp-Name: {n}"));
    }
    let ok_note = if note_parts.is_empty() {
        "OK".to_string()
    } else {
        note_parts.join(" | ")
    };

    let status_code = if outs.is_empty() { 202 } else { 200 };
    if outs.is_empty() {
        let notif_note = if note_parts.is_empty() {
            "Accepted notification".to_string()
        } else {
            format!("Accepted notification ({})", note_parts.join(" | "))
        };
        state
            .record_http_traffic(HttpTrafficEntry {
                timestamp: AppState::now_timestamp(),
                method: "POST".to_string(),
                path: req_path.to_string(),
                rpc_method: if rpc_summary.is_empty() {
                    None
                } else {
                    Some(rpc_summary)
                },
                status: status_code,
                duration_ms: started.elapsed().as_millis() as u64,
                note: notif_note,
                request_preview: req_preview,
                response_preview: None,
            })
            .await;
        return StatusCode::ACCEPTED.into_response();
    }

    let body = if is_batch {
        Value::Array(outs)
    } else {
        outs.into_iter().next().unwrap_or(Value::Null)
    };
    let resp_preview = serde_json::to_string_pretty(&body)
        .ok()
        .map(|raw| sanitize_preview(&raw, &auth_token, 3000));

    state
        .record_http_traffic(HttpTrafficEntry {
            timestamp: AppState::now_timestamp(),
            method: "POST".to_string(),
            path: req_path.to_string(),
            rpc_method: if rpc_summary.is_empty() {
                None
            } else {
                Some(rpc_summary)
            },
            status: status_code,
            duration_ms: started.elapsed().as_millis() as u64,
            note: ok_note,
            request_preview: req_preview,
            response_preview: resp_preview,
        })
        .await;

    let mut resp = (StatusCode::OK, Json(body)).into_response();
    if let Some(sid) = session_id {
        if let Ok(val) = HeaderValue::from_str(&sid) {
            resp.headers_mut()
                .insert(HeaderName::from_static("mcp-session-id"), val);
        }
    }
    resp
}

pub(super) async fn handle_mcp_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    Json(payload): Json<Value>,
) -> Response {
    process_mcp_post(state, headers, query, None, payload).await
}

pub(super) async fn handle_mcp_post_with_token(
    State(state): State<Arc<AppState>>,
    Path(path_token): Path<String>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    Json(payload): Json<Value>,
) -> Response {
    process_mcp_post(state, headers, query, Some(path_token), payload).await
}

async fn process_mcp_delete(
    state: Arc<AppState>,
    headers: HeaderMap,
    query: TokenQuery,
    path_token: Option<String>,
) -> Response {
    let req_path = if path_token.is_some() { "/mcp/<token>" } else { "/mcp" };
    if !check_auth(&state, &headers, &query, path_token.as_deref()).await {
        let base = compute_base_url(&state, &headers).await;
        return unauthorized_response(&base);
    }
    state
        .record_http_traffic(HttpTrafficEntry {
            timestamp: AppState::now_timestamp(),
            method: "DELETE".to_string(),
            path: req_path.to_string(),
            rpc_method: Some("session/close".to_string()),
            status: 200,
            duration_ms: 0,
            note: "Session closed".to_string(),
            request_preview: None,
            response_preview: None,
        })
        .await;
    (StatusCode::OK, Json(json!({ "closed": true }))).into_response()
}

pub(super) async fn handle_mcp_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Response {
    process_mcp_delete(state, headers, query, None).await
}

pub(super) async fn handle_mcp_delete_with_token(
    State(state): State<Arc<AppState>>,
    Path(path_token): Path<String>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Response {
    process_mcp_delete(state, headers, query, Some(path_token)).await
}
