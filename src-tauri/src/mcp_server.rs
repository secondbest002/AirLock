use crate::oauth;
use crate::state::{AppState, HttpTrafficEntry};
use crate::tools::{execute_tool, mcp_tools_list};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    token: Option<String>,
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

fn sanitize_preview(raw: &str, auth_token: &str, max_chars: usize) -> String {
    let redacted = if !auth_token.is_empty() {
        raw.replace(auth_token, "<redacted>")
    } else {
        raw.to_string()
    };
    truncate_chars(&redacted, max_chars)
}

async fn check_auth(
    state: &AppState,
    headers: &HeaderMap,
    query: &TokenQuery,
    path_token: Option<&str>,
) -> bool {
    let expected = state.auth_token.read().await;
    if let Some(pt) = path_token {
        if pt.trim() == expected.as_str() {
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
        if token_part == expected.as_str() {
            return true;
        }
        if !token_part.is_empty() {
            bearer_candidate = Some(token_part.to_string());
        }
    }
    drop(expected);

    if let Some(token) = bearer_candidate {
        let now = AppState::now_epoch_secs();
        let matched_cid = {
            let tokens = state.oauth_access_tokens.read().await;
            tokens
                .get(&token)
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
        if q.trim() == expected.as_str() {
            return true;
        }
    }
    false
}

fn unauthorized_response(base: &str) -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized", "message": "Invalid or missing Bearer token" })),
    )
        .into_response();
    let header_val = format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\"");
    if let Ok(val) = HeaderValue::from_str(&header_val) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, val);
    }
    resp
}

pub async fn total_tools_count(state: &AppState) -> usize {
    let builtin_count = mcp_tools_list()
        .as_array()
        .map(|a| a.len())
        .unwrap_or(14);
    let cfgs = state.config.read().await.downstream_mcps.clone();
    let hub_count = state.mcp_hub.list_aggregated_tools(&cfgs).await.len();
    builtin_count + hub_count
}

async fn handle_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let tools = total_tools_count(&state).await;
    Json(json!({ "status": "ok", "service": "smart-tauri-hub", "tools": tools }))
}

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

async fn handle_mcp_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Response {
    process_mcp_get(state, headers, query, None).await
}

async fn handle_mcp_get_with_token(
    State(state): State<Arc<AppState>>,
    Path(path_token): Path<String>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Response {
    process_mcp_get(state, headers, query, Some(path_token)).await
}

pub(crate) async fn handle_single_rpc(state: &AppState, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

    if method == "notifications/initialized" || (id.is_none() && method.starts_with("notifications/"))
    {
        return None;
    }

    let rpc_id = id.unwrap_or(Value::Null);
    let result_body = match method {
        "initialize" => {
            let requested_version = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2025-03-26");
            json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "result": {
                    "protocolVersion": requested_version,
                    "capabilities": {
                        "tools": { "listChanged": true },
                        "resources": { "subscribe": false, "listChanged": false },
                        "prompts": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "smart-tauri-hub",
                        "version": "0.1.0"
                    }
                }
            })
        }
        "ping" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {}
        }),
        "server/discover" => {
            let mut merged_tools = mcp_tools_list().as_array().cloned().unwrap_or_default();
            let cfgs = state.config.read().await.downstream_mcps.clone();
            let agg = state.mcp_hub.list_aggregated_tools(&cfgs).await;
            let downstream_count = cfgs.iter().filter(|c| c.enabled).count();
            merged_tools.extend(agg);
            let guardrail_mode = state.guardrail_mode.read().await.clone();
            json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "result": {
                    "protocolVersions": ["2025-03-26", "2026-06-01"],
                    "serverInfo": {
                        "name": "smart-tauri-hub",
                        "version": "0.1.0"
                    },
                    "capabilities": {
                        "tools": { "listChanged": true },
                        "resources": { "subscribe": false, "listChanged": false },
                        "prompts": { "listChanged": false }
                    },
                    "tools": merged_tools,
                    "resources": crate::mcp_resources::resources_list(),
                    "resourceTemplates": crate::mcp_resources::resource_templates_list(),
                    "prompts": crate::mcp_resources::prompts_list(),
                    "hub": {
                        "downstreamServersCount": downstream_count,
                        "guardrailMode": guardrail_mode
                    },
                    "_meta": { "cacheControl": { "type": "ephemeral" } }
                }
            })
        }
        "tools/list" => {
            let mut merged_tools = mcp_tools_list().as_array().cloned().unwrap_or_default();
            let cfgs = state.config.read().await.downstream_mcps.clone();
            let agg = state.mcp_hub.list_aggregated_tools(&cfgs).await;
            merged_tools.extend(agg);
            json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "result": {
                    "tools": merged_tools,
                    "_meta": { "cacheControl": { "type": "ephemeral" } }
                }
            })
        }
        "resources/list" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {
                "resources": crate::mcp_resources::resources_list(),
                "_meta": { "cacheControl": { "type": "ephemeral" } }
            }
        }),
        "resources/templates/list" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {
                "resourceTemplates": crate::mcp_resources::resource_templates_list(),
                "_meta": { "cacheControl": { "type": "ephemeral" } }
            }
        }),
        "resources/read" => {
            let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
            if uri.trim().is_empty() {
                json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "error": { "code": -32602, "message": "Missing required param 'uri'" }
                })
            } else {
                match crate::mcp_resources::read_resource(state, uri).await {
                    Ok(res) => json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "result": res
                    }),
                    Err(err) => json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "error": { "code": -32602, "message": err }
                    }),
                }
            }
        }
        "prompts/list" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {
                "prompts": crate::mcp_resources::prompts_list(),
                "_meta": { "cacheControl": { "type": "ephemeral" } }
            }
        }),
        "prompts/get" => {
            let pname = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let pargs = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match crate::mcp_resources::get_prompt(state, pname, &pargs).await {
                Ok(res) => json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "result": res
                }),
                Err(err) => json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "error": { "code": -32602, "message": err }
                }),
            }
        }
        "logging/setLevel" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {}
        }),
        "tools/call" => {
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let fut = async {
                if tool_name.contains("__") {
                    state
                        .mcp_hub
                        .call_downstream_tool(state, "WebMCP", tool_name, args)
                        .await
                } else {
                    execute_tool(state, "WebMCP", tool_name, args).await
                }
            };
            let output = match tokio::time::timeout(Duration::from_secs(24), fut).await {
                Ok(out) => out,
                Err(_) => crate::tools::ToolOutput {
                    text: format!(
                        "Anti-hang timeout (24s): tool '{tool_name}' memakan waktu terlalu lama. \
                        Persempit parameter 'path' ke subfolder spesifik atau gunakan start_command_job."
                    ),
                    is_error: true,
                },
            };
            json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "result": {
                    "content": [{ "type": "text", "text": output.text }],
                    "isError": output.is_error
                }
            })
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {other}")
            }
        }),
    };
    Some(result_body)
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

    let mut outs = Vec::new();
    for msg in &messages {
        if let Some(out) = handle_single_rpc(&state, msg).await {
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

async fn handle_mcp_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    Json(payload): Json<Value>,
) -> Response {
    process_mcp_post(state, headers, query, None, payload).await
}

async fn handle_mcp_post_with_token(
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

async fn handle_mcp_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Response {
    process_mcp_delete(state, headers, query, None).await
}

async fn handle_mcp_delete_with_token(
    State(state): State<Arc<AppState>>,
    Path(path_token): Path<String>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Response {
    process_mcp_delete(state, headers, query, Some(path_token)).await
}

fn is_allowed_origin(state: &AppState, origin: &HeaderValue) -> bool {
    let Ok(raw) = origin.to_str() else {
        return false;
    };
    let lower = raw.trim().to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "https://gemini.google.com"
            | "https://claude.ai"
            | "https://chatgpt.com"
            | "https://aistudio.google.com"
            | "http://localhost:1420"
            | "http://127.0.0.1:1420"
            | "tauri://localhost"
            | "http://tauri.localhost"
            | "https://tauri.localhost"
    ) {
        return true;
    }
    if let Ok(origin_url) = url::Url::parse(&lower) {
        if origin_url.scheme() == "https" {
            if let Some(host) = origin_url.host_str() {
                if host.ends_with(".ts.net") || host.ends_with(".trycloudflare.com") {
                    return true;
                }
            }
        }
        if let Ok(guard) = state.tunnel_url.try_read() {
            if let Some(tunnel_raw) = guard.as_ref() {
                if let Ok(tunnel_parsed) = url::Url::parse(&tunnel_raw.trim().to_ascii_lowercase()) {
                    if origin_url.scheme() == tunnel_parsed.scheme()
                        && origin_url.host_str() == tunnel_parsed.host_str()
                        && origin_url.port_or_known_default()
                            == tunnel_parsed.port_or_known_default()
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let state_for_cors = state.clone();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _parts| {
            is_allowed_origin(&state_for_cors, origin)
        }))
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            HeaderName::from_static("mcp-session-id"),
            HeaderName::from_static("mcp-protocol-version"),
            HeaderName::from_static("mcp-method"),
            HeaderName::from_static("mcp-name"),
        ])
        .expose_headers([HeaderName::from_static("mcp-session-id")]);

    Router::new()
        .route("/health", get(handle_health))
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth::handle_oauth_protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/*suffix",
            get(oauth::handle_oauth_protected_resource_suffix),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::handle_oauth_authorization_server),
        )
        .route(
            "/.well-known/openid-configuration",
            get(oauth::handle_oauth_authorization_server),
        )
        .route("/oauth/register", post(oauth::handle_oauth_register))
        .route("/oauth/authorize", get(oauth::handle_oauth_authorize))
        .route("/oauth/token", post(oauth::handle_oauth_token))
        .route(
            "/mcp",
            get(handle_mcp_get)
                .post(handle_mcp_post)
                .delete(handle_mcp_delete),
        )
        .route(
            "/mcp/:path_token",
            get(handle_mcp_get_with_token)
                .post(handle_mcp_post_with_token)
                .delete(handle_mcp_delete_with_token),
        )
        .route(
            "/",
            get(handle_mcp_get)
                .post(handle_mcp_post)
                .delete(handle_mcp_delete),
        )
        .layer(cors)
        .with_state(state)
}

pub async fn start_server(state: Arc<AppState>) -> Result<(), String> {
    if state.server_running.load(Ordering::SeqCst) {
        return Ok(());
    }

    let addr = format!("127.0.0.1:{}", state.server_port.load(Ordering::SeqCst));
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("Gagal membuka port {addr}: {e}"))?;

    let (tx, rx) = oneshot::channel::<()>();
    {
        let mut shutdown_guard = state.server_shutdown.lock().await;
        *shutdown_guard = Some(tx);
    }

    state.server_running.store(true, Ordering::SeqCst);
    state.touch_activity().await;
    state.emit_dashboard().await;

    let app = build_router(state.clone());
    let state_for_task = state.clone();

    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
        state_for_task.server_running.store(false, Ordering::SeqCst);
        state_for_task.emit_dashboard().await;
    });

    Ok(())
}

pub async fn stop_server(state: &Arc<AppState>) {
    let tx = {
        let mut guard = state.server_shutdown.lock().await;
        guard.take()
    };
    if let Some(sender) = tx {
        let _ = sender.send(());
    }
    state.server_running.store(false, Ordering::SeqCst);
    state.emit_dashboard().await;
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReport {
    pub server_running: bool,
    pub port: u16,
    pub health_ok: bool,
    pub mcp_initialize_ok: bool,
    pub tools_count: usize,
    pub latency_ms: u64,
    pub tunnel_active: bool,
    pub tunnel_url: Option<String>,
    pub oauth_clients_count: usize,
    pub notes: Vec<String>,
}

pub async fn run_self_diagnostic(state: &AppState) -> DiagnosticReport {
    let server_running = state.server_running.load(Ordering::SeqCst);
    let port = state.server_port.load(Ordering::SeqCst);
    let tunnel_active = state.tunnel_running.load(Ordering::SeqCst);
    let tunnel_url = state.tunnel_url.read().await.clone();
    let oauth_clients_count = state.oauth_clients.read().await.len();
    let auth_token = state.auth_token.read().await.clone();

    let mut notes = Vec::new();
    if !server_running {
        notes.push("MCP Server saat ini offline. Klik Start Server untuk memulai listener.".to_string());
        return DiagnosticReport {
            server_running,
            port,
            health_ok: false,
            mcp_initialize_ok: false,
            tools_count: 0,
            latency_ms: 0,
            tunnel_active,
            tunnel_url,
            oauth_clients_count,
            notes,
        };
    }

    let started = Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            notes.push(format!("Gagal membuat HTTP client diagnostik: {e}"));
            return DiagnosticReport {
                server_running,
                port,
                health_ok: false,
                mcp_initialize_ok: false,
                tools_count: 0,
                latency_ms: 0,
                tunnel_active,
                tunnel_url,
                oauth_clients_count,
                notes,
            };
        }
    };

    let base_local = format!("http://127.0.0.1:{port}");
    let health_ok = match client.get(format!("{base_local}/health")).send().await {
        Ok(r) if r.status().is_success() => true,
        Ok(r) => {
            notes.push(format!("GET /health mengembalikan status {}.", r.status()));
            false
        }
        Err(e) => {
            notes.push(format!("GET /health gagal: {e}"));
            false
        }
    };

    let mcp_initialize_ok = match client
        .post(format!("{base_local}/mcp"))
        .bearer_auth(&auth_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 9001,
            "method": "initialize",
            "params": { "protocolVersion": "2025-03-26" }
        }))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) => v["result"]["serverInfo"]["name"] == "smart-tauri-hub",
            Err(e) => {
                notes.push(format!("Respons initialize tidak valid: {e}"));
                false
            }
        },
        Ok(r) => {
            notes.push(format!("POST /mcp initialize mengembalikan status {}.", r.status()));
            false
        }
        Err(e) => {
            notes.push(format!("POST /mcp initialize gagal: {e}"));
            false
        }
    };

    let tools_count = match client
        .post(format!("{base_local}/mcp"))
        .bearer_auth(&auth_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 9002,
            "method": "tools/list"
        }))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) => v["result"]["tools"]
                .as_array()
                .map(|arr| arr.len())
                .unwrap_or(0),
            Err(_) => 0,
        },
        _ => 0,
    };

    let latency_ms = started.elapsed().as_millis() as u64;

    if health_ok && mcp_initialize_ok && tools_count >= 14 {
        notes.push(format!("Loopback MCP sehat ({latency_ms}ms, {tools_count} tools siap)."));
    }
    if !tunnel_active {
        notes.push("Tunnel remote tidak aktif (hanya dapat diakses dari localhost saat ini).".to_string());
    } else if let Some(url) = &tunnel_url {
        notes.push(format!("Tunnel aktif mengarah ke {url}."));
    } else {
        notes.push("Tunnel sedang berjalan tetapi URL publik belum tersedia.".to_string());
    }
    if oauth_clients_count == 0 {
        notes.push("Belum ada OAuth client terdaftar (normal jika baru menggunakan Bearer token statis).".to_string());
    }

    DiagnosticReport {
        server_running,
        port,
        health_ok,
        mcp_initialize_ok,
        tools_count,
        latency_ms,
        tunnel_active,
        tunnel_url,
        oauth_clients_count,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::OAuthDecision;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::fs;

    async fn approve_next_pending_oauth(state: Arc<AppState>, decision: OAuthDecision) {
        for _ in 0..100 {
            let req_id = {
                let pending = state.pending_oauth.read().await;
                pending.keys().next().cloned()
            };
            if let Some(id) = req_id {
                oauth::resolve_oauth_consent(&state, &id, decision)
                    .await
                    .unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Timed out waiting for pending OAuth consent request");
    }

    #[tokio::test]
    async fn test_mcp_http_endpoints_end_to_end() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_http_e2e_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("hello.txt"), "alpha\nbeta\ngamma\n").unwrap();

        let state = Arc::new(AppState::new(temp_dir.clone()));
        let token = state.auth_token.read().await.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = build_router(state.clone());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let client = reqwest::Client::new();
        let endpoint = format!("http://{addr}/mcp");

        // 1. Unauthorized must fail with 401
        let unauth = client
            .post(&endpoint)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

        // 2. Initialize via path-token (/mcp/<token>) and verify Mcp-Session-Id + protocolVersion echo
        let path_endpoint = format!("http://{addr}/mcp/{token}");
        let init_http = client
            .post(&path_endpoint)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2025-03-26" }
            }))
            .send()
            .await
            .unwrap();
        assert!(init_http.headers().contains_key("mcp-session-id"));
        let init_resp: Value = init_http.json().await.unwrap();
        assert_eq!(init_resp["result"]["protocolVersion"], "2025-03-26");

        // 3. tools/list returns all 14 smart tools with MCP annotations & _meta cacheControl
        let list_resp: Value = client
            .post(&endpoint)
            .bearer_auth(&token)
            .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let tools_arr = list_resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools_arr.len(), 14);
        assert_eq!(
            list_resp["result"]["_meta"]["cacheControl"]["type"],
            "ephemeral"
        );
        assert_eq!(tools_arr[0]["annotations"]["readOnlyHint"], true);
        let run_cmd_tool = tools_arr
            .iter()
            .find(|t| t["name"] == "run_command_compressed")
            .unwrap();
        assert_eq!(run_cmd_tool["annotations"]["destructiveHint"], true);

        // 3b. MCP 2026 forward-compat server/discover, resources/*, prompts/*
        let discover_resp: Value = client
            .post(&endpoint)
            .bearer_auth(&token)
            .header("Mcp-Method", "server/discover")
            .header("Mcp-Name", "smart-tauri-hub")
            .json(&json!({"jsonrpc":"2.0","id":21,"method":"server/discover"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            discover_resp["result"]["tools"].as_array().unwrap().len(),
            14
        );
        assert_eq!(
            discover_resp["result"]["resources"].as_array().unwrap().len(),
            4
        );
        assert_eq!(
            discover_resp["result"]["prompts"].as_array().unwrap().len(),
            4
        );

        let read_res_resp: Value = client
            .post(&endpoint)
            .bearer_auth(&token)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 22,
                "method": "resources/read",
                "params": { "uri": "workspace://overview" }
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(read_res_resp["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("hello.txt"));

        let get_prompt_resp: Value = client
            .post(&endpoint)
            .bearer_auth(&token)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 23,
                "method": "prompts/get",
                "params": { "name": "code_review", "arguments": { "focus": "src" } }
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(get_prompt_resp["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("git_ops"));

        // 4. tools/call project_overview + surgical_edit + DELETE session
        let call_overview: Value = client
            .post(&path_endpoint)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": "project_overview", "arguments": {} }
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let overview_text = call_overview["result"]["content"][0]["text"].as_str().unwrap();
        assert!(overview_text.contains("hello.txt"));

        let call_edit: Value = client
            .post(&endpoint)
            .bearer_auth(&token)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "surgical_edit",
                    "arguments": {
                        "path": "hello.txt",
                        "edits": [{ "search": "beta", "replace": "BETA_UPDATED" }]
                    }
                }
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(call_edit["result"]["isError"], false);
        assert!(fs::read_to_string(temp_dir.join("hello.txt"))
            .unwrap()
            .contains("BETA_UPDATED"));

        let del_resp = client.delete(&path_endpoint).send().await.unwrap();
        assert_eq!(del_resp.status(), StatusCode::OK);

        handle.abort();
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_cors_allows_trusted_and_blocks_untrusted_origin() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_cors_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let state = Arc::new(AppState::new(temp_dir.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = build_router(state);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let client = reqwest::Client::new();
        let allowed_resp = client
            .get(format!("http://{addr}/health"))
            .header("Origin", "https://gemini.google.com")
            .send()
            .await
            .unwrap();
        assert_eq!(
            allowed_resp
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("https://gemini.google.com")
        );

        let blocked_resp = client
            .get(format!("http://{addr}/health"))
            .header("Origin", "https://evil.example.com")
            .send()
            .await
            .unwrap();
        assert!(blocked_resp
            .headers()
            .get("access-control-allow-origin")
            .is_none());

        handle.abort();
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_cors_allows_custom_named_tunnel_origin() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_cors_named_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let state = Arc::new(AppState::new(temp_dir.clone()));
        *state.tunnel_url.write().await = Some("https://mcp.customdomain.dev".to_string());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = build_router(state);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let client = reqwest::Client::new();
        let allowed_resp = client
            .get(format!("http://{addr}/health"))
            .header("Origin", "https://mcp.customdomain.dev")
            .send()
            .await
            .unwrap();
        assert_eq!(
            allowed_resp
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("https://mcp.customdomain.dev")
        );

        let blocked_resp = client
            .get(format!("http://{addr}/health"))
            .header("Origin", "https://evil.example.com")
            .send()
            .await
            .unwrap();
        assert!(blocked_resp
            .headers()
            .get("access-control-allow-origin")
            .is_none());

        handle.abort();
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_mcp_oauth2_pkce_flow_end_to_end() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_oauth_e2e_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let state = Arc::new(AppState::new(temp_dir.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = build_router(state.clone());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let base = format!("http://{addr}");
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        // 1. Unauthenticated /mcp returns 401 with WWW-Authenticate resource_metadata
        let unauth = client
            .post(format!("{base}/mcp"))
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
        let www_auth = unauth
            .headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(www_auth.contains("/.well-known/oauth-protected-resource"));

        // 2. Discovery endpoints
        let prm: Value = client
            .get(format!("{base}/.well-known/oauth-protected-resource"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(prm["authorization_servers"][0].as_str().is_some());

        let as_meta: Value = client
            .get(format!("{base}/.well-known/oauth-authorization-server"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(as_meta["authorization_endpoint"]
            .as_str()
            .unwrap()
            .ends_with("/oauth/authorize"));
        assert!(as_meta["token_endpoint"]
            .as_str()
            .unwrap()
            .ends_with("/oauth/token"));

        // 3. Dynamic Client Registration
        let reg_resp: Value = client
            .post(format!("{base}/oauth/register"))
            .json(&json!({
                "client_name": "Gemini Web Test",
                "redirect_uris": ["https://gemini.google.com/oauth/callback"],
                "token_endpoint_auth_method": "client_secret_post"
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let client_id = reg_resp["client_id"].as_str().unwrap().to_string();
        let client_secret = reg_resp["client_secret"].as_str().unwrap().to_string();

        // 4a. Denied consent for unregistered client must redirect with access_denied
        let deny_task = tokio::spawn(approve_next_pending_oauth(
            state.clone(),
            OAuthDecision::Deny,
        ));
        let denied_resp = client
            .get(format!("{base}/oauth/authorize"))
            .query(&[
                ("client_id", "untrusted-stranger"),
                ("redirect_uri", "https://gemini.google.com/oauth/callback"),
                ("state", "deny-state"),
            ])
            .send()
            .await
            .unwrap();
        deny_task.await.unwrap();
        assert_eq!(denied_resp.status(), StatusCode::FOUND);
        let deny_loc = denied_resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(deny_loc.contains("error=access_denied"));

        // 4b. Authorize with PKCE S256 -> Approve & Trust via consent gate
        let code_verifier = "test_pkce_verifier_secret_value_123456";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
        let approve_task = tokio::spawn(approve_next_pending_oauth(
            state.clone(),
            OAuthDecision::ApproveAndTrust,
        ));
        let auth_resp = client
            .get(format!("{base}/oauth/authorize"))
            .query(&[
                ("client_id", client_id.as_str()),
                ("redirect_uri", "https://gemini.google.com/oauth/callback"),
                ("state", "xyz-state"),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
            ])
            .send()
            .await
            .unwrap();
        approve_task.await.unwrap();
        assert_eq!(auth_resp.status(), StatusCode::FOUND);
        let location = auth_resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap();
        let loc_url = url::Url::parse(location).unwrap();
        let pairs: HashMap<String, String> = loc_url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("state").map(|s| s.as_str()), Some("xyz-state"));
        let code = pairs.get("code").unwrap().clone();

        // 4c. Second authorize for trusted client succeeds immediately WITHOUT consent prompt
        let auth_resp_2 = client
            .get(format!("{base}/oauth/authorize"))
            .query(&[
                ("client_id", client_id.as_str()),
                ("redirect_uri", "https://gemini.google.com/oauth/callback"),
                ("state", "trusted-second"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(auth_resp_2.status(), StatusCode::FOUND);
        assert!(auth_resp_2
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("code="));

        // 5. Exchange code for OAuth Bearer access_token
        let token_resp: Value = client
            .post(format!("{base}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("redirect_uri", "https://gemini.google.com/oauth/callback"),
                ("code_verifier", code_verifier),
            ])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let access_token = token_resp["access_token"].as_str().unwrap().to_string();
        assert!(access_token.starts_with("smcp_at_"));

        // 6. Call /mcp using the OAuth Bearer token
        let mcp_init: Value = client
            .post(format!("{base}/mcp"))
            .bearer_auth(&access_token)
            .json(&json!({"jsonrpc":"2.0","id":10,"method":"initialize"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(mcp_init["result"]["serverInfo"]["name"], "smart-tauri-hub");

        let mcp_list: Value = client
            .post(format!("{base}/mcp"))
            .bearer_auth(&access_token)
            .json(&json!({"jsonrpc":"2.0","id":11,"method":"tools/list"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(mcp_list["result"]["tools"].as_array().unwrap().len(), 14);

        // 7. GET /mcp with Accept: text/event-stream must return 405 Method Not Allowed
        let sse_probe = client
            .get(format!("{base}/mcp"))
            .bearer_auth(&access_token)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(sse_probe.status(), StatusCode::METHOD_NOT_ALLOWED);

        // 8. resources/list and prompts/list return Phase 3 built-in collections
        let res_list: Value = client
            .post(format!("{base}/mcp"))
            .bearer_auth(&access_token)
            .json(&json!({"jsonrpc":"2.0","id":12,"method":"resources/list"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(res_list["result"]["resources"].as_array().unwrap().len(), 4);

        let prm_list: Value = client
            .post(format!("{base}/mcp"))
            .bearer_auth(&access_token)
            .json(&json!({"jsonrpc":"2.0","id":13,"method":"prompts/list"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(prm_list["result"]["prompts"].as_array().unwrap().len(), 4);

        // 9. Revoking the OAuth client must immediately return 401 on subsequent /mcp calls
        oauth::revoke_oauth_client(&state, &client_id)
            .await
            .unwrap();
        let revoked_call = client
            .post(format!("{base}/mcp"))
            .bearer_auth(&access_token)
            .json(&json!({"jsonrpc":"2.0","id":14,"method":"ping"}))
            .send()
            .await
            .unwrap();
        assert_eq!(revoked_call.status(), StatusCode::UNAUTHORIZED);

        handle.abort();
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_oauth_persistence_across_restart() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_oauth_persist_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        let auth_file = temp_dir.join("auth.json");
        let config_file = temp_dir.join("config.json");

        let access_token = {
            let state1 = Arc::new(AppState::new_persistent(
                temp_dir.clone(),
                auth_file.clone(),
                config_file.clone(),
            ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let router = build_router(state1.clone());
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });

            let base = format!("http://{addr}");
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();

            let approve_task = tokio::spawn(approve_next_pending_oauth(
                state1.clone(),
                OAuthDecision::ApproveOnce,
            ));
            let auth_resp = client
                .get(format!("{base}/oauth/authorize"))
                .query(&[
                    ("client_id", "gemini-web"),
                    ("redirect_uri", "https://gemini.google.com/oauth/callback"),
                ])
                .send()
                .await
                .unwrap();
            approve_task.await.unwrap();
            let loc = auth_resp
                .headers()
                .get("location")
                .unwrap()
                .to_str()
                .unwrap();
            let parsed = url::Url::parse(loc).unwrap();
            let code = parsed
                .query_pairs()
                .find(|(k, _)| k == "code")
                .unwrap()
                .1
                .to_string();

            let tok: Value = client
                .post(format!("{base}/oauth/token"))
                .form(&[("grant_type", "authorization_code"), ("code", code.as_str())])
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            handle.abort();
            tok["access_token"].as_str().unwrap().to_string()
        };

        // Restart with fresh AppState loading the same auth.json and config.json files
        let state2 = Arc::new(AppState::new_persistent(
            temp_dir.clone(),
            auth_file,
            config_file,
        ));
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let router2 = build_router(state2);
        let handle2 = tokio::spawn(async move {
            let _ = axum::serve(listener2, router2).await;
        });

        let client2 = reqwest::Client::new();
        let resp: Value = client2
            .post(format!("http://{addr2}/mcp"))
            .bearer_auth(&access_token)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["id"], 1);
        assert!(resp.get("error").is_none());

        handle2.abort();
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_mcp_hub_http_aggregation_end_to_end() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_hub_agg_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        // Start a mock downstream MCP HTTP server
        let mock_router = Router::new().route(
            "/mcp",
            post(|Json(payload): Json<Value>| async move {
                let id = payload.get("id").cloned().unwrap_or(Value::Null);
                let method = payload.get("method").and_then(|v| v.as_str()).unwrap_or("");
                match method {
                    "initialize" => Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-03-26",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-docs", "version": "1.0" }
                        }
                    }))
                    .into_response(),
                    "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
                    "tools/list" => Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "ping_doc",
                                "description": "Look up mock documentation",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "topic": { "type": "string" }
                                    },
                                    "required": ["topic"]
                                },
                                "annotations": { "readOnlyHint": true }
                            }]
                        }
                    }))
                    .into_response(),
                    "tools/call" => {
                        let topic = payload["params"]["arguments"]["topic"]
                            .as_str()
                            .unwrap_or("none");
                        Json(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{ "type": "text", "text": format!("doc-result:{topic}") }],
                                "isError": false
                            }
                        }))
                        .into_response()
                    }
                    _ => Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "not found" }
                    }))
                    .into_response(),
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        let mock_handle = tokio::spawn(async move {
            let _ = axum::serve(mock_listener, mock_router).await;
        });

        let state = Arc::new(AppState::new(temp_dir.clone()));
        let cfg = crate::config::DownstreamMcpConfig {
            id: "mock1".to_string(),
            name: "DocsHub".to_string(),
            namespace: "docshub".to_string(),
            transport: "http".to_string(),
            url: format!("http://{mock_addr}/mcp"),
            bearer_token: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            enabled: true,
        };
        {
            let mut app_cfg = state.config.write().await;
            app_cfg.downstream_mcps.push(cfg.clone());
        }
        let discovered = state.mcp_hub.refresh_server(&cfg).await.unwrap();
        assert_eq!(discovered, 1);

        let hub_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub_addr = hub_listener.local_addr().unwrap();
        let hub_router = build_router(state.clone());
        let hub_handle = tokio::spawn(async move {
            let _ = axum::serve(hub_listener, hub_router).await;
        });

        let token = state.auth_token.read().await.clone();
        let client = reqwest::Client::new();
        let list_resp: Value = client
            .post(format!("http://{hub_addr}/mcp"))
            .bearer_auth(&token)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let tools_arr = list_resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools_arr.len(), 15);
        assert!(tools_arr.iter().any(|t| t["name"] == "docshub__ping_doc"));

        let call_resp: Value = client
            .post(format!("http://{hub_addr}/mcp"))
            .bearer_auth(&token)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "docshub__ping_doc",
                    "arguments": { "topic": "tauri" }
                }
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(call_resp["result"]["isError"], false);
        assert_eq!(call_resp["result"]["content"][0]["text"], "doc-result:tauri");

        hub_handle.abort();
        mock_handle.abort();
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
