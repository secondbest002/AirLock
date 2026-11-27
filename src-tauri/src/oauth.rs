use crate::mcp_server::compute_base_url;
use crate::security::{hash_oauth_token, secure_token_eq};
use crate::state::{
    AppState, HttpTrafficEntry, OAuthClient, OAuthCode, OAuthDecision, OAuthTokenRecord,
    PendingOAuthInfo,
};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};
use tokio::sync::oneshot;

fn build_protected_resource_url(base: &str, suffix: Option<&str>, raw_query: Option<&str>) -> String {
    let clean_suffix = suffix
        .map(|s| s.trim_start_matches('/'))
        .filter(|s| !s.is_empty())
        .unwrap_or("mcp");

    let safe_query = raw_query
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .and_then(|q| {
            let mut ser = url::form_urlencoded::Serializer::new(String::new());
            let mut count = 0usize;
            for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
                if !k.eq_ignore_ascii_case("token") {
                    ser.append_pair(&k, &v);
                    count += 1;
                }
            }
            if count > 0 {
                Some(ser.finish())
            } else {
                None
            }
        });

    match safe_query {
        Some(qs) => format!("{base}/{clean_suffix}?{qs}"),
        None => format!("{base}/{clean_suffix}"),
    }
}

pub async fn handle_oauth_protected_resource(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> impl IntoResponse {
    let base = compute_base_url(&state, &headers).await;
    let resource_url = build_protected_resource_url(&base, None, raw_query.as_deref());
    Json(json!({
        "resource": resource_url,
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["mcp"]
    }))
}

pub async fn handle_oauth_protected_resource_suffix(
    State(state): State<Arc<AppState>>,
    Path(suffix): Path<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> impl IntoResponse {
    let base = compute_base_url(&state, &headers).await;
    let resource_url = build_protected_resource_url(&base, Some(&suffix), raw_query.as_deref());
    Json(json!({
        "resource": resource_url,
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["mcp"]
    }))
}

pub async fn handle_oauth_authorization_server(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let base = compute_base_url(&state, &headers).await;
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "scopes_supported": ["mcp", "mcp:read", "mcp:write"],
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic", "none"]
    }))
}

pub fn valid_redirect_uri(raw: &str) -> bool {
    if let Ok(parsed) = url::Url::parse(raw) {
        if parsed.scheme() == "https" {
            return true;
        }
        if parsed.scheme() == "http" {
            if let Some(host) = parsed.host_str() {
                return host == "127.0.0.1" || host == "localhost" || host == "::1";
            }
        }
    }
    false
}

fn redirect_oauth_error(
    redirect_uri: &str,
    oauth_state: Option<&str>,
    error: &str,
    description: &str,
) -> Response {
    let mut target = match url::Url::parse(redirect_uri) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error, "error_description": description })),
            )
                .into_response()
        }
    };
    {
        let mut qp = target.query_pairs_mut();
        qp.append_pair("error", error);
        qp.append_pair("error_description", description);
        if let Some(st) = oauth_state {
            qp.append_pair("state", st);
        }
    }
    let mut resp = StatusCode::FOUND.into_response();
    if let Ok(loc) = HeaderValue::from_str(target.as_str()) {
        resp.headers_mut().insert(header::LOCATION, loc);
    }
    resp
}

pub async fn handle_oauth_register(
    State(state): State<Arc<AppState>>,
    Json(meta): Json<Value>,
) -> Response {
    let raw_uris: Vec<String> = meta
        .get("redirect_uris")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|u| u.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let redirect_uris: Vec<String> = raw_uris
        .iter()
        .filter(|u| valid_redirect_uri(u))
        .cloned()
        .collect();

    if !raw_uris.is_empty() && redirect_uris.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_redirect_uri",
                "error_description": "redirect_uris must be https or http loopback"
            })),
        )
            .into_response();
    }

    let client_name = meta
        .get("client_name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let requested_method = meta
        .get("token_endpoint_auth_method")
        .and_then(|v| v.as_str())
        .unwrap_or("client_secret_post");
    let auth_method = match requested_method {
        "none" | "client_secret_post" | "client_secret_basic" => requested_method.to_string(),
        _ => "client_secret_post".to_string(),
    };

    let client_id = format!("mcp_{}", AppState::generate_token());
    let client_secret = if auth_method == "none" {
        String::new()
    } else {
        format!("{}{}", AppState::generate_token(), AppState::generate_token())
    };
    let now = AppState::now_epoch_secs();

    {
        let mut clients = state.oauth_clients.write().await;
        clients.insert(
            client_id.clone(),
            OAuthClient {
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
                auth_method: auth_method.clone(),
                client_name: client_name.clone(),
                redirect_uris: redirect_uris.clone(),
                trusted: false,
                created_at_epoch: now,
                last_used_epoch: None,
            },
        );
    }
    state.save_auth_state().await;
    state.emit_dashboard().await;
    state
        .record_http_traffic(HttpTrafficEntry {
            timestamp: AppState::now_timestamp(),
            method: "POST".to_string(),
            path: "/oauth/register".to_string(),
            rpc_method: Some("oauth/register".to_string()),
            status: 201,
            duration_ms: 0,
            note: format!(
                "Registered client {}{}",
                client_id,
                client_name
                    .as_ref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default()
            ),
            request_preview: None,
            response_preview: None,
        })
        .await;

    let mut body = json!({
        "client_id": client_id,
        "client_id_issued_at": now,
        "client_secret_expires_at": 0,
        "redirect_uris": redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": auth_method,
        "scope": "mcp"
    });
    if let Some(name) = client_name {
        body["client_name"] = Value::String(name);
    }
    if !client_secret.is_empty() {
        body["client_secret"] = Value::String(client_secret);
    }

    (StatusCode::CREATED, Json(body)).into_response()
}

pub async fn handle_oauth_authorize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let started = Instant::now();
    let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
    if redirect_uri.is_empty() || !valid_redirect_uri(&redirect_uri) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_redirect_uri",
                "error_description": "A valid https or http loopback redirect_uri is required"
            })),
        )
            .into_response();
    }

    let client_id = params.get("client_id").cloned().unwrap_or_default();
    if client_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_request",
                "error_description": "client_id is required"
            })),
        )
            .into_response();
    }

    let challenge_raw = params.get("code_challenge").cloned().unwrap_or_default();
    let method = params
        .get("code_challenge_method")
        .cloned()
        .unwrap_or_else(|| "S256".to_string());
    if challenge_raw.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_request",
                "error_description": "PKCE code_challenge (S256) is required"
            })),
        )
            .into_response();
    }
    if method != "S256" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_request",
                "error_description": "Only code_challenge_method=S256 is supported"
            })),
        )
            .into_response();
    }
    let challenge = Some(challenge_raw);

    let oauth_state = params.get("state").cloned();
    let scope = params
        .get("scope")
        .cloned()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "mcp".to_string());

    let (is_trusted_client, client_name) = {
        let clients = state.oauth_clients.read().await;
        let Some(client) = clients.get(&client_id) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "invalid_client",
                    "error_description": "Unknown client_id. Register first via /oauth/register."
                })),
            )
                .into_response();
        };
        let uri_allowed = client.redirect_uris.is_empty()
            || client.redirect_uris.iter().any(|u| u == &redirect_uri);
        if !uri_allowed {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "invalid_redirect_uri",
                    "error_description": "redirect_uri is not registered for this client_id"
                })),
            )
                .into_response();
        }
        (client.trusted, client.client_name.clone())
    };

    if !is_trusted_client && state.require_oauth_consent.load(Ordering::SeqCst) {
        let request_id = format!("req_{}", AppState::generate_token());
        let info = PendingOAuthInfo {
            request_id: request_id.clone(),
            client_id: client_id.clone(),
            client_name,
            redirect_uri: redirect_uri.clone(),
            scope,
            created_at: AppState::now_timestamp(),
        };

        let (tx, rx) = oneshot::channel::<OAuthDecision>();
        {
            let mut pending = state.pending_oauth.write().await;
            pending.insert(request_id.clone(), (info.clone(), tx));
        }

        if let Some(app) = state.app_handle.read().await.clone() {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
            }
            let _ = app.emit("oauth-consent-request", &info);
        }

        let decision = tokio::time::timeout(Duration::from_secs(120), rx).await;

        {
            let mut pending = state.pending_oauth.write().await;
            pending.remove(&request_id);
        }
        if let Some(app) = state.app_handle.read().await.clone() {
            let _ = app.emit("oauth-consent-resolved", &request_id);
        }

        match decision {
            Ok(Ok(OAuthDecision::ApproveOnce)) => {}
            Ok(Ok(OAuthDecision::ApproveAndTrust)) => {
                if !client_id.is_empty() {
                    {
                        let mut clients = state.oauth_clients.write().await;
                        if let Some(entry) = clients.get_mut(&client_id) {
                            entry.trusted = true;
                            if !entry.redirect_uris.iter().any(|u| u == &redirect_uri) {
                                entry.redirect_uris.push(redirect_uri.clone());
                            }
                        }
                    }
                    state.save_auth_state().await;
                    state.emit_dashboard().await;
                }
            }
            Ok(Ok(OAuthDecision::Deny)) => {
                state
                    .record_http_traffic(HttpTrafficEntry {
                        timestamp: AppState::now_timestamp(),
                        method: "GET".to_string(),
                        path: "/oauth/authorize".to_string(),
                        rpc_method: Some("oauth/authorize".to_string()),
                        status: 302,
                        duration_ms: started.elapsed().as_millis() as u64,
                        note: "OAuth denied by user".to_string(),
                        request_preview: None,
                        response_preview: None,
                    })
                    .await;
                return redirect_oauth_error(
                    &redirect_uri,
                    oauth_state.as_deref(),
                    "access_denied",
                    "User denied authorization",
                );
            }
            Err(_) | Ok(Err(_)) => {
                state
                    .record_http_traffic(HttpTrafficEntry {
                        timestamp: AppState::now_timestamp(),
                        method: "GET".to_string(),
                        path: "/oauth/authorize".to_string(),
                        rpc_method: Some("oauth/authorize".to_string()),
                        status: 302,
                        duration_ms: started.elapsed().as_millis() as u64,
                        note: "OAuth consent timed out".to_string(),
                        request_preview: None,
                        response_preview: None,
                    })
                    .await;
                return redirect_oauth_error(
                    &redirect_uri,
                    oauth_state.as_deref(),
                    "access_denied",
                    "Authorization timed out",
                );
            }
        }
    }

    let code = format!("{}{}", AppState::generate_token(), AppState::generate_token());
    {
        let mut codes = state.oauth_codes.write().await;
        codes.retain(|_, v| v.expires_at > Instant::now());
        codes.insert(
            code.clone(),
            OAuthCode {
                client_id,
                redirect_uri: redirect_uri.clone(),
                challenge,
                method,
                expires_at: Instant::now() + Duration::from_secs(600),
            },
        );
    }

    let base = compute_base_url(&state, &headers).await;
    let mut target = match url::Url::parse(&redirect_uri) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_redirect_uri" })),
            )
                .into_response()
        }
    };
    {
        let mut qp = target.query_pairs_mut();
        qp.append_pair("code", &code);
        if let Some(st) = oauth_state {
            qp.append_pair("state", &st);
        }
        qp.append_pair("iss", &base);
    }

    let mut resp = StatusCode::FOUND.into_response();
    if let Ok(loc) = HeaderValue::from_str(target.as_str()) {
        resp.headers_mut().insert(header::LOCATION, loc);
    }
    state
        .record_http_traffic(HttpTrafficEntry {
            timestamp: AppState::now_timestamp(),
            method: "GET".to_string(),
            path: "/oauth/authorize".to_string(),
            rpc_method: Some("oauth/authorize".to_string()),
            status: 302,
            duration_ms: started.elapsed().as_millis() as u64,
            note: if is_trusted_client {
                "Issued OAuth auth code (trusted client)".to_string()
            } else {
                "Issued OAuth auth code (user approved)".to_string()
            },
            request_preview: None,
            response_preview: None,
        })
        .await;
    resp
}

pub async fn handle_oauth_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    raw_body: String,
) -> Response {
    let form: HashMap<String, String> = url::form_urlencoded::parse(raw_body.as_bytes())
        .into_owned()
        .collect();

    let mut client_id = form.get("client_id").cloned().unwrap_or_default();
    let mut client_secret = form.get("client_secret").cloned().unwrap_or_default();

    if let Some(basic) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Basic "))
    {
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(basic.trim()) {
            if let Ok(creds) = String::from_utf8(decoded) {
                if let Some((id, sec)) = creds.split_once(':') {
                    if client_id.is_empty() {
                        client_id = id.to_string();
                    }
                    if client_secret.is_empty() {
                        client_secret = sec.to_string();
                    }
                }
            }
        }
    }

    let grant_type = form
        .get("grant_type")
        .map(|s| s.as_str())
        .unwrap_or("authorization_code");

    let now = AppState::now_epoch_secs();

    if grant_type == "refresh_token" {
        let rt = form.get("refresh_token").cloned().unwrap_or_default();
        let rt_hash = hash_oauth_token(&rt);
        let rec_opt = {
            let rts = state.oauth_refresh_tokens.read().await;
            rts.get(&rt_hash).cloned()
        };
        let Some(rec) = rec_opt else {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid_grant", "error_description": "Unknown refresh token" })),
            )
                .into_response();
        };
        if rec.expires_at_epoch.map(|e| e <= now).unwrap_or(false) {
            state.oauth_refresh_tokens.write().await.remove(&rt_hash);
            state.save_auth_state().await;
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid_grant", "error_description": "Expired refresh token" })),
            )
                .into_response();
        }
        let cid = rec.client_id;
        if !client_id.is_empty() && client_id != cid {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid_grant", "error_description": "client_id mismatch" })),
            )
                .into_response();
        }
        {
            let mut clients = state.oauth_clients.write().await;
            if !cid.is_empty() {
                let Some(client) = clients.get_mut(&cid) else {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "error": "invalid_grant", "error_description": "Client has been revoked" })),
                    )
                        .into_response();
                };
                if client.auth_method != "none"
                    && !client.client_secret.is_empty()
                    && !secure_token_eq(&client.client_secret, &client_secret)
                {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "error": "invalid_client", "error_description": "Client secret mismatch" })),
                    )
                        .into_response();
                }
                client.last_used_epoch = Some(now);
            }
        }
        let access = format!("smcp_at_{}{}", AppState::generate_token(), AppState::generate_token());
        let exp_epoch = now + 86400;
        state.oauth_access_tokens.write().await.insert(
            hash_oauth_token(&access),
            OAuthTokenRecord {
                client_id: cid,
                expires_at_epoch: Some(exp_epoch),
                issued_at_epoch: now,
            },
        );
        state.save_auth_state().await;
        state.emit_dashboard().await;
        state
            .record_http_traffic(HttpTrafficEntry {
                timestamp: AppState::now_timestamp(),
                method: "POST".to_string(),
                path: "/oauth/token".to_string(),
                rpc_method: Some("oauth/refresh".to_string()),
                status: 200,
                duration_ms: 0,
                note: "Refreshed OAuth access token".to_string(),
                request_preview: None,
                response_preview: None,
            })
            .await;
        return (
            StatusCode::OK,
            Json(json!({
                "access_token": access,
                "token_type": "Bearer",
                "expires_in": 86400,
                "refresh_token": rt,
                "scope": "mcp"
            })),
        )
            .into_response();
    }

    let code = form.get("code").cloned().unwrap_or_default();
    let rec = state.oauth_codes.write().await.remove(&code);
    let Some(rec) = rec else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_grant", "error_description": "Unknown or expired code" })),
        )
            .into_response();
    };
    if rec.expires_at < Instant::now() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_grant", "error_description": "Expired code" })),
        )
            .into_response();
    }
    if !rec.client_id.is_empty() && rec.client_id != client_id {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_grant", "error_description": "client_id mismatch" })),
        )
            .into_response();
    }
    let redirect_uri = form.get("redirect_uri").cloned().unwrap_or_default();
    if !rec.redirect_uri.is_empty() && redirect_uri != rec.redirect_uri {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_grant", "error_description": "redirect_uri mismatch" })),
        )
            .into_response();
    }

    let Some(challenge) = &rec.challenge else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_grant", "error_description": "PKCE code_challenge missing" })),
        )
            .into_response();
    };
    if rec.method != "S256" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_grant", "error_description": "Unsupported PKCE method" })),
        )
            .into_response();
    }
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let digest = Sha256::digest(verifier.as_bytes());
    let computed = URL_SAFE_NO_PAD.encode(digest);
    if verifier.is_empty() || !secure_token_eq(&computed, challenge) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_grant", "error_description": "PKCE verification failed" })),
        )
            .into_response();
    }

    let effective_client_id = if !rec.client_id.is_empty() {
        rec.client_id.clone()
    } else {
        client_id.clone()
    };

    {
        let mut clients = state.oauth_clients.write().await;
        let Some(client) = clients.get_mut(&effective_client_id) else {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid_client", "error_description": "Unknown or revoked OAuth client" })),
            )
                .into_response();
        };
        if client.auth_method != "none"
            && !client.client_secret.is_empty()
            && !secure_token_eq(&client.client_secret, &client_secret)
        {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid_client", "error_description": "Client secret mismatch" })),
            )
                .into_response();
        }
        client.last_used_epoch = Some(now);
    }

    let access = format!("smcp_at_{}{}", AppState::generate_token(), AppState::generate_token());
    let refresh = format!("smcp_rt_{}{}", AppState::generate_token(), AppState::generate_token());
    let exp_epoch = now + 86400;
    let refresh_exp_epoch = now + 30 * 86400;

    state.oauth_access_tokens.write().await.insert(
        hash_oauth_token(&access),
        OAuthTokenRecord {
            client_id: effective_client_id.clone(),
            expires_at_epoch: Some(exp_epoch),
            issued_at_epoch: now,
        },
    );
    state.oauth_refresh_tokens.write().await.insert(
        hash_oauth_token(&refresh),
        OAuthTokenRecord {
            client_id: effective_client_id,
            expires_at_epoch: Some(refresh_exp_epoch),
            issued_at_epoch: now,
        },
    );
    state.save_auth_state().await;
    state.emit_dashboard().await;
    state
        .record_http_traffic(HttpTrafficEntry {
            timestamp: AppState::now_timestamp(),
            method: "POST".to_string(),
            path: "/oauth/token".to_string(),
            rpc_method: Some("oauth/token".to_string()),
            status: 200,
            duration_ms: 0,
            note: "Minted OAuth access & refresh token".to_string(),
            request_preview: None,
            response_preview: None,
        })
        .await;

    (
        StatusCode::OK,
        Json(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": 86400,
            "refresh_token": refresh,
            "scope": "mcp"
        })),
    )
        .into_response()
}

pub async fn resolve_oauth_consent(
    state: &AppState,
    request_id: &str,
    decision: OAuthDecision,
) -> Result<(), String> {
    let entry = {
        let mut pending = state.pending_oauth.write().await;
        pending.remove(request_id)
    };
    let Some((_, tx)) = entry else {
        return Err("Permintaan OAuth sudah kadaluarsa atau tidak ditemukan.".to_string());
    };
    let _ = tx.send(decision);
    Ok(())
}

pub async fn revoke_oauth_client(state: &AppState, client_id: &str) -> Result<String, String> {
    let removed = {
        let mut clients = state.oauth_clients.write().await;
        clients.remove(client_id)
    };
    if removed.is_none() {
        return Err(format!("OAuth client '{client_id}' tidak ditemukan."));
    }
    state
        .oauth_access_tokens
        .write()
        .await
        .retain(|_, rec| rec.client_id != client_id);
    state
        .oauth_refresh_tokens
        .write()
        .await
        .retain(|_, rec| rec.client_id != client_id);
    state
        .oauth_codes
        .write()
        .await
        .retain(|_, code| code.client_id != client_id);
    state
        .provider_secrets
        .write()
        .await
        .oauth_client_secrets
        .remove(client_id);

    state.save_auth_state().await;
    state.emit_dashboard().await;
    Ok(format!("OAuth client '{client_id}' dan seluruh tokennya telah dicabut."))
}

pub async fn revoke_all_oauth(state: &AppState) -> usize {
    let cleared_clients = {
        let mut clients = state.oauth_clients.write().await;
        let count = clients.len();
        clients.clear();
        count
    };
    let cleared_access = {
        let mut at = state.oauth_access_tokens.write().await;
        let count = at.len();
        at.clear();
        count
    };
    let cleared_refresh = {
        let mut rt = state.oauth_refresh_tokens.write().await;
        let count = rt.len();
        rt.clear();
        count
    };
    state.oauth_codes.write().await.clear();
    state
        .provider_secrets
        .write()
        .await
        .oauth_client_secrets
        .clear();

    let pending_senders: Vec<oneshot::Sender<OAuthDecision>> = {
        let mut pending = state.pending_oauth.write().await;
        pending.drain().map(|(_, (_, tx))| tx).collect()
    };
    for tx in pending_senders {
        let _ = tx.send(OAuthDecision::Deny);
    }

    state.save_auth_state().await;
    state.emit_dashboard().await;
    cleared_clients + cleared_access + cleared_refresh
}
