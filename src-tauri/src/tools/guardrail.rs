use super::schemas::requires_hitl_in_safe_edit;
use crate::security::{is_airlock_internal_path, is_sensitive_path};
use crate::state::{AppState, PendingToolApproval, ToolApprovalDecision};
use crate::workspace::ResolvedWorkspace;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{Emitter, Manager};
use tokio::sync::oneshot;

pub async fn resolve_tool_approval(
    state: &AppState,
    request_id: &str,
    decision: ToolApprovalDecision,
) -> Result<(), String> {
    let entry = {
        let mut pending = state.pending_tool_approvals.write().await;
        pending.remove(request_id)
    };
    let Some((_, tx)) = entry else {
        return Err("Permintaan konfirmasi tool sudah kadaluarsa atau tidak ditemukan.".to_string());
    };
    let _ = tx.send(decision);
    Ok(())
}

pub async fn enforce_guardrail_gate(
    state: &AppState,
    source: &str,
    tool_name: &str,
    args: &Value,
    is_read_only: bool,
) -> Result<(), String> {
    enforce_guardrail_gate_for_workspace(state, source, tool_name, args, is_read_only, None).await
}

pub(crate) async fn enforce_run_mode_gate(
    state: &AppState,
    run_ctx: Option<&crate::agent_manager::RunContext>,
    tool_name: &str,
    is_read_only: bool,
) -> Result<(), String> {
    let Some(ctx) = run_ctx else {
        return Ok(());
    };
    if !is_read_only && ctx.execution_mode == "read_only_subagent" {
        return Err(format!(
            "Diblokir oleh Subagent Read-Only Sandbox: tool mutatif '{tool_name}' tidak diizinkan."
        ));
    }
    if !is_read_only
        && ctx.execution_mode == "plan_first"
        && !matches!(tool_name, "upsert_artifact" | "request_plan_approval")
        && !state
            .artifact_store
            .is_plan_approved(&ctx.workspace_clean_path, &ctx.session_id)
            .await
    {
        return Err(
            "Mode Plan-First aktif: buat Implementation Plan via upsert_artifact lalu panggil request_plan_approval. Tunggu persetujuan user sebelum mengedit kode."
                .to_string(),
        );
    }
    Ok(())
}

pub async fn enforce_guardrail_gate_for_workspace(
    state: &AppState,
    source: &str,
    tool_name: &str,
    args: &Value,
    is_read_only: bool,
    ws: Option<&ResolvedWorkspace>,
) -> Result<(), String> {
    let mode = match ws {
        Some(w) => w.effective_guardrail.clone(),
        None => state.guardrail_mode.read().await.clone(),
    };
    let ws_label = ws.map(|w| format!(" [workspace: @{}]", w.alias)).unwrap_or_default();

    if mode == "read_only" && !is_read_only {
        return Err(format!(
            "Diblokir oleh Guardrail Mode (Read-Only){ws_label}: tool '{tool_name}' bersifat mutatif."
        ));
    }

    let is_shell_tool = matches!(
        tool_name,
        "run_command_compressed" | "patch_and_verify" | "start_command_job"
    );
    let cmd_policy = state.config.read().await.command_execution_policy.clone();
    let hitl_from_safe_edit =
        mode == "safe_edit" && requires_hitl_in_safe_edit(tool_name, args, is_read_only);
    let hitl_from_cmd_policy = cmd_policy == "hitl" && is_shell_tool;

    if source != "Simulator" && (hitl_from_safe_edit || hitl_from_cmd_policy) {
        let session_key = ws
            .map(|w| format!("{}::{tool_name}", w.id))
            .unwrap_or_else(|| tool_name.to_string());

        if state
            .session_approved_tools
            .read()
            .await
            .contains(&session_key)
        {
            return Ok(());
        }

        let request_id = format!("hitl_{}", AppState::generate_token());
        let args_pretty = serde_json::to_string_pretty(args).unwrap_or_else(|_| "{}".to_string());
        let args_preview: String = args_pretty.chars().take(1200).collect();
        let reason_label = if hitl_from_cmd_policy && mode != "safe_edit" {
            "kebijakan Command Policy (Always HITL)"
        } else {
            "mode Safe-Edit"
        };
        let summary =
            format!("{source} ingin menjalankan '{tool_name}'{ws_label} dalam {reason_label}.");

        let info = PendingToolApproval {
            request_id: request_id.clone(),
            source: source.to_string(),
            tool_name: tool_name.to_string(),
            summary,
            args_preview,
            created_at: AppState::now_timestamp(),
        };

        let (tx, rx) = oneshot::channel::<ToolApprovalDecision>();
        {
            let mut pending = state.pending_tool_approvals.write().await;
            pending.insert(request_id.clone(), (info.clone(), tx));
        }

        if let Some(acp_res) = state.acp_bridge.request_permission(&info).await {
            let mut pending = state.pending_tool_approvals.write().await;
            pending.remove(&request_id);
            return match acp_res {
                Ok(ToolApprovalDecision::ApproveOnce) => Ok(()),
                Ok(ToolApprovalDecision::ApproveForSession) => {
                    state
                        .session_approved_tools
                        .write()
                        .await
                        .insert(session_key);
                    Ok(())
                }
                Ok(ToolApprovalDecision::Deny) => {
                    Err(format!("Eksekusi tool '{tool_name}' ditolak oleh user (ACP HITL)."))
                }
                Err(msg) => Err(msg),
            };
        }

        if let Some(app) = state.app_handle.read().await.clone() {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
            }
            let _ = app.emit("tool-approval-request", &info);
        }

        let res = tokio::time::timeout(Duration::from_secs(90), rx).await;

        {
            let mut pending = state.pending_tool_approvals.write().await;
            pending.remove(&request_id);
        }
        if let Some(app) = state.app_handle.read().await.clone() {
            let _ = app.emit("tool-approval-resolved", &request_id);
        }

        match res {
            Ok(Ok(ToolApprovalDecision::ApproveOnce)) => Ok(()),
            Ok(Ok(ToolApprovalDecision::ApproveForSession)) => {
                state
                    .session_approved_tools
                    .write()
                    .await
                    .insert(session_key);
                Ok(())
            }
            Ok(Ok(ToolApprovalDecision::Deny)) => {
                Err(format!("Eksekusi tool '{tool_name}' ditolak oleh user (HITL)."))
            }
            Err(_) | Ok(Err(_)) => Err(format!(
                "Eksekusi tool '{tool_name}' timeout pada konfirmasi HITL (90s)."
            )),
        }
    } else {
        Ok(())
    }
}

pub(crate) async fn current_turn_group_id(state: &AppState) -> Option<String> {
    state.active_turn_group_id.read().await.clone()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn protected_internal_files(state: &AppState) -> Vec<PathBuf> {
    [
        state.persist_auth_path.as_ref(),
        state.persist_config_path.as_ref(),
        state.secrets_store.path.as_ref(),
    ]
    .into_iter()
    .flatten()
    .cloned()
    .collect()
}

pub(crate) fn check_internal_file_block(
    protected_files: &[PathBuf],
    target: &Path,
    rel_label: &str,
) -> Result<(), String> {
    if is_airlock_internal_path(target) {
        return Err(format!(
            "Akses ditolak permanen: '{rel_label}' mengarah ke direktori internal .smart-tauri-hub."
        ));
    }

    for p in protected_files {
        if let Ok(canon_p) = std::fs::canonicalize(p) {
            if target == canon_p {
                return Err(format!(
                    "Akses ditolak permanen: '{rel_label}' mengarah ke file konfigurasi/kredensial internal Airlock."
                ));
            }
        }
        if target == p {
            return Err(format!(
                "Akses ditolak permanen: '{rel_label}' mengarah ke file konfigurasi/kredensial internal Airlock."
            ));
        }
    }
    Ok(())
}

pub(crate) fn check_sensitive_guard_parts(
    protected_files: &[PathBuf],
    canon_root: &Path,
    allow_sensitive: bool,
    patterns: &[String],
    target: &Path,
    rel_label: &str,
) -> Result<(), String> {
    check_internal_file_block(protected_files, target, rel_label)?;
    if allow_sensitive {
        return Ok(());
    }
    let rel = target.strip_prefix(canon_root).unwrap_or(target);
    if let Some(pat) = is_sensitive_path(rel, patterns) {
        return Err(format!(
            "Akses ditolak oleh Sensitive File Guard ('{rel_label}' cocok pola '{pat}'). Aktifkan 'Allow Sensitive Files' di dashboard jika memang diperlukan."
        ));
    }
    Ok(())
}

pub(crate) async fn check_sensitive_guard_for_workspace(
    state: &AppState,
    ws: &ResolvedWorkspace,
    target: &Path,
    rel_label: &str,
) -> Result<(), String> {
    let protected = protected_internal_files(state);
    check_sensitive_guard_parts(
        &protected,
        &ws.canon_root,
        ws.effective_allow_sensitive,
        &ws.effective_sensitive_patterns,
        target,
        rel_label,
    )
}

#[allow(dead_code)]
pub(crate) async fn check_sensitive_guard(
    state: &AppState,
    root: &Path,
    target: &Path,
    rel_label: &str,
) -> Result<(), String> {
    let protected = protected_internal_files(state);
    let allow_sensitive = state.allow_sensitive_reads.load(Ordering::SeqCst);
    let patterns = state.config.read().await.sensitive_deny_patterns.clone();
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    check_sensitive_guard_parts(&protected, &canon_root, allow_sensitive, &patterns, target, rel_label)
}
