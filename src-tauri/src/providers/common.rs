use crate::agent_manager::{AgentRunStatus, RunContext};
use crate::local_agent::{TokenUsage, ToolStepTrace};
use crate::security::resolve_jailed_path;
use crate::state::{AppState, TurnCheckpointInfo};
use crate::tools::{
    check_sensitive_guard_for_workspace, enforce_run_mode_gate, execute_tool_with_run_context,
    is_tool_read_only, mcp_tools_list, ToolOutput,
};
use crate::workspace::resolve_workspace;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Emitter;
use tokio::sync::{watch, RwLock, Semaphore};
use tokio::task::JoinSet;

pub fn is_local_url(raw: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw.trim()) else {
        return false;
    };
    matches!(
        parsed.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]")
    )
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentAttachment {
    pub kind: String, // "file" | "image"
    pub name: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub data_base64: Option<String>,
}

pub struct ProviderTurnOutcome {
    pub reply: String,
    pub steps: Vec<ToolStepTrace>,
    pub usage: TokenUsage,
    pub cancelled: bool,
    pub awaiting_approval: bool,
    pub checkpoint: Option<TurnCheckpointInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogInterrupt {
    Cancelled,
    TurnBudgetTimeout,
    StreamIdleTimeout,
}

impl WatchdogInterrupt {
    pub fn reason_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::TurnBudgetTimeout => "turn_budget_timeout",
            Self::StreamIdleTimeout => "stream_idle_timeout",
        }
    }

    pub fn display_note(self, turn_budget_secs: u64, stream_idle_secs: u64) -> String {
        match self {
            Self::Cancelled => "Dibatalkan oleh user".to_string(),
            Self::TurnBudgetTimeout => format!(
                "Batas waktu giliran ({turn_budget_secs}s) tercapai — checkpoint tersimpan"
            ),
            Self::StreamIdleTimeout => format!(
                "Stream idle timeout ({stream_idle_secs}s tanpa data) — checkpoint tersimpan"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WatchdogSettings {
    pub max_steps: usize,
    pub turn_budget_secs: u64,
    pub stream_idle_secs: u64,
    pub ttfb_timeout_secs: u64,
}

pub async fn load_watchdog_settings(state: &AppState) -> WatchdogSettings {
    let cfg = state.config.read().await;
    WatchdogSettings {
        max_steps: cfg.agent_max_steps.clamp(4, 30),
        turn_budget_secs: cfg.agent_turn_timeout_secs.clamp(30, 600),
        stream_idle_secs: cfg.stream_idle_timeout_secs.clamp(10, 120),
        ttfb_timeout_secs: 45,
    }
}

pub async fn await_with_watchdog<F, T>(
    fut: F,
    cancel_rx: &mut watch::Receiver<bool>,
    op_timeout_secs: u64,
    timeout_kind: WatchdogInterrupt,
    turn_start: Instant,
    turn_budget_secs: u64,
) -> Result<T, WatchdogInterrupt>
where
    F: std::future::Future<Output = T>,
{
    if *cancel_rx.borrow() {
        return Err(WatchdogInterrupt::Cancelled);
    }
    let remaining_budget =
        Duration::from_secs(turn_budget_secs).saturating_sub(turn_start.elapsed());
    if remaining_budget.is_zero() {
        return Err(WatchdogInterrupt::TurnBudgetTimeout);
    }
    let op_dur = Duration::from_secs(op_timeout_secs.max(1));
    let (limit, kind) = if remaining_budget < op_dur {
        (remaining_budget, WatchdogInterrupt::TurnBudgetTimeout)
    } else {
        (op_dur, timeout_kind)
    };
    tokio::select! {
        res = cancel_rx.wait_for(|&c| c) => {
            match res {
                Ok(_) | Err(_) => Err(WatchdogInterrupt::Cancelled),
            }
        }
        _ = tokio::time::sleep(limit) => Err(kind),
        val = fut => Ok(val),
    }
}

pub(super) async fn wait_retry_backoff(
    wait_secs: u64,
    cancel_rx: &mut watch::Receiver<bool>,
    turn_start: Instant,
    turn_budget_secs: u64,
) -> Result<(), WatchdogInterrupt> {
    if await_with_watchdog(
        tokio::time::sleep(Duration::from_secs(wait_secs)),
        cancel_rx,
        wait_secs + 1,
        WatchdogInterrupt::TurnBudgetTimeout,
        turn_start,
        turn_budget_secs,
    )
    .await
    .is_err()
    {
        return Err(if *cancel_rx.borrow() {
            WatchdogInterrupt::Cancelled
        } else {
            WatchdogInterrupt::TurnBudgetTimeout
        });
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PendingToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone)]
pub struct ExecutedToolCall {
    pub id: String,
    pub name: String,
    pub output: ToolOutput,
    pub tokens_saved: usize,
}

pub async fn is_safe_parallel_call(state: &AppState, name: &str, args: &Value) -> bool {
    if name.contains("__") {
        let cfgs = state.resolved_downstream_mcps().await;
        state.mcp_hub.is_tool_read_only(&cfgs, name).await
    } else {
        is_tool_read_only(name, args)
    }
}

async fn latest_tokens_saved(state: &AppState, tool_name: &str) -> usize {
    let logs = state.audit_logs.read().await;
    logs.iter()
        .rev()
        .take(24)
        .find(|l| l.tool_name == tool_name)
        .map(|l| l.tokens_saved)
        .unwrap_or(0)
}

pub fn emit_stream_thought(
    app_handle: &Option<tauri::AppHandle>,
    run_ctx: &RunContext,
    delta: &str,
) {
    if delta.is_empty() {
        return;
    }
    if run_ctx.depth == 0 {
        if let Some(tx) = &run_ctx.acp_event_tx {
            let _ = tx.send(crate::acp_server::AcpStreamEvent::ThoughtChunk {
                session_id: run_ctx.session_id.clone(),
                text: delta.to_string(),
            });
        }
    }
    if let Some(app) = app_handle {
        let _ = app.emit(
            "agent-thought-v2",
            json!({
                "run_id": run_ctx.run_id,
                "session_id": run_ctx.session_id,
                "delta": delta
            }),
        );
        if run_ctx.depth == 0 {
            let _ = app.emit("agent-thought-chunk", delta);
        }
    }
}

pub fn emit_stream_chunk(
    app_handle: &Option<tauri::AppHandle>,
    run_ctx: &RunContext,
    delta: &str,
) {
    if delta.is_empty() {
        return;
    }
    if run_ctx.depth == 0 {
        if let Some(tx) = &run_ctx.acp_event_tx {
            let _ = tx.send(crate::acp_server::AcpStreamEvent::MessageChunk {
                session_id: run_ctx.session_id.clone(),
                text: delta.to_string(),
            });
        }
    }
    if let Some(app) = app_handle {
        let _ = app.emit(
            "agent-chunk-v2",
            json!({
                "run_id": run_ctx.run_id,
                "session_id": run_ctx.session_id,
                "delta": delta
            }),
        );
        if run_ctx.depth == 0 {
            let _ = app.emit("agent-text-chunk", delta);
        }
    }
}

pub async fn emit_stream_step(
    app_handle: &Option<tauri::AppHandle>,
    state: &AppState,
    run_ctx: &RunContext,
    step: usize,
    max_steps: usize,
    text: &str,
) {
    state
        .agent_manager
        .update_run_step(&run_ctx.run_id, step, max_steps, text.to_string())
        .await;
    if let Some(app) = app_handle {
        let _ = app.emit(
            "agent-step-v2",
            json!({
                "run_id": run_ctx.run_id,
                "session_id": run_ctx.session_id,
                "step": step,
                "max_steps": max_steps,
                "text": text
            }),
        );
        if run_ctx.depth == 0 {
            let _ = app.emit("agent-step", text);
        }
    }
}

fn acp_tool_kind(name: &str) -> &'static str {
    match name {
        "run_command_compressed" | "start_command_job" | "poll_command_job" | "stop_command_job" => {
            "execute"
        }
        "surgical_edit"
        | "multi_surgical_edit"
        | "patch_and_verify"
        | "write_file_jailed"
        | "file_ops" => "edit",
        "smart_search" | "find_files" => "search",
        "read_file_range"
        | "read_symbol"
        | "code_outline"
        | "batch_inspect"
        | "project_overview"
        | "lsp_diagnostics"
        | "lsp_definition"
        | "lsp_references"
        | "lsp_hover" => "read",
        _ => "other",
    }
}

fn maybe_emit_acp_plan(ctx: Option<&RunContext>, tool_name: &str, args: &Value, output: &ToolOutput) {
    let Some(c) = ctx else { return };
    if c.depth != 0 || output.is_error || tool_name != "upsert_artifact" {
        return;
    }
    let Some(tx) = &c.acp_event_tx else { return };
    let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(kind, "task_list" | "tasks" | "task") {
        return;
    }
    if let Some(md) = args.get("content_md").and_then(|v| v.as_str()) {
        let entries = crate::acp_server::extract_plan_entries_from_markdown(md);
        if !entries.is_empty() {
            let _ = tx.send(crate::acp_server::AcpStreamEvent::PlanUpdate {
                session_id: c.session_id.clone(),
                entries,
            });
        }
    }
}

/// Executes contiguous read-only tool calls in parallel (bounded concurrency = 6) while preserving
/// original call order, and runs mutative tool calls strictly sequentially.
pub async fn execute_tool_calls_hybrid(
    state: Arc<AppState>,
    source: &str,
    calls: Vec<PendingToolCall>,
    cancel_rx: &mut watch::Receiver<bool>,
    turn_start: Instant,
    turn_budget_secs: u64,
    app_handle: &Option<tauri::AppHandle>,
    run_ctx: Option<&RunContext>,
) -> Result<Vec<ExecutedToolCall>, WatchdogInterrupt> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }

    let mut tagged: Vec<(bool, PendingToolCall)> = Vec::with_capacity(calls.len());
    for call in calls {
        let ro = is_safe_parallel_call(&state, &call.name, &call.args).await;
        tagged.push((ro, call));
    }

    let mut segments: Vec<(bool, Vec<PendingToolCall>)> = Vec::new();
    for (is_ro, call) in tagged {
        if let Some((last_ro, vec)) = segments.last_mut() {
            if *last_ro == is_ro {
                vec.push(call);
                continue;
            }
        }
        segments.push((is_ro, vec![call]));
    }

    let mut all_results: Vec<ExecutedToolCall> = Vec::with_capacity(
        segments.iter().map(|(_, v)| v.len()).sum(),
    );

    for (is_ro, seg) in segments {
        if is_ro && seg.len() > 1 {
            if let Some(ctx) = run_ctx {
                let names: Vec<&str> = seg.iter().map(|c| c.name.as_str()).collect();
                emit_stream_step(
                    app_handle,
                    &state,
                    ctx,
                    0,
                    0,
                    &format!(
                        "Menjalankan {} tool read-only paralel: {}...",
                        seg.len(),
                        names.join(", ")
                    ),
                )
                .await;
            } else if let Some(app) = app_handle {
                let names: Vec<&str> = seg.iter().map(|c| c.name.as_str()).collect();
                let _ = app.emit(
                    "agent-step",
                    format!(
                        "Menjalankan {} tool read-only paralel: {}...",
                        seg.len(),
                        names.join(", ")
                    ),
                );
            }

            let sem = Arc::new(Semaphore::new(6));
            let mut join_set: JoinSet<(usize, String, String, Value, ToolOutput)> = JoinSet::new();
            for (idx, call) in seg.into_iter().enumerate() {
                if let Some(ctx) = run_ctx.filter(|c| c.depth == 0) {
                    if let Some(tx) = &ctx.acp_event_tx {
                        let _ = tx.send(crate::acp_server::AcpStreamEvent::ToolCallStart {
                            session_id: ctx.session_id.clone(),
                            call_id: call.id.clone(),
                            title: call.name.clone(),
                            kind: acp_tool_kind(&call.name).to_string(),
                            raw_input: call.args.clone(),
                        });
                    }
                }
                let st = state.clone();
                let sem_c = sem.clone();
                let src = source.to_string();
                let ctx_owned = run_ctx.cloned();
                join_set.spawn(async move {
                    let _permit = sem_c.acquire_owned().await;
                    let args_clone = call.args.clone();
                    let out =
                        execute_any_tool(st, &src, &call.name, call.args, ctx_owned.as_ref()).await;
                    (idx, call.id, call.name, args_clone, out)
                });
            }

            let mut seg_results: Vec<(usize, ExecutedToolCall)> = Vec::new();
            loop {
                let next_res = await_with_watchdog(
                    join_set.join_next(),
                    cancel_rx,
                    turn_budget_secs,
                    WatchdogInterrupt::TurnBudgetTimeout,
                    turn_start,
                    turn_budget_secs,
                )
                .await;

                match next_res {
                    Ok(Some(Ok((idx, id, name, args_val, output)))) => {
                        if let Some(ctx) = run_ctx.filter(|c| c.depth == 0) {
                            if let Some(tx) = &ctx.acp_event_tx {
                                let _ = tx.send(crate::acp_server::AcpStreamEvent::ToolCallUpdate {
                                    session_id: ctx.session_id.clone(),
                                    call_id: id.clone(),
                                    status: if output.is_error {
                                        "failed".to_string()
                                    } else {
                                        "completed".to_string()
                                    },
                                    text: output.text.chars().take(1500).collect(),
                                });
                            }
                            maybe_emit_acp_plan(Some(ctx), &name, &args_val, &output);
                        }
                        let tokens_saved = latest_tokens_saved(&state, &name).await;
                        seg_results.push((
                            idx,
                            ExecutedToolCall {
                                id,
                                name,
                                output,
                                tokens_saved,
                            },
                        ));
                    }
                    Ok(Some(Err(_))) => {}
                    Ok(None) => break,
                    Err(interrupt) => {
                        join_set.abort_all();
                        return Err(interrupt);
                    }
                }
            }
            seg_results.sort_by_key(|(idx, _)| *idx);
            all_results.extend(seg_results.into_iter().map(|(_, item)| item));
        } else {
            for call in seg {
                if let Some(ctx) = run_ctx {
                    emit_stream_step(
                        app_handle,
                        &state,
                        ctx,
                        0,
                        0,
                        &format!("Menjalankan tool: {}...", call.name),
                    )
                    .await;
                    if ctx.depth == 0 {
                        if let Some(tx) = &ctx.acp_event_tx {
                            let _ = tx.send(crate::acp_server::AcpStreamEvent::ToolCallStart {
                                session_id: ctx.session_id.clone(),
                                call_id: call.id.clone(),
                                title: call.name.clone(),
                                kind: acp_tool_kind(&call.name).to_string(),
                                raw_input: call.args.clone(),
                            });
                        }
                    }
                } else if let Some(app) = app_handle {
                    let _ = app.emit("agent-step", format!("Menjalankan tool: {}...", call.name));
                }
                let tname = call.name.clone();
                let args_clone = call.args.clone();
                let output = await_with_watchdog(
                    execute_any_tool(state.clone(), source, &tname, call.args, run_ctx),
                    cancel_rx,
                    turn_budget_secs,
                    WatchdogInterrupt::TurnBudgetTimeout,
                    turn_start,
                    turn_budget_secs,
                )
                .await?;
                if let Some(ctx) = run_ctx.filter(|c| c.depth == 0) {
                    if let Some(tx) = &ctx.acp_event_tx {
                        let _ = tx.send(crate::acp_server::AcpStreamEvent::ToolCallUpdate {
                            session_id: ctx.session_id.clone(),
                            call_id: call.id.clone(),
                            status: if output.is_error {
                                "failed".to_string()
                            } else {
                                "completed".to_string()
                            },
                            text: output.text.chars().take(1500).collect(),
                        });
                    }
                    maybe_emit_acp_plan(Some(ctx), &tname, &args_clone, &output);
                }
                let tokens_saved = latest_tokens_saved(&state, &tname).await;
                all_results.push(ExecutedToolCall {
                    id: call.id,
                    name: tname,
                    output,
                    tokens_saved,
                });
            }
        }
    }

    Ok(all_results)
}

pub fn format_interrupted_reply(
    partial_text: &str,
    interrupt: WatchdogInterrupt,
    turn_budget_secs: u64,
    stream_idle_secs: u64,
) -> String {
    let note = interrupt.display_note(turn_budget_secs, stream_idle_secs);
    if partial_text.trim().is_empty() {
        format!("({note})")
    } else {
        format!("{}\n\n[{note}]", partial_text.trim_end())
    }
}

pub(super) fn truncate_str_chars(s: &str, max_chars: usize) -> String {
    let mut count = 0usize;
    let mut end_byte = s.len();
    for (idx, _) in s.char_indices() {
        if count == max_chars {
            end_byte = idx;
            break;
        }
        count += 1;
    }
    if count >= max_chars && end_byte < s.len() {
        format!("{}\n...[truncated]", &s[..end_byte])
    } else {
        s.to_string()
    }
}

fn user_home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
}

fn collect_rules_from_dir(
    dir: &Path,
    label_prefix: &str,
    candidates: &[&str],
    budget: &mut usize,
    seen_contents: &mut std::collections::HashSet<String>,
    chunks: &mut Vec<String>,
) {
    for rel in candidates {
        if *budget == 0 {
            break;
        }
        let path = dir.join(rel);
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() || !seen_contents.insert(trimmed.to_string()) {
            continue;
        }
        let clipped = truncate_str_chars(trimmed, *budget);
        let used = clipped.chars().count();
        chunks.push(format!("### {label_prefix} ({rel})\n{clipped}"));
        *budget = budget.saturating_sub(used);
    }
}

pub fn load_project_rules_snippet(root: &Path) -> Option<String> {
    const CANDIDATES: [&str; 5] = [
        "AGENTS.md",
        "CLAUDE.md",
        "GEMINI.md",
        ".cursorrules",
        ".github/copilot-instructions.md",
    ];
    let mut chunks = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut remaining = 6000usize;
    collect_rules_from_dir(
        root,
        "Project Rules",
        &CANDIDATES,
        &mut remaining,
        &mut seen,
        &mut chunks,
    );

    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n\n"))
    }
}

pub fn load_global_rules_snippet_with_dirs(
    hub_data_dir: Option<&Path>,
    home_dir: Option<&Path>,
) -> Option<String> {
    const HUB_CANDIDATES: [&str; 3] = ["GEMINI.md", "AGENTS.md", "CLAUDE.md"];
    const HOME_CANDIDATES: [&str; 5] = [
        ".gemini/GEMINI.md",
        "GEMINI.md",
        "CLAUDE.md",
        ".claude/CLAUDE.md",
        "AGENTS.md",
    ];
    let mut chunks = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut remaining = 3000usize;

    if let Some(hub_dir) = hub_data_dir {
        collect_rules_from_dir(
            hub_dir,
            "Global Airlock Rules",
            &HUB_CANDIDATES,
            &mut remaining,
            &mut seen,
            &mut chunks,
        );
    }
    if let Some(home) = home_dir {
        collect_rules_from_dir(
            home,
            "User Global Rules",
            &HOME_CANDIDATES,
            &mut remaining,
            &mut seen,
            &mut chunks,
        );
    }

    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n\n"))
    }
}

pub fn load_combined_rules_snippet(hub_data_dir: Option<&Path>, root: &Path) -> Option<String> {
    let mut parts = Vec::new();
    if hub_data_dir.is_some() {
        let home = user_home_dir();
        if let Some(global_rules) =
            load_global_rules_snippet_with_dirs(hub_data_dir, home.as_deref())
        {
            parts.push(global_rules);
        }
    }
    if let Some(project_rules) = load_project_rules_snippet(root) {
        parts.push(project_rules);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

pub fn sanitize_schema_for_gemini(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "$schema"
                        | "additionalProperties"
                        | "$defs"
                        | "definitions"
                        | "default"
                        | "title"
                        | "$id"
                        | "examples"
                ) {
                    continue;
                }
                if k == "type" {
                    if let Some(arr) = v.as_array() {
                        if let Some(first_non_null) = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .find(|&s| s != "null")
                        {
                            out.insert(k.clone(), Value::String(first_non_null.to_string()));
                            continue;
                        }
                    }
                }
                out.insert(k.clone(), sanitize_schema_for_gemini(v));
            }
            if !out.contains_key("type") && out.contains_key("properties") {
                out.insert("type".to_string(), Value::String("object".to_string()));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_schema_for_gemini).collect()),
        other => other.clone(),
    }
}

pub async fn all_mcp_tools(state: &AppState) -> Vec<Value> {
    let mut tools: Vec<Value> = mcp_tools_list()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.get("name").and_then(|v| v.as_str()) != Some("delegate_local_agent"))
        .collect();
    let cfgs = state.resolved_downstream_mcps().await;
    let aggregated = state.mcp_hub.list_aggregated_tools(&cfgs).await;
    tools.extend(aggregated);
    tools
}

pub async fn execute_any_tool(
    state: Arc<AppState>,
    source: &str,
    name: &str,
    args: Value,
    run_ctx: Option<&RunContext>,
) -> ToolOutput {
    if name.contains("__") {
        let cfgs = state.resolved_downstream_mcps().await;
        let is_ro = state.mcp_hub.is_tool_read_only(&cfgs, name).await;
        if let Err(err) = enforce_run_mode_gate(&state, run_ctx, name, is_ro).await {
            return ToolOutput {
                text: err,
                is_error: true,
            };
        }
        state
            .mcp_hub
            .call_downstream_tool(&state, source, name, args)
            .await
    } else {
        execute_tool_with_run_context(state, source, name, args, None, run_ctx).await
    }
}

fn split_workspace_attachment_ref(att: &AgentAttachment) -> (Option<String>, String) {
    let raw = att.path.as_deref().unwrap_or(&att.name).trim();
    if let Some(explicit_ws) = att.workspace.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return (Some(explicit_ws.to_string()), raw.to_string());
    }
    if let Some(after_at) = raw.strip_prefix('@') {
        if let Some((alias, rest)) = after_at.split_once(':') {
            if !alias.is_empty() && !alias.contains('/') && !alias.contains('\\') {
                return (Some(alias.to_string()), rest.trim().to_string());
            }
        }
    }
    (None, raw.to_string())
}

pub async fn build_attachment_text_context(
    state: &AppState,
    attachments: &[AgentAttachment],
) -> Result<String, String> {
    let mut sections = Vec::new();

    for att in attachments {
        if att.kind != "file" {
            continue;
        }
        let (ws_req, rel) = split_workspace_attachment_ref(att);
        if rel.is_empty() {
            continue;
        }
        let ws = resolve_workspace(state, ws_req.as_deref(), None).await?;
        let target = resolve_jailed_path(&ws.canon_root, &rel)?;
        check_sensitive_guard_for_workspace(state, &ws, &target, &rel).await?;
        let bytes = std::fs::read(&target)
            .map_err(|e| format!("Gagal membaca lampiran '{rel}': {e}"))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("Lampiran '{rel}' bukan file teks UTF-8 yang valid."))?;
        let clipped = truncate_str_chars(&text, 12_000);
        let label = if ws_req.is_some() {
            format!("@{}:{}", ws.alias, rel)
        } else {
            rel
        };
        sections.push(format!("### Attached File: {label}\n```\n{clipped}\n```"));
    }

    Ok(sections.join("\n\n"))
}

pub(super) struct TurnFinalizeParams<'a> {
    pub state: &'a AppState,
    pub provider_key: &'static str,
    pub legacy_history: &'a RwLock<Vec<Value>>,
    pub run_ctx: &'a RunContext,
    pub clean_history: Vec<Value>,
    pub steps: Vec<ToolStepTrace>,
    pub total_usage: TokenUsage,
    pub last_streamed_text: &'a str,
    pub final_reply: String,
    pub interrupt_opt: Option<WatchdogInterrupt>,
    pub hit_max_steps: bool,
    pub awaiting_approval: bool,
    pub turn_budget_secs: u64,
    pub stream_idle_secs: u64,
}

pub(super) async fn finalize_provider_turn(params: TurnFinalizeParams<'_>) -> ProviderTurnOutcome {
    let TurnFinalizeParams {
        state,
        provider_key,
        legacy_history,
        run_ctx,
        clean_history,
        steps,
        total_usage,
        last_streamed_text,
        mut final_reply,
        interrupt_opt,
        hit_max_steps,
        awaiting_approval,
        turn_budget_secs,
        stream_idle_secs,
    } = params;

    let checkpoint_opt = if run_ctx.depth > 0 || run_ctx.execution_mode.starts_with("delegated_") {
        None
    } else {
        state
            .agent_manager
            .set_history(provider_key, &run_ctx.session_id, clean_history.clone())
            .await;
        if run_ctx.session_id == "default" {
            *legacy_history.write().await = clean_history;
        }
        if let Some(kind) = interrupt_opt {
            let cp = TurnCheckpointInfo {
                can_resume: true,
                reason: kind.reason_str().to_string(),
                completed_steps: steps.len(),
                last_tool: steps.last().map(|s| s.tool_name.clone()),
            };
            state
                .agent_manager
                .set_checkpoint(&run_ctx.session_id, Some(cp.clone()))
                .await;
            *state.pending_checkpoint.write().await = Some(cp.clone());
            final_reply = format_interrupted_reply(
                last_streamed_text,
                kind,
                turn_budget_secs,
                stream_idle_secs,
            );
            Some(cp)
        } else if hit_max_steps {
            let cp = TurnCheckpointInfo {
                can_resume: true,
                reason: "max_steps".to_string(),
                completed_steps: steps.len(),
                last_tool: steps.last().map(|s| s.tool_name.clone()),
            };
            state
                .agent_manager
                .set_checkpoint(&run_ctx.session_id, Some(cp.clone()))
                .await;
            *state.pending_checkpoint.write().await = Some(cp.clone());
            Some(cp)
        } else {
            state
                .agent_manager
                .set_checkpoint(&run_ctx.session_id, None)
                .await;
            *state.pending_checkpoint.write().await = None;
            None
        }
    };

    if awaiting_approval {
        state
            .agent_manager
            .finish_run(
                &run_ctx.run_id,
                AgentRunStatus::AwaitingApproval,
                "Awaiting plan approval".to_string(),
            )
            .await;
    }

    ProviderTurnOutcome {
        reply: final_reply,
        steps,
        usage: total_usage,
        cancelled: interrupt_opt == Some(WatchdogInterrupt::Cancelled),
        awaiting_approval,
        checkpoint: checkpoint_opt,
    }
}
