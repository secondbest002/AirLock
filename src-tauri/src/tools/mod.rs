mod fs_tools;
mod guardrail;
mod revert;
mod schemas;
mod shell;
#[cfg(test)]
mod tests;

use crate::state::{AppState, AuditEntry};
use crate::tools_extra;
use crate::workspace::{resolve_workspace, ResolvedWorkspace};
use serde_json::Value;

pub use guardrail::{enforce_guardrail_gate, resolve_tool_approval};
pub use revert::{revert_audit_entry, revert_audit_group};
pub use schemas::{is_tool_read_only, mcp_tools_list, mcp_tools_list_for_profile};

pub(crate) use fs_tools::overview_text_for_resource;
pub(crate) use guardrail::{
    check_sensitive_guard_for_workspace, check_sensitive_guard_parts, current_turn_group_id,
    enforce_run_mode_gate, protected_internal_files, sha256_hex,
};
pub(crate) use schemas::is_known_builtin_tool;
#[allow(unused_imports)]
pub(crate) use shell::run_shell_raw;

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

async fn record_tool_error(
    state: &AppState,
    source: &str,
    tool_name: &str,
    args: &Value,
    ws: Option<&ResolvedWorkspace>,
    err: &str,
) {
    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: tool_name.to_string(),
                target_path: args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                summary: format!("Error: {err}"),
                diff_text: None,
                can_revert: false,
                reverted: false,
                tokens_saved: 0,
                original_content: None,
                original_blob_hash: None,
                file_existed_before: false,
                post_edit_hash: None,
                group_id: None,
                move_from_path: None,
                workspace_id: ws.map(|w| w.id.clone()),
                workspace_alias: ws.map(|w| w.alias.clone()),
                workspace_root: ws.map(|w| w.clean_path.clone()),
            },
            0,
        )
        .await;
}

pub async fn execute_tool(
    state: &AppState,
    source: &str,
    tool_name: &str,
    args: Value,
) -> ToolOutput {
    execute_tool_with_context(state, source, tool_name, args, None).await
}

pub async fn execute_tool_with_context(
    state: &AppState,
    source: &str,
    tool_name: &str,
    args: Value,
    pinned_ws: Option<&str>,
) -> ToolOutput {
    execute_tool_core(state, None, source, tool_name, args, pinned_ws, None, None).await
}

pub async fn execute_tool_with_run_context(
    state: std::sync::Arc<AppState>,
    source: &str,
    tool_name: &str,
    args: Value,
    pinned_ws: Option<&str>,
    run_ctx: Option<&crate::agent_manager::RunContext>,
) -> ToolOutput {
    execute_tool_core(
        state.as_ref(),
        Some(state.clone()),
        source,
        tool_name,
        args,
        pinned_ws,
        run_ctx,
        None,
    )
    .await
}

pub async fn execute_tool_with_session_context(
    state: std::sync::Arc<AppState>,
    source: &str,
    tool_name: &str,
    args: Value,
    pinned_ws: Option<&str>,
    run_ctx: Option<&crate::agent_manager::RunContext>,
    session_id: Option<&str>,
) -> ToolOutput {
    execute_tool_core(
        state.as_ref(),
        Some(state.clone()),
        source,
        tool_name,
        args,
        pinned_ws,
        run_ctx,
        session_id,
    )
    .await
}

fn format_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn take_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn take_tail_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        s.to_string()
    } else {
        s.chars().skip(total - max).collect()
    }
}

fn is_webmcp_control_line(trimmed: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "JOB_ID:",
        "JOB_STARTED:",
        "RUN_ID:",
        "GROUP_ID:",
        "EXIT CODE:",
        "EXIT_CODE:",
        "STATUS:",
        "NEXT_FROM_LINE:",
        "AUTO_PROMOTED",
        "DELEGATION_RUNNING",
        "STALL_WARNING:",
        "LSP_POST_CHECK:",
        "WORKSPACE:",
        "ELAPSED:",
        "LIVE_DRIFT_SINCE_CHECKPOINT:",
        "CHECKPOINT_STAGE:",
        "WEBMCP_NEXT_SESSION_PROMPT:",
        "ANTIGRAVITY_HANDOFF_PROMPT:",
        "[HARNESS_HINT:",
    ];
    PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

fn is_webmcp_priority_error_line(line: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "error[",
        "error:",
        "FAILED",
        "panicked at",
        "TS2",
        "Traceback",
        "SyntaxError",
    ];
    PATTERNS.iter().any(|p| line.contains(p))
}

pub(crate) fn compress_for_webmcp_budget(tool_name: &str, text: &str, max_chars: usize) -> String {
    let total_chars = text.chars().count();
    if total_chars <= max_chars || max_chars == 0 {
        return text.to_string();
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut claimed = vec![false; lines.len()];

    let mut control_lines: Vec<String> = Vec::new();
    let mut control_chars = 0usize;
    let control_budget = (max_chars / 3).max(200).min(max_chars);
    for (idx, line) in lines.iter().enumerate() {
        if is_webmcp_control_line(line.trim()) {
            let line_chars = line.chars().count();
            if control_lines.len() < 16 && control_chars + line_chars + 1 <= control_budget {
                claimed[idx] = true;
                control_chars += line_chars + 1;
                control_lines.push((*line).to_string());
            }
        }
    }

    let mut error_lines: Vec<String> = Vec::new();
    let mut error_chars = 0usize;
    let error_budget = (max_chars.saturating_sub(control_chars) * 40) / 100;
    if !matches!(tool_name, "read_file_range" | "read_symbol" | "code_outline") {
        for (idx, line) in lines.iter().enumerate() {
            if claimed[idx] {
                continue;
            }
            if is_webmcp_priority_error_line(line) {
                let clipped = if line.chars().count() > 300 {
                    format!("{}...", take_chars(line, 297))
                } else {
                    (*line).to_string()
                };
                let c_len = clipped.chars().count() + 1;
                if error_lines.len() < 25 && error_chars + c_len <= error_budget {
                    claimed[idx] = true;
                    error_chars += c_len;
                    error_lines.push(clipped);
                }
            }
        }
    }

    let control_block = control_lines.join("\n");
    let error_block = if error_lines.is_empty() {
        String::new()
    } else {
        format!("[WebMCP Priority Errors]\n{}", error_lines.join("\n"))
    };

    let marker_overhead = 175usize;
    let fixed_chars = control_block.chars().count() + error_block.chars().count() + marker_overhead;
    let remaining_budget = max_chars.saturating_sub(fixed_chars);
    let head_budget = (remaining_budget * 55) / 100;
    let tail_budget = remaining_budget.saturating_sub(head_budget);

    let mut head_lines: Vec<String> = Vec::new();
    let mut head_chars = 0usize;
    let mut last_head_idx: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        if claimed[idx] {
            continue;
        }
        let line_chars = line.chars().count();
        if head_chars + line_chars + 1 <= head_budget {
            head_lines.push((*line).to_string());
            head_chars += line_chars + 1;
            claimed[idx] = true;
            last_head_idx = Some(idx);
        } else {
            if head_lines.is_empty() && head_budget > 20 {
                head_lines.push(take_chars(line, head_budget));
                head_chars = head_budget;
            }
            break;
        }
    }

    let mut tail_lines: Vec<String> = Vec::new();
    let mut tail_chars = 0usize;
    for (idx, line) in lines.iter().enumerate().rev() {
        if let Some(lh) = last_head_idx {
            if idx <= lh {
                break;
            }
        }
        if claimed[idx] {
            continue;
        }
        let line_chars = line.chars().count();
        if tail_chars + line_chars + 1 <= tail_budget {
            tail_lines.push((*line).to_string());
            tail_chars += line_chars + 1;
            claimed[idx] = true;
        } else {
            if tail_lines.is_empty() && tail_budget > 20 {
                tail_lines.push(take_tail_chars(line, tail_budget));
                tail_chars = tail_budget;
            }
            break;
        }
    }
    tail_lines.reverse();

    let kept_content_chars =
        control_block.chars().count() + error_chars + head_chars + tail_chars;
    let omitted_chars = total_chars.saturating_sub(kept_content_chars).max(1);
    let marker = if tool_name == "list_skills" {
        format!(
            "... [WebMCP Output Guard: {} chars omitted to protect browser context. Pass compact=true, query=\"...\", or scope=\"workspace|plugin|global\" to list_skills.] ...",
            format_thousands(omitted_chars)
        )
    } else {
        format!(
            "... [WebMCP Output Guard: {} chars omitted to protect browser context. Narrow 'path'/'start_line' or call with targeted slice/from_line.] ...",
            format_thousands(omitted_chars)
        )
    };

    let mut sections: Vec<String> = Vec::new();
    if !control_block.is_empty() {
        sections.push(control_block);
    }
    if !head_lines.is_empty() {
        sections.push(head_lines.join("\n"));
    }
    if !error_block.is_empty() {
        sections.push(error_block);
    }
    sections.push(marker);
    if !tail_lines.is_empty() {
        sections.push(tail_lines.join("\n"));
    }

    let assembled = sections.join("\n");
    if assembled.chars().count() <= max_chars {
        assembled
    } else {
        take_chars(&assembled, max_chars)
    }
}

async fn apply_webmcp_output_guard(
    state: &AppState,
    source: &str,
    tool_name: &str,
    mut out: ToolOutput,
) -> ToolOutput {
    if source != "WebMCP" {
        return out;
    }
    let max_chars = state.config.read().await.effective_webmcp_max_output_chars();
    let orig_chars = out.text.chars().count();
    if orig_chars <= max_chars {
        return out;
    }
    let compressed = compress_for_webmcp_budget(tool_name, &out.text, max_chars);
    let new_chars = compressed.chars().count();
    let extra_tokens_saved = orig_chars.saturating_sub(new_chars) / 4;
    out.text = compressed;

    if extra_tokens_saved > 0 {
        state.metrics.write().await.tokens_saved_total += extra_tokens_saved;
        let mut logs = state.audit_logs.write().await;
        if let Some(entry) = logs
            .iter_mut()
            .rev()
            .take(8)
            .find(|e| e.source == source && e.tool_name == tool_name)
        {
            entry.tokens_saved = entry.tokens_saved.saturating_add(extra_tokens_saved);
            state.snapshot_store.save_audit_entries(&logs);
        }
    }
    out
}

fn should_sync_webmcp_tool(
    effective_source: &str,
    tool_name: &str,
    args: &Value,
    out: &ToolOutput,
) -> Option<crate::handoff::HandoffTrigger> {
    if !crate::handoff::is_webmcp_source(effective_source) {
        return None;
    }
    if out.is_error {
        let blocked_or_invalid = out.text.starts_with("Diblokir oleh ")
            || out.text.starts_with("Akses ditolak")
            || out.text.starts_with("Command Policy")
            || out.text.starts_with("Argumen ")
            || out.text.starts_with("Format ")
            || out.text.starts_with("Tool tidak dikenal")
            || out.text.starts_with("Workspace ")
            || out.text.starts_with("Koneksi ini di-pin")
            || out.text.starts_with("Mode Plan-First");
        if blocked_or_invalid {
            return None;
        }
    }

    match tool_name {
        "surgical_edit" | "write_file_jailed" | "file_ops" => {
            if out.is_error {
                None
            } else {
                Some(crate::handoff::HandoffTrigger::ToolMutation(
                    tool_name.to_string(),
                ))
            }
        }
        "multi_surgical_edit" | "patch_and_verify" => {
            if !out.is_error
                || out.text.contains("PATCH DIFF")
                || out.text.contains("Applied atomic")
            {
                Some(crate::handoff::HandoffTrigger::ToolMutation(
                    tool_name.to_string(),
                ))
            } else {
                None
            }
        }
        "run_command_compressed" | "start_command_job" => Some(
            crate::handoff::HandoffTrigger::CommandFinished(tool_name.to_string()),
        ),
        "run_command" => {
            let act = schemas::normalized_arg_str(args, "action", "run");
            match act.as_str() {
                "run" | "start" => Some(crate::handoff::HandoffTrigger::CommandFinished(
                    tool_name.to_string(),
                )),
                "poll"
                    if !out.is_error
                        && (out.text.contains("STATUS: completed")
                            || out.text.contains("STATUS: failed")
                            || out.text.contains("STATUS: killed")
                            || out.text.contains("STATUS: timed_out")) =>
                {
                    Some(crate::handoff::HandoffTrigger::CommandFinished(
                        tool_name.to_string(),
                    ))
                }
                _ => None,
            }
        }
        "poll_command_job" => {
            if !out.is_error
                && (out.text.contains("STATUS: completed")
                    || out.text.contains("STATUS: failed")
                    || out.text.contains("STATUS: killed")
                    || out.text.contains("STATUS: timed_out"))
            {
                Some(crate::handoff::HandoffTrigger::CommandFinished(
                    tool_name.to_string(),
                ))
            } else {
                None
            }
        }
        "git_ops" => {
            let op = schemas::normalized_arg_str(args, "op", "");
            if !out.is_error && matches!(op.as_str(), "add" | "commit") {
                Some(crate::handoff::HandoffTrigger::ToolMutation(format!(
                    "git_{op}"
                )))
            } else {
                None
            }
        }
        "upsert_artifact" | "request_plan_approval" => {
            if out.is_error {
                None
            } else {
                let kind = args
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("plan")
                    .to_string();
                Some(crate::handoff::HandoffTrigger::ArtifactUpdated(kind))
            }
        }
        _ => None,
    }
}

async fn execute_tool_core(
    state: &AppState,
    state_arc: Option<std::sync::Arc<AppState>>,
    source: &str,
    tool_name: &str,
    args: Value,
    pinned_ws: Option<&str>,
    run_ctx: Option<&crate::agent_manager::RunContext>,
    session_id: Option<&str>,
) -> ToolOutput {
    let effective_source: &str = if source != "WebMCP"
        && run_ctx
            .map(|c| c.execution_mode.starts_with("delegated_"))
            .unwrap_or(false)
    {
        "WebMCP:delegate"
    } else {
        source
    };
    let is_webmcp_origin = crate::handoff::is_webmcp_source(effective_source);
    let args_snapshot = if (source == "WebMCP" && session_id.is_some()) || is_webmcp_origin {
        Some(args.clone())
    } else {
        None
    };

    let mut out = execute_tool_core_inner(
        state,
        state_arc,
        effective_source,
        tool_name,
        args,
        pinned_ws,
        run_ctx,
    )
    .await;

    if is_webmcp_origin {
        if let Some(ref args_ref) = args_snapshot {
            if let Some(trigger) =
                should_sync_webmcp_tool(effective_source, tool_name, args_ref, &out)
            {
                let mut req_ws = args_ref
                    .get("workspace")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| run_ctx.map(|c| c.workspace_alias.clone()));
                if req_ws.is_none()
                    && matches!(tool_name, "poll_command_job" | "run_command")
                {
                    if let Some(jid) = args_ref.get("job_id").and_then(|v| v.as_str()) {
                        req_ws = crate::jobs::get_job_workspace_alias(state, jid).await;
                    }
                }
                if let Ok(ws) =
                    resolve_workspace(state, req_ws.as_deref(), pinned_ws).await
                {
                    let _ = crate::handoff::sync_from_state(state, &ws, trigger, false).await;
                }
            }
        }
    }

    if source == "WebMCP" && session_id.is_some() {
        if let Some(ref args_ref) = args_snapshot {
            let req_ws = args_ref
                .get("workspace")
                .and_then(|v| v.as_str())
                .or_else(|| run_ctx.map(|c| c.workspace_alias.as_str()));
            let is_multi_ws = state
                .config
                .read()
                .await
                .workspaces
                .iter()
                .filter(|w| w.enabled)
                .count()
                > 1;
            let is_global_meta_without_ws = is_multi_ws
                && matches!(
                    tool_name,
                    "list_workspaces" | "list_skills" | "load_skill" | "read_skill_resource"
                )
                && req_ws.map(str::trim).filter(|s| !s.is_empty()).is_none()
                && pinned_ws.map(str::trim).filter(|s| !s.is_empty()).is_none();
            let ws_opt = if is_global_meta_without_ws {
                None
            } else {
                resolve_workspace(state, req_ws, pinned_ws).await.ok()
            };
            state
                .webmcp_harness
                .after_tool_call(
                    state,
                    session_id,
                    ws_opt.as_ref(),
                    tool_name,
                    args_ref,
                    &mut out,
                )
                .await;
        }
    }
    apply_webmcp_output_guard(state, source, tool_name, out).await
}

async fn execute_tool_core_inner(
    state: &AppState,
    state_arc: Option<std::sync::Arc<AppState>>,
    source: &str,
    tool_name: &str,
    args: Value,
    pinned_ws: Option<&str>,
    run_ctx: Option<&crate::agent_manager::RunContext>,
) -> ToolOutput {
    if !is_known_builtin_tool(tool_name) {
        let err = format!("Tool tidak dikenal: {tool_name}");
        record_tool_error(state, source, tool_name, &args, None, &err).await;
        return ToolOutput {
            text: err,
            is_error: true,
        };
    }

    let is_ro = is_tool_read_only(tool_name, &args);

    if let Err(err) = guardrail::enforce_run_mode_gate(state, run_ctx, tool_name, is_ro).await {
        record_tool_error(state, source, tool_name, &args, None, &err).await;
        return ToolOutput {
            text: err,
            is_error: true,
        };
    }

    if tool_name == "spawn_subagent" {
        let Some(arc_st) = state_arc else {
            let err = "spawn_subagent hanya dapat dijalankan dari dalam Local Agent run."
                .to_string();
            record_tool_error(state, source, tool_name, &args, None, &err).await;
            return ToolOutput {
                text: err,
                is_error: true,
            };
        };
        let Some(ctx) = run_ctx else {
            let err = "spawn_subagent memerlukan RunContext aktif.".to_string();
            record_tool_error(state, source, tool_name, &args, None, &err).await;
            return ToolOutput {
                text: err,
                is_error: true,
            };
        };
        let res = crate::local_agent::run_subagent_turn(arc_st, ctx, &args).await;
        return match res {
            Ok(text) => ToolOutput {
                text,
                is_error: false,
            },
            Err(err) => {
                record_tool_error(state, source, tool_name, &args, None, &err).await;
                ToolOutput {
                    text: err,
                    is_error: true,
                }
            }
        };
    }

    if tool_name == "list_workspaces" {
        return match fs_tools::exec_list_workspaces(state, source, pinned_ws).await {
            Ok(text) => ToolOutput {
                text,
                is_error: false,
            },
            Err(err) => {
                record_tool_error(state, source, tool_name, &args, None, &err).await;
                ToolOutput {
                    text: err,
                    is_error: true,
                }
            }
        };
    }

    if matches!(
        tool_name,
        "list_skills" | "load_skill" | "read_skill_resource"
    ) {
        let pin_ws_opt = match pinned_ws.map(str::trim).filter(|s| !s.is_empty()) {
            Some(pin) => match resolve_workspace(state, None, Some(pin)).await {
                Ok(w) => Some(w),
                Err(err) => {
                    record_tool_error(state, source, tool_name, &args, None, &err).await;
                    return ToolOutput {
                        text: err,
                        is_error: true,
                    };
                }
            },
            None => None,
        };
        let gate_res = guardrail::enforce_guardrail_gate_for_workspace(
            state,
            source,
            tool_name,
            &args,
            true,
            pin_ws_opt.as_ref(),
        )
        .await;
        let result = match gate_res {
            Err(err) => Err(err),
            Ok(()) => match tool_name {
                "list_skills" => fs_tools::exec_list_skills(state, source, pinned_ws, &args).await,
                "load_skill" => {
                    fs_tools::exec_load_skill(state, source, pinned_ws, &args).await
                }
                "read_skill_resource" => {
                    fs_tools::exec_read_skill_resource(state, source, pinned_ws, &args).await
                }
                _ => unreachable!(),
            },
        };
        return match result {
            Ok(text) => ToolOutput {
                text,
                is_error: false,
            },
            Err(err) => {
                record_tool_error(
                    state,
                    source,
                    tool_name,
                    &args,
                    pin_ws_opt.as_ref(),
                    &err,
                )
                .await;
                ToolOutput {
                    text: err,
                    is_error: true,
                }
            }
        };
    }

    let unified_action = args
        .get("action")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let is_unified_job_control =
        tool_name == "run_command" && matches!(unified_action.as_deref(), Some("poll" | "stop"));
    let is_delegated_control = tool_name == "delegate_local_agent"
        && matches!(unified_action.as_deref(), Some("poll" | "cancel" | "stop"));

    if tool_name == "poll_command_job"
        || tool_name == "stop_command_job"
        || is_unified_job_control
        || is_delegated_control
    {
        let pin_ws_opt = match pinned_ws.map(str::trim).filter(|s| !s.is_empty()) {
            Some(pin) => match resolve_workspace(state, None, Some(pin)).await {
                Ok(w) => Some(w),
                Err(err) => {
                    record_tool_error(state, source, tool_name, &args, None, &err).await;
                    return ToolOutput {
                        text: err,
                        is_error: true,
                    };
                }
            },
            None => None,
        };
        let gate_res = guardrail::enforce_guardrail_gate_with_run_status(
            state,
            source,
            tool_name,
            &args,
            is_ro,
            pin_ws_opt.as_ref(),
            run_ctx.map(|c| c.run_id.as_str()),
        )
        .await;
        let result = match gate_res {
            Err(err) => Err(err),
            Ok(()) => match tool_name {
                "poll_command_job" => {
                    tools_extra::exec_poll_command_job(state, source, &args, pinned_ws).await
                }
                "stop_command_job" => {
                    tools_extra::exec_stop_command_job(state, source, &args, pinned_ws).await
                }
                "run_command" if unified_action.as_deref() == Some("poll") => {
                    tools_extra::exec_poll_command_job(state, source, &args, pinned_ws).await
                }
                "run_command" => {
                    tools_extra::exec_stop_command_job(state, source, &args, pinned_ws).await
                }
                "delegate_local_agent" => {
                    if let Some(arc_st) = state_arc {
                        let act = unified_action.as_deref().unwrap_or("poll");
                        crate::local_agent::exec_delegate_local_agent_poll_or_cancel(
                            arc_st, source, act, &args, pinned_ws,
                        )
                        .await
                    } else {
                        Err("delegate_local_agent memerlukan runtime AppState Arc.".to_string())
                    }
                }
                _ => unreachable!(),
            },
        };
        return match result {
            Ok(text) => ToolOutput {
                text,
                is_error: false,
            },
            Err(err) => {
                record_tool_error(
                    state,
                    source,
                    tool_name,
                    &args,
                    pin_ws_opt.as_ref(),
                    &err,
                )
                .await;
                ToolOutput {
                    text: err,
                    is_error: true,
                }
            }
        };
    }

    let req_ws = args
        .get("workspace")
        .and_then(|v| v.as_str())
        .or_else(|| run_ctx.map(|c| c.workspace_alias.as_str()));
    let ws = match resolve_workspace(state, req_ws, pinned_ws).await {
        Ok(w) => w,
        Err(err) => {
            record_tool_error(state, source, tool_name, &args, None, &err).await;
            return ToolOutput {
                text: err,
                is_error: true,
            };
        }
    };

    let result = match guardrail::enforce_guardrail_gate_with_run_status(
        state,
        source,
        tool_name,
        &args,
        is_ro,
        Some(&ws),
        run_ctx.map(|c| c.run_id.as_str()),
    )
    .await
    {
        Err(err) => Err(err),
        Ok(()) => {
            // Command tools and verify-capable edits manage locking narrowly or run async background jobs;
            // holding workspace_write_lock across 18s auto-promote waits would starve concurrent sessions into 32s timeouts.
            let _ws_write_guard = if !is_ro
                && !matches!(
                    tool_name,
                    "upsert_artifact"
                        | "request_plan_approval"
                        | "delegate_local_agent"
                        | "run_command"
                        | "run_command_compressed"
                        | "start_command_job"
                        | "multi_surgical_edit"
                        | "patch_and_verify"
                ) {
                Some(
                    state
                        .agent_manager
                        .workspace_write_lock(&ws.id)
                        .await
                        .lock_owned()
                        .await,
                )
            } else {
                None
            };
            match tool_name {
                "project_overview" => {
                    fs_tools::exec_project_overview(state, source, &ws, &args).await
                }
                "smart_search" => fs_tools::exec_smart_search(state, source, &ws, &args).await,
                "read_file_range" => {
                    fs_tools::exec_read_file_range(state, source, &ws, req_ws, pinned_ws, &args)
                        .await
                }
                "batch_inspect" => {
                    fs_tools::exec_batch_inspect(state, source, &ws, req_ws, pinned_ws, &args).await
                }
                "surgical_edit" => fs_tools::exec_surgical_edit(state, source, &ws, &args).await,
                "multi_surgical_edit" => {
                    fs_tools::exec_multi_surgical_edit(state, source, &ws, &args).await
                }
                "patch_and_verify" => {
                    fs_tools::exec_patch_and_verify(state, source, &ws, &args).await
                }
                "run_command_compressed" => shell::exec_run_command(state, source, &ws, &args).await,
                "run_command" => shell::exec_run_command_unified(state, source, &ws, &args).await,
                "write_file_jailed" => fs_tools::exec_write_file(state, source, &ws, &args).await,
                "find_files" => tools_extra::exec_find_files(state, source, &ws, &args).await,
                "code_outline" => tools_extra::exec_code_outline(state, source, &ws, &args).await,
                "read_symbol" => tools_extra::exec_read_symbol(state, source, &ws, &args).await,
                "lsp_diagnostics" => {
                    crate::lsp_bridge::exec_lsp_diagnostics(state, source, &ws, &args).await
                }
                "lsp_definition" => {
                    crate::lsp_bridge::exec_lsp_definition(state, source, &ws, &args).await
                }
                "lsp_references" => {
                    crate::lsp_bridge::exec_lsp_references(state, source, &ws, &args).await
                }
                "lsp_hover" => crate::lsp_bridge::exec_lsp_hover(state, source, &ws, &args).await,
                "lsp_query" => {
                    crate::lsp_bridge::exec_lsp_query(state, source, &ws, &args).await
                }
                "session_checkpoint" => {
                    crate::checkpoints::exec_session_checkpoint(state, source, &ws, &args).await
                }
                "delegate_local_agent" => {
                    if let Some(arc_st) = state_arc {
                        crate::local_agent::exec_delegate_local_agent_start(
                            arc_st, source, &ws, &args, pinned_ws,
                        )
                        .await
                    } else {
                        Err("delegate_local_agent memerlukan runtime AppState Arc.".to_string())
                    }
                }
                "git_ops" => tools_extra::exec_git_ops(state, source, &ws, &args).await,
                "file_ops" => tools_extra::exec_file_ops(state, source, &ws, &args).await,
                "start_command_job" => {
                    tools_extra::exec_start_command_job(state, source, &ws, &args).await
                }
                "upsert_artifact" => {
                    fs_tools::exec_upsert_artifact(state, source, &ws, run_ctx, &args).await
                }
                "request_plan_approval" => {
                    fs_tools::exec_request_plan_approval(state, source, &ws, run_ctx, &args).await
                }
                other => Err(format!("Tool tidak dikenal: {other}")),
            }
        }
    };

    match result {
        Ok(text) => ToolOutput {
            text,
            is_error: false,
        },
        Err(err) => {
            record_tool_error(state, source, tool_name, &args, Some(&ws), &err).await;
            ToolOutput {
                text: err,
                is_error: true,
            }
        }
    }
}
