#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod acp_server;
mod agent_manager;
mod artifacts;
mod chat_store;
mod checkpoints;
mod config;
mod handoff;
mod hooks;
mod jobs;
mod local_agent;
mod lsp_bridge;
mod mcp_hub;
mod mcp_resources;
mod mcp_server;
mod mcp_tasks;
mod oauth;
mod plugins;
mod process_util;
mod providers;
mod secrets;
mod security;
mod semantic;
mod skills;
mod snapshots;
mod state;
mod stdio_bridge;
mod tools;
mod tools_extra;
mod tunnel;
mod webmcp_benchmark;
mod webmcp_harness;
mod workspace;

use crate::config::{AppConfig, DownstreamMcpConfig};
use crate::local_agent::{
    fetch_gemini_models as agent_fetch_models, resume_agent_turn_for_session,
    run_agent_turn_with_options, AgentTurnResponse, GeminiModelInfo,
};
use crate::mcp_server::DiagnosticReport;
use crate::providers::AgentAttachment;
use crate::state::{AppState, DashboardSnapshot, OAuthDecision, ToolApprovalDecision};
use crate::workspace::{
    default_alias_for_path, ensure_unique_alias, normalize_clean_path, paths_equal,
    resolve_workspace, valid_guardrail_value, WorkspaceProfile,
};
use serde_json::Value;
use std::collections::HashMap;
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
    let (canon, clean_canon) = normalize_clean_path(&PathBuf::from(path.trim()))?;
    let chosen_id = {
        let mut cfg = state.config.write().await;
        let id = if let Some(existing) = cfg
            .workspaces
            .iter_mut()
            .find(|w| paths_equal(&w.path, &clean_canon))
        {
            existing.enabled = true;
            existing.id.clone()
        } else {
            let new_id = format!("ws-{}", &AppState::generate_token()[..8]);
            let alias = ensure_unique_alias(&default_alias_for_path(&canon), &cfg.workspaces, None);
            cfg.workspaces.push(WorkspaceProfile {
                id: new_id.clone(),
                alias,
                path: clean_canon.clone(),
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: Vec::new(),
                pinned: false,
            });
            new_id
        };
        cfg.default_workspace_id = Some(id.clone());
        cfg.record_recent_workspace(&clean_canon);
        id
    };
    *state.default_workspace_id.write().await = chosen_id;
    state.sync_default_workspace_root().await;
    state.save_config_state().await;
    state.save_auth_state().await;
    state.touch_activity().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn add_workspace(
    state: State<'_, Arc<AppState>>,
    path: String,
    alias: Option<String>,
    guardrail_override: Option<String>,
    set_default: Option<bool>,
) -> Result<DashboardSnapshot, String> {
    let (canon, clean_canon) = normalize_clean_path(&PathBuf::from(path.trim()))?;
    let parsed_guard = match guardrail_override
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "inherit")
    {
        Some(g) if valid_guardrail_value(g) => Some(g.to_string()),
        Some(other) => return Err(format!("Guardrail override tidak valid: {other}")),
        None => None,
    };

    let target_id = {
        let mut cfg = state.config.write().await;
        let existing_idx = cfg
            .workspaces
            .iter()
            .position(|w| paths_equal(&w.path, &clean_canon));

        let id = if let Some(idx) = existing_idx {
            let existing_id = cfg.workspaces[idx].id.clone();
            if let Some(raw_alias) = alias.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let unique = ensure_unique_alias(raw_alias, &cfg.workspaces, Some(&existing_id));
                cfg.workspaces[idx].alias = unique;
            }
            if guardrail_override.is_some() {
                cfg.workspaces[idx].guardrail_override = parsed_guard;
            }
            cfg.workspaces[idx].enabled = true;
            existing_id
        } else {
            let new_id = format!("ws-{}", &AppState::generate_token()[..8]);
            let base_alias = alias
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| default_alias_for_path(&canon));
            let unique_alias = ensure_unique_alias(&base_alias, &cfg.workspaces, None);
            cfg.workspaces.push(WorkspaceProfile {
                id: new_id.clone(),
                alias: unique_alias,
                path: clean_canon.clone(),
                enabled: true,
                guardrail_override: parsed_guard,
                allow_sensitive_override: None,
                extra_sensitive_patterns: Vec::new(),
                pinned: false,
            });
            new_id
        };

        cfg.record_recent_workspace(&clean_canon);
        if set_default.unwrap_or(false) || cfg.workspaces.len() == 1 {
            cfg.default_workspace_id = Some(id.clone());
        }
        id
    };

    let should_set_def = set_default.unwrap_or(false)
        || state.default_workspace_id.read().await.trim().is_empty();
    if should_set_def {
        *state.default_workspace_id.write().await = target_id;
    }

    state.sync_default_workspace_root().await;
    state.save_config_state().await;
    state.save_auth_state().await;
    state.touch_activity().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn update_workspace(
    state: State<'_, Arc<AppState>>,
    id: String,
    alias: Option<String>,
    enabled: Option<bool>,
    guardrail_override: Option<String>,
    allow_sensitive_override: Option<Option<bool>>,
    pinned: Option<bool>,
) -> Result<DashboardSnapshot, String> {
    let ws_id = id.trim().to_string();
    let mut clear_session_cache = false;

    {
        let mut cfg = state.config.write().await;
        let snapshot_list = cfg.workspaces.clone();
        let idx = cfg
            .workspaces
            .iter()
            .position(|w| w.id == ws_id)
            .ok_or_else(|| format!("Workspace '{ws_id}' tidak ditemukan."))?;

        if let Some(en) = enabled {
            if !en {
                let other_enabled = cfg
                    .workspaces
                    .iter()
                    .enumerate()
                    .any(|(i, w)| i != idx && w.enabled);
                if !other_enabled {
                    return Err("Minimal satu workspace harus tetap aktif.".to_string());
                }
            }
            cfg.workspaces[idx].enabled = en;
            clear_session_cache = true;
        }

        if let Some(raw_alias) = alias.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let unique = ensure_unique_alias(raw_alias, &snapshot_list, Some(&ws_id));
            cfg.workspaces[idx].alias = unique;
        }

        if let Some(ref g_raw) = guardrail_override {
            let trimmed = g_raw.trim();
            if trimmed.is_empty() || trimmed == "inherit" {
                cfg.workspaces[idx].guardrail_override = None;
            } else if valid_guardrail_value(trimmed) {
                cfg.workspaces[idx].guardrail_override = Some(trimmed.to_string());
            } else {
                return Err(format!("Guardrail override tidak valid: {trimmed}"));
            }
            clear_session_cache = true;
        }

        if let Some(sens_ov) = allow_sensitive_override {
            cfg.workspaces[idx].allow_sensitive_override = sens_ov;
        }

        if let Some(pin) = pinned {
            cfg.workspaces[idx].pinned = pin;
        }

        // If default workspace was disabled, promote first remaining enabled workspace
        if cfg.default_workspace_id.as_deref() == Some(&ws_id) && !cfg.workspaces[idx].enabled {
            let next_def = cfg
                .workspaces
                .iter()
                .find(|w| w.enabled)
                .map(|w| w.id.clone());
            cfg.default_workspace_id = next_def;
        }
    }

    if clear_session_cache {
        let prefix = format!("{ws_id}::");
        state
            .session_approved_tools
            .write()
            .await
            .retain(|k| !k.starts_with(&prefix));
    }

    let next_def_id = state.config.read().await.default_workspace_id.clone().unwrap_or_default();
    *state.default_workspace_id.write().await = next_def_id;
    state.sync_default_workspace_root().await;
    state.save_config_state().await;
    state.save_auth_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn remove_workspace(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<DashboardSnapshot, String> {
    let ws_id = id.trim().to_string();
    {
        let mut cfg = state.config.write().await;
        if cfg.workspaces.len() <= 1 {
            return Err("Tidak dapat menghapus satu-satunya workspace yang terpasang.".to_string());
        }
        let before = cfg.workspaces.len();
        cfg.workspaces.retain(|w| w.id != ws_id);
        if cfg.workspaces.len() == before {
            return Err(format!("Workspace '{ws_id}' tidak ditemukan."));
        }
        if !cfg.workspaces.iter().any(|w| w.enabled) {
            if let Some(first) = cfg.workspaces.first_mut() {
                first.enabled = true;
            }
        }
        if cfg.default_workspace_id.as_deref() == Some(&ws_id) {
            cfg.default_workspace_id = cfg
                .workspaces
                .iter()
                .find(|w| w.enabled)
                .map(|w| w.id.clone());
        }
    }

    let prefix = format!("{ws_id}::");
    state
        .session_approved_tools
        .write()
        .await
        .retain(|k| !k.starts_with(&prefix));

    let next_def_id = state.config.read().await.default_workspace_id.clone().unwrap_or_default();
    *state.default_workspace_id.write().await = next_def_id;
    state.sync_default_workspace_root().await;
    state.save_config_state().await;
    state.save_auth_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn set_default_workspace(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<DashboardSnapshot, String> {
    let ws_id = id.trim().to_string();
    let (prof_id, prof_path) = {
        let mut cfg = state.config.write().await;
        let (pid, ppath) = {
            let prof = cfg
                .workspaces
                .iter_mut()
                .find(|w| w.id == ws_id || w.alias.eq_ignore_ascii_case(&ws_id))
                .ok_or_else(|| format!("Workspace '{ws_id}' tidak ditemukan."))?;
            prof.enabled = true;
            (prof.id.clone(), prof.path.clone())
        };
        cfg.default_workspace_id = Some(pid.clone());
        cfg.record_recent_workspace(&ppath);
        (pid, ppath)
    };
    let _ = prof_path;
    *state.default_workspace_id.write().await = prof_id;
    state.sync_default_workspace_root().await;
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
        } else if !trimmed.is_empty() {
            secs.gemini_api_key = trimmed.to_string();
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
async fn clear_agent_history(
    state: State<'_, Arc<AppState>>,
    session_id: Option<String>,
) -> Result<(), String> {
    let clean_sid = session_id
        .as_deref()
        .map(artifacts::ArtifactStore::sanitize_session_id);
    state
        .agent_manager
        .clear_history(clean_sid.as_deref())
        .await;
    if clean_sid.is_none() || clean_sid.as_deref() == Some("default") {
        state.gemini_config.write().await.history.clear();
        state.openai_history.write().await.clear();
        state.anthropic_history.write().await.clear();
        *state.pending_checkpoint.write().await = None;
    }
    Ok(())
}

#[tauri::command]
async fn run_local_agent_turn(
    state: State<'_, Arc<AppState>>,
    prompt: String,
    attachments: Option<Vec<AgentAttachment>>,
    session_id: Option<String>,
    execution_mode: Option<String>,
    workspace_alias: Option<String>,
) -> Result<AgentTurnResponse, String> {
    run_agent_turn_with_options(
        state.inner().clone(),
        prompt,
        attachments.unwrap_or_default(),
        session_id,
        execution_mode,
        workspace_alias,
    )
    .await
}

#[tauri::command]
async fn resume_local_agent_turn(
    state: State<'_, Arc<AppState>>,
    session_id: Option<String>,
) -> Result<AgentTurnResponse, String> {
    resume_agent_turn_for_session(state.inner().clone(), session_id).await
}

#[tauri::command]
async fn discard_agent_checkpoint(
    state: State<'_, Arc<AppState>>,
    session_id: Option<String>,
) -> Result<DashboardSnapshot, String> {
    let clean_sid = session_id
        .as_deref()
        .map(artifacts::ArtifactStore::sanitize_session_id)
        .unwrap_or_else(|| "default".to_string());
    state.agent_manager.set_checkpoint(&clean_sid, None).await;
    if clean_sid == "default" {
        *state.pending_checkpoint.write().await = None;
    }
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
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
    let next_tok = AppState::generate_token();
    *state.auth_token.write().await = next_tok.clone();
    {
        let mut secs = state.provider_secrets.write().await;
        secs.auth_token = next_tok;
        state.secrets_store.save(&secs)?;
    }
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
    command_execution_policy: Option<String>,
    command_allowlist: Option<Vec<String>>,
) -> Result<DashboardSnapshot, String> {
    if let Some(val) = close_to_tray {
        state.close_to_tray.store(val, Ordering::SeqCst);
    }
    if let Some(val) = allow_sensitive_reads {
        state.allow_sensitive_reads.store(val, Ordering::SeqCst);
    }
    if let Some(policy) = command_execution_policy {
        let norm = policy.trim().to_string();
        if !crate::security::is_valid_command_execution_policy(&norm) {
            return Err(format!("Command execution policy tidak valid: {norm}"));
        }
        {
            let mut cfg = state.config.write().await;
            cfg.command_execution_policy = norm;
        }
        state.session_approved_tools.write().await.clear();
    }
    if let Some(list) = command_allowlist {
        let cleaned: Vec<String> = list
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let mut cfg = state.config.write().await;
        cfg.command_allowlist = if cleaned.is_empty() {
            crate::security::default_command_allowlist()
        } else {
            cleaned
        };
    }
    state.save_config_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn update_webmcp_settings(
    state: State<'_, Arc<AppState>>,
    webmcp_default_profile: Option<String>,
    webmcp_auto_promote_secs: Option<u64>,
    webmcp_poll_wait_secs: Option<u64>,
    webmcp_rpc_timeout_secs: Option<u64>,
    webmcp_max_output_chars: Option<usize>,
    webmcp_sse_keepalive: Option<bool>,
    webmcp_allow_write_delegation: Option<bool>,
    webmcp_expose_skills: Option<bool>,
    webmcp_session_harness: Option<bool>,
    webmcp_auto_verify_on_edit: Option<bool>,
    webmcp_auto_handoff_dir: Option<bool>,
) -> Result<DashboardSnapshot, String> {
    {
        let mut cfg = state.config.write().await;
        if let Some(profile) = webmcp_default_profile {
            let parsed = crate::config::McpToolProfile::parse(&profile).ok_or_else(|| {
                format!("WebMCP profile tidak valid: '{profile}'. Pilih 'compact' atau 'full'.")
            })?;
            cfg.webmcp_default_profile = parsed.as_str().to_string();
        }
        if let Some(val) = webmcp_auto_promote_secs {
            cfg.webmcp_auto_promote_secs = val.clamp(10, 24);
        }
        if let Some(val) = webmcp_poll_wait_secs {
            cfg.webmcp_poll_wait_secs = val.clamp(5, 26);
        }
        if let Some(val) = webmcp_rpc_timeout_secs {
            let min_rpc = cfg.effective_webmcp_poll_wait_secs().saturating_add(6);
            cfg.webmcp_rpc_timeout_secs = val.clamp(min_rpc.max(20), 55);
        } else {
            let min_rpc = cfg.effective_webmcp_poll_wait_secs().saturating_add(6);
            if cfg.webmcp_rpc_timeout_secs < min_rpc {
                cfg.webmcp_rpc_timeout_secs = min_rpc;
            }
        }
        if let Some(val) = webmcp_max_output_chars {
            cfg.webmcp_max_output_chars = val.clamp(2500, 24000);
        }
        if let Some(val) = webmcp_sse_keepalive {
            cfg.webmcp_sse_keepalive = val;
        }
        if let Some(val) = webmcp_allow_write_delegation {
            cfg.webmcp_allow_write_delegation = val;
        }
        if let Some(val) = webmcp_expose_skills {
            cfg.webmcp_expose_skills = val;
        }
        if let Some(val) = webmcp_session_harness {
            cfg.webmcp_session_harness = val;
        }
        if let Some(val) = webmcp_auto_verify_on_edit {
            cfg.webmcp_auto_verify_on_edit = val;
        }
        if let Some(val) = webmcp_auto_handoff_dir {
            cfg.webmcp_auto_handoff_dir = val;
        }
    }
    state.save_config_state().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn sync_workspace_handoff_now(
    state: State<'_, Arc<AppState>>,
    workspace_alias: Option<String>,
) -> Result<String, String> {
    let ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;
    handoff::sync_from_state(&state, &ws, handoff::HandoffTrigger::ManualSync, true).await
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
    let tok_trim = token.trim();
    if tok_trim == "__CLEAR__" || !tok_trim.is_empty() {
        let mut secs = state.provider_secrets.write().await;
        if tok_trim == "__CLEAR__" {
            secs.cloudflare_tunnel_token.clear();
        } else {
            secs.cloudflare_tunnel_token = tok_trim.to_string();
        }
        state.secrets_store.save(&secs)?;
    }
    {
        let mut cfg = state.config.write().await;
        cfg.cloudflare_tunnel_token.clear();
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
    workspace_alias: Option<String>,
    workspace_root: Option<String>,
) -> Result<String, String> {
    let root = if let Some(explicit_root) = workspace_root
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        normalize_clean_path(std::path::Path::new(explicit_root))
            .map(|(canon, _)| canon)?
    } else if let Some(alias) = workspace_alias
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        resolve_workspace(&state, Some(alias), None).await?.canon_root
    } else {
        state.workspace_root.read().await.clone()
    };
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
async fn cancel_local_agent(
    state: State<'_, Arc<AppState>>,
    run_id: Option<String>,
) -> Result<(), String> {
    state.agent_manager.cancel_run(run_id.as_deref()).await;
    if run_id.is_none() {
        state.cancel_agent();
    }
    let app_handle = state.app_handle.read().await.clone();
    local_agent::emit_runs_updated(&app_handle, &state).await;
    Ok(())
}

#[tauri::command]
async fn cancel_agent_run(
    state: State<'_, Arc<AppState>>,
    run_id: Option<String>,
) -> Result<Vec<agent_manager::AgentRunSummary>, String> {
    state.agent_manager.cancel_run(run_id.as_deref()).await;
    if run_id.is_none() {
        state.cancel_agent();
    }
    let app_handle = state.app_handle.read().await.clone();
    local_agent::emit_runs_updated(&app_handle, &state).await;
    Ok(state.agent_manager.list_runs().await)
}

#[tauri::command]
async fn list_agent_runs(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<agent_manager::AgentRunSummary>, String> {
    Ok(state.agent_manager.list_runs().await)
}

#[tauri::command]
async fn fetch_gemini_models(
    state: State<'_, Arc<AppState>>,
    api_key: String,
) -> Result<Vec<GeminiModelInfo>, String> {
    agent_fetch_models(&state.http_client, api_key).await
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
async fn resolve_mcp_elicitation(
    state: State<'_, Arc<AppState>>,
    request_id: String,
    action: String,
    content: Option<Value>,
) -> Result<DashboardSnapshot, String> {
    let parsed_action = match action.trim() {
        "accept" => crate::state::ElicitationAction::Accept,
        "decline" => crate::state::ElicitationAction::Decline,
        "cancel" => crate::state::ElicitationAction::Cancel,
        other => return Err(format!("Keputusan elicitation tidak valid: {other}")),
    };
    mcp_hub::resolve_mcp_elicitation(&state, &request_id, parsed_action, content).await?;
    if let Some(app) = state.app_handle.read().await.clone() {
        let _ = app.emit("mcp-elicitation-resolved", &request_id);
    }
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn stop_lsp_server(
    state: State<'_, Arc<AppState>>,
    key: Option<String>,
) -> Result<DashboardSnapshot, String> {
    let _ = state.lsp_manager.stop_server(key.as_deref()).await;
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
async fn get_background_job_logs_delta(
    state: State<'_, Arc<AppState>>,
    job_id: String,
    from_line: usize,
) -> Result<jobs::JobLogsDelta, String> {
    jobs::get_job_logs_delta(&state, &job_id, from_line).await
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

    let hydrated_cfg = {
        let mut secs = state.provider_secrets.write().await;
        if tok_trim == "__CLEAR__" {
            secs.downstream_bearer_tokens.remove(&cfg.id);
            cfg.bearer_token.clear();
        } else if tok_trim.is_empty() {
            cfg.bearer_token = secs
                .downstream_bearer_tokens
                .get(&cfg.id)
                .cloned()
                .unwrap_or_default();
        } else {
            secs.downstream_bearer_tokens
                .insert(cfg.id.clone(), tok_trim.clone());
            cfg.bearer_token = tok_trim;
        }

        if cfg.env.is_empty() {
            secs.downstream_envs.remove(&cfg.id);
        } else {
            secs.downstream_envs.insert(cfg.id.clone(), cfg.env.clone());
        }
        state.secrets_store.save(&secs)?;
        cfg.clone()
    };

    let mut sanitized_cfg = hydrated_cfg.clone();
    sanitized_cfg.bearer_token.clear();
    sanitized_cfg.env.clear();

    {
        let mut app_cfg = state.config.write().await;
        if let Some(existing) = app_cfg
            .downstream_mcps
            .iter_mut()
            .find(|c| c.id == sanitized_cfg.id)
        {
            *existing = sanitized_cfg;
        } else {
            app_cfg.downstream_mcps.push(sanitized_cfg);
        }
    }

    state.save_config_state().await;
    let _ = state
        .mcp_hub
        .refresh_server(&state.http_client, &hydrated_cfg)
        .await;
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
        let app_cfg = state.config.read().await;
        let managed_by_plugin = id.contains("::")
            || app_cfg
                .installed_plugins
                .iter()
                .any(|p| p.mcp_server_ids.iter().any(|sid| sid == &id));
        if managed_by_plugin {
            return Err(
                "Server MCP ini dikelola oleh plugin — nonaktifkan atau uninstall lewat panel Installed Plugins."
                    .to_string(),
            );
        }
    }
    {
        let mut app_cfg = state.config.write().await;
        app_cfg.downstream_mcps.retain(|c| c.id != id);
    }
    {
        let mut secs = state.provider_secrets.write().await;
        let removed_tok = secs.downstream_bearer_tokens.remove(&id).is_some();
        let removed_env = secs.downstream_envs.remove(&id).is_some();
        if removed_tok || removed_env {
            let _ = state.secrets_store.save(&secs);
        }
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
    let target_cfg = state
        .resolved_downstream_mcps()
        .await
        .into_iter()
        .find(|c| c.id == id);
    if let Some(cfg) = target_cfg {
        let _ = state
            .mcp_hub
            .refresh_server(&state.http_client, &cfg)
            .await;
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
    gemini_thinking_mode: Option<String>,
    agent_max_steps: Option<usize>,
    agent_turn_timeout_secs: Option<u64>,
    stream_idle_timeout_secs: Option<u64>,
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
        if let Some(ref tm) = gemini_thinking_mode {
            let norm = tm.trim().to_ascii_lowercase();
            if !matches!(norm.as_str(), "auto" | "off" | "low" | "medium" | "high") {
                return Err(format!("Mode thinking Gemini tidak valid: {norm}"));
            }
            app_cfg.gemini_thinking_mode = norm;
        }
        if let Some(steps) = agent_max_steps {
            app_cfg.agent_max_steps = steps.clamp(4, 30);
        }
        if let Some(budget) = agent_turn_timeout_secs {
            app_cfg.agent_turn_timeout_secs = budget.clamp(30, 600);
        }
        if let Some(idle) = stream_idle_timeout_secs {
            app_cfg.stream_idle_timeout_secs = idle.clamp(10, 120);
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
                if key_trim == "__CLEAR__" {
                    secs.gemini_api_key.clear();
                } else {
                    secs.gemini_api_key = key_trim.to_string();
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
            providers::fetch_openai_compat_models(&state.http_client, eff_base, eff_key).await
        }
        _ => {
            let eff_key = if !trimmed_key.is_empty() {
                trimmed_key
            } else {
                state.provider_secrets.read().await.gemini_api_key.clone()
            };
            agent_fetch_models(&state.http_client, eff_key).await
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

#[derive(serde::Serialize)]
struct ArtifactFeedbackResult {
    artifact: artifacts::ArtifactDocument,
    feedback_prompt: String,
}

#[tauri::command]
async fn list_session_artifacts(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    workspace_alias: Option<String>,
) -> Result<Vec<artifacts::ArtifactDocument>, String> {
    let ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;
    Ok(state
        .artifact_store
        .list_session_artifacts(&ws.clean_path, &session_id)
        .await)
}

#[tauri::command]
async fn add_artifact_comment(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    artifact_id: String,
    line_anchor: Option<u32>,
    quote: Option<String>,
    body: String,
    workspace_alias: Option<String>,
) -> Result<artifacts::ArtifactDocument, String> {
    let ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;
    let doc = state
        .artifact_store
        .add_comment(
            &ws.clean_path,
            &session_id,
            &artifact_id,
            line_anchor,
            quote,
            &body,
        )
        .await?;
    if let Some(app) = state.app_handle.read().await.clone() {
        let _ = app.emit("artifact-updated", &doc);
    }
    Ok(doc)
}

#[tauri::command]
async fn resolve_artifact_comment(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    artifact_id: String,
    comment_id: String,
    resolved: bool,
    workspace_alias: Option<String>,
) -> Result<artifacts::ArtifactDocument, String> {
    let ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;
    let doc = state
        .artifact_store
        .resolve_comment(&ws.clean_path, &session_id, &artifact_id, &comment_id, resolved)
        .await?;
    if let Some(app) = state.app_handle.read().await.clone() {
        let _ = app.emit("artifact-updated", &doc);
    }
    Ok(doc)
}

#[tauri::command]
async fn approve_artifact(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    artifact_id: String,
    workspace_alias: Option<String>,
) -> Result<artifacts::ArtifactDocument, String> {
    let ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;
    let doc = state
        .artifact_store
        .approve_artifact(&ws.clean_path, &session_id, &artifact_id)
        .await?;
    if let Some(app) = state.app_handle.read().await.clone() {
        let _ = app.emit("artifact-updated", &doc);
    }
    if (doc.session_id == "webmcp-latest" || doc.session_id.starts_with("delegated_"))
        && handoff::workspace_has_handoff(&ws)
    {
        let _ = handoff::sync_from_state(
            &state,
            &ws,
            handoff::HandoffTrigger::ArtifactUpdated(doc.session_id.clone()),
            false,
        )
        .await;
    }
    Ok(doc)
}

#[tauri::command]
async fn request_artifact_changes(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    artifact_id: String,
    workspace_alias: Option<String>,
) -> Result<ArtifactFeedbackResult, String> {
    let ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;
    let (doc, feedback_prompt) = state
        .artifact_store
        .request_changes_and_format_feedback(&ws.clean_path, &session_id, &artifact_id)
        .await?;
    if let Some(app) = state.app_handle.read().await.clone() {
        let _ = app.emit("artifact-updated", &doc);
    }
    if (doc.session_id == "webmcp-latest" || doc.session_id.starts_with("delegated_"))
        && handoff::workspace_has_handoff(&ws)
    {
        let _ = handoff::sync_from_state(
            &state,
            &ws,
            handoff::HandoffTrigger::ArtifactUpdated(doc.session_id.clone()),
            false,
        )
        .await;
    }
    Ok(ArtifactFeedbackResult {
        artifact: doc,
        feedback_prompt,
    })
}

#[tauri::command]
async fn export_artifact_to_workspace(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    artifact_id: String,
    workspace_alias: Option<String>,
) -> Result<String, String> {
    let ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;
    state
        .artifact_store
        .export_artifact_to_workspace(&ws.clean_path, &ws.canon_root, &session_id, &artifact_id)
        .await
}

#[tauri::command]
async fn list_workflows(
    state: State<'_, Arc<AppState>>,
    workspace_alias: Option<String>,
) -> Result<Vec<hooks::WorkflowSummary>, String> {
    let ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;
    Ok(hooks::list_workflows(&ws.canon_root))
}

#[tauri::command]
async fn get_workspace_hooks_status(
    state: State<'_, Arc<AppState>>,
    workspace_alias: Option<String>,
) -> Result<hooks::HooksStatusSummary, String> {
    let ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;
    Ok(hooks::get_hooks_status(&state, &ws).await)
}

#[tauri::command]
async fn trust_workspace_hooks(
    state: State<'_, Arc<AppState>>,
    workspace_alias: Option<String>,
    expected_sha256: String,
) -> Result<hooks::HooksStatusSummary, String> {
    let ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;
    let status = hooks::trust_workspace_hooks(&state, &ws, &expected_sha256).await?;
    state.emit_dashboard().await;
    Ok(status)
}

#[tauri::command]
async fn list_skills(state: State<'_, Arc<AppState>>) -> Result<Vec<skills::SkillSummary>, String> {
    let entries = state.refresh_skills().await;
    Ok(entries.into_iter().map(|s| s.summary).collect())
}

#[tauri::command]
async fn get_skill_detail(
    state: State<'_, Arc<AppState>>,
    name: String,
) -> Result<skills::SkillDetail, String> {
    let trimmed = name.trim();
    let entries = state.refresh_skills().await;
    let entry = entries
        .into_iter()
        .find(|s| s.summary.name.eq_ignore_ascii_case(trimmed))
        .ok_or_else(|| format!("Skill '{trimmed}' tidak ditemukan."))?;
    Ok(skills::SkillDetail {
        summary: entry.summary,
        body_md: entry.body_md,
        raw_md: entry.raw_md,
    })
}

#[tauri::command]
async fn toggle_skill_enabled(
    state: State<'_, Arc<AppState>>,
    name: String,
    enabled: bool,
) -> Result<DashboardSnapshot, String> {
    let norm = name.trim().to_ascii_lowercase();
    if norm.is_empty() {
        return Err("Nama skill tidak boleh kosong.".to_string());
    }
    {
        let mut cfg = state.config.write().await;
        cfg.disabled_skills
            .retain(|existing| !existing.eq_ignore_ascii_case(&norm));
        if !enabled {
            cfg.disabled_skills.push(norm);
        }
    }
    state.save_config_state().await;
    let _ = state.refresh_skills().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn create_global_skill(
    state: State<'_, Arc<AppState>>,
    name: String,
    description: String,
    body_md: String,
) -> Result<DashboardSnapshot, String> {
    let global_dir = state
        .global_skills_dir()
        .ok_or_else(|| "Direktori global skills tidak tersedia.".to_string())?;
    skills::create_global_skill_file(&global_dir, &name, &description, &body_md)?;
    let _ = state.refresh_skills().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn rescan_skills(state: State<'_, Arc<AppState>>) -> Result<DashboardSnapshot, String> {
    let _ = state.refresh_skills().await;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn inspect_plugin_source(
    state: State<'_, Arc<AppState>>,
    source_type: String,
    location: String,
    subdir: Option<String>,
) -> Result<plugins::PluginInstallPreview, String> {
    plugins::inspect_plugin_source(&state, &source_type, &location, subdir.as_deref()).await
}

#[tauri::command]
async fn cancel_plugin_staging(
    state: State<'_, Arc<AppState>>,
    staging_id: String,
) -> Result<(), String> {
    plugins::cancel_staging(&state, &staging_id).await;
    Ok(())
}

#[tauri::command]
async fn confirm_install_plugin(
    state: State<'_, Arc<AppState>>,
    staging_id: String,
    enable_immediately: bool,
    secrets: HashMap<String, String>,
) -> Result<DashboardSnapshot, String> {
    plugins::confirm_install_plugin(&state, &staging_id, enable_immediately, secrets).await?;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn toggle_plugin(
    state: State<'_, Arc<AppState>>,
    plugin_id: String,
    enabled: bool,
) -> Result<DashboardSnapshot, String> {
    plugins::toggle_plugin(&state, &plugin_id, enabled).await?;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn update_plugin_secrets(
    state: State<'_, Arc<AppState>>,
    plugin_id: String,
    secrets: HashMap<String, String>,
) -> Result<DashboardSnapshot, String> {
    plugins::update_plugin_secrets(&state, &plugin_id, secrets).await?;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

#[tauri::command]
async fn uninstall_plugin(
    state: State<'_, Arc<AppState>>,
    plugin_id: String,
) -> Result<DashboardSnapshot, String> {
    plugins::uninstall_plugin(&state, &plugin_id).await?;
    let snap = state.snapshot().await;
    state.emit_dashboard().await;
    Ok(snap)
}

fn default_workspace_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn main() {
    if std::env::args().any(|a| a == "--stdio") {
        std::process::exit(stdio_bridge::run_stdio_bridge());
    }
    if std::env::args().any(|a| a == "--acp") {
        std::process::exit(acp_server::run_acp_server());
    }
    if std::env::args().any(|a| a == "--benchmark-webmcp") {
        let json_output = std::env::args().any(|a| a == "--json");
        std::process::exit(webmcp_benchmark::run_benchmark_cli(json_output));
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
                plugins::cleanup_stale_staging(&state_for_handle.plugins_dir());
                let _ = state_for_handle.refresh_skills().await;
                let _ = mcp_server::start_server(state_for_handle.clone()).await;
                tunnel::sync_existing_tailscale_funnel(state_for_handle.clone()).await;
                let cfgs = state_for_handle.resolved_downstream_mcps().await;
                if !cfgs.is_empty() {
                    state_for_handle
                        .mcp_hub
                        .refresh_all(&state_for_handle.http_client, &cfgs)
                        .await;
                }
                state_for_handle.emit_dashboard().await;
            });

            // Auto-Kill Idle watcher
            let state_for_idle = state_setup.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    state_for_idle.mcp_tasks.gc_expired_tasks().await;
                    state_for_idle
                        .lsp_manager
                        .reap_idle_servers(Duration::from_secs(300))
                        .await;
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
                                st.lsp_manager.shutdown_all().await;
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
            add_workspace,
            update_workspace,
            remove_workspace,
            set_default_workspace,
            configure_gemini,
            clear_agent_history,
            run_local_agent_turn,
            resume_local_agent_turn,
            discard_agent_checkpoint,
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
            update_webmcp_settings,
            sync_workspace_handoff_now,
            toggle_pin_workspace,
            set_server_port,
            configure_cloudflare_named,
            clear_tunnel_logs,
            clear_http_traffic,
            run_self_diagnostic,
            open_in_editor,
            cancel_local_agent,
            cancel_agent_run,
            list_agent_runs,
            fetch_gemini_models,
            set_guardrail_mode,
            resolve_tool_approval,
            resolve_mcp_elicitation,
            stop_lsp_server,
            revert_audit_group,
            list_background_jobs,
            get_background_job_logs,
            get_background_job_logs_delta,
            stop_background_job,
            clear_finished_jobs,
            upsert_downstream_mcp,
            remove_downstream_mcp,
            refresh_downstream_mcp,
            configure_agent_provider,
            fetch_provider_models,
            list_chat_sessions,
            save_chat_session,
            delete_chat_session,
            list_session_artifacts,
            add_artifact_comment,
            resolve_artifact_comment,
            approve_artifact,
            request_artifact_changes,
            export_artifact_to_workspace,
            list_workflows,
            get_workspace_hooks_status,
            trust_workspace_hooks,
            list_skills,
            get_skill_detail,
            toggle_skill_enabled,
            create_global_skill,
            rescan_skills,
            inspect_plugin_source,
            cancel_plugin_staging,
            confirm_install_plugin,
            toggle_plugin,
            update_plugin_secrets,
            uninstall_plugin
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
