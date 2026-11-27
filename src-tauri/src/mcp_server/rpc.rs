use super::auth::McpToolProfile;
use crate::state::AppState;
use crate::tools::mcp_tools_list_for_profile;
use crate::workspace::{build_workspace_summaries, clean_path_string, default_alias_for_path};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RpcRequestContext<'a> {
    pub pinned_ws: Option<&'a str>,
    pub tool_profile: McpToolProfile,
    pub source: &'a str,
    pub session_id: Option<&'a str>,
}

async fn build_mcp_initialize_instructions(
    state: &AppState,
    profile: McpToolProfile,
    pinned_ws: Option<&str>,
    enable_delegation: bool,
    expose_skills: bool,
) -> String {
    let (workspaces_raw, cfg_def_id, auto_promote_secs, poll_wait_secs) = {
        let cfg = state.config.read().await;
        (
            cfg.workspaces.clone(),
            cfg.default_workspace_id.clone(),
            cfg.effective_webmcp_auto_promote_secs(),
            cfg.effective_webmcp_poll_wait_secs(),
        )
    };
    let state_def_id = state.default_workspace_id.read().await.clone();
    let def_id = if !state_def_id.trim().is_empty() {
        state_def_id
    } else {
        cfg_def_id.unwrap_or_default()
    };
    let global_guard = state.guardrail_mode.read().await.clone();

    let ws_summary = if let Some(pin) = pinned_ws.map(str::trim).filter(|s| !s.is_empty()) {
        match crate::workspace::resolve_workspace(state, None, Some(pin)).await {
            Ok(w) => format!("@{} (pinned, guardrail: {})", w.alias, w.effective_guardrail),
            Err(_) => format!("@{pin} (pinned)"),
        }
    } else {
        let mut enabled: Vec<_> =
            build_workspace_summaries(&workspaces_raw, &def_id, &global_guard)
                .into_iter()
                .filter(|w| w.enabled)
                .collect();
        if enabled.is_empty() {
            let root = state.workspace_root.read().await.clone();
            let alias = default_alias_for_path(&root);
            let _clean = clean_path_string(&root);
            format!("@{alias} (guardrail: {global_guard})")
        } else {
            enabled.sort_by(|a, b| b.is_default.cmp(&a.is_default).then_with(|| a.alias.cmp(&b.alias)));
            let total = enabled.len();
            let shown: Vec<String> = enabled
                .into_iter()
                .take(3)
                .map(|w| {
                    let def_tag = if w.is_default { "default, " } else { "" };
                    format!("@{} ({def_tag}guardrail: {})", w.alias, w.effective_guardrail)
                })
                .collect();
            if total > 3 {
                format!("{}, (+{} more)", shown.join(", "), total - 3)
            } else {
                shown.join(", ")
            }
        }
    };

    let delegate_line = if enable_delegation {
        "\n5. DELEGATE HEAVY LOOPS: Use `delegate_local_agent(goal=..., mode='read_only')` to offload deep multi-step codebase exploration cleanly without polluting this chat context."
    } else {
        ""
    };
    let next_idx = if enable_delegation { 6 } else { 5 };
    let skills_rules_line = if expose_skills {
        format!(
            "\n{next_idx}. SKILLS & RULES: Call `list_skills` then `load_skill(name)` (and `read_skill_resource`) before specialized/UI/design/architecture tasks; read rules via `airlock://instructions`."
        )
    } else {
        format!(
            "\n{next_idx}. RULES: Check global & workspace rules via resource `airlock://instructions`."
        )
    };

    let handoff_clause = "Use skill 'airlock-blueprint-workflow' (1 blueprint/turn, end active turns with 2-4 choices); save `session_checkpoint(op='save', stage=...)` & copy `WEBMCP_NEXT_SESSION_PROMPT` + `ANTIGRAVITY_HANDOFF_PROMPT` verbatim.";
    let body = match profile {
        McpToolProfile::Compact => format!(
            "AIRLOCK WEBMCP PROTOCOL (Profile: compact):\n\
            Active workspace(s): {ws_summary}.\n\
            1. BATCH INSPECT: Start with `batch_inspect` (overview, find_patterns, outline_paths, searches, read_slices, symbol_reads). Issue independent read-only calls in parallel.\n\
            2. ATOMIC EDITS: Use `multi_surgical_edit` (with `post_check_lsp: true` and/or `verify_command` when verifying); avoid re-reading edited files.\n\
            3. COMMANDS & JOBS: Use `run_command` (`action='run'` auto-promotes at {auto_promote_secs}s; `action='poll'` waits {poll_wait_secs}s automatically). NEVER run `sleep`.\n\
            4. CHECKPOINTS & HANDOFF: Use `session_checkpoint` (`op='save'|'load'`) across turns. {handoff_clause}{delegate_line}{skills_rules_line}"
        ),
        McpToolProfile::Full => format!(
            "AIRLOCK MCP PROTOCOL (Profile: full):\n\
            Active workspace(s): {ws_summary}.\n\
            1. MINIMIZE ROUND-TRIPS: Prefer `batch_inspect` (with `read_slices`/`symbol_reads`) and issue independent read-only calls in parallel.\n\
            2. ATOMIC EDITS: Prefer `multi_surgical_edit` (`post_check_lsp: true`, `verify_command`) or `surgical_edit`; avoid re-reading edited files.\n\
            3. LONG COMMANDS: `run_command` / `run_command_compressed` auto-promotes after {auto_promote_secs}s; `poll` waits {poll_wait_secs}s. NEVER run `sleep`.\n\
            4. CHECKPOINTS & HANDOFF: Use `session_checkpoint` (`op='save'|'load'`) across multi-turn sessions. {handoff_clause}{delegate_line}{skills_rules_line}"
        ),
    };

    if body.chars().count() > 1200 {
        let mut clipped: String = body.chars().take(1197).collect();
        if let Some(last_nl) = clipped.rfind('\n') {
            if last_nl > 300 {
                clipped.truncate(last_nl);
            }
        }
        format!("{clipped}...")
    } else {
        body
    }
}

async fn build_skills_rpc_list(state: &AppState, pinned_ws: Option<&str>) -> Vec<Value> {
    let skills = state.skills_for_context(pinned_ws).await;
    skills
        .into_iter()
        .filter(|s| s.summary.enabled && s.summary.parse_error.is_none())
        .map(|s| {
            let uri = format!("skill://{}", s.summary.name);
            let resources: Vec<String> = s
                .summary
                .resource_files
                .iter()
                .map(|p| format!("{uri}/{p}"))
                .collect();
            json!({
                "name": s.summary.name,
                "description": s.summary.description,
                "scope": s.summary.scope,
                "sourceLabel": s.summary.source_label,
                "disableModelInvocation": s.summary.disable_model_invocation,
                "uri": uri,
                "resources": resources
            })
        })
        .collect()
}

fn require_task_id<'a>(params: &'a Value, rpc_id: &Value) -> Result<&'a str, Value> {
    params
        .get("taskId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "error": { "code": -32602, "message": "Parameter 'taskId' wajib diisi." }
            })
        })
}

pub(crate) async fn handle_single_rpc(state: &Arc<AppState>, msg: &Value) -> Option<Value> {
    handle_single_rpc_with_request_context(
        state,
        msg,
        RpcRequestContext {
            pinned_ws: None,
            tool_profile: McpToolProfile::Full,
            source: "Stdio",
            session_id: None,
        },
    )
    .await
}

#[allow(dead_code)]
pub(crate) async fn handle_single_rpc_with_context(
    state: &Arc<AppState>,
    msg: &Value,
    pinned_ws: Option<&str>,
) -> Option<Value> {
    handle_single_rpc_with_request_context(
        state,
        msg,
        RpcRequestContext {
            pinned_ws,
            tool_profile: McpToolProfile::Full,
            source: "WebMCP",
            session_id: None,
        },
    )
    .await
}

pub(crate) async fn handle_single_rpc_with_request_context(
    state: &Arc<AppState>,
    msg: &Value,
    ctx: RpcRequestContext<'_>,
) -> Option<Value> {
    let pinned_ws = ctx.pinned_ws;
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

    if method == "notifications/initialized" || (id.is_none() && method.starts_with("notifications/"))
    {
        return None;
    }

    let rpc_id = id.unwrap_or(Value::Null);
    let result_body = match method {
        "initialize" => {
            let requested_version = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2025-03-26");
            let expose_skills = state.config.read().await.webmcp_expose_skills;
            let enable_delegation = ctx.source == "WebMCP"
                && crate::local_agent::has_delegation_provider_ready(state).await;
            let instructions = build_mcp_initialize_instructions(
                state,
                ctx.tool_profile,
                pinned_ws,
                enable_delegation,
                expose_skills,
            )
            .await;
            json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "result": {
                    "protocolVersion": requested_version,
                    "capabilities": {
                        "tools": { "listChanged": true },
                        "resources": { "subscribe": false, "listChanged": true },
                        "prompts": { "listChanged": true },
                        "tasks": {
                            "list": {},
                            "cancel": {},
                            "requests": {
                                "tools": {
                                    "call": {}
                                }
                            }
                        },
                        "experimental": {
                            "io.modelcontextprotocol/skills": { "listChanged": true }
                        }
                    },
                    "serverInfo": {
                        "name": "smart-tauri-hub",
                        "version": "0.1.0"
                    },
                    "instructions": instructions
                }
            })
        }
        "ping" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {}
        }),
        "server/discover" => {
            let expose_skills = state.config.read().await.webmcp_expose_skills;
            let is_webmcp = ctx.source == "WebMCP";
            let enable_delegation = is_webmcp
                && crate::local_agent::has_delegation_provider_ready(state).await;
            let mut merged_tools = mcp_tools_list_for_profile(
                ctx.tool_profile,
                enable_delegation,
                expose_skills,
                is_webmcp,
            )
            .as_array()
            .cloned()
            .unwrap_or_default();
            let workspaces_count = if pinned_ws.is_some() {
                1
            } else {
                state
                    .config
                    .read()
                    .await
                    .workspaces
                    .iter()
                    .filter(|w| w.enabled)
                    .count()
                    .max(1)
            };
            let cfgs = state.resolved_downstream_mcps().await;
            let agg = state.mcp_hub.list_aggregated_tools(&cfgs).await;
            let downstream_count = cfgs.iter().filter(|c| c.enabled).count();
            merged_tools.extend(agg);
            let guardrail_mode = state.guardrail_mode.read().await.clone();
            let skills_list = build_skills_rpc_list(state, pinned_ws).await;
            json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "result": {
                    "protocolVersions": ["2025-03-26", "2026-06-01"],
                    "serverInfo": {
                        "name": "smart-tauri-hub",
                        "version": "0.1.0"
                    },
                    "capabilities": {
                        "tools": { "listChanged": true },
                        "resources": { "subscribe": false, "listChanged": true },
                        "prompts": { "listChanged": true },
                        "tasks": {
                            "list": {},
                            "cancel": {},
                            "requests": {
                                "tools": {
                                    "call": {}
                                }
                            }
                        },
                        "experimental": {
                            "io.modelcontextprotocol/skills": { "listChanged": true }
                        }
                    },
                    "tools": merged_tools,
                    "resources": crate::mcp_resources::resources_list(),
                    "resourceTemplates": crate::mcp_resources::resource_templates_list(),
                    "prompts": crate::mcp_resources::prompts_list_for_context(state, pinned_ws).await,
                    "skills": skills_list,
                    "hub": {
                        "downstreamServersCount": downstream_count,
                        "guardrailMode": guardrail_mode,
                        "workspacesCount": workspaces_count,
                        "pinnedWorkspace": pinned_ws
                    },
                    "_meta": { "cacheControl": { "type": "ephemeral" } }
                }
            })
        }
        "tools/list" => {
            let expose_skills = state.config.read().await.webmcp_expose_skills;
            let is_webmcp = ctx.source == "WebMCP";
            let enable_delegation = is_webmcp
                && crate::local_agent::has_delegation_provider_ready(state).await;
            let mut merged_tools = mcp_tools_list_for_profile(
                ctx.tool_profile,
                enable_delegation,
                expose_skills,
                is_webmcp,
            )
            .as_array()
            .cloned()
            .unwrap_or_default();
            let cfgs = state.resolved_downstream_mcps().await;
            let agg = state.mcp_hub.list_aggregated_tools(&cfgs).await;
            merged_tools.extend(agg);
            json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "result": {
                    "tools": merged_tools,
                    "_meta": { "cacheControl": { "type": "ephemeral" } }
                }
            })
        }
        "skills/list" => {
            let skills_list = build_skills_rpc_list(state, pinned_ws).await;
            json!({
                "jsonrpc": "2.0",
                "id": rpc_id,
                "result": {
                    "skills": skills_list,
                    "_meta": { "cacheControl": { "type": "ephemeral" } }
                }
            })
        }
        "skills/get" => {
            let sname = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if sname.is_empty() {
                json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "error": { "code": -32602, "message": "Missing required param 'name'" }
                })
            } else {
                let skills = state.skills_for_context(pinned_ws).await;
                if let Some(s) = skills.into_iter().find(|item| {
                    item.summary.enabled
                        && item.summary.parse_error.is_none()
                        && item.summary.name.eq_ignore_ascii_case(sname)
                }) {
                    let uri = format!("skill://{}", s.summary.name);
                    let resources: Vec<Value> = s
                        .summary
                        .resource_files
                        .iter()
                        .map(|p| {
                            json!({
                                "path": p,
                                "uri": format!("{uri}/{p}")
                            })
                        })
                        .collect();
                    json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "result": {
                            "name": s.summary.name,
                            "description": s.summary.description,
                            "scope": s.summary.scope,
                            "sourceLabel": s.summary.source_label,
                            "uri": uri,
                            "content": s.raw_md,
                            "body": s.body_md,
                            "resources": resources
                        }
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "error": { "code": -32602, "message": format!("Skill '{sname}' tidak ditemukan atau nonaktif") }
                    })
                }
            }
        }
        "resources/list" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {
                "resources": crate::mcp_resources::resources_list(),
                "_meta": { "cacheControl": { "type": "ephemeral" } }
            }
        }),
        "resources/templates/list" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {
                "resourceTemplates": crate::mcp_resources::resource_templates_list(),
                "_meta": { "cacheControl": { "type": "ephemeral" } }
            }
        }),
        "resources/read" => {
            let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
            if uri.trim().is_empty() {
                json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "error": { "code": -32602, "message": "Missing required param 'uri'" }
                })
            } else {
                match crate::mcp_resources::read_resource_with_context(state, uri, pinned_ws).await {
                    Ok(res) => json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "result": res
                    }),
                    Err(err) => json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "error": { "code": -32602, "message": err }
                    }),
                }
            }
        }
        "prompts/list" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {
                "prompts": crate::mcp_resources::prompts_list_for_context(state, pinned_ws).await,
                "_meta": { "cacheControl": { "type": "ephemeral" } }
            }
        }),
        "prompts/get" => {
            let pname = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let pargs = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match crate::mcp_resources::get_prompt_with_context(state, pname, &pargs, pinned_ws).await {
                Ok(res) => json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "result": res
                }),
                Err(err) => json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "error": { "code": -32602, "message": err }
                }),
            }
        }
        "logging/setLevel" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {}
        }),
        "tools/call" => {
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            if let Some(task_cfg) = params.get("task").and_then(|v| v.as_object()) {
                if tool_name.trim().is_empty() {
                    json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "error": {
                            "code": -32602,
                            "message": "Missing tool 'name' in tools/call"
                        }
                    })
                } else {
                    let ttl_ms = task_cfg.get("ttl").and_then(|v| v.as_u64());
                    let task_info = state
                        .mcp_tasks
                        .create_tool_task(
                            Arc::clone(state),
                            ctx.source.to_string(),
                            tool_name.to_string(),
                            args,
                            pinned_ws.map(str::to_string),
                            ttl_ms,
                        )
                        .await;
                    json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "result": {
                            "task": task_info
                        }
                    })
                }
            } else {
                let rpc_timeout_secs = state
                    .config
                    .read()
                    .await
                    .effective_webmcp_rpc_timeout_secs();
                let fut = async {
                    if tool_name.contains("__") {
                        state
                            .mcp_hub
                            .call_downstream_tool(state, ctx.source, tool_name, args)
                            .await
                    } else {
                        crate::tools::execute_tool_with_session_context(
                            Arc::clone(state),
                            ctx.source,
                            tool_name,
                            args,
                            pinned_ws,
                            None,
                            ctx.session_id,
                        )
                        .await
                    }
                };
                tokio::pin!(fut);
                let timeout_err = || crate::tools::ToolOutput {
                    text: format!(
                        "Anti-hang timeout ({rpc_timeout_secs}s): tool '{tool_name}' memakan waktu terlalu lama. \
                        Persempit parameter 'path' ke subfolder spesifik atau gunakan start_command_job + poll_command_job untuk build/test panjang."
                    ),
                    is_error: true,
                };
                let output =
                    match tokio::time::timeout(Duration::from_secs(rpc_timeout_secs), &mut fut)
                        .await
                    {
                        Ok(out) => out,
                        Err(_) if !state.pending_tool_approvals.read().await.is_empty() => {
                            match tokio::time::timeout(Duration::from_secs(70), &mut fut).await {
                                Ok(out) => out,
                                Err(_) => timeout_err(),
                            }
                        }
                        Err(_) => timeout_err(),
                    };
                json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "result": {
                        "content": [{ "type": "text", "text": output.text }],
                        "isError": output.is_error
                    }
                })
            }
        }
        "tasks/get" => {
            let task_id = match require_task_id(&params, &rpc_id) {
                Ok(id) => id,
                Err(resp) => return Some(resp),
            };
            match state.mcp_tasks.get_task(task_id, pinned_ws).await {
                Ok(info) => json!({ "jsonrpc": "2.0", "id": rpc_id, "result": info }),
                Err((code, message)) => json!({ "jsonrpc": "2.0", "id": rpc_id, "error": { "code": code, "message": message } }),
            }
        }
        "tasks/result" => {
            let task_id = match require_task_id(&params, &rpc_id) {
                Ok(id) => id,
                Err(resp) => return Some(resp),
            };
            let task_wait_secs = state
                .config
                .read()
                .await
                .effective_webmcp_rpc_timeout_secs()
                .saturating_sub(4)
                .clamp(15, 55);
            match state
                .mcp_tasks
                .get_task_result(task_id, pinned_ws, task_wait_secs)
                .await
            {
                Ok(res) => json!({ "jsonrpc": "2.0", "id": rpc_id, "result": res }),
                Err((code, message)) => json!({ "jsonrpc": "2.0", "id": rpc_id, "error": { "code": code, "message": message } }),
            }
        }
        "tasks/list" => {
            let cursor = params.get("cursor").and_then(|v| v.as_str());
            match state.mcp_tasks.list_tasks(cursor, pinned_ws).await {
                Ok(res) => json!({ "jsonrpc": "2.0", "id": rpc_id, "result": res }),
                Err((code, message)) => json!({ "jsonrpc": "2.0", "id": rpc_id, "error": { "code": code, "message": message } }),
            }
        }
        "tasks/cancel" => {
            let task_id = match require_task_id(&params, &rpc_id) {
                Ok(id) => id,
                Err(resp) => return Some(resp),
            };
            match state.mcp_tasks.cancel_task(state, task_id, pinned_ws).await {
                Ok(info) => json!({ "jsonrpc": "2.0", "id": rpc_id, "result": info }),
                Err((code, message)) => json!({ "jsonrpc": "2.0", "id": rpc_id, "error": { "code": code, "message": message } }),
            }
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {other}")
            }
        }),
    };
    Some(result_body)
}

#[cfg(test)]
mod init_instructions_tests {
    use super::*;

    #[tokio::test]
    async fn test_initialize_instructions_places_active_workspaces_near_top() {
        let base_dir = std::env::temp_dir().join("smart_mcp_init_instr_order_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();

        let state = AppState::new(base_dir.clone());
        let instr = build_mcp_initialize_instructions(
            &state,
            McpToolProfile::Compact,
            None,
            true,
            true,
        )
        .await;

        let lines: Vec<&str> = instr.lines().collect();
        assert!(lines.len() >= 2);
        assert!(
            lines[1].starts_with("Active workspace(s):"),
            "Expected Active workspace(s) on line 2, got:\n{instr}"
        );
        assert!(instr.chars().count() <= 1200);

        let _ = std::fs::remove_dir_all(&base_dir);
    }
}
