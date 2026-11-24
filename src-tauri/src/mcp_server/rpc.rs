use crate::state::AppState;
use crate::tools::{execute_tool_with_context, mcp_tools_list};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

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
    handle_single_rpc_with_context(state, msg, None).await
}

pub(crate) async fn handle_single_rpc_with_context(
    state: &Arc<AppState>,
    msg: &Value,
    pinned_ws: Option<&str>,
) -> Option<Value> {
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
                    }
                }
            })
        }
        "ping" => json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": {}
        }),
        "server/discover" => {
            let mut merged_tools = mcp_tools_list().as_array().cloned().unwrap_or_default();
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
                    "prompts": crate::mcp_resources::prompts_list(),
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
            let mut merged_tools = mcp_tools_list().as_array().cloned().unwrap_or_default();
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
                "prompts": crate::mcp_resources::prompts_list(),
                "_meta": { "cacheControl": { "type": "ephemeral" } }
            }
        }),
        "prompts/get" => {
            let pname = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let pargs = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match crate::mcp_resources::get_prompt(state, pname, &pargs).await {
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
                            "WebMCP".to_string(),
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
                let fut = async {
                    if tool_name.contains("__") {
                        state
                            .mcp_hub
                            .call_downstream_tool(state, "WebMCP", tool_name, args)
                            .await
                    } else {
                        execute_tool_with_context(state, "WebMCP", tool_name, args, pinned_ws).await
                    }
                };
                tokio::pin!(fut);
                let timeout_err = || crate::tools::ToolOutput {
                    text: format!(
                        "Anti-hang timeout (24s): tool '{tool_name}' memakan waktu terlalu lama. \
                        Persempit parameter 'path' ke subfolder spesifik atau gunakan start_command_job + poll_command_job untuk build/test panjang."
                    ),
                    is_error: true,
                };
                let output = match tokio::time::timeout(Duration::from_secs(24), &mut fut).await {
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
            match state.mcp_tasks.get_task_result(task_id, pinned_ws, 28).await {
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
