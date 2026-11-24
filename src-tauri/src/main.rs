#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chat_store;
mod config;
mod jobs;
mod local_agent;
mod mcp_hub;
mod mcp_resources;
mod mcp_server;
mod oauth;
mod providers;
mod secrets;
mod security;
mod snapshots;
mod state;
mod stdio_bridge;
mod tools;
mod tools_extra;
mod tunnel;

use crate::config::{AppConfig, DownstreamMcpConfig};
use crate::local_agent::{
    fetch_gemini_models as agent_fetch_models, run_agent_turn, AgentTurnResponse, GeminiModelInfo,
};
use crate::mcp_server::DiagnosticReport;
use crate::providers::AgentAttachment;
use crate::state::{AppState, DashboardSnapshot, OAuthDecision, ToolApprovalDecision};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[tauri::command]
async fn get_dashboard_state(state: State<'_, Arc<AppState>>) -> Result<DashboardSnapshot, String> {
    Ok(state.snapshot().await)
}

#[tauri::command]
async fn set_workspace_root(
    state: State<'_, Arc<AppState>>,
    path: String,
) -> Result<DashboardSnapshot, String> {
    let candidate = PathBuf::from(path.trim());
    if !candidate.exists() || !candidate.is_dir() {
        return Err(format!("Folder tidak ditemukan: {}", candidate.display()));
    }
    let canon = std::fs::canonicalize(&candidate)
        .map_err(|e| format!("Gagal memverifikasi folder: {e}"))?;
    let raw_canon = canon.display().to_string();
    let clean_canon = raw_canon
        .strip_prefix(r"\\?\")
        .unwrap_or(&raw_canon)
        .to_string();
    *state.workspace_root.write().await = canon;
    {
        let mut cfg = state.config.write().await;
        cfg.record_recent_workspace(&clean_canon);
    }
    state.save_config_state().await;
    state.save_auth_state().await;
    state.touch_activity().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn configure_gemini(
    state: State<'_, Arc<AppState>>,
    api_key: String,
    model: String,
) -> Result<DashboardSnapshot, String> {
    let trimmed = api_key.trim();
    {
        let mut secs = state.provider_secrets.write().await;
        let mut cfg = state.gemini_config.write().await;
        if trimmed == "__CLEAR__" {
            secs.gemini_api_key.clear();
            cfg.api_key.clear();
        } else if !trimmed.is_empty() {
            secs.gemini_api_key = trimmed.to_string();
            cfg.api_key = trimmed.to_string();
        }
        if !model.trim().is_empty() {
            cfg.model = model.trim().to_string();
        }
        state.secrets_store.save(&secs)?;
    }
    state.save_config_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn clear_agent_history(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.gemini_config.write().await.history.clear();
    state.openai_history.write().await.clear();
    state.anthropic_history.write().await.clear();
    Ok(())
}

#[tauri::command]
async fn run_local_agent_turn(
    state: State<'_, Arc<AppState>>,
    prompt: String,
    attachments: Option<Vec<AgentAttachment>>,
) -> Result<AgentTurnResponse, String> {
    run_agent_turn(&state, prompt, attachments.unwrap_or_default()).await
}

#[tauri::command]
async fn start_mcp_server(state: State<'_, Arc<AppState>>) -> Result<DashboardSnapshot, String> {
    mcp_server::start_server(state.inner().clone()).await?;
    Ok(state.snapshot().await)
}

#[tauri::command]
async fn stop_mcp_server(state: State<'_, Arc<AppState>>) -> Result<DashboardSnapshot, String> {
    mcp_server::stop_server(state.inner()).await;
    Ok(state.snapshot().await)
}

#[tauri::command]
async fn start_tunnel(
    state: State<'_, Arc<AppState>>,
    provider: Option<String>,
) -> Result<DashboardSnapshot, String> {
    if !state.server_running.load(Ordering::SeqCst) {
        mcp_server::start_server(state.inner().clone()).await?;
    }
    let chosen = match provider {
        Some(p) if !p.trim().is_empty() => p,
        _ => state.tunnel_provider.read().await.clone(),
    };
    tunnel::start_tunnel(state.inner().clone(), chosen).await?;
    state.save_config_state().await;
    Ok(state.snapshot().await)
}

#[tauri::command]
async fn stop_tunnel(state: State<'_, Arc<AppState>>) -> Result<DashboardSnapshot, String> {
    tunnel::stop_tunnel(state.inner()).await;
    Ok(state.snapshot().await)
}

#[tauri::command]
async fn rotate_auth_token(state: State<'_, Arc<AppState>>) -> Result<DashboardSnapshot, String> {
    *state.auth_token.write().await = AppState::generate_token();
    state.save_auth_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn set_idle_timeout(
    state: State<'_, Arc<AppState>>,
    mins: u64,
) -> Result<DashboardSnapshot, String> {
    state.idle_timeout_mins.store(mins, Ordering::SeqCst);
    state.save_config_state().await;
    state.touch_activity().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn run_tool_direct(
    state: State<'_, Arc<AppState>>,
    tool_name: String,
    args: Value,
) -> Result<String, String> {
    let out = if tool_name.contains("__") {
        state
            .mcp_hub
            .call_downstream_tool(&state, "Simulator", &tool_name, args)
            .await
    } else {
        tools::execute_tool(&state, "Simulator", &tool_name, args).await
    };
    if out.is_error {
        Err(out.text)
    } else {
        Ok(out.text)
    }
}

#[tauri::command]
async fn revert_audit_entry(
    state: State<'_, Arc<AppState>>,
    audit_id: String,
    force: Option<bool>,
) -> Result<String, String> {
    tools::revert_audit_entry(&state, &audit_id, force.unwrap_or(false)).await
}

#[tauri::command]
async fn resolve_oauth_consent(
    state: State<'_, Arc<AppState>>,
    request_id: String,
    decision: String,
) -> Result<DashboardSnapshot, String> {
    let parsed = match decision.trim() {
        "approve_once" => OAuthDecision::ApproveOnce,
        "approve_and_trust" => OAuthDecision::ApproveAndTrust,
        "deny" => OAuthDecision::Deny,
        other => return Err(format!("Keputusan OAuth tidak valid: {other}")),
    };
    oauth::resolve_oauth_consent(&state, &request_id, parsed).await?;
    Ok(state.snapshot().await)
}

#[tauri::command]
async fn revoke_oauth_client(
    state: State<'_, Arc<AppState>>,
    client_id: String,
) -> Result<DashboardSnapshot, String> {
    oauth::revoke_oauth_client(&state, &client_id).await?;
    Ok(state.snapshot().await)
}

#[tauri::command]
async fn revoke_all_oauth(state: State<'_, Arc<AppState>>) -> Result<DashboardSnapshot, String> {
    oauth::revoke_all_oauth(&state).await;
    Ok(state.snapshot().await)
}

#[tauri::command]
async fn update_app_settings(
    state: State<'_, Arc<AppState>>,
    close_to_tray: Option<bool>,
    allow_sensitive_reads: Option<bool>,
) -> Result<DashboardSnapshot, String> {
    if let Some(val) = close_to_tray {
        state.close_to_tray.store(val, Ordering::SeqCst);
    }
    if let Some(val) = allow_sensitive_reads {
        state.allow_sensitive_reads.store(val, Ordering::SeqCst);
    }
    state.save_config_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn toggle_pin_workspace(
    state: State<'_, Arc<AppState>>,
    path: String,
) -> Result<DashboardSnapshot, String> {
    let trimmed = path.trim();
    let candidate = PathBuf::from(trimmed);
    let clean_path = if candidate.exists() {
        let raw = std::fs::canonicalize(&candidate)
            .unwrap_or(candidate)
            .display()
            .to_string();
        raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string()
    } else {
        trimmed.to_string()
    };

    {
        let mut cfg = state.config.write().await;
        cfg.toggle_pin_workspace(&clean_path);
    }
    state.save_config_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn set_server_port(
    state: State<'_, Arc<AppState>>,
    port: u16,
) -> Result<DashboardSnapshot, String> {
    if port < 1024 {
        return Err("Port harus antara 1024 dan 65535.".to_string());
    }
    let old_port = state.server_port.load(Ordering::SeqCst);
    if port == old_port {
        return Ok(state.snapshot().await);
    }

    let probe_addr = format!("127.0.0.1:{port}");
    let probe = tokio::net::TcpListener::bind(&probe_addr)
        .await
        .map_err(|e| format!("Port {port} tidak dapat digunakan: {e}"))?;
    drop(probe);

    if state.tunnel_running.load(Ordering::SeqCst) {
        tunnel::stop_tunnel(state.inner()).await;
    }

    let was_running = state.server_running.load(Ordering::SeqCst);
    if was_running {
        mcp_server::stop_server(state.inner()).await;
        state.server_port.store(port, Ordering::SeqCst);
        if let Err(err) = mcp_server::start_server(state.inner().clone()).await {
            state.server_port.store(old_port, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(60)).await;
            let _ = mcp_server::start_server(state.inner().clone()).await;
            return Err(err);
        }
    } else {
        state.server_port.store(port, Ordering::SeqCst);
    }

    state.save_config_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn configure_cloudflare_named(
    state: State<'_, Arc<AppState>>,
    token: String,
    public_url: String,
) -> Result<DashboardSnapshot, String> {
    {
        let mut cfg = state.config.write().await;
        let tok_trim = token.trim();
        if tok_trim == "__CLEAR__" {
            cfg.cloudflare_tunnel_token.clear();
        } else if !tok_trim.is_empty() {
            cfg.cloudflare_tunnel_token = tok_trim.to_string();
        }
        cfg.cloudflare_public_url = public_url.trim().trim_end_matches('/').to_string();
    }
    state.save_config_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn clear_tunnel_logs(state: State<'_, Arc<AppState>>) -> Result<DashboardSnapshot, String> {
    state.tunnel_logs.write().await.clear();
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn clear_http_traffic(state: State<'_, Arc<AppState>>) -> Result<DashboardSnapshot, String> {
    state.http_traffic.write().await.clear();
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn run_self_diagnostic(state: State<'_, Arc<AppState>>) -> Result<DiagnosticReport, String> {
    Ok(mcp_server::run_self_diagnostic(&state).await)
}

#[tauri::command]
async fn open_in_editor(
    state: State<'_, Arc<AppState>>,
    rel_path: Option<String>,
    mode: String,
) -> Result<String, String> {
    let root = state.workspace_root.read().await.clone();
    let target = security::resolve_jailed_path(&root, rel_path.as_deref().unwrap_or("."))?;
    if !target.exists() {
        return Err(format!("Target path tidak ditemukan: {}", target.display()));
    }

    let raw = target.display().to_string();
    let clean_path = raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string();

    match mode.trim() {
        "vscode" => {
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                std::process::Command::new("cmd.exe")
                    .args(["/C", "code", "-g", &clean_path])
                    .creation_flags(0x08000000)
                    .spawn()
                    .map_err(|e| format!("Gagal membuka VS Code: {e}"))?;
            }
            #[cfg(not(target_os = "windows"))]
            {
                std::process::Command::new("code")
                    .args(["-g", &clean_path])
                    .spawn()
                    .map_err(|e| format!("Gagal membuka VS Code: {e}"))?;
            }
            Ok(format!("Membuka {clean_path} di VS Code."))
        }
        "explorer" => {
            #[cfg(target_os = "windows")]
            {
                if target.is_file() {
                    std::process::Command::new("explorer.exe")
                        .arg(format!("/select,{clean_path}"))
                        .spawn()
                        .map_err(|e| format!("Gagal membuka Explorer: {e}"))?;
                } else {
                    std::process::Command::new("explorer.exe")
                        .arg(&clean_path)
                        .spawn()
                        .map_err(|e| format!("Gagal membuka Explorer: {e}"))?;
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                std::process::Command::new("xdg-open")
                    .arg(&clean_path)
                    .spawn()
                    .map_err(|e| format!("Gagal membuka file manager: {e}"))?;
            }
            Ok(format!("Membuka {clean_path} di Explorer."))
        }
        other => Err(format!("Mode editor tidak dikenal: {other}")),
    }
}

#[tauri::command]
async fn cancel_local_agent(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.agent_cancel.notify_waiters();
    Ok(())
}

#[tauri::command]
async fn fetch_gemini_models(api_key: String) -> Result<Vec<GeminiModelInfo>, String> {
    agent_fetch_models(api_key).await
}

#[tauri::command]
async fn set_guardrail_mode(
    state: State<'_, Arc<AppState>>,
    mode: String,
) -> Result<DashboardSnapshot, String> {
    let norm = mode.trim().to_string();
    if !matches!(norm.as_str(), "read_only" | "safe_edit" | "autonomous") {
        return Err(format!("Guardrail mode tidak valid: {norm}"));
    }
    *state.guardrail_mode.write().await = norm;
    state.session_approved_tools.write().await.clear();
    state.save_config_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn resolve_tool_approval(
    state: State<'_, Arc<AppState>>,
    request_id: String,
    decision: String,
) -> Result<DashboardSnapshot, String> {
    let parsed = match decision.trim() {
        "approve_once" => ToolApprovalDecision::ApproveOnce,
        "approve_for_session" => ToolApprovalDecision::ApproveForSession,
        "deny" => ToolApprovalDecision::Deny,
        other => return Err(format!("Keputusan tool approval tidak valid: {other}")),
    };
    tools::resolve_tool_approval(&state, &request_id, parsed).await?;
    if let Some(app) = state.app_handle.read().await.clone() {
        let _ = app.emit("tool-approval-resolved", &request_id);
    }
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn revert_audit_group(
    state: State<'_, Arc<AppState>>,
    group_id: String,
    force: Option<bool>,
) -> Result<String, String> {
    tools::revert_audit_group(&state, &group_id, force.unwrap_or(false)).await
}

#[tauri::command]
async fn list_background_jobs(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<jobs::JobSummary>, String> {
    Ok(jobs::list_jobs(&state).await)
}

#[tauri::command]
async fn get_background_job_logs(
    state: State<'_, Arc<AppState>>,
    job_id: String,
) -> Result<String, String> {
    jobs::get_job_logs(&state, &job_id).await
}

#[tauri::command]
async fn stop_background_job(
    state: State<'_, Arc<AppState>>,
    job_id: String,
) -> Result<jobs::JobSummary, String> {
    jobs::stop_job(&state, &job_id).await
}

#[tauri::command]
async fn clear_finished_jobs(
    state: State<'_, Arc<AppState>>,
) -> Result<DashboardSnapshot, String> {
    jobs::clear_finished_jobs(&state).await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn upsert_downstream_mcp(
    state: State<'_, Arc<AppState>>,
    mut cfg: DownstreamMcpConfig,
) -> Result<DashboardSnapshot, String> {
    cfg.name = cfg.name.trim().to_string();
    if cfg.name.is_empty() {
        return Err("Nama downstream MCP server tidak boleh kosong.".to_string());
    }
    let ns_clean = mcp_hub::sanitize_namespace(&cfg.namespace);
    if ns_clean.is_empty() {
        return Err("Namespace downstream MCP tidak valid.".to_string());
    }
    cfg.namespace = ns_clean;

    if cfg.id.trim().is_empty() {
        cfg.id = format!("mcp-{}", &AppState::generate_token()[..8]);
    }
    let tok_trim = cfg.bearer_token.trim().to_string();

    {
        let mut app_cfg = state.config.write().await;
        if let Some(existing) = app_cfg.downstream_mcps.iter_mut().find(|c| c.id == cfg.id) {
            if tok_trim == "__CLEAR__" {
                cfg.bearer_token.clear();
            } else if tok_trim.is_empty() && !existing.bearer_token.is_empty() {
                cfg.bearer_token = existing.bearer_token.clone();
            } else {
                cfg.bearer_token = tok_trim;
            }
            *existing = cfg.clone();
        } else {
            cfg.bearer_token = if tok_trim == "__CLEAR__" {
                String::new()
            } else {
                tok_trim
            };
            app_cfg.downstream_mcps.push(cfg.clone());
        }
    }

    state.save_config_state().await;
    let _ = state.mcp_hub.refresh_server(&cfg).await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn remove_downstream_mcp(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<DashboardSnapshot, String> {
    {
        let mut app_cfg = state.config.write().await;
        app_cfg.downstream_mcps.retain(|c| c.id != id);
    }
    state.mcp_hub.remove_server(&id).await;
    state.save_config_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn refresh_downstream_mcp(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<DashboardSnapshot, String> {
    let target_cfg = {
        let app_cfg = state.config.read().await;
        app_cfg
            .downstream_mcps
            .iter()
            .find(|c| c.id == id)
            .cloned()
    };
    if let Some(cfg) = target_cfg {
        let _ = state.mcp_hub.refresh_server(&cfg).await;
    } else {
        return Err(format!("Downstream MCP server '{id}' tidak ditemukan."));
    }
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn configure_agent_provider(
    state: State<'_, Arc<AppState>>,
    provider: String,
    api_key: String,
    base_url: Option<String>,
    model: Option<String>,
    auto_inject_project_rules: Option<bool>,
) -> Result<DashboardSnapshot, String> {
    let prov = provider.trim().to_string();
    if !matches!(prov.as_str(), "gemini" | "openai_compat" | "anthropic") {
        return Err(format!("Provider agent tidak dikenal: {prov}"));
    }

    let key_trim = api_key.trim();
    {
        let mut app_cfg = state.config.write().await;
        app_cfg.agent_provider = prov.clone();
        if let Some(rules_flag) = auto_inject_project_rules {
            app_cfg.auto_inject_project_rules = rules_flag;
        }
        if let Some(ref bu) = base_url {
            let trimmed_bu = bu.trim().trim_end_matches('/');
            if !trimmed_bu.is_empty() {
                app_cfg.openai_base_url = trimmed_bu.to_string();
            }
        }
        if let Some(ref m) = model {
            let m_trim = m.trim();
            if !m_trim.is_empty() {
                match prov.as_str() {
                    "openai_compat" => app_cfg.openai_model = m_trim.to_string(),
                    "anthropic" => app_cfg.anthropic_model = m_trim.to_string(),
                    _ => app_cfg.gemini_model = m_trim.to_string(),
                }
            }
        }
    }

    if let Some(ref m) = model {
        let m_trim = m.trim();
        if prov == "gemini" && !m_trim.is_empty() {
            state.gemini_config.write().await.model = m_trim.to_string();
        }
    }

    if !key_trim.is_empty() {
        let mut secs = state.provider_secrets.write().await;
        match prov.as_str() {
            "openai_compat" => {
                if key_trim == "__CLEAR__" {
                    secs.openai_api_key.clear();
                } else {
                    secs.openai_api_key = key_trim.to_string();
                }
            }
            "anthropic" => {
                if key_trim == "__CLEAR__" {
                    secs.anthropic_api_key.clear();
                } else {
                    secs.anthropic_api_key = key_trim.to_string();
                }
            }
            _ => {
                let mut gcfg = state.gemini_config.write().await;
                if key_trim == "__CLEAR__" {
                    secs.gemini_api_key.clear();
                    gcfg.api_key.clear();
                } else {
                    secs.gemini_api_key = key_trim.to_string();
                    gcfg.api_key = key_trim.to_string();
                }
            }
        }
        state.secrets_store.save(&secs)?;
    }

    state.save_config_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn fetch_provider_models(
    state: State<'_, Arc<AppState>>,
    provider: String,
    api_key: String,
    base_url: Option<String>,
) -> Result<Vec<GeminiModelInfo>, String> {
    let trimmed_key = api_key.trim().to_string();
    match provider.trim() {
        "anthropic" => Ok(providers::anthropic_known_models()),
        "openai_compat" => {
            let eff_base = base_url
                .map(|b| b.trim().to_string())
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| {
                    state
                        .config
                        .try_read()
                        .map(|c| c.openai_base_url.clone())
                        .unwrap_or_default()
                });
            let eff_key = if !trimmed_key.is_empty() {
                trimmed_key
            } else {
                state.provider_secrets.read().await.openai_api_key.clone()
            };
            providers::fetch_openai_compat_models(eff_base, eff_key).await
        }
        _ => {
            let eff_key = if !trimmed_key.is_empty() {
                trimmed_key
            } else {
                let sec = state.provider_secrets.read().await.gemini_api_key.clone();
                if !sec.is_empty() {
                    sec
                } else {
                    state.gemini_config.read().await.api_key.clone()
                }
            };
            agent_fetch_models(eff_key).await
        }
    }
}

async fn clean_workspace_string(state: &AppState) -> String {
    let raw = state.workspace_root.read().await.display().to_string();
    raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string()
}

#[tauri::command]
async fn list_chat_sessions(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<chat_store::ChatSession>, String> {
    let ws = clean_workspace_string(&state).await;
    Ok(state.chat_store.list_sessions(&ws))
}

#[tauri::command]
async fn save_chat_session(
    state: State<'_, Arc<AppState>>,
    session: chat_store::ChatSession,
) -> Result<Vec<chat_store::ChatSession>, String> {
    let ws = clean_workspace_string(&state).await;
    state.chat_store.save_session(&ws, session)
}

#[tauri::command]
async fn delete_chat_session(
    state: State<'_, Arc<AppState>>,
    session_id: String,
) -> Result<Vec<chat_store::ChatSession>, String> {
    let ws = clean_workspace_string(&state).await;
    state.chat_store.delete_session(&ws, &session_id)
}

fn default_workspace_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn main() {
    if std::env::args().any(|a| a == "--stdio") {
        std::process::exit(stdio_bridge::run_stdio_bridge());
    }

    let app_state = Arc::new(AppState::new_persistent(
        default_workspace_dir(),
        AppState::default_auth_file_path(),
        AppConfig::default_path(),
    ));
    let state_setup = app_state.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(app_state)
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<Arc<AppState>>();
                if state.close_to_tray.load(Ordering::SeqCst) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            let state_for_handle = state_setup.clone();
            tauri::async_runtime::spawn(async move {
                *state_for_handle.app_handle.write().await = Some(handle);
                let _ = mcp_server::start_server(state_for_handle.clone()).await;
                tunnel::sync_existing_tailscale_funnel(state_for_handle.clone()).await;
                let cfgs = state_for_handle.config.read().await.downstream_mcps.clone();
                if !cfgs.is_empty() {
                    state_for_handle.mcp_hub.refresh_all(&cfgs).await;
                }
                state_for_handle.emit_dashboard().await;
            });

            // Auto-Kill Idle watcher
            let state_for_idle = state_setup.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    let timeout_mins = state_for_idle.idle_timeout_mins.load(Ordering::SeqCst);
                    if timeout_mins == 0 {
                        continue;
                    }
                    let active_network = state_for_idle.server_running.load(Ordering::SeqCst)
                        || state_for_idle.tunnel_running.load(Ordering::SeqCst);
                    if !active_network {
                        continue;
                    }
                    let elapsed = state_for_idle.last_activity.read().await.elapsed();
                    if elapsed >= Duration::from_secs(timeout_mins * 60) {
                        tunnel::stop_tunnel(&state_for_idle).await;
                        mcp_server::stop_server(&state_for_idle).await;
                    }
                }
            });

            // System Tray setup
            let toggle_item =
                MenuItem::with_id(app, "toggle_server", "Toggle MCP Server", true, None::<&str>)?;
            let copy_item =
                MenuItem::with_id(app, "copy_mcp", "Copy MCP URL + Token", true, None::<&str>)?;
            let show_item =
                MenuItem::with_id(app, "open_dashboard", "Open Dashboard", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let tray_menu =
                Menu::with_items(app, &[&toggle_item, &copy_item, &show_item, &quit_item])?;

            let state_for_tray = state_setup.clone();
            let mut tray_builder = TrayIconBuilder::new()
                .menu(&tray_menu)
                .tooltip("Airlock — MCP Bridge & Local Agent");

            if let Some(icon) = app.default_window_icon().cloned() {
                tray_builder = tray_builder.icon(icon);
            }

            tray_builder
                .on_menu_event(move |app, event| {
                    let st = state_for_tray.clone();
                    match event.id.as_ref() {
                        "open_dashboard" => {
                            if let Some(win) = app.get_webview_window("main") {
                                let _ = win.unminimize();
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                        "toggle_server" => {
                            tauri::async_runtime::spawn(async move {
                                if st.server_running.load(Ordering::SeqCst) {
                                    mcp_server::stop_server(&st).await;
                                } else {
                                    let _ = mcp_server::start_server(st).await;
                                }
                            });
                        }
                        "copy_mcp" => {
                            let app_clone = app.clone();
                            tauri::async_runtime::spawn(async move {
                                let snap = st.snapshot().await;
                                let base = snap
                                    .tunnel_url
                                    .unwrap_or_else(|| format!("http://127.0.0.1:{}", snap.server_port));
                                let full_url = format!("{base}/mcp/{}", snap.auth_token);
                                let _ = app_clone.clipboard().write_text(full_url);
                            });
                        }
                        "quit" => {
                            let app_clone = app.clone();
                            tauri::async_runtime::spawn(async move {
                                jobs::stop_all_jobs(&st).await;
                                st.mcp_hub.shutdown_all().await;
                                tunnel::stop_tunnel(&st).await;
                                mcp_server::stop_server(&st).await;
                                app_clone.exit(0);
                            });
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.unminimize();
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_dashboard_state,
            set_workspace_root,
            configure_gemini,
            clear_agent_history,
            run_local_agent_turn,
            start_mcp_server,
            stop_mcp_server,
            start_tunnel,
            stop_tunnel,
            rotate_auth_token,
            set_idle_timeout,
            run_tool_direct,
            revert_audit_entry,
            resolve_oauth_consent,
            revoke_oauth_client,
            revoke_all_oauth,
            update_app_settings,
            toggle_pin_workspace,
            set_server_port,
            configure_cloudflare_named,
            clear_tunnel_logs,
            clear_http_traffic,
            run_self_diagnostic,
            open_in_editor,
            cancel_local_agent,
            fetch_gemini_models,
            set_guardrail_mode,
            resolve_tool_approval,
            revert_audit_group,
            list_background_jobs,
            get_background_job_logs,
            stop_background_job,
            clear_finished_jobs,
            upsert_downstream_mcp,
            remove_downstream_mcp,
            refresh_downstream_mcp,
            configure_agent_provider,
            fetch_provider_models,
            list_chat_sessions,
            save_chat_session,
            delete_chat_session
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
