use super::*;
use crate::oauth;
use crate::state::{AppState, OAuthDecision};
use crate::tools::{mcp_tools_list, mcp_tools_list_for_profile};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

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

    // 3. tools/list with ?profile=full returns WebMCP-eligible built-in tools (excluding Local-Agent-only spawn_subagent, and gating delegate_local_agent on provider readiness)
    let expected_webmcp_full =
        mcp_tools_list_for_profile(McpToolProfile::Full, false, true, true)
            .as_array()
            .unwrap()
            .len();
    assert_eq!(
        expected_webmcp_full,
        mcp_tools_list().as_array().unwrap().len() - 2
    );
    let expected_webmcp_full_with_delegate =
        mcp_tools_list_for_profile(McpToolProfile::Full, true, true, true)
            .as_array()
            .unwrap()
            .len();
    assert_eq!(
        expected_webmcp_full_with_delegate,
        mcp_tools_list().as_array().unwrap().len() - 1
    );
    let full_endpoint = format!("{endpoint}?profile=full");
    let list_resp: Value = client
        .post(&full_endpoint)
        .bearer_auth(&token)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tools_arr = list_resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools_arr.len(), expected_webmcp_full);
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
        .post(&full_endpoint)
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
        expected_webmcp_full
    );
    assert_eq!(
        discover_resp["result"]["resources"].as_array().unwrap().len(),
        crate::mcp_resources::resources_list()
            .as_array()
            .unwrap()
            .len()
    );
    assert_eq!(
        discover_resp["result"]["prompts"].as_array().unwrap().len(),
        crate::mcp_resources::prompts_list_for_context(&state, None)
            .await
            .as_array()
            .unwrap()
            .len()
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
    assert_eq!(as_meta["code_challenge_methods_supported"], json!(["S256"]));

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

    // 4a. Unregistered client_id must be rejected at /oauth/authorize, and denied consent redirects with access_denied
    let code_verifier = "test_pkce_verifier_secret_value_123456";
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));

    let unknown_resp = client
        .get(format!("{base}/oauth/authorize"))
        .query(&[
            ("client_id", "untrusted-stranger"),
            ("redirect_uri", "https://gemini.google.com/oauth/callback"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", "unknown-state"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(unknown_resp.status(), StatusCode::BAD_REQUEST);

    let deny_reg: Value = client
        .post(format!("{base}/oauth/register"))
        .json(&json!({
            "client_name": "Untrusted Stranger",
            "redirect_uris": ["https://gemini.google.com/oauth/callback"],
            "token_endpoint_auth_method": "none"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let deny_client_id = deny_reg["client_id"].as_str().unwrap().to_string();

    let deny_task = tokio::spawn(approve_next_pending_oauth(
        state.clone(),
        OAuthDecision::Deny,
    ));
    let denied_resp = client
        .get(format!("{base}/oauth/authorize"))
        .query(&[
            ("client_id", deny_client_id.as_str()),
            ("redirect_uri", "https://gemini.google.com/oauth/callback"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
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

    // 4c. Second authorize for trusted client succeeds immediately WITHOUT consent prompt (with PKCE S256)
    let auth_resp_2 = client
        .get(format!("{base}/oauth/authorize"))
        .query(&[
            ("client_id", client_id.as_str()),
            ("redirect_uri", "https://gemini.google.com/oauth/callback"),
            ("state", "trusted-second"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
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
        .post(format!("{base}/mcp?profile=full"))
        .bearer_auth(&access_token)
        .json(&json!({"jsonrpc":"2.0","id":11,"method":"tools/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let expected_full_webmcp =
        mcp_tools_list_for_profile(McpToolProfile::Full, false, true, true)
            .as_array()
            .unwrap()
            .len();
    assert_eq!(
        mcp_list["result"]["tools"].as_array().unwrap().len(),
        expected_full_webmcp
    );

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
    assert_eq!(
        res_list["result"]["resources"].as_array().unwrap().len(),
        crate::mcp_resources::resources_list()
            .as_array()
            .unwrap()
            .len()
    );

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
    assert_eq!(prm_list["result"]["prompts"].as_array().unwrap().len(), 5);

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

    let (access_token, refresh_token, client_id, client_secret, master_token) = {
        let state1 = Arc::new(AppState::new_persistent(
            temp_dir.clone(),
            auth_file.clone(),
            config_file.clone(),
        ));
        let master_tok = state1.auth_token.read().await.clone();
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

        let reg: Value = client
            .post(format!("{base}/oauth/register"))
            .json(&json!({
                "client_name": "Gemini Persist Test",
                "redirect_uris": ["https://gemini.google.com/oauth/callback"],
                "token_endpoint_auth_method": "client_secret_post"
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let cid = reg["client_id"].as_str().unwrap().to_string();
        let csec = reg["client_secret"].as_str().unwrap().to_string();

        let verifier = "persist_verifier_pkce_secret_value_999";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

        let approve_task = tokio::spawn(approve_next_pending_oauth(
            state1.clone(),
            OAuthDecision::ApproveOnce,
        ));
        let auth_resp = client
            .get(format!("{base}/oauth/authorize"))
            .query(&[
                ("client_id", cid.as_str()),
                ("redirect_uri", "https://gemini.google.com/oauth/callback"),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
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
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("client_id", cid.as_str()),
                ("client_secret", csec.as_str()),
                ("redirect_uri", "https://gemini.google.com/oauth/callback"),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        handle.abort();
        (
            tok["access_token"].as_str().unwrap().to_string(),
            tok["refresh_token"].as_str().unwrap().to_string(),
            cid,
            csec,
            master_tok,
        )
    };

    // auth.json on disk must NOT store raw tokens or secrets in plaintext
    let auth_disk = fs::read_to_string(&auth_file).unwrap();
    assert!(!auth_disk.contains(&access_token));
    assert!(!auth_disk.contains(&refresh_token));
    assert!(!auth_disk.contains(&master_token));
    assert!(!auth_disk.contains(&client_secret));
    assert!(auth_disk.contains("sha256:"));

    // Restart with fresh AppState loading the same auth.json, config.json, and secrets.dpapi
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

    // Refresh token flow also works across restart using hashed token & DPAPI client_secret
    let refresh_resp: Value = client2
        .post(format!("http://{addr2}/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let refreshed_access = refresh_resp["access_token"].as_str().unwrap();
    let resp2: Value = client2
        .post(format!("http://{addr2}/mcp"))
        .bearer_auth(refreshed_access)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"ping"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp2["id"], 2);
    assert!(resp2.get("error").is_none());

    handle2.abort();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_oauth2_strict_pkce_and_binding_rejections() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_oauth_strict_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    state.require_oauth_consent.store(false, Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let base = format!("http://{addr}");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let reg: Value = client
        .post(format!("{base}/oauth/register"))
        .json(&json!({
            "client_name": "Strict PKCE Client",
            "redirect_uris": ["https://gemini.google.com/oauth/callback"],
            "token_endpoint_auth_method": "client_secret_post"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cid = reg["client_id"].as_str().unwrap().to_string();
    let csec = reg["client_secret"].as_str().unwrap().to_string();

    let verifier = "strict_verifier_123456789";
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

    // 1. Missing code_challenge -> 400
    let missing_pkce = client
        .get(format!("{base}/oauth/authorize"))
        .query(&[
            ("client_id", cid.as_str()),
            ("redirect_uri", "https://gemini.google.com/oauth/callback"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(missing_pkce.status(), StatusCode::BAD_REQUEST);

    // 2. code_challenge_method=plain -> 400
    let plain_pkce = client
        .get(format!("{base}/oauth/authorize"))
        .query(&[
            ("client_id", cid.as_str()),
            ("redirect_uri", "https://gemini.google.com/oauth/callback"),
            ("code_challenge", verifier),
            ("code_challenge_method", "plain"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(plain_pkce.status(), StatusCode::BAD_REQUEST);

    // 3. Unregistered redirect_uri for registered client -> 400
    let bad_redir = client
        .get(format!("{base}/oauth/authorize"))
        .query(&[
            ("client_id", cid.as_str()),
            ("redirect_uri", "https://evil.example.com/cb"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(bad_redir.status(), StatusCode::BAD_REQUEST);

    // Helper to mint a valid code
    let issue_code = || async {
        let r = client
            .get(format!("{base}/oauth/authorize"))
            .query(&[
                ("client_id", cid.as_str()),
                ("redirect_uri", "https://gemini.google.com/oauth/callback"),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FOUND);
        let loc = r.headers().get("location").unwrap().to_str().unwrap();
        url::Url::parse(loc)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .to_string()
    };

    // 4. Empty redirect_uri at /oauth/token -> 401
    let c1 = issue_code().await;
    let t1 = client
        .post(format!("{base}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", c1.as_str()),
            ("client_id", cid.as_str()),
            ("client_secret", csec.as_str()),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(t1.status(), StatusCode::UNAUTHORIZED);

    // 5. Empty client_id at /oauth/token -> 401
    let c2 = issue_code().await;
    let t2 = client
        .post(format!("{base}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", c2.as_str()),
            ("redirect_uri", "https://gemini.google.com/oauth/callback"),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(t2.status(), StatusCode::UNAUTHORIZED);

    // 6. Wrong code_verifier -> 401
    let c3 = issue_code().await;
    let t3 = client
        .post(format!("{base}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", c3.as_str()),
            ("client_id", cid.as_str()),
            ("client_secret", csec.as_str()),
            ("redirect_uri", "https://gemini.google.com/oauth/callback"),
            ("code_verifier", "wrong_verifier"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(t3.status(), StatusCode::UNAUTHORIZED);

    handle.abort();
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
        headers: HashMap::new(),
        plugin_id: None,
        enabled: true,
    };
    {
        let mut app_cfg = state.config.write().await;
        app_cfg.downstream_mcps.push(cfg.clone());
    }
    let discovered = state
        .mcp_hub
        .refresh_server(&state.http_client, &cfg)
        .await
        .unwrap();
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
        .post(format!("http://{hub_addr}/mcp?profile=full"))
        .bearer_auth(&token)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let expected_builtin =
        mcp_tools_list_for_profile(McpToolProfile::Full, false, true, true)
            .as_array()
            .unwrap()
            .len();
    let tools_arr = list_resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools_arr.len(), expected_builtin + 1);
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

#[tokio::test]
async fn test_mcp_hub_http_session_id_and_404_reinit() {
    use std::sync::atomic::AtomicUsize;

    let temp_dir = std::env::temp_dir().join("smart_mcp_hub_sid_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let init_count = Arc::new(AtomicUsize::new(0));
    let expired_once = Arc::new(AtomicUsize::new(0));
    let init_c = init_count.clone();
    let exp_c = expired_once.clone();

    let mock_router = Router::new().route(
        "/mcp",
        post(move |headers: HeaderMap, Json(payload): Json<Value>| {
            let init_c = init_c.clone();
            let exp_c = exp_c.clone();
            async move {
                let id = payload.get("id").cloned().unwrap_or(Value::Null);
                let method = payload.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let sid = headers
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                match method {
                    "initialize" => {
                        let n = init_c.fetch_add(1, Ordering::SeqCst) + 1;
                        let mut resp = Json(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-03-26",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "stateful-mock", "version": "1.0" }
                            }
                        }))
                        .into_response();
                        resp.headers_mut().insert(
                            HeaderName::from_static("mcp-session-id"),
                            HeaderValue::from_str(&format!("sess-{n}")).unwrap(),
                        );
                        resp
                    }
                    "notifications/initialized" => {
                        if sid.is_none() {
                            return (StatusCode::BAD_REQUEST, "missing mcp-session-id").into_response();
                        }
                        StatusCode::ACCEPTED.into_response()
                    }
                    "tools/list" => {
                        if sid.is_none() {
                            return (StatusCode::BAD_REQUEST, "missing mcp-session-id").into_response();
                        }
                        Json(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [{
                                    "name": "ping_sid",
                                    "description": "Stateful session check",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": { "topic": { "type": "string" } }
                                    },
                                    "annotations": { "readOnlyHint": true }
                                }]
                            }
                        }))
                        .into_response()
                    }
                    "tools/call" => {
                        let Some(actual_sid) = sid else {
                            return (StatusCode::BAD_REQUEST, "missing mcp-session-id").into_response();
                        };
                        if actual_sid == "sess-1"
                            && exp_c.fetch_add(1, Ordering::SeqCst) == 0
                        {
                            return (StatusCode::NOT_FOUND, "session expired").into_response();
                        }
                        let topic = payload["params"]["arguments"]["topic"]
                            .as_str()
                            .unwrap_or("none");
                        Json(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{ "type": "text", "text": format!("{actual_sid}:{topic}") }],
                                "isError": false
                            }
                        }))
                        .into_response()
                    }
                    _ => StatusCode::BAD_REQUEST.into_response(),
                }
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
        id: "stateful1".to_string(),
        name: "StatefulMock".to_string(),
        namespace: "stateful".to_string(),
        transport: "http".to_string(),
        url: format!("http://{mock_addr}/mcp"),
        bearer_token: String::new(),
        command: String::new(),
        args: Vec::new(),
        env: HashMap::new(),
        headers: HashMap::new(),
        plugin_id: None,
        enabled: true,
    };
    state.config.write().await.downstream_mcps.push(cfg.clone());

    let count = state
        .mcp_hub
        .refresh_server(&state.http_client, &cfg)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // First tools/call hits simulated 404 on sess-1, auto-reinitializes to sess-2, and succeeds!
    let out = state
        .mcp_hub
        .call_downstream_tool(
            &state,
            "WebMCP",
            "stateful__ping_sid",
            json!({ "topic": "session-ok" }),
        )
        .await;
    assert!(!out.is_error, "Unexpected error: {}", out.text);
    assert_eq!(out.text, "sess-2:session-ok");
    assert_eq!(init_count.load(Ordering::SeqCst), 2);

    mock_handle.abort();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_mcp_http_pinned_workspace_query_and_header() {
    use crate::workspace::{normalize_clean_path, WorkspaceProfile};

    let base = std::env::temp_dir().join("smart_mcp_http_pin_ws_test");
    let _ = fs::remove_dir_all(&base);
    let dir_api = base.join("api");
    let dir_web = base.join("web");
    fs::create_dir_all(&dir_api).unwrap();
    fs::create_dir_all(&dir_web).unwrap();
    fs::write(dir_api.join("api.txt"), "API_MARKER\n").unwrap();
    fs::write(dir_web.join("web.txt"), "WEB_MARKER\n").unwrap();

    let (_, clean_api) = normalize_clean_path(&dir_api).unwrap();
    let (_, clean_web) = normalize_clean_path(&dir_web).unwrap();

    let state = Arc::new(AppState::new(dir_api.clone()));
    {
        let mut cfg = state.config.write().await;
        cfg.workspaces = vec![
            WorkspaceProfile {
                id: "ws-api".to_string(),
                alias: "api".to_string(),
                path: clean_api,
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
            WorkspaceProfile {
                id: "ws-web".to_string(),
                alias: "web".to_string(),
                path: clean_web,
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
        ];
        cfg.default_workspace_id = Some("ws-api".to_string());
    }
    state.sync_default_workspace_root().await;

    let token = state.auth_token.read().await.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();

    // 1. Pinned via ?workspace=web defaults reads to @web (even though default is @api)
    let pin_query_url = format!("http://{addr}/mcp?workspace=web");
    let read_web: Value = client
        .post(&pin_query_url)
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "read_file_range",
                "arguments": { "path": "web.txt" }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(read_web["result"]["isError"], false);
    assert!(read_web["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("WEB_MARKER"));

    // 2. Pinned via x-airlock-workspace header rejects cross-workspace request to @api
    let cross_header: Value = client
        .post(format!("http://{addr}/mcp"))
        .bearer_auth(&token)
        .header("x-airlock-workspace", "web")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "read_file_range",
                "arguments": { "workspace": "api", "path": "api.txt" }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cross_header["result"]["isError"], true);
    assert!(cross_header["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("ditolak"));

    // 3. list_workspaces when pinned ONLY shows @web
    let lw_resp: Value = client
        .post(format!("http://{addr}/mcp"))
        .bearer_auth(&token)
        .header("x-airlock-workspace", "web")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "list_workspaces",
                "arguments": {}
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let lw_text = lw_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(lw_text.contains("@web"));
    assert!(!lw_text.contains("@api"));

    handle.abort();
    let _ = fs::remove_dir_all(&base);
}

#[tokio::test]
async fn test_mcp_skills_over_mcp_rpc_and_resource_read() {
    let base = std::env::temp_dir().join("smart_mcp_skills_sep2640_test");
    let _ = fs::remove_dir_all(&base);
    let data_dir = base.join("data");
    let ws_dir = base.join("ws");
    fs::create_dir_all(ws_dir.join(".airlock/skills/rust-check/references")).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    fs::write(
        ws_dir.join(".airlock/skills/rust-check/SKILL.md"),
        "---\nname: rust-check\ndescription: Run focused Rust checks.\n---\n# Rust Check\nUse cargo test.",
    )
    .unwrap();
    fs::write(
        ws_dir.join(".airlock/skills/rust-check/references/rules.md"),
        "Zero warnings policy.\n",
    )
    .unwrap();

    let state = Arc::new(AppState::new_persistent(
        ws_dir.clone(),
        data_dir.join("auth.json"),
        data_dir.join("config.json"),
    ));
    let token = state.auth_token.read().await.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/mcp");

    // 1. initialize advertises io.modelcontextprotocol/skills capability
    let init_val: Value = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        init_val["result"]["capabilities"]["experimental"]["io.modelcontextprotocol/skills"]["listChanged"],
        true
    );

    // 2. skills/list returns discovered skill metadata
    let list_val: Value = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"skills/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let skills_arr = list_val["result"]["skills"].as_array().unwrap();
    assert_eq!(skills_arr.len(), 2);
    let rust_skill = skills_arr
        .iter()
        .find(|s| s["name"] == "rust-check")
        .expect("workspace rust-check skill should be listed");
    assert_eq!(rust_skill["uri"], "skill://rust-check");
    let builtin_skill = skills_arr
        .iter()
        .find(|s| s["name"] == "airlock-blueprint-workflow")
        .expect("builtin airlock-blueprint-workflow skill should be listed");
    assert_eq!(builtin_skill["sourceLabel"], "builtin:airlock");

    // 3. skills/get returns full SKILL.md and reference URIs
    let get_val: Value = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"skills/get",
            "params": { "name": "rust-check" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(get_val["result"]["content"]
        .as_str()
        .unwrap()
        .contains("# Rust Check"));
    assert_eq!(
        get_val["result"]["resources"][0]["uri"],
        "skill://rust-check/references/rules.md"
    );

    // 4. resources/read reads skill://<name> and skill://<name>/<rel_path>
    let res_md: Value = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"resources/read",
            "params": { "uri": "skill://rust-check" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res_md["result"]["contents"][0]["mimeType"], "text/markdown");

    let res_ref: Value = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc":"2.0",
            "id":5,
            "method":"resources/read",
            "params": { "uri": "skill://rust-check/references/rules.md" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        res_ref["result"]["contents"][0]["text"],
        "Zero warnings policy.\n"
    );

    handle.abort();
    let _ = fs::remove_dir_all(&base);
}

#[tokio::test]
async fn test_webmcp_compact_profile_hides_granular_and_dead_tools() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_compact_profile_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(temp_dir.join("notes.txt"), "line one\nline two\n").unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    let token = state.auth_token.read().await.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();

    // 1. Default /mcp and explicit ?profile=compact return <= 10 tools without dead/excluded tools
    for url in [
        format!("http://{addr}/mcp"),
        format!("http://{addr}/mcp?profile=compact"),
    ] {
        let resp: Value = client
            .post(&url)
            .bearer_auth(&token)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert!(tools.len() <= 10, "Expected <=10 compact tools, got {}", tools.len());
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"batch_inspect"));
        assert!(names.contains(&"multi_surgical_edit"));
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"lsp_query"));
        assert!(names.contains(&"session_checkpoint"));
        for hidden in [
            "spawn_subagent",
            "upsert_artifact",
            "request_plan_approval",
            "surgical_edit",
            "project_overview",
            "smart_search",
            "list_skills",
            "read_file_range",
            "run_command_compressed",
            "poll_command_job",
            "lsp_diagnostics",
        ] {
            assert!(
                !names.contains(&hidden),
                "Compact tools/list on {url} must hide '{hidden}'"
            );
        }
    }

    // 2. Query ?profile=compact overrides X-Airlock-Profile: full (Priority 1 > Priority 2)
    let override_resp: Value = client
        .post(format!("http://{addr}/mcp?profile=compact"))
        .bearer_auth(&token)
        .header("X-Airlock-Profile", "full")
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(override_resp["result"]["tools"].as_array().unwrap().len() <= 10);

    // 3. Header X-Airlock-Profile: full exposes full WebMCP tools WITHOUT spawn_subagent
    let full_hdr_resp: Value = client
        .post(format!("http://{addr}/mcp"))
        .bearer_auth(&token)
        .header("X-Airlock-Profile", "full")
        .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let full_names: Vec<&str> = full_hdr_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(!full_names.contains(&"spawn_subagent"));
    assert!(full_names.contains(&"surgical_edit"));
    assert!(full_names.contains(&"project_overview"));

    // 3b. When a Local Agent provider is ready (e.g. local Ollama URL), WebMCP tools/list exposes delegate_local_agent even in compact mode
    {
        let mut cfg = state.config.write().await;
        cfg.agent_provider = "openai_compat".to_string();
        cfg.openai_base_url = "http://127.0.0.1:11434/v1".to_string();
    }
    let compact_with_delegate: Value = client
        .post(format!("http://{addr}/mcp?profile=compact"))
        .bearer_auth(&token)
        .json(&json!({"jsonrpc":"2.0","id":10,"method":"tools/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let compact_del_names: Vec<&str> = compact_with_delegate["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        compact_del_names.contains(&"delegate_local_agent"),
        "Compact WebMCP should expose delegate_local_agent when provider is ready: {compact_del_names:?}"
    );
    assert!(compact_del_names.len() <= 10);

    // 4. Backward compatibility: calling hidden granular tools directly on ?profile=compact still works
    let call_read: Value = client
        .post(format!("http://{addr}/mcp?profile=compact"))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "read_file_range", "arguments": { "path": "notes.txt" } }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(call_read["result"]["isError"], false);
    assert!(call_read["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("line one"));

    let call_edit: Value = client
        .post(format!("http://{addr}/mcp?profile=compact"))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "surgical_edit",
                "arguments": {
                    "path": "notes.txt",
                    "edits": [{ "search": "line two", "replace": "line TWO" }]
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
    assert!(fs::read_to_string(temp_dir.join("notes.txt"))
        .unwrap()
        .contains("line TWO"));

    handle.abort();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_initialize_returns_profile_aware_instructions() {
    use crate::workspace::{normalize_clean_path, WorkspaceProfile};

    let base = std::env::temp_dir().join("smart_mcp_init_instructions_test");
    let _ = fs::remove_dir_all(&base);
    let dir_api = base.join("api-core");
    let dir_web = base.join("web-ui");
    fs::create_dir_all(&dir_api).unwrap();
    fs::create_dir_all(&dir_web).unwrap();

    let (_, clean_api) = normalize_clean_path(&dir_api).unwrap();
    let (_, clean_web) = normalize_clean_path(&dir_web).unwrap();

    let state = Arc::new(AppState::new(dir_api.clone()));
    {
        let mut cfg = state.config.write().await;
        cfg.workspaces = vec![
            WorkspaceProfile {
                id: "ws-api".to_string(),
                alias: "api-core".to_string(),
                path: clean_api,
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
            WorkspaceProfile {
                id: "ws-web".to_string(),
                alias: "web-ui".to_string(),
                path: clean_web,
                enabled: true,
                guardrail_override: Some("read_only".to_string()),
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
        ];
        cfg.default_workspace_id = Some("ws-api".to_string());
    }
    state.sync_default_workspace_root().await;

    let token = state.auth_token.read().await.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();

    let init_compact: Value = client
        .post(format!("http://{addr}/mcp?profile=compact"))
        .bearer_auth(&token)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let instr = init_compact["result"]["instructions"].as_str().unwrap();
    assert!(!instr.trim().is_empty());
    assert!(instr.chars().count() <= 1200, "Instructions too long: {}", instr.len());
    assert!(instr.contains("Profile: compact"));
    assert!(instr.contains("@api-core"));
    assert!(instr.contains("batch_inspect"));
    assert!(instr.contains("multi_surgical_edit"));

    // Pinned workspace initialize only advertises pinned workspace
    let init_pinned: Value = client
        .post(format!("http://{addr}/mcp?profile=full&workspace=web-ui"))
        .bearer_auth(&token)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"initialize"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pin_instr = init_pinned["result"]["instructions"].as_str().unwrap();
    assert!(pin_instr.contains("Profile: full"));
    assert!(pin_instr.contains("@web-ui"));
    assert!(!pin_instr.contains("@api-core"));

    handle.abort();
    let _ = fs::remove_dir_all(&base);
}

#[tokio::test]
async fn test_http_batch_executes_read_only_in_parallel_and_mutative_in_order() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_http_hybrid_batch_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(temp_dir.join("src")).unwrap();
    fs::write(
        temp_dir.join("src").join("alpha.txt"),
        "alpha line 1\nalpha_value_before\n",
    )
    .unwrap();
    fs::write(
        temp_dir.join("src").join("beta.txt"),
        "beta line 1\nbeta_target_marker\n",
    )
    .unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    let token = state.auth_token.read().await.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();
    let batch_payload = json!([
        { "jsonrpc": "2.0", "id": 10, "method": "ping" },
        {
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "batch_inspect",
                "arguments": {
                    "read_slices": [{ "path": "src/alpha.txt", "start_line": 1, "end_line": 5 }],
                    "searches": [{ "query": "beta_target_marker" }]
                }
            }
        },
        {
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        },
        {
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "multi_surgical_edit",
                "arguments": {
                    "files": [{
                        "path": "src/alpha.txt",
                        "edits": [{ "search": "alpha_value_before", "replace": "alpha_value_after" }]
                    }]
                }
            }
        },
        {
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": {
                "name": "batch_inspect",
                "arguments": {
                    "read_slices": [{ "path": "src/alpha.txt", "start_line": 1, "end_line": 5 }]
                }
            }
        },
        {
            "jsonrpc": "2.0",
            "id": 14,
            "method": "tools/call",
            "params": {
                "name": "session_checkpoint",
                "arguments": {
                    "op": "save",
                    "goal": "Hybrid batch check",
                    "status": "in_progress",
                    "next_steps": ["verify resource read"]
                }
            }
        }
    ]);

    let resp: Value = client
        .post(format!("http://{addr}/mcp?profile=compact"))
        .bearer_auth(&token)
        .json(&batch_payload)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let arr = resp.as_array().expect("Batch must return JSON array");
    assert_eq!(arr.len(), 5, "Notification omits response; 5 ID requests remain");
    assert_eq!(arr[0]["id"], 10);
    assert_eq!(arr[1]["id"], 11);
    assert_eq!(arr[2]["id"], 12);
    assert_eq!(arr[3]["id"], 13);
    assert_eq!(arr[4]["id"], 14);

    // Pre-edit read saw old content; post-edit read strictly saw new content
    let before_txt = arr[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(before_txt.contains("alpha_value_before"));
    assert!(before_txt.contains("beta_target_marker"));

    let after_txt = arr[3]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(after_txt.contains("alpha_value_after"));

    // Verify airlock://workspace/checkpoint resource serves the saved checkpoint
    let res_read: Value = client
        .post(format!("http://{addr}/mcp"))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 15,
            "method": "resources/read",
            "params": { "uri": "airlock://workspace/checkpoint" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let res_text = res_read["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(res_text.contains("Hybrid batch check"));

    handle.abort();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_http_sse_keepalive_fast_vs_slow() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_http_sse_keepalive_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    state.config.write().await.webmcp_sse_keepalive = true;
    let token = state.auth_token.read().await.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();

    // 1. Fast tools/call (<3s) even with Accept: text/event-stream returns standard JSON
    let fast_resp = client
        .post(format!("http://{addr}/mcp"))
        .bearer_auth(&token)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "list_workspaces", "arguments": {} }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(fast_resp.status(), StatusCode::OK);
    let fast_ct = fast_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        fast_ct.contains("application/json"),
        "Fast call should stay JSON, got content-type: {fast_ct}"
    );

    // 2. Slow tools/call (>3s) upgrades cleanly to text/event-stream and delivers final message
    let slow_cmd = if cfg!(target_os = "windows") {
        "ping -n 5 127.0.0.1 >NUL && echo sse_slow_done"
    } else {
        "sleep 4 && echo sse_slow_done"
    };
    let slow_resp = client
        .post(format!("http://{addr}/mcp"))
        .bearer_auth(&token)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "run_command",
                "arguments": { "action": "run", "command": slow_cmd }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(slow_resp.status(), StatusCode::OK);
    let slow_ct = slow_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        slow_ct.contains("text/event-stream"),
        "Slow call >3s should stream SSE, got content-type: {slow_ct}"
    );
    let body_text = slow_resp.text().await.unwrap();
    assert!(body_text.contains("event: message"), "SSE body:\n{body_text}");
    assert!(body_text.contains("sse_slow_done"), "SSE body:\n{body_text}");

    handle.abort();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_webmcp_session_harness_steering_and_checkpoint_anchor() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_webmcp_harness_http_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(temp_dir.join("notes.txt"), "alpha\nbeta\n").unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    let token = state.auth_token.read().await.clone();

    // Seed an existing session_checkpoint with next_steps
    let ws = crate::workspace::resolve_workspace(&state, None, None)
        .await
        .unwrap();
    crate::checkpoints::exec_session_checkpoint(
        &state,
        "WebMCP",
        &ws,
        &json!({
            "op": "save",
            "goal": "Finish auth flow",
            "next_steps": ["Verify auth flow"]
        }),
    )
    .await
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/mcp?profile=compact");

    let init_resp = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // 1. First non-checkpoint call surfaces active checkpoint anchor
    let first_call: Value = client
        .post(&url)
        .bearer_auth(&token)
        .header("mcp-session-id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "list_workspaces", "arguments": {} }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first_txt = first_call["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        first_txt.contains("[HARNESS_HINT:") && first_txt.contains("Verify auth flow"),
        "Expected checkpoint anchor hint, got:\n{first_txt}"
    );

    // 2. Reading same file twice consecutively without editing triggers read-loop hint
    let _read1: Value = client
        .post(&url)
        .bearer_auth(&token)
        .header("mcp-session-id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "read_file_range", "arguments": { "path": "notes.txt" } }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let read2: Value = client
        .post(&url)
        .bearer_auth(&token)
        .header("mcp-session-id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "read_file_range", "arguments": { "path": "notes.txt" } }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let read2_txt = read2["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        read2_txt.contains("[HARNESS_HINT:") && read2_txt.contains("notes.txt"),
        "Expected read-loop hint, got:\n{read2_txt}"
    );

    // 3. Two consecutive identical command errors trigger error-loop hint
    let fail_cmd = if cfg!(target_os = "windows") {
        "cmd /C exit 9"
    } else {
        "exit 9"
    };
    for rpc_id in [5, 6] {
        let cmd_res: Value = client
            .post(&url)
            .bearer_auth(&token)
            .header("mcp-session-id", &session_id)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "method": "tools/call",
                "params": { "name": "run_command", "arguments": { "action": "run", "command": fail_cmd } }
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let txt = cmd_res["result"]["content"][0]["text"].as_str().unwrap();
        if rpc_id == 6 {
            assert!(
                txt.contains("[HARNESS_HINT:") && txt.contains("Repeated failure"),
                "Expected error-loop hint on 2nd failure, got:\n{txt}"
            );
        }
    }

    handle.abort();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_webmcp_blueprint_write_and_checkpoint_emits_handoff_prompt() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_webmcp_blueprint_handoff_e2e");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    {
        let mut cfg = state.config.write().await;
        cfg.webmcp_expose_skills = true;
        cfg.agent_provider = "openai_compat".to_string();
        cfg.openai_base_url = "http://127.0.0.1:11434/v1".to_string();
    }
    let token = state.auth_token.read().await.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/mcp?profile=compact");

    // 1. initialize.instructions includes handoff rule and stays within 1200 chars even with delegation+skills
    let init_resp = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let init_val: Value = init_resp.json().await.unwrap();
    let instr = init_val["result"]["instructions"].as_str().unwrap();
    assert!(
        instr.chars().count() <= 1200,
        "Instructions exceeded 1200 chars ({}):\n{instr}",
        instr.chars().count()
    );
    assert!(
        instr.contains("ANTIGRAVITY_HANDOFF_PROMPT"),
        "initialize.instructions missing ANTIGRAVITY_HANDOFF_PROMPT rule:\n{instr}"
    );

    // 2. Writing a blueprint triggers the harness handoff nudge
    let bp_write: Value = client
        .post(&url)
        .bearer_auth(&token)
        .header("mcp-session-id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "write_file_jailed",
                "arguments": {
                    "path": ".gemini/Blueprints/10-test-blueprint.md",
                    "content": "# Blueprint 10\nAuto handoff flow.\n"
                }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let bp_txt = bp_write["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        bp_txt.contains("[HARNESS_HINT:")
            && bp_txt.contains("ANTIGRAVITY_HANDOFF_PROMPT")
            && bp_txt.contains(".gemini/Blueprints/10-test-blueprint.md"),
        "Missing blueprint harness nudge:\n{bp_txt}"
    );

    // 3. Saving session_checkpoint emits a copy-pasteable ANTIGRAVITY_HANDOFF_PROMPT referencing the blueprint
    let cp_save: Value = client
        .post(&url)
        .bearer_auth(&token)
        .header("mcp-session-id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "session_checkpoint",
                "arguments": {
                    "op": "save",
                    "goal": "Implement Blueprint 10 handoff",
                    "next_steps": ["Verify E2E flow"]
                }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cp_txt = cp_save["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        cp_txt.contains("ANTIGRAVITY_HANDOFF_PROMPT:")
            && cp_txt.contains(".gemini/Blueprints/10-test-blueprint.md")
            && cp_txt.contains("airlock://workspace/checkpoint"),
        "Missing handoff prompt from session_checkpoint save:\n{cp_txt}"
    );

    handle.abort();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_oauth_protected_resource_preserves_query_and_cors_expose_headers() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_oauth_prm_query_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // 1. Unauthenticated /mcp?profile=compact&workspace=alpha preserves safe query in resource_metadata
    //    AND exposes WWW-Authenticate via Access-Control-Expose-Headers for Gemini Web origin.
    let unauth = client
        .post(format!("{base}/mcp?profile=compact&workspace=alpha"))
        .header("Origin", "https://gemini.google.com")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
    let expose = unauth
        .headers()
        .get("access-control-expose-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    assert!(
        expose.contains("mcp-session-id") && expose.contains("www-authenticate"),
        "CORS expose headers should expose mcp-session-id and www-authenticate, got: {expose}"
    );
    let www_auth = unauth
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        www_auth.contains("/.well-known/oauth-protected-resource/mcp?profile=compact&workspace=alpha"),
        "Expected query-preserving resource_metadata URL, got: {www_auth}"
    );

    // 2. Protected resource endpoints preserve safe query params and strip sensitive token query param
    let prm_suffix: Value = client
        .get(format!("{base}/.well-known/oauth-protected-resource/mcp?profile=compact&workspace=alpha"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        prm_suffix["resource"].as_str(),
        Some(format!("{base}/mcp?profile=compact&workspace=alpha").as_str())
    );

    let prm_root: Value = client
        .get(format!("{base}/.well-known/oauth-protected-resource?workspace=alpha&token=supersecret"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        prm_root["resource"].as_str(),
        Some(format!("{base}/mcp?workspace=alpha").as_str())
    );

    handle.abort();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_mcp_post_notification_echoes_session_id_and_sse_headers() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_post_202_sse_hdr_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    state.config.write().await.webmcp_sse_keepalive = true;
    let token = state.auth_token.read().await.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/mcp");

    let init_resp = client
        .post(&url)
        .bearer_auth(&token)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    let sid = init_resp
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // 1. 202 Accepted on notification MUST echo mcp-session-id header back
    let notif_resp = client
        .post(&url)
        .bearer_auth(&token)
        .header("mcp-session-id", &sid)
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await
        .unwrap();
    assert_eq!(notif_resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        notif_resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok()),
        Some(sid.as_str()),
        "202 Accepted must preserve mcp-session-id header"
    );

    // 2. Slow SSE keepalive (>3s) MUST include x-accel-buffering: no
    let slow_cmd = if cfg!(target_os = "windows") {
        "ping -n 5 127.0.0.1 >NUL && echo sse_hdr_ok"
    } else {
        "sleep 4 && echo sse_hdr_ok"
    };
    let slow_resp = client
        .post(&url)
        .bearer_auth(&token)
        .header("mcp-session-id", &sid)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "run_command",
                "arguments": { "action": "run", "command": slow_cmd }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(slow_resp.status(), StatusCode::OK);
    assert_eq!(
        slow_resp
            .headers()
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok()),
        Some("no"),
        "SSE response must include X-Accel-Buffering: no"
    );
    let _ = slow_resp.text().await.unwrap();

    handle.abort();
    let _ = fs::remove_dir_all(&temp_dir);
}
