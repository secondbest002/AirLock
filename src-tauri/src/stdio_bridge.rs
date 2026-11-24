use crate::config::AppConfig;
use crate::jobs;
use crate::mcp_server::handle_single_rpc;
use crate::secrets::SecretsStore;
use crate::state::AppState;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn read_persisted_auth_token(auth_path: &PathBuf) -> Option<String> {
    if let Some(dir) = auth_path.parent() {
        let vault = SecretsStore::new_persistent(dir.to_path_buf()).load();
        let tok = vault.auth_token.trim().to_string();
        if !tok.is_empty() {
            return Some(tok);
        }
    }
    let raw = std::fs::read_to_string(auth_path).ok()?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    val.get("auth_token")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn probe_running_hub(port: u16, token: &str) -> Option<reqwest::Client> {
    if token.is_empty() {
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .ok()?;
    let health_url = format!("http://127.0.0.1:{port}/health");
    let resp = tokio::time::timeout(Duration::from_millis(500), client.get(&health_url).send())
        .await
        .ok()?
        .ok()?;
    if resp.status().is_success() {
        Some(client)
    } else {
        None
    }
}

async fn proxy_line_to_hub(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    session_id: &mut Option<String>,
    msg: &Value,
) -> Result<Option<Value>, String> {
    let mut req = client
        .post(endpoint)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(msg);
    if let Some(sid) = session_id.as_deref() {
        req = req.header("mcp-session-id", sid);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("Proxy ke Hub gagal: {e}"))?;

    if let Some(sid) = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        *session_id = Some(sid.to_string());
    }

    let status = resp.status();
    if status.as_u16() == 202 {
        return Ok(None);
    }
    if !status.is_success() {
        let err_txt = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {err_txt}"));
    }
    let parsed: Value = resp
        .json()
        .await
        .map_err(|e| format!("Respons JSON dari Hub tidak valid: {e}"))?;
    Ok(Some(parsed))
}

pub async fn process_stdio_json_line_embedded(state: &Arc<AppState>, line: &str) -> Option<Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(msg) => handle_single_rpc(state, &msg).await,
        Err(e) => Some(json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {
                "code": -32700,
                "message": format!("Parse error: {e}")
            }
        })),
    }
}

pub fn run_stdio_bridge() -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[smart-tauri-hub --stdio] Gagal menginisialisasi Tokio runtime: {e}");
            return 1;
        }
    };

    rt.block_on(async move {
        let cfg_path = AppConfig::default_path();
        let auth_path = AppState::default_auth_file_path();
        let app_cfg = AppConfig::load(&cfg_path);
        let port = app_cfg.server_port;
        let saved_token = read_persisted_auth_token(&auth_path).unwrap_or_default();

        let proxy_client = probe_running_hub(port, &saved_token).await;
        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let mut session_id: Option<String> = None;

        let mut embedded_state: Option<Arc<AppState>> = None;
        if proxy_client.is_none() {
            let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let st = Arc::new(AppState::new_persistent(ws, auth_path, cfg_path));
            let downstream = st.resolved_downstream_mcps().await;
            if !downstream.is_empty() {
                st.mcp_hub.refresh_all(&st.http_client, &downstream).await;
            }
            embedded_state = Some(st);
        }

        let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let locked = stdin.lock();
            for line_res in locked.lines() {
                match line_res {
                    Ok(l) => {
                        if line_tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("[smart-tauri-hub --stdio] Stdin read error: {e}");
                        break;
                    }
                }
            }
        });

        while let Some(line) = line_rx.recv().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let resp_opt = if let Some(client) = &proxy_client {
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(msg) => {
                        match proxy_line_to_hub(
                            client,
                            &endpoint,
                            &saved_token,
                            &mut session_id,
                            &msg,
                        )
                        .await
                        {
                            Ok(r) => r,
                            Err(err) => {
                                eprintln!("[smart-tauri-hub --stdio] {err}");
                                if embedded_state.is_none() {
                                    let ws = std::env::current_dir()
                                        .unwrap_or_else(|_| PathBuf::from("."));
                                    let st = Arc::new(AppState::new_persistent(
                                        ws,
                                        AppState::default_auth_file_path(),
                                        AppConfig::default_path(),
                                    ));
                                    let downstream = st.resolved_downstream_mcps().await;
                                    if !downstream.is_empty() {
                                        st.mcp_hub.refresh_all(&st.http_client, &downstream).await;
                                    }
                                    embedded_state = Some(st);
                                }
                                if let Some(st) = &embedded_state {
                                    handle_single_rpc(st, &msg).await
                                } else {
                                    None
                                }
                            }
                        }
                    }
                    Err(e) => Some(json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": format!("Parse error: {e}") }
                    })),
                }
            } else if let Some(st) = &embedded_state {
                process_stdio_json_line_embedded(st, trimmed).await
            } else {
                None
            };

            if let Some(resp_val) = resp_opt {
                if let Ok(json_line) = serde_json::to_string(&resp_val) {
                    let stdout = std::io::stdout();
                    let mut out_lock = stdout.lock();
                    let _ = writeln!(out_lock, "{json_line}");
                    let _ = out_lock.flush();
                }
            }
        }

        if let Some(st) = embedded_state {
            jobs::stop_all_jobs(&st).await;
            st.mcp_hub.shutdown_all().await;
            st.lsp_manager.shutdown_all().await;
        }
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_stdio_bridge_embedded_ping_and_parse_error() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_stdio_bridge_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let state = Arc::new(AppState::new(temp_dir.clone()));

        let pong = process_stdio_json_line_embedded(
            &state,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        )
        .await
        .unwrap();
        assert_eq!(pong["id"], 1);
        assert_eq!(pong["result"], json!({}));

        let notif = process_stdio_json_line_embedded(
            &state,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;
        assert!(notif.is_none());

        let parse_err = process_stdio_json_line_embedded(&state, "{not-valid-json")
            .await
            .unwrap();
        assert_eq!(parse_err["error"]["code"], -32700);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
