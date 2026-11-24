use crate::agent_manager::{AgentRunStatus, RunContext, MAX_SUBAGENTS_PER_RUN};
use crate::providers::{
    self, await_with_watchdog, emit_stream_chunk, emit_stream_step, emit_stream_thought,
    execute_tool_calls_hybrid, format_interrupted_reply, load_watchdog_settings, AgentAttachment,
    PendingToolCall, ProviderTurnOutcome, WatchdogInterrupt,
};
use crate::state::{AppState, DashboardSnapshot, TurnCheckpointInfo};
use crate::workspace::{build_workspace_summaries, clean_path_string, resolve_workspace};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Emitter;
use tokio::sync::watch;

#[derive(Debug, Clone, Serialize)]
pub struct ToolStepTrace {
    pub tool_name: String,
    pub summary: String,
    pub tokens_saved: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiModelInfo {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub input_token_limit: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentTurnResponse {
    pub run_id: String,
    pub reply: String,
    pub steps: Vec<ToolStepTrace>,
    pub usage: TokenUsage,
    pub cancelled: bool,
    pub awaiting_approval: bool,
    pub checkpoint: Option<TurnCheckpointInfo>,
    pub snapshot: DashboardSnapshot,
}

#[derive(Debug, Default)]
pub struct StreamStepAccumulator {
    pub parts: Vec<Value>,
    pub text_buf: String,
    pub thought_buf: String,
    pub function_calls: Vec<Value>,
    pub usage: TokenUsage,
}

fn is_plain_text_part(part: &Value) -> bool {
    let Some(obj) = part.as_object() else {
        return false;
    };
    obj.get("text").and_then(|v| v.as_str()).is_some()
        && !obj.contains_key("functionCall")
        && !obj.contains_key("thoughtSignature")
        && obj.keys().all(|k| k == "text" || k == "thought")
}

fn append_part_preserving_signatures(parts: &mut Vec<Value>, part: &Value) {
    if is_plain_text_part(part) {
        let is_thought = part.get("thought").and_then(|v| v.as_bool()) == Some(true);
        let new_txt = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(last) = parts.last_mut() {
            if is_plain_text_part(last)
                && (last.get("thought").and_then(|v| v.as_bool()) == Some(true)) == is_thought
            {
                if let Some(existing) = last.get("text").and_then(|v| v.as_str()) {
                    let combined = format!("{existing}{new_txt}");
                    last["text"] = Value::String(combined);
                    return;
                }
            }
        }
    }
    parts.push(part.clone());
}

/// Parses a single SSE `data:` JSON payload into incremental `(text_delta, thought_delta)`
/// while updating `acc.parts`, `acc.function_calls`, and `acc.usage`.
pub fn process_sse_data_line(
    acc: &mut StreamStepAccumulator,
    data_json: &str,
) -> Result<(String, String), String> {
    let trimmed = data_json.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok((String::new(), String::new()));
    }

    let val: Value =
        serde_json::from_str(trimmed).map_err(|e| format!("SSE JSON parse error: {e}"))?;

    if let Some(um) = val.get("usageMetadata") {
        if let Some(v) = um["promptTokenCount"].as_u64() {
            if v > 0 {
                acc.usage.prompt_tokens = v;
            }
        }
        if let Some(v) = um["candidatesTokenCount"].as_u64() {
            if v > 0 {
                acc.usage.output_tokens = v;
            }
        }
        if let Some(v) = um["cachedContentTokenCount"].as_u64() {
            if v > 0 {
                acc.usage.cached_tokens = v;
            }
        }
        if let Some(v) = um["totalTokenCount"].as_u64() {
            if v > 0 {
                acc.usage.total_tokens = v;
            }
        }
    }

    let mut text_delta = String::new();
    let mut thought_delta = String::new();

    if let Some(parts) = val["candidates"][0]["content"]["parts"].as_array() {
        for part in parts {
            if let Some(fc) = part.get("functionCall") {
                acc.parts.push(part.clone());
                acc.function_calls.push(fc.clone());
            } else if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
                if let Some(txt) = part.get("text").and_then(|v| v.as_str()) {
                    acc.thought_buf.push_str(txt);
                    thought_delta.push_str(txt);
                }
                append_part_preserving_signatures(&mut acc.parts, part);
            } else if let Some(txt) = part.get("text").and_then(|v| v.as_str()) {
                acc.text_buf.push_str(txt);
                text_delta.push_str(txt);
                append_part_preserving_signatures(&mut acc.parts, part);
            } else {
                acc.parts.push(part.clone());
            }
        }
    }

    Ok((text_delta, thought_delta))
}

fn is_clean_user_turn(entry: &Value) -> bool {
    if entry.get("role").and_then(|v| v.as_str()) != Some("user") {
        return false;
    }
    let Some(parts) = entry.get("parts").and_then(|v| v.as_array()) else {
        return false;
    };
    !parts.iter().any(|p| p.get("functionResponse").is_some())
}

/// Removes any trailing `model` turn that ends with unresolved `functionCall` parts
/// so Gemini history never contains a dangling functionCall without a paired functionResponse.
pub fn ensure_clean_gemini_history(history: &mut Vec<Value>) {
    while let Some(last) = history.last() {
        let is_model = last.get("role").and_then(|v| v.as_str()) == Some("model");
        let has_fc = last
            .get("parts")
            .and_then(|v| v.as_array())
            .map(|parts| parts.iter().any(|p| p.get("functionCall").is_some()))
            .unwrap_or(false);
        if is_model && has_fc {
            history.pop();
        } else {
            break;
        }
    }
}

pub fn build_gemini_thinking_config(model: &str, mode: &str) -> Option<Value> {
    let m = model.trim().to_ascii_lowercase();
    let md = mode.trim().to_ascii_lowercase();

    if m.starts_with("gemini-2.5") {
        let is_pro = m.contains("pro");
        let budget: i64 = match md.as_str() {
            "off" => {
                if is_pro {
                    128
                } else {
                    0
                }
            }
            "low" => 1024,
            "medium" => 4096,
            "high" => 12288,
            _ => -1,
        };
        return Some(json!({
            "thinkingBudget": budget,
            "includeThoughts": budget != 0
        }));
    }

    if m.starts_with("gemini-3") {
        let is_pro = m.contains("pro");
        return match md.as_str() {
            "low" | "medium" | "high" => Some(json!({
                "thinkingLevel": md,
                "includeThoughts": true
            })),
            "off" => {
                if is_pro {
                    None
                } else {
                    Some(json!({
                        "thinkingLevel": "minimal",
                        "includeThoughts": true
                    }))
                }
            }
            _ => Some(json!({
                "includeThoughts": true
            })),
        };
    }

    None
}

/// Trims conversation history to at most `max_entries` while always cutting on a
/// clean user text turn boundary so `functionCall` / `functionResponse` pairs stay intact.
pub fn prune_gemini_history(history: &mut Vec<Value>, max_entries: usize) {
    if history.len() <= max_entries || max_entries == 0 {
        return;
    }
    let min_cut = history.len() - max_entries;

    if let Some(idx) = (min_cut..history.len()).find(|&i| is_clean_user_turn(&history[i])) {
        history.drain(0..idx);
        return;
    }

    if let Some(idx) = (1..min_cut).rev().find(|&i| is_clean_user_turn(&history[i])) {
        history.drain(0..idx);
    }
}

pub async fn fetch_gemini_models(
    client: &reqwest::Client,
    api_key: String,
) -> Result<Vec<GeminiModelInfo>, String> {
    if api_key.trim().is_empty() {
        return Err("Google AI Studio API Key belum diisi.".to_string());
    }

    let resp = client
        .get("https://generativelanguage.googleapis.com/v1beta/models?pageSize=100")
        .header("x-goog-api-key", api_key.trim())
        .send()
        .await
        .map_err(|e| format!("Gagal menghubungi endpoint daftar model Gemini: {e}"))?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Gagal mem-parsing daftar model Gemini: {e}"))?;

    if !status.is_success() {
        let msg = body["error"]["message"]
            .as_str()
            .unwrap_or("Unknown Gemini API error");
        return Err(format!("Gemini API error ({status}): {msg}"));
    }

    let mut out = Vec::new();
    if let Some(models) = body["models"].as_array() {
        for m in models {
            let name = m["name"].as_str().unwrap_or("");
            let methods = m["supportedGenerationMethods"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let supports_generate = methods
                .iter()
                .any(|x| x.as_str() == Some("generateContent"));
            if !supports_generate {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            if !lower.contains("gemini") && !lower.contains("gemma") {
                continue;
            }
            let id = name.strip_prefix("models/").unwrap_or(name).to_string();
            let display_name = m["displayName"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| id.clone());
            let description = m["description"].as_str().unwrap_or("").to_string();
            let input_token_limit = m["inputTokenLimit"].as_u64().unwrap_or(0);
            out.push(GeminiModelInfo {
                id,
                display_name,
                description,
                input_token_limit,
            });
        }
    }

    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

fn process_sse_raw_line(
    step_acc: &mut StreamStepAccumulator,
    raw_line: &[u8],
    app_handle: &Option<tauri::AppHandle>,
    run_ctx: &RunContext,
) {
    let line_str = String::from_utf8_lossy(raw_line);
    let trimmed = line_str.trim_end_matches(&['\r', '\n'][..]);
    if let Some(data_payload) = trimmed.strip_prefix("data:") {
        let data_trimmed = data_payload.trim();
        if !data_trimmed.is_empty() && data_trimmed != "[DONE]" {
            if let Ok((text_delta, thought_delta)) = process_sse_data_line(step_acc, data_trimmed) {
                emit_stream_thought(app_handle, run_ctx, &thought_delta);
                emit_stream_chunk(app_handle, run_ctx, &text_delta);
            }
        }
    }
}

pub fn filter_subagent_read_only_tools(tools: Vec<Value>) -> Vec<Value> {
    tools
        .into_iter()
        .filter(|t| {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            name != "spawn_subagent"
                && matches!(
                    name,
                    "list_workspaces"
                        | "project_overview"
                        | "smart_search"
                        | "read_file_range"
                        | "batch_inspect"
                        | "find_files"
                        | "code_outline"
                        | "read_symbol"
                        | "lsp_diagnostics"
                        | "lsp_definition"
                        | "lsp_references"
                        | "lsp_hover"
                        | "list_skills"
                        | "load_skill"
                        | "read_skill_resource"
                )
        })
        .collect()
}

pub async fn emit_runs_updated(app_handle: &Option<tauri::AppHandle>, state: &AppState) {
    if let Some(app) = app_handle {
        let runs = state.agent_manager.list_runs().await;
        let _ = app.emit("agent-runs-updated", &runs);
    }
}

async fn build_agent_system_prompt(
    state: &AppState,
    root_path: &Path,
    run_ctx: &RunContext,
) -> String {
    if run_ctx.depth > 0 {
        return format!(
            "You are a focused READ-ONLY Airlock Research Subagent inside jailed workspace @{} ({}). \
            Do NOT attempt file edits or mutative commands; mutative tools are disabled. \
            Investigate thoroughly using find_files, smart_search, code_outline, read_file_range, and batch_inspect, \
            then return a concise structured report citing exact file paths and line numbers.",
            run_ctx.workspace_alias, run_ctx.workspace_clean_path
        );
    }

    let workspace_root = clean_path_string(root_path);
    let (auto_inject, raw_workspaces, cfg_def_id) = {
        let cfg = state.config.read().await;
        (
            cfg.auto_inject_project_rules,
            cfg.workspaces.clone(),
            cfg.default_workspace_id.clone(),
        )
    };
    let state_def_id = state.default_workspace_id.read().await.clone();
    let def_id = if !state_def_id.trim().is_empty() {
        state_def_id
    } else {
        cfg_def_id.unwrap_or_default()
    };
    let global_guard = state.guardrail_mode.read().await.clone();
    let mut enabled_ws: Vec<_> = build_workspace_summaries(&raw_workspaces, &def_id, &global_guard)
        .into_iter()
        .filter(|w| w.enabled)
        .collect();
    enabled_ws.sort_by(|a, b| a.alias.cmp(&b.alias));

    // Keep static instruction prefix identical across turns to maximize Gemini Implicit Prompt Caching.
    let mut system_prompt = String::from(
        "You are Airlock Local Coding Agent operating inside jailed workspace(s). \
        Minimize tokens and round-trips: use batch_inspect, project_overview, find_files, smart_search, or code_outline first, \
        read_file_range for slices, surgical_edit or multi_surgical_edit for edits, and start_command_job + poll_command_job for slow builds/tests. \
        Respond concisely in the user's language.",
    );

    if enabled_ws.len() > 1 {
        let mut ws_lines = Vec::with_capacity(enabled_ws.len());
        let mut other_rules = Vec::new();
        for w in &enabled_ws {
            let def_tag = if w.is_default { "default, " } else { "" };
            ws_lines.push(format!(
                "- @{} ({def_tag}guardrail={}) -> {}",
                w.alias, w.effective_guardrail, w.path
            ));
            if !w.is_default {
                let ws_path = Path::new(&w.path);
                let has_rules = ["AGENTS.md", "CLAUDE.md", "GEMINI.md"]
                    .iter()
                    .any(|f| ws_path.join(f).is_file());
                if has_rules {
                    other_rules.push(format!("@{}", w.alias));
                }
            }
        }
        system_prompt.push_str(&format!(
            "\n\nActive Default Workspace: {}\nMounted Workspaces ({}):\n{}\nPass workspace: \"<alias>\" to operate on any non-default mounted workspace (or read_file_range batch_paths across workspaces).",
            workspace_root,
            enabled_ws.len(),
            ws_lines.join("\n")
        ));
        if !other_rules.is_empty() {
            system_prompt.push_str(&format!(
                "\nNote: Non-default workspaces with AGENTS.md/CLAUDE.md/GEMINI.md: {} (read selectively if needed).",
                other_rules.join(", ")
            ));
        }
    } else {
        system_prompt.push_str(&format!("\nActive Jailed Workspace: {}", workspace_root));
    }

    if auto_inject {
        if let Some(rules) = providers::load_project_rules_snippet(root_path) {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&rules);
        }
    }

    let discovered_skills = state.refresh_skills().await;
    if let Some(skills_block) = crate::skills::build_skills_prompt_section(&discovered_skills) {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&skills_block);
    }

    system_prompt.push_str(
        "\n\nSession Artifacts & Subagents:\n\
        - Use `upsert_artifact` to maintain structured session documents (`implementation_plan`, `task_list`, `walkthrough`, `review_note`) for non-trivial changes or when reviewing.\n\
        - Use `spawn_subagent` (max 3 per turn, read-only) when parallel investigation or independent read-only verification helps avoid cluttering the main context.",
    );

    if run_ctx.execution_mode == "plan_first" {
        let plan_approved = state
            .artifact_store
            .is_plan_approved(&run_ctx.workspace_clean_path, &run_ctx.session_id)
            .await;
        if plan_approved {
            system_prompt.push_str(
                "\n\n[EXECUTION MODE: PLAN APPROVED]\n\
                The user has approved your `implementation_plan` for this session. Execute the plan, keep `task_list` updated via `upsert_artifact`, verify changes, and finish with a concise `walkthrough` artifact.",
            );
        } else {
            system_prompt.push_str(
                "\n\n[EXECUTION MODE: PLAN FIRST — APPROVAL REQUIRED]\n\
                Mutative tools and shell edits are currently BLOCKED until the user approves your plan.\n\
                1. Inspect the codebase using read-only tools (and optionally `spawn_subagent`).\n\
                2. Save your plan via `upsert_artifact(kind=\"implementation_plan\", ...)` (and optionally `task_list`).\n\
                3. Call `request_plan_approval(summary=\"...\")` to pause execution cleanly for user review.",
            );
        }
    }

    system_prompt
}

#[allow(dead_code)]
pub async fn run_agent_turn(
    state: Arc<AppState>,
    prompt: String,
    attachments: Vec<AgentAttachment>,
) -> Result<AgentTurnResponse, String> {
    run_agent_turn_with_options(state, prompt, attachments, None, None, None).await
}

pub async fn run_agent_turn_with_options(
    state: Arc<AppState>,
    prompt: String,
    attachments: Vec<AgentAttachment>,
    session_id: Option<String>,
    execution_mode: Option<String>,
    workspace_alias: Option<String>,
) -> Result<AgentTurnResponse, String> {
    execute_agent_turn_or_resume(
        state,
        prompt,
        attachments,
        false,
        session_id,
        execution_mode,
        workspace_alias,
    )
    .await
}

#[allow(dead_code)]
pub async fn resume_agent_turn(state: Arc<AppState>) -> Result<AgentTurnResponse, String> {
    resume_agent_turn_for_session(state, None).await
}

pub async fn resume_agent_turn_for_session(
    state: Arc<AppState>,
    session_id: Option<String>,
) -> Result<AgentTurnResponse, String> {
    let clean_session =
        crate::artifacts::ArtifactStore::sanitize_session_id(session_id.as_deref().unwrap_or("default"));
    let can_resume = if let Some(cp) = state.agent_manager.get_checkpoint(&clean_session).await {
        cp.can_resume
    } else if clean_session == "default" {
        state
            .pending_checkpoint
            .read()
            .await
            .as_ref()
            .map(|c| c.can_resume)
            .unwrap_or(false)
    } else {
        false
    };
    if !can_resume {
        return Err("Tidak ada checkpoint giliran yang dapat dilanjutkan.".to_string());
    }
    execute_agent_turn_or_resume(
        state,
        String::new(),
        Vec::new(),
        true,
        Some(clean_session),
        None,
        None,
    )
    .await
}

pub fn run_subagent_turn<'a>(
    state: Arc<AppState>,
    parent_ctx: &'a RunContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let goal = args
            .get("goal")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let workspace_opt = args
            .get("workspace")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let focus_paths: Vec<String> = args
            .get("focus_paths")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let max_steps_opt = args
            .get("max_steps")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);

        if parent_ctx.depth > 0 {
            return Err("Subagent tidak diizinkan memanggil spawn_subagent (anti-rekursi).".to_string());
        }
        loop {
            let cur = parent_ctx.subagent_counter.load(Ordering::SeqCst);
            if cur >= MAX_SUBAGENTS_PER_RUN {
                return Err(format!(
                    "Batas maksimum {MAX_SUBAGENTS_PER_RUN} subagent dalam satu turn telah tercapai."
                ));
            }
            if parent_ctx
                .subagent_counter
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }

        let clean_goal = goal.trim();
        if clean_goal.is_empty() {
            return Err("Parameter 'goal' wajib diisi untuk spawn_subagent.".to_string());
        }

        let ws_ref = workspace_opt
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&parent_ctx.workspace_alias);
        let target_ws = resolve_workspace(&state, Some(ws_ref), None).await?;
        let (provider, model) = {
            let cfg = state.config.read().await;
            let gem_model = state.gemini_config.read().await.model.clone();
            let active_model = match cfg.agent_provider.trim() {
                "openai_compat" => cfg.openai_model.clone(),
                "anthropic" => cfg.anthropic_model.clone(),
                _ => gem_model,
            };
            (cfg.agent_provider.clone(), active_model)
        };
        let eff_steps = max_steps_opt.unwrap_or(6).clamp(1, 10);
        let (sub_run_id, cancel_rx, sub_counter) = state
            .agent_manager
            .start_run(
                Some(parent_ctx.run_id.clone()),
                parent_ctx.session_id.clone(),
                target_ws.alias.clone(),
                format!("Subagent: {}", clean_goal),
                provider.clone(),
                model,
                "read_only_subagent".to_string(),
                eff_steps,
            )
            .await?;

        let app_handle = state.app_handle.read().await.clone();
        emit_runs_updated(&app_handle, &state).await;

        let sub_ctx = RunContext {
            run_id: sub_run_id.clone(),
            parent_run_id: Some(parent_ctx.run_id.clone()),
            session_id: format!("sub_{sub_run_id}"),
            workspace_id: target_ws.id.clone(),
            workspace_alias: target_ws.alias.clone(),
            workspace_clean_path: target_ws.clean_path.clone(),
            execution_mode: "read_only_subagent".to_string(),
            depth: parent_ctx.depth + 1,
            turn_group_id: parent_ctx.turn_group_id.clone(),
            subagent_counter: sub_counter,
            acp_event_tx: parent_ctx.acp_event_tx.clone(),
        };

        let focus_note = if focus_paths.is_empty() {
            String::new()
        } else {
            format!("\nFocus paths to inspect first: {}", focus_paths.join(", "))
        };
        let sub_prompt = format!(
            "Workspace: @{} ({})\nGoal: {}{}\n\nProvide a concise, grounded report citing exact file paths and line numbers.",
            target_ws.alias, target_ws.clean_path, clean_goal, focus_note
        );

        let timed_res = tokio::time::timeout(
            Duration::from_secs(60),
            run_agent_turn_inner(
                state.clone(),
                sub_prompt,
                Vec::new(),
                cancel_rx,
                false,
                &target_ws.canon_root,
                sub_ctx,
            ),
        )
        .await;

        match timed_res {
            Ok(Ok(outcome)) => {
                let status = if outcome.cancelled {
                    AgentRunStatus::Cancelled
                } else {
                    AgentRunStatus::Completed
                };
                state
                    .agent_manager
                    .finish_run(&sub_run_id, status, "Subagent finished".to_string())
                    .await;
                emit_runs_updated(&app_handle, &state).await;
                let trimmed = outcome.reply.trim();
                if trimmed.chars().count() > 6000 {
                    let clipped: String = trimmed.chars().take(6000).collect();
                    Ok(format!("{clipped}\n...(truncated)"))
                } else {
                    Ok(trimmed.to_string())
                }
            }
            Ok(Err(err)) => {
                state
                    .agent_manager
                    .finish_run(&sub_run_id, AgentRunStatus::Failed, err.clone())
                    .await;
                emit_runs_updated(&app_handle, &state).await;
                Err(format!("Subagent gagal: {err}"))
            }
            Err(_) => {
                state.agent_manager.cancel_run(Some(&sub_run_id)).await;
                state
                    .agent_manager
                    .finish_run(
                        &sub_run_id,
                        AgentRunStatus::Cancelled,
                        "Subagent timeout 60s".to_string(),
                    )
                    .await;
                emit_runs_updated(&app_handle, &state).await;
                Err("Subagent melebihi batas waktu 60 detik.".to_string())
            }
        }
    })
}

async fn execute_agent_turn_or_resume(
    state: Arc<AppState>,
    prompt: String,
    attachments: Vec<AgentAttachment>,
    is_resume: bool,
    session_id: Option<String>,
    execution_mode: Option<String>,
    workspace_alias: Option<String>,
) -> Result<AgentTurnResponse, String> {
    let clean_session =
        crate::artifacts::ArtifactStore::sanitize_session_id(session_id.as_deref().unwrap_or("default"));
    let norm_mode = match execution_mode
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("plan_first") | Some("plan") => "plan_first".to_string(),
        _ => "direct".to_string(),
    };
    let target_ws = resolve_workspace(&state, workspace_alias.as_deref(), None).await?;

    if !is_resume {
        state
            .agent_manager
            .set_checkpoint(&clean_session, None)
            .await;
        if clean_session == "default" {
            *state.pending_checkpoint.write().await = None;
        }
    }

    let (provider, model) = {
        let cfg = state.config.read().await;
        let gem_model = state.gemini_config.read().await.model.clone();
        let active_model = match cfg.agent_provider.trim() {
            "openai_compat" => cfg.openai_model.clone(),
            "anthropic" => cfg.anthropic_model.clone(),
            _ => gem_model,
        };
        (cfg.agent_provider.clone(), active_model)
    };
    let max_steps = load_watchdog_settings(&state).await.max_steps;
    let label = if is_resume {
        "Resume Turn".to_string()
    } else {
        prompt.clone()
    };

    let (run_id, cancel_rx, subagent_counter) = state
        .agent_manager
        .start_run(
            None,
            clean_session.clone(),
            target_ws.alias.clone(),
            label,
            provider,
            model,
            norm_mode.clone(),
            max_steps,
        )
        .await?;

    state.agent_running.store(true, Ordering::SeqCst);
    let app_handle = state.app_handle.read().await.clone();
    emit_runs_updated(&app_handle, &state).await;

    let turn_id = format!("turn-{}", &AppState::generate_token()[..8]);
    let set_legacy_turn_id = {
        let mut active = state.active_turn_group_id.write().await;
        if active.is_none() {
            *active = Some(turn_id.clone());
            true
        } else {
            false
        }
    };

    let acp_event_tx = state.acp_bridge.current_event_tx().await;
    let run_ctx = RunContext {
        run_id: run_id.clone(),
        parent_run_id: None,
        session_id: clean_session,
        workspace_id: target_ws.id.clone(),
        workspace_alias: target_ws.alias.clone(),
        workspace_clean_path: target_ws.clean_path.clone(),
        execution_mode: norm_mode,
        depth: 0,
        turn_group_id: turn_id.clone(),
        subagent_counter,
        acp_event_tx,
    };

    let outcome_res = run_agent_turn_inner(
        state.clone(),
        prompt,
        attachments,
        cancel_rx,
        is_resume,
        &target_ws.canon_root,
        run_ctx,
    )
    .await;

    if set_legacy_turn_id {
        let mut active = state.active_turn_group_id.write().await;
        if active.as_deref() == Some(turn_id.as_str()) {
            *active = None;
        }
    }

    match outcome_res {
        Ok(mut outcome) => {
            if !outcome.cancelled && !outcome.awaiting_approval {
                if let Some(hook_note) = crate::hooks::run_post_turn_hooks(&state, &target_ws).await
                {
                    if !hook_note.trim().is_empty() {
                        if outcome.reply.is_empty() {
                            outcome.reply = hook_note;
                        } else {
                            outcome.reply.push_str("\n\n");
                            outcome.reply.push_str(&hook_note);
                        }
                    }
                }
            }

            let status = if outcome.cancelled {
                AgentRunStatus::Cancelled
            } else if outcome.awaiting_approval {
                AgentRunStatus::AwaitingApproval
            } else {
                AgentRunStatus::Completed
            };
            let note = if outcome.awaiting_approval {
                "Awaiting plan approval".to_string()
            } else if outcome.cancelled {
                "Cancelled".to_string()
            } else {
                "Completed".to_string()
            };
            state.agent_manager.finish_run(&run_id, status, note).await;
            let has_running = state.agent_manager.has_active_runs().await;
            state.agent_running.store(has_running, Ordering::SeqCst);
            emit_runs_updated(&app_handle, &state).await;

            let snapshot = state.snapshot().await;
            Ok(AgentTurnResponse {
                run_id,
                reply: outcome.reply,
                steps: outcome.steps,
                usage: outcome.usage,
                cancelled: outcome.cancelled,
                awaiting_approval: outcome.awaiting_approval,
                checkpoint: outcome.checkpoint,
                snapshot,
            })
        }
        Err(err) => {
            state
                .agent_manager
                .finish_run(&run_id, AgentRunStatus::Failed, err.clone())
                .await;
            let has_running = state.agent_manager.has_active_runs().await;
            state.agent_running.store(has_running, Ordering::SeqCst);
            emit_runs_updated(&app_handle, &state).await;
            Err(err)
        }
    }
}

async fn run_agent_turn_inner(
    state: Arc<AppState>,
    prompt: String,
    attachments: Vec<AgentAttachment>,
    cancel_rx: watch::Receiver<bool>,
    is_resume: bool,
    root_path: &Path,
    run_ctx: RunContext,
) -> Result<ProviderTurnOutcome, String> {
    let app_handle = state.app_handle.read().await.clone();
    let provider = state.config.read().await.agent_provider.clone();
    let system_prompt = build_agent_system_prompt(&state, root_path, &run_ctx).await;
    let all_tools = providers::all_mcp_tools(&state).await;
    let tools = if run_ctx.depth > 0 {
        filter_subagent_read_only_tools(all_tools)
    } else {
        all_tools
    };

    match provider.trim() {
        "openai_compat" => {
            providers::run_openai_compat_turn(
                state,
                prompt,
                attachments,
                system_prompt,
                tools,
                app_handle,
                cancel_rx,
                is_resume,
                run_ctx,
            )
            .await
        }
        "anthropic" => {
            providers::run_anthropic_turn(
                state,
                prompt,
                attachments,
                system_prompt,
                tools,
                app_handle,
                cancel_rx,
                is_resume,
                run_ctx,
            )
            .await
        }
        _ => {
            run_gemini_turn(
                state,
                prompt,
                attachments,
                system_prompt,
                tools,
                app_handle,
                cancel_rx,
                is_resume,
                run_ctx,
            )
            .await
        }
    }
}

async fn run_gemini_turn(
    state: Arc<AppState>,
    prompt: String,
    attachments: Vec<AgentAttachment>,
    system_prompt: String,
    tools: Vec<Value>,
    app_handle: Option<tauri::AppHandle>,
    mut cancel_rx: watch::Receiver<bool>,
    is_resume: bool,
    run_ctx: RunContext,
) -> Result<ProviderTurnOutcome, String> {
    let api_key = state.provider_secrets.read().await.gemini_api_key.clone();
    let model = state.gemini_config.read().await.model.clone();
    let mut history = if run_ctx.depth > 0 {
        Vec::new()
    } else {
        let per_sess = state
            .agent_manager
            .get_history("gemini", &run_ctx.session_id)
            .await;
        if per_sess.is_empty() && run_ctx.session_id == "default" {
            state.gemini_config.read().await.history.clone()
        } else {
            per_sess
        }
    };
    let thinking_mode = if run_ctx.depth > 0 {
        "low".to_string()
    } else {
        state.config.read().await.gemini_thinking_mode.clone()
    };

    if api_key.trim().is_empty() {
        return Err("Google AI Studio API Key belum diisi.".to_string());
    }

    ensure_clean_gemini_history(&mut history);

    if is_resume {
        let needs_continue_prompt = history
            .last()
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            != Some("user")
            || history.is_empty();
        if needs_continue_prompt {
            history.push(json!({
                "role": "user",
                "parts": [{ "text": "Continue from the last completed step and finish the task." }]
            }));
        }
    } else {
        let attachment_text = providers::build_attachment_text_context(&state, &attachments).await?;
        let full_prompt = if attachment_text.is_empty() {
            prompt.clone()
        } else {
            format!("{prompt}\n\n{attachment_text}")
        };

        let mut user_parts = vec![json!({ "text": full_prompt })];
        for att in &attachments {
            if att.kind == "image" {
                if let Some(b64) = att
                    .data_base64
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    let mime = att.mime_type.as_deref().unwrap_or("image/png");
                    user_parts.push(json!({
                        "inlineData": {
                            "mimeType": mime,
                            "data": b64
                        }
                    }));
                }
            }
        }

        history.push(json!({
            "role": "user",
            "parts": user_parts
        }));
    }

    let mut last_clean_history = history.clone();
    let watchdog = load_watchdog_settings(&state).await;
    let max_steps = if run_ctx.depth > 0 {
        watchdog.max_steps.min(10)
    } else {
        watchdog.max_steps
    };
    let turn_budget_secs = if run_ctx.depth > 0 {
        watchdog.turn_budget_secs.min(60)
    } else {
        watchdog.turn_budget_secs
    };
    let stream_idle_secs = watchdog.stream_idle_secs;
    let ttfb_timeout_secs = watchdog.ttfb_timeout_secs;
    let turn_start = Instant::now();
    let mut soft_budget_notified = false;
    let mut thinking_config = build_gemini_thinking_config(&model, &thinking_mode);

    let client = &state.http_client;
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse",
        model.trim()
    );

    let system_instruction = json!({
        "parts": [{ "text": system_prompt }]
    });

    let declarations: Vec<Value> = tools
        .into_iter()
        .map(|t| {
            json!({
                "name": t["name"],
                "description": t["description"],
                "parameters": providers::sanitize_schema_for_gemini(&t["inputSchema"])
            })
        })
        .collect();
    let tools_decl = json!([{ "functionDeclarations": declarations }]);

    let mut steps: Vec<ToolStepTrace> = Vec::new();
    let mut total_usage = TokenUsage::default();
    let mut final_reply = String::new();
    let mut last_streamed_text = String::new();
    let mut interrupt_opt: Option<WatchdogInterrupt> = None;
    let mut hit_max_steps = false;
    let mut awaiting_approval = false;
    const BACKOFF_SECS: [u64; 3] = [2, 5, 10];

    for step_idx in 0..max_steps {
        let elapsed_secs = turn_start.elapsed().as_secs();
        if *cancel_rx.borrow() {
            interrupt_opt = Some(WatchdogInterrupt::Cancelled);
            break;
        }
        if elapsed_secs >= turn_budget_secs {
            interrupt_opt = Some(WatchdogInterrupt::TurnBudgetTimeout);
            break;
        }
        if !soft_budget_notified && elapsed_secs >= (turn_budget_secs * 4) / 5 {
            soft_budget_notified = true;
            emit_stream_step(
                &app_handle,
                &state,
                &run_ctx,
                step_idx + 1,
                max_steps,
                "80% batas waktu giliran tercapai — menyelesaikan langkah saat ini...",
            )
            .await;
        }

        emit_stream_step(
            &app_handle,
            &state,
            &run_ctx,
            step_idx + 1,
            max_steps,
            &format!("Step {}/{}: Streaming {}...", step_idx + 1, max_steps, model),
        )
        .await;

        let mut attempt = 0usize;
        let mut resp_opt: Option<reqwest::Response> = None;

        loop {
            let mut body_map = serde_json::Map::new();
            body_map.insert("systemInstruction".to_string(), system_instruction.clone());
            body_map.insert("contents".to_string(), Value::Array(history.clone()));
            body_map.insert("tools".to_string(), tools_decl.clone());
            if let Some(ref tc) = thinking_config {
                body_map.insert(
                    "generationConfig".to_string(),
                    json!({ "thinkingConfig": tc }),
                );
            }
            let body = Value::Object(body_map);

            let send_fut = client
                .post(&url)
                .header("x-goog-api-key", api_key.trim())
                .json(&body)
                .send();
            let send_res = match await_with_watchdog(
                send_fut,
                &mut cancel_rx,
                ttfb_timeout_secs,
                WatchdogInterrupt::StreamIdleTimeout,
                turn_start,
                turn_budget_secs,
            )
            .await
            {
                Ok(r) => r,
                Err(kind) => {
                    interrupt_opt = Some(kind);
                    break;
                }
            };

            let resp = send_res.map_err(|e| format!("Koneksi ke Gemini API gagal: {e}"))?;
            let status = resp.status();
            if status.is_success() {
                resp_opt = Some(resp);
                break;
            }

            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|s| s.clamp(1, 15));

            let raw_text = resp
                .text()
                .await
                .map_err(|e| format!("Gagal membaca respons Gemini API: {e}"))?;

            // Graceful 1x fallback when model/API rejects thinkingConfig on HTTP 400
            let raw_lower = raw_text.to_ascii_lowercase();
            if status.as_u16() == 400
                && thinking_config.is_some()
                && (raw_lower.contains("thinkingconfig")
                    || raw_lower.contains("thinkingbudget")
                    || raw_lower.contains("thinkinglevel"))
            {
                thinking_config = None;
                continue;
            }

            if (status.as_u16() == 429 || status.as_u16() == 503) && attempt < BACKOFF_SECS.len()
            {
                let wait_secs = retry_after_secs.unwrap_or(BACKOFF_SECS[attempt]);
                attempt += 1;
                emit_stream_step(
                    &app_handle,
                    &state,
                    &run_ctx,
                    step_idx + 1,
                    max_steps,
                    &format!(
                        "Gemini API ({status}), retry {}/{} dalam {wait_secs}s...",
                        attempt,
                        BACKOFF_SECS.len()
                    ),
                )
                .await;
                if await_with_watchdog(
                    tokio::time::sleep(Duration::from_secs(wait_secs)),
                    &mut cancel_rx,
                    wait_secs + 1,
                    WatchdogInterrupt::TurnBudgetTimeout,
                    turn_start,
                    turn_budget_secs,
                )
                .await
                .is_err()
                {
                    interrupt_opt = Some(if *cancel_rx.borrow() {
                        WatchdogInterrupt::Cancelled
                    } else {
                        WatchdogInterrupt::TurnBudgetTimeout
                    });
                    break;
                }
                continue;
            }

            let err_msg = serde_json::from_str::<Value>(&raw_text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| {
                    if raw_text.trim().is_empty() {
                        "Unknown Gemini API error".to_string()
                    } else {
                        raw_text.chars().take(300).collect()
                    }
                });
            return Err(format!("Gemini API error ({status}): {err_msg}"));
        }

        if interrupt_opt.is_some() {
            break;
        }

        let mut resp = resp_opt.expect("response present on non-cancelled success");
        let mut step_acc = StreamStepAccumulator::default();
        let mut byte_buf: Vec<u8> = Vec::new();

        loop {
            match await_with_watchdog(
                resp.chunk(),
                &mut cancel_rx,
                stream_idle_secs,
                WatchdogInterrupt::StreamIdleTimeout,
                turn_start,
                turn_budget_secs,
            )
            .await
            {
                Ok(Ok(Some(bytes))) => {
                    byte_buf.extend_from_slice(&bytes);
                    while let Some(pos) = byte_buf.iter().position(|&b| b == b'\n') {
                        let raw_line: Vec<u8> = byte_buf.drain(..=pos).collect();
                        process_sse_raw_line(&mut step_acc, &raw_line, &app_handle, &run_ctx);
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => return Err(format!("Streaming dari Gemini API terputus: {e}")),
                Err(kind) => {
                    interrupt_opt = Some(kind);
                    break;
                }
            }
        }

        total_usage.prompt_tokens += step_acc.usage.prompt_tokens;
        total_usage.output_tokens += step_acc.usage.output_tokens;
        total_usage.cached_tokens += step_acc.usage.cached_tokens;
        total_usage.total_tokens += step_acc.usage.total_tokens;

        if !step_acc.text_buf.trim().is_empty() {
            last_streamed_text = step_acc.text_buf.clone();
        }
        if interrupt_opt.is_some() {
            break;
        }
        if !byte_buf.is_empty() {
            process_sse_raw_line(&mut step_acc, &byte_buf, &app_handle, &run_ctx);
        }

        if step_acc.parts.is_empty()
            && step_acc.function_calls.is_empty()
            && step_acc.text_buf.is_empty()
        {
            return Err("Gemini API tidak mengembalikan kandidat jawaban.".to_string());
        }

        history.push(json!({
            "role": "model",
            "parts": step_acc.parts
        }));

        if step_acc.function_calls.is_empty() {
            last_clean_history = history.clone();
            final_reply = if step_acc.text_buf.trim().is_empty() {
                "(Selesai tanpa teks tambahan)".to_string()
            } else {
                step_acc.text_buf
            };
            break;
        }

        let pending_calls: Vec<PendingToolCall> = step_acc
            .function_calls
            .iter()
            .enumerate()
            .map(|(idx, fc)| PendingToolCall {
                id: format!("gem_{step_idx}_{idx}"),
                name: fc["name"].as_str().unwrap_or("").to_string(),
                args: fc.get("args").cloned().unwrap_or_else(|| json!({})),
            })
            .collect();

        match execute_tool_calls_hybrid(
            state.clone(),
            if run_ctx.depth > 0 {
                "Subagent"
            } else {
                "LocalAgent"
            },
            pending_calls,
            &mut cancel_rx,
            turn_start,
            turn_budget_secs,
            &app_handle,
            Some(&run_ctx),
        )
        .await
        {
            Ok(exec_results) => {
                let mut requested_approval_msg: Option<String> = None;
                let mut function_response_parts = Vec::with_capacity(exec_results.len());
                for item in exec_results {
                    if item.name == "request_plan_approval" && !item.output.is_error {
                        requested_approval_msg = Some(item.output.text.clone());
                    }
                    steps.push(ToolStepTrace {
                        tool_name: item.name.clone(),
                        summary: item
                            .output
                            .text
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(80)
                            .collect(),
                        tokens_saved: item.tokens_saved,
                    });
                    function_response_parts.push(json!({
                        "functionResponse": {
                            "name": item.name,
                            "response": {
                                "isError": item.output.is_error,
                                "content": item.output.text
                            }
                        }
                    }));
                }
                history.push(json!({
                    "role": "user",
                    "parts": function_response_parts
                }));
                last_clean_history = history.clone();
                if let Some(msg) = requested_approval_msg {
                    awaiting_approval = true;
                    final_reply = if last_streamed_text.trim().is_empty() {
                        format!(
                            "Implementation Plan siap ditinjau pada panel Artifacts.\n\n{msg}"
                        )
                    } else {
                        last_streamed_text.clone()
                    };
                    break;
                }
            }
            Err(kind) => {
                interrupt_opt = Some(kind);
                break;
            }
        }

        if step_idx + 1 == max_steps {
            hit_max_steps = true;
            final_reply = format!(
                "Batas maksimum {max_steps} langkah tool per giliran tercapai. Klik Resume Turn atau kirim pesan lanjutan untuk melanjutkan."
            );
        }
    }

    ensure_clean_gemini_history(&mut last_clean_history);

    let checkpoint_opt = if run_ctx.depth > 0 {
        None
    } else if let Some(kind) = interrupt_opt {
        prune_gemini_history(&mut last_clean_history, 30);
        state
            .agent_manager
            .set_history("gemini", &run_ctx.session_id, last_clean_history.clone())
            .await;
        if run_ctx.session_id == "default" {
            state.gemini_config.write().await.history = last_clean_history;
        }
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
        if run_ctx.session_id == "default" {
            *state.pending_checkpoint.write().await = Some(cp.clone());
        }
        final_reply =
            format_interrupted_reply(&last_streamed_text, kind, turn_budget_secs, stream_idle_secs);
        Some(cp)
    } else if hit_max_steps {
        prune_gemini_history(&mut last_clean_history, 30);
        state
            .agent_manager
            .set_history("gemini", &run_ctx.session_id, last_clean_history.clone())
            .await;
        if run_ctx.session_id == "default" {
            state.gemini_config.write().await.history = last_clean_history;
        }
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
        if run_ctx.session_id == "default" {
            *state.pending_checkpoint.write().await = Some(cp.clone());
        }
        Some(cp)
    } else {
        prune_gemini_history(&mut last_clean_history, 30);
        state
            .agent_manager
            .set_history("gemini", &run_ctx.session_id, last_clean_history.clone())
            .await;
        if run_ctx.session_id == "default" {
            state.gemini_config.write().await.history = last_clean_history;
            *state.pending_checkpoint.write().await = None;
        }
        state
            .agent_manager
            .set_checkpoint(&run_ctx.session_id, None)
            .await;
        None
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

    Ok(ProviderTurnOutcome {
        reply: final_reply,
        steps,
        usage: total_usage,
        cancelled: interrupt_opt == Some(WatchdogInterrupt::Cancelled),
        awaiting_approval,
        checkpoint: checkpoint_opt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_text(s: &str) -> Value {
        json!({ "role": "user", "parts": [{ "text": s }] })
    }

    fn model_call(name: &str) -> Value {
        json!({ "role": "model", "parts": [{ "functionCall": { "name": name, "args": {} } }] })
    }

    fn user_resp(name: &str) -> Value {
        json!({ "role": "user", "parts": [{ "functionResponse": { "name": name, "response": {} } }] })
    }

    fn model_text(s: &str) -> Value {
        json!({ "role": "model", "parts": [{ "text": s }] })
    }

    #[test]
    fn test_prune_history_preserves_function_call_pairs() {
        let mut hist = vec![
            user_text("turn 1"),
            model_call("a"),
            user_resp("a"),
            model_text("done 1"),
            user_text("turn 2"),
            model_call("b"),
            user_resp("b"),
            model_call("c"),
            user_resp("c"),
            model_text("done 2"),
        ];
        prune_gemini_history(&mut hist, 4);
        assert!(is_clean_user_turn(&hist[0]));
        assert_eq!(hist[0]["parts"][0]["text"], "turn 2");
        assert_eq!(hist.len(), 6);

        let mut hist2 = vec![
            user_text("t1"),
            model_call("x"),
            user_resp("x"),
            model_text("r1"),
            user_text("t2"),
            model_text("r2"),
            user_text("t3"),
            model_text("r3"),
        ];
        prune_gemini_history(&mut hist2, 5);
        assert!(is_clean_user_turn(&hist2[0]));
        assert_eq!(hist2[0]["parts"][0]["text"], "t2");
        assert_eq!(hist2.len(), 4);
    }

    #[test]
    fn test_sse_accumulator_parses_thought_text_and_function_call() {
        let mut acc = StreamStepAccumulator::default();

        let chunk1 = r#"{"candidates":[{"content":{"parts":[{"thought":true,"text":"Analyzing "}]}}],"usageMetadata":{"promptTokenCount":120,"candidatesTokenCount":4,"totalTokenCount":124}}"#;
        let (t1, th1) = process_sse_data_line(&mut acc, chunk1).unwrap();
        assert_eq!(t1, "");
        assert_eq!(th1, "Analyzing ");

        let chunk2 = r#"{"candidates":[{"content":{"parts":[{"thought":true,"text":"workspace..."}]}}],"usageMetadata":{"promptTokenCount":120,"candidatesTokenCount":9,"totalTokenCount":129}}"#;
        let (t2, th2) = process_sse_data_line(&mut acc, chunk2).unwrap();
        assert_eq!(t2, "");
        assert_eq!(th2, "workspace...");

        let chunk3 = r#"{"candidates":[{"content":{"parts":[{"text":"Checking "}]}}]}"#;
        let (t3, th3) = process_sse_data_line(&mut acc, chunk3).unwrap();
        assert_eq!(t3, "Checking ");
        assert_eq!(th3, "");

        let chunk4 = r#"{"candidates":[{"content":{"parts":[{"text":"files."},{"functionCall":{"name":"project_overview","args":{}},"thoughtSignature":"sig_abc_123"}]}}],"usageMetadata":{"promptTokenCount":120,"candidatesTokenCount":25,"cachedContentTokenCount":80,"totalTokenCount":145}}"#;
        let (t4, th4) = process_sse_data_line(&mut acc, chunk4).unwrap();
        assert_eq!(t4, "files.");
        assert_eq!(th4, "");

        assert_eq!(acc.thought_buf, "Analyzing workspace...");
        assert_eq!(acc.text_buf, "Checking files.");
        assert_eq!(acc.function_calls.len(), 1);
        assert_eq!(acc.function_calls[0]["name"], "project_overview");
        assert_eq!(acc.usage.prompt_tokens, 120);
        assert_eq!(acc.usage.output_tokens, 25);
        assert_eq!(acc.usage.cached_tokens, 80);
        assert_eq!(acc.usage.total_tokens, 145);

        // Consecutive plain thought parts coalesce, consecutive plain text parts coalesce,
        // and the signed functionCall part keeps its thoughtSignature intact.
        assert_eq!(acc.parts.len(), 3);
        assert_eq!(acc.parts[0]["thought"], true);
        assert_eq!(acc.parts[0]["text"], "Analyzing workspace...");
        assert_eq!(acc.parts[1]["text"], "Checking files.");
        assert_eq!(acc.parts[2]["thoughtSignature"], "sig_abc_123");
    }

    #[test]
    fn test_thinking_config_builder_per_model_family() {
        // Gemini 2.5 Flash supports budget 0 on off
        let f_off = build_gemini_thinking_config("gemini-2.5-flash", "off").unwrap();
        assert_eq!(f_off["thinkingBudget"], 0);
        assert_eq!(f_off["includeThoughts"], false);

        // Gemini 2.5 Pro minimum thinking budget is 128 on off
        let p_off = build_gemini_thinking_config("gemini-2.5-pro", "off").unwrap();
        assert_eq!(p_off["thinkingBudget"], 128);
        assert_eq!(p_off["includeThoughts"], true);

        let p_high = build_gemini_thinking_config("gemini-2.5-pro", "high").unwrap();
        assert_eq!(p_high["thinkingBudget"], 12288);

        let f_auto = build_gemini_thinking_config("gemini-2.5-flash-preview", "auto").unwrap();
        assert_eq!(f_auto["thinkingBudget"], -1);

        // Gemini 3 family uses thinkingLevel
        let g3_low = build_gemini_thinking_config("gemini-3-flash-preview", "low").unwrap();
        assert_eq!(g3_low["thinkingLevel"], "low");

        let g3_off_flash = build_gemini_thinking_config("gemini-3-flash", "off").unwrap();
        assert_eq!(g3_off_flash["thinkingLevel"], "minimal");

        assert!(build_gemini_thinking_config("gemini-3-pro", "off").is_none());

        // Legacy models receive None
        assert!(build_gemini_thinking_config("gemini-2.0-flash", "high").is_none());
        assert!(build_gemini_thinking_config("gemini-1.5-pro", "auto").is_none());
    }

    #[tokio::test]
    async fn test_parallel_read_only_preserves_order_and_keeps_mutative_sequential() {
        use std::fs;
        let temp_dir = std::env::temp_dir().join("smart_mcp_hybrid_parallel_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("a.txt"), "ALPHA_CONTENT\n").unwrap();
        fs::write(temp_dir.join("b.txt"), "BETA_CONTENT\n").unwrap();
        fs::write(temp_dir.join("c.txt"), "initial\n").unwrap();

        let state = Arc::new(AppState::new(temp_dir.clone()));
        let mut cancel_rx = state.reset_agent_cancel();
        let turn_start = Instant::now();

        let calls = vec![
            PendingToolCall {
                id: "call_0".to_string(),
                name: "read_file_range".to_string(),
                args: json!({ "path": "a.txt" }),
            },
            PendingToolCall {
                id: "call_1".to_string(),
                name: "read_file_range".to_string(),
                args: json!({ "path": "b.txt" }),
            },
            PendingToolCall {
                id: "call_2".to_string(),
                name: "surgical_edit".to_string(),
                args: json!({
                    "path": "c.txt",
                    "edits": [{ "search": "initial", "replace": "step_one" }]
                }),
            },
            PendingToolCall {
                id: "call_3".to_string(),
                name: "surgical_edit".to_string(),
                args: json!({
                    "path": "c.txt",
                    "edits": [{ "search": "step_one", "replace": "step_two" }]
                }),
            },
            PendingToolCall {
                id: "call_4".to_string(),
                name: "read_file_range".to_string(),
                args: json!({ "path": "c.txt" }),
            },
        ];

        let results = execute_tool_calls_hybrid(
            state.clone(),
            "Test",
            calls,
            &mut cancel_rx,
            turn_start,
            60,
            &None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 5);
        assert_eq!(results[0].id, "call_0");
        assert!(results[0].output.text.contains("ALPHA_CONTENT"));
        assert_eq!(results[1].id, "call_1");
        assert!(results[1].output.text.contains("BETA_CONTENT"));
        assert_eq!(results[2].id, "call_2");
        assert!(!results[2].output.is_error);
        assert_eq!(results[3].id, "call_3");
        assert!(!results[3].output.is_error);
        assert_eq!(results[4].id, "call_4");
        assert!(results[4].output.text.contains("step_two"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_cancel_saves_clean_checkpoint_without_dangling_function_call() {
        let mut hist = vec![
            user_text("Step 1"),
            json!({
                "role": "model",
                "parts": [{
                    "functionCall": { "name": "read_file_range", "args": { "path": "a.rs" } },
                    "thoughtSignature": "sig_complete_turn"
                }]
            }),
            user_resp("read_file_range"),
            // Imagine interruption occurred after pushing dangling model functionCall without functionResponse:
            json!({
                "role": "model",
                "parts": [{
                    "functionCall": { "name": "smart_search", "args": { "query": "foo" } },
                    "thoughtSignature": "sig_dangling_turn"
                }]
            }),
        ];

        ensure_clean_gemini_history(&mut hist);
        assert_eq!(hist.len(), 3, "Dangling model functionCall must be stripped");
        assert_eq!(
            hist[1]["parts"][0]["thoughtSignature"],
            "sig_complete_turn",
            "Completed turn thoughtSignature must remain intact"
        );
        assert!(hist[2]["parts"][0].get("functionResponse").is_some());
    }

    #[tokio::test]
    async fn test_subagent_is_strictly_read_only_and_cannot_recurse() {
        let all_tools = crate::tools::mcp_tools_list()
            .as_array()
            .cloned()
            .unwrap_or_default();
        let filtered = filter_subagent_read_only_tools(all_tools);
        let names: Vec<&str> = filtered
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
            .collect();

        assert!(names.contains(&"read_file_range"));
        assert!(names.contains(&"smart_search"));
        assert!(names.contains(&"project_overview"));
        assert!(names.contains(&"find_files"));
        assert!(names.contains(&"code_outline"));
        assert!(names.contains(&"batch_inspect"));

        for banned in [
            "spawn_subagent",
            "surgical_edit",
            "multi_surgical_edit",
            "patch_and_verify",
            "write_file_jailed",
            "run_command_compressed",
            "start_command_job",
            "file_ops",
            "git_ops",
            "upsert_artifact",
            "request_plan_approval",
        ] {
            assert!(
                !names.contains(&banned),
                "Subagent must never expose mutative or recursive tool '{banned}'"
            );
        }

        let temp_dir = std::env::temp_dir().join("smart_mcp_subagent_recursion_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        let state = Arc::new(AppState::new(temp_dir.clone()));

        let nested_parent_ctx = RunContext {
            run_id: "run_child".to_string(),
            parent_run_id: Some("run_root".to_string()),
            session_id: "sess_1".to_string(),
            workspace_id: "ws-default".to_string(),
            workspace_alias: "repo".to_string(),
            workspace_clean_path: clean_path_string(&temp_dir),
            execution_mode: "read_only_subagent".to_string(),
            depth: 1,
            turn_group_id: "turn-1".to_string(),
            subagent_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            acp_event_tx: None,
        };

        let recurse_err = run_subagent_turn(
            state,
            &nested_parent_ctx,
            &json!({
                "goal": "Try nested subagent",
                "max_steps": 3
            }),
        )
        .await
        .unwrap_err();
        assert!(recurse_err.contains("anti-rekursi"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
