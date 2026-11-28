use super::auth::{
    check_auth, compute_base_url, extract_execution_mode, extract_pinned_workspace,
    extract_tool_profile, sanitize_preview, unauthorized_response, McpToolProfile, TokenQuery,
    WebMcpExecutionMode,
};
use super::rpc::{handle_single_rpc_with_request_context, RpcRequestContext};
use super::total_tools_count;
use crate::state::{AppState, HttpTrafficEntry};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

fn is_rpc_message_read_only(msg: &Value) -> bool {
    let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
    match method {
        "ping"
        | "tools/list"
        | "resources/list"
        | "resources/read"
        | "prompts/list"
        | "skills/list"
        | "server/discover"
        | "tasks/list"
        | "tasks/get"
        | "tasks/result" => true,
        "tools/call" => {
            let name = msg
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name.contains("__") {
                return false;
            }
            let args = msg
                .get("params")
                .and_then(|p| p.get("arguments"))
                .unwrap_or(&Value::Null);
            crate::tools::is_tool_read_only(name, args)
        }
        _ => false,
    }
}

async fn execute_rpc_batch_hybrid(
    state: Arc<AppState>,
    messages: Vec<Value>,
    pinned_ws: Option<String>,
    tool_profile: McpToolProfile,
    execution_mode: WebMcpExecutionMode,
    source: &'static str,
    session_id: Option<String>,
) -> Vec<Value> {
    let len = messages.len();
    let mut results: Vec<Option<Value>> = vec![None; len];
    let mut idx = 0usize;

    while idx < len {
        if is_rpc_message_read_only(&messages[idx]) {
            let mut end = idx + 1;
            while end < len && is_rpc_message_read_only(&messages[end]) {
                end += 1;
            }
            if end - idx == 1 {
                let ctx = RpcRequestContext {
                    pinned_ws: pinned_ws.as_deref(),
                    tool_profile,
                    execution_mode,
                    source,
                    session_id: session_id.as_deref(),
                };
                results[idx] =
                    handle_single_rpc_with_request_context(&state, &messages[idx], ctx).await;
            } else {
                let mut set: JoinSet<(usize, Option<Value>)> = JoinSet::new();
                for pos in idx..end {
                    let st = Arc::clone(&state);
                    let msg_owned = messages[pos].clone();
                    let pin_owned = pinned_ws.clone();
                    let sid_owned = session_id.clone();
                    set.spawn(async move {
                        let ctx = RpcRequestContext {
                            pinned_ws: pin_owned.as_deref(),
                            tool_profile,
                            execution_mode,
                            source,
                            session_id: sid_owned.as_deref(),
                        };
                        let out =
                            handle_single_rpc_with_request_context(&st, &msg_owned, ctx).await;
                        (pos, out)
                    });
                }
                while let Some(joined) = set.join_next().await {
                    if let Ok((pos, out)) = joined {
                        results[pos] = out;
                    }
                }
            }
            idx = end;
        } else {
            let ctx = RpcRequestContext {
                pinned_ws: pinned_ws.as_deref(),
                tool_profile,
                execution_mode,
                source,
                session_id: session_id.as_deref(),
            };
            results[idx] =
                handle_single_rpc_with_request_context(&state, &messages[idx], ctx).await;
            idx += 1;
        }
    }

    results.into_iter().flatten().collect()
}

fn attach_session_id_header(resp: &mut Response, session_id: Option<&str>) {
    if let Some(sid) = session_id {
        if let Ok(val) = HeaderValue::from_str(sid) {
            resp.headers_mut()
                .insert(HeaderName::from_static("mcp-session-id"), val);
        }
    }
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
        return unauthorized_response(&base, Some(&query));
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
        return unauthorized_response(&base, Some(&query));
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
    let (default_profile, default_mode, sse_keepalive_enabled) = {
        let cfg = state.config.read().await;
        (
            cfg.webmcp_default_profile.clone(),
            cfg.webmcp_execution_mode.clone(),
            cfg.webmcp_sse_keepalive,
        )
    };
    let tool_profile = extract_tool_profile(&headers, &query, &default_profile);
    let execution_mode = extract_execution_mode(&headers, &query, &default_mode);
    let effective_sse_keepalive =
        sse_keepalive_enabled || execution_mode == WebMcpExecutionMode::Task;

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
    if execution_mode == WebMcpExecutionMode::Task {
        note_parts.push("mode: task".to_string());
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

    let accepts_sse = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains("text/event-stream");
    let has_tools_call = messages
        .iter()
        .any(|m| m.get("method").and_then(|v| v.as_str()) == Some("tools/call"));
    let first_req_id = messages
        .iter()
        .find_map(|m| m.get("id").cloned())
        .unwrap_or(Value::Null);

    let outs = if !is_init && effective_sse_keepalive && accepts_sse && has_tools_call {
        let (tx, mut rx) = tokio::sync::oneshot::channel::<Vec<Value>>();
        let st_bg = Arc::clone(&state);
        let msgs_bg = messages;
        let pin_bg = pinned_ws.clone();
        let sid_bg = session_id.clone();
        tokio::spawn(async move {
            let res = execute_rpc_batch_hybrid(
                st_bg,
                msgs_bg,
                pin_bg,
                tool_profile,
                execution_mode,
                "WebMCP",
                sid_bg,
            )
            .await;
            let _ = tx.send(res);
        });

        let wait_fast = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(wait_fast);
        tokio::select! {
            res = &mut rx => res.unwrap_or_default(),
            _ = &mut wait_fast => {
                let sse_note = if note_parts.is_empty() {
                    "SSE keepalive stream".to_string()
                } else {
                    format!("{ok_note} | SSE keepalive")
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
                        status: 200,
                        duration_ms: started.elapsed().as_millis() as u64,
                        note: sse_note,
                        request_preview: req_preview,
                        response_preview: Some("(streaming final result via SSE)".to_string()),
                    })
                    .await;

                let stream = futures_util::stream::once(async move {
                    let body = match rx.await {
                        Ok(outs) => {
                            if is_batch {
                                Value::Array(outs)
                            } else {
                                outs.into_iter().next().unwrap_or_else(|| {
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": first_req_id,
                                        "error": {
                                            "code": -32603,
                                            "message": "SSE tool execution returned empty response"
                                        }
                                    })
                                })
                            }
                        }
                        Err(_) => json!({
                            "jsonrpc": "2.0",
                            "id": first_req_id,
                            "error": {
                                "code": -32603,
                                "message": "SSE worker task terminated before producing a response"
                            }
                        }),
                    };
                    let data = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
                    Ok::<Event, Infallible>(Event::default().event("message").data(data))
                });
                let mut resp = Sse::new(stream)
                    .keep_alive(KeepAlive::new().interval(Duration::from_secs(6)).text("keepalive"))
                    .into_response();
                resp.headers_mut().insert(
                    HeaderName::from_static("x-accel-buffering"),
                    HeaderValue::from_static("no"),
                );
                attach_session_id_header(&mut resp, session_id.as_deref());
                return resp;
            }
        }
    } else {
        execute_rpc_batch_hybrid(
            Arc::clone(&state),
            messages,
            pinned_ws.clone(),
            tool_profile,
            execution_mode,
            "WebMCP",
            session_id.clone(),
        )
        .await
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
        let mut resp = StatusCode::ACCEPTED.into_response();
        attach_session_id_header(&mut resp, session_id.as_deref());
        return resp;
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
    attach_session_id_header(&mut resp, session_id.as_deref());
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
        return unauthorized_response(&base, Some(&query));
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
