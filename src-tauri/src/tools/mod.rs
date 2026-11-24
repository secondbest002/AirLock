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
pub use schemas::{is_tool_read_only, mcp_tools_list};

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
    execute_tool_core(state, None, source, tool_name, args, pinned_ws, None).await
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
    )
    .await
}

async fn execute_tool_core(
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
                "list_skills" => fs_tools::exec_list_skills(state, source, pinned_ws).await,
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

    if tool_name == "poll_command_job" || tool_name == "stop_command_job" {
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
            is_ro,
            pin_ws_opt.as_ref(),
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

    let result = match guardrail::enforce_guardrail_gate_for_workspace(
        state,
        source,
        tool_name,
        &args,
        is_ro,
        Some(&ws),
    )
    .await
    {
        Err(err) => Err(err),
        Ok(()) => {
            let _ws_write_guard =
                if !is_ro && !matches!(tool_name, "upsert_artifact" | "request_plan_approval") {
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
                "batch_inspect" => fs_tools::exec_batch_inspect(state, source, &ws, &args).await,
                "surgical_edit" => fs_tools::exec_surgical_edit(state, source, &ws, &args).await,
                "multi_surgical_edit" => {
                    fs_tools::exec_multi_surgical_edit(state, source, &ws, &args).await
                }
                "patch_and_verify" => {
                    fs_tools::exec_patch_and_verify(state, source, &ws, &args).await
                }
                "run_command_compressed" => shell::exec_run_command(state, source, &ws, &args).await,
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
