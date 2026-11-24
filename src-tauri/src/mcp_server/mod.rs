mod auth;
mod http;
mod rpc;
#[cfg(test)]
mod tests;

pub(crate) use auth::compute_base_url;
#[allow(unused_imports)]
pub(crate) use rpc::{handle_single_rpc, handle_single_rpc_with_context};

use crate::oauth;
use crate::state::AppState;
use crate::tools::mcp_tools_list;
use axum::extract::State;
use axum::http::{header, HeaderName, HeaderValue, Method};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use http::{
    handle_mcp_delete, handle_mcp_delete_with_token, handle_mcp_get, handle_mcp_get_with_token,
    handle_mcp_post, handle_mcp_post_with_token,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tower_http::cors::{AllowOrigin, CorsLayer};

pub async fn total_tools_count(state: &AppState) -> usize {
    let builtin_count = mcp_tools_list()
        .as_array()
        .map(|a| a.len())
        .unwrap_or(28);
    let cfgs = state.resolved_downstream_mcps().await;
    let hub_count = state.mcp_hub.list_aggregated_tools(&cfgs).await.len();
    builtin_count + hub_count
}

async fn handle_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let tools = total_tools_count(&state).await;
    Json(json!({ "status": "ok", "service": "smart-tauri-hub", "tools": tools }))
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
            HeaderName::from_static("x-airlock-workspace"),
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
    let client = &state.http_client;
    let diag_timeout = Duration::from_secs(4);

    let base_local = format!("http://127.0.0.1:{port}");
    let health_ok = match client
        .get(format!("{base_local}/health"))
        .timeout(diag_timeout)
        .send()
        .await
    {
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
        .timeout(diag_timeout)
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
        .timeout(diag_timeout)
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

    let expected_builtin = mcp_tools_list().as_array().map(|a| a.len()).unwrap_or(28);
    if health_ok && mcp_initialize_ok && tools_count >= expected_builtin {
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
