use crate::providers::{self, AgentAttachment, ProviderTurnOutcome};
use crate::state::{AppState, DashboardSnapshot};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::Emitter;

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
    pub reply: String,
    pub steps: Vec<ToolStepTrace>,
    pub usage: TokenUsage,
    pub cancelled: bool,
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

struct AgentRunGuard<'a> {
    flag: &'a AtomicBool,
}

impl Drop for AgentRunGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
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

pub async fn fetch_gemini_models(api_key: String) -> Result<Vec<GeminiModelInfo>, String> {
    if api_key.trim().is_empty() {
        return Err("Google AI Studio API Key belum diisi.".to_string());
    }

    let client = reqwest::Client::new();
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
) {
    let line_str = String::from_utf8_lossy(raw_line);
    let trimmed = line_str.trim_end_matches(&['\r', '\n'][..]);
    if let Some(data_payload) = trimmed.strip_prefix("data:") {
        let data_trimmed = data_payload.trim();
        if !data_trimmed.is_empty() && data_trimmed != "[DONE]" {
            if let Ok((text_delta, thought_delta)) = process_sse_data_line(step_acc, data_trimmed) {
                if !thought_delta.is_empty() {
                    if let Some(app) = app_handle {
                        let _ = app.emit("agent-thought-chunk", &thought_delta);
                    }
                }
                if !text_delta.is_empty() {
                    if let Some(app) = app_handle {
                        let _ = app.emit("agent-text-chunk", &text_delta);
                    }
                }
            }
        }
    }
}

struct TurnGroupGuard<'a> {
    group_lock: &'a tokio::sync::RwLock<Option<String>>,
}

impl Drop for TurnGroupGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut g) = self.group_lock.try_write() {
            *g = None;
        }
    }
}

pub async fn run_agent_turn(
    state: &AppState,
    prompt: String,
    attachments: Vec<AgentAttachment>,
) -> Result<AgentTurnResponse, String> {
    if state.agent_running.swap(true, Ordering::SeqCst) {
        return Err("Local Agent sedang berjalan. Klik Stop terlebih dahulu.".to_string());
    }
    let guard = AgentRunGuard {
        flag: &state.agent_running,
    };

    let turn_id = format!("turn-{}", &AppState::generate_token()[..8]);
    *state.active_turn_group_id.write().await = Some(turn_id);
    let turn_guard = TurnGroupGuard {
        group_lock: &state.active_turn_group_id,
    };

    let outcome_res = run_agent_turn_inner(state, prompt, attachments).await;
    *state.active_turn_group_id.write().await = None;
    drop(turn_guard);
    drop(guard);

    let outcome = outcome_res?;
    let snapshot = state.snapshot().await;
    Ok(AgentTurnResponse {
        reply: outcome.reply,
        steps: outcome.steps,
        usage: outcome.usage,
        cancelled: outcome.cancelled,
        snapshot,
    })
}

async fn run_agent_turn_inner(
    state: &AppState,
    prompt: String,
    attachments: Vec<AgentAttachment>,
) -> Result<ProviderTurnOutcome, String> {
    let root_path = state.workspace_root.read().await.clone();
    let raw_root = root_path.display().to_string();
    let workspace_root = raw_root
        .strip_prefix(r"\\?\")
        .unwrap_or(&raw_root)
        .to_string();
    let app_handle = state.app_handle.read().await.clone();

    let (provider, auto_inject) = {
        let cfg = state.config.read().await;
        (cfg.agent_provider.clone(), cfg.auto_inject_project_rules)
    };

    let mut system_prompt = format!(
        "You are Airlock Local Coding Agent operating directly inside jailed workspace: {}. \
        Minimize tokens and iterations: use project_overview, find_files, smart_search, or code_outline first, \
        read_file_range for slices, and prefer surgical_edit or patch_and_verify over write_file_jailed. Respond concisely in the user's language.",
        workspace_root
    );
    if auto_inject {
        if let Some(rules) = providers::load_project_rules_snippet(&root_path) {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&rules);
        }
    }

    let tools = providers::all_mcp_tools(state).await;

    match provider.trim() {
        "openai_compat" => {
            providers::run_openai_compat_turn(
                state,
                prompt,
                attachments,
                system_prompt,
                tools,
                app_handle,
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
            )
            .await
        }
    }
}

async fn run_gemini_turn(
    state: &AppState,
    prompt: String,
    attachments: Vec<AgentAttachment>,
    system_prompt: String,
    tools: Vec<Value>,
    app_handle: Option<tauri::AppHandle>,
) -> Result<ProviderTurnOutcome, String> {
    let secret_key = state.provider_secrets.read().await.gemini_api_key.clone();
    let (mem_key, model, mut history) = {
        let cfg = state.gemini_config.read().await;
        (cfg.api_key.clone(), cfg.model.clone(), cfg.history.clone())
    };
    let api_key = if !secret_key.trim().is_empty() {
        secret_key
    } else {
        mem_key
    };

    if api_key.trim().is_empty() {
        return Err("Google AI Studio API Key belum diisi.".to_string());
    }

    let attachment_text = providers::build_attachment_text_context(state, &attachments).await?;
    let full_prompt = if attachment_text.is_empty() {
        prompt.clone()
    } else {
        format!("{prompt}\n\n{attachment_text}")
    };

    let mut user_parts = vec![json!({ "text": full_prompt })];
    for att in &attachments {
        if att.kind == "image" {
            if let Some(b64) = att.data_base64.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty())
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

    let client = reqwest::Client::new();
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
    let mut was_cancelled = false;
    const BACKOFF_SECS: [u64; 3] = [2, 5, 10];

    for step_idx in 0..8 {
        if let Some(app) = &app_handle {
            let _ = app.emit(
                "agent-step",
                format!("Step {}/8: Streaming {}...", step_idx + 1, model),
            );
        }

        let body = json!({
            "systemInstruction": system_instruction,
            "contents": history,
            "tools": tools_decl
        });

        let mut attempt = 0usize;
        let mut resp_opt: Option<reqwest::Response> = None;

        loop {
            let send_res = tokio::select! {
                _ = state.agent_cancel.notified() => {
                    was_cancelled = true;
                    break;
                }
                res = client
                    .post(&url)
                    .header("x-goog-api-key", api_key.trim())
                    .json(&body)
                    .send() => res,
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

            if (status.as_u16() == 429 || status.as_u16() == 503) && attempt < BACKOFF_SECS.len()
            {
                let wait_secs = retry_after_secs.unwrap_or(BACKOFF_SECS[attempt]);
                attempt += 1;
                if let Some(app) = &app_handle {
                    let _ = app.emit(
                        "agent-step",
                        format!(
                            "Gemini API ({status}), retry {}/{} dalam {wait_secs}s...",
                            attempt,
                            BACKOFF_SECS.len()
                        ),
                    );
                }
                tokio::select! {
                    _ = state.agent_cancel.notified() => {
                        was_cancelled = true;
                        break;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(wait_secs)) => {}
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

        if was_cancelled {
            break;
        }

        let mut resp = resp_opt.expect("response present on non-cancelled success");
        let mut step_acc = StreamStepAccumulator::default();
        let mut byte_buf: Vec<u8> = Vec::new();

        loop {
            tokio::select! {
                _ = state.agent_cancel.notified() => {
                    was_cancelled = true;
                    break;
                }
                chunk_res = resp.chunk() => {
                    match chunk_res {
                        Ok(Some(bytes)) => {
                            byte_buf.extend_from_slice(&bytes);
                            while let Some(pos) = byte_buf.iter().position(|&b| b == b'\n') {
                                let raw_line: Vec<u8> = byte_buf.drain(..=pos).collect();
                                process_sse_raw_line(&mut step_acc, &raw_line, &app_handle);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => return Err(format!("Streaming dari Gemini API terputus: {e}")),
                    }
                }
            }
        }

        if !was_cancelled && !byte_buf.is_empty() {
            process_sse_raw_line(&mut step_acc, &byte_buf, &app_handle);
        }

        total_usage.prompt_tokens += step_acc.usage.prompt_tokens;
        total_usage.output_tokens += step_acc.usage.output_tokens;
        total_usage.cached_tokens += step_acc.usage.cached_tokens;
        total_usage.total_tokens += step_acc.usage.total_tokens;

        if !step_acc.text_buf.trim().is_empty() {
            last_streamed_text = step_acc.text_buf.clone();
        }

        if was_cancelled {
            break;
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
            final_reply = if step_acc.text_buf.trim().is_empty() {
                "(Selesai tanpa teks tambahan)".to_string()
            } else {
                step_acc.text_buf
            };
            break;
        }

        let mut function_response_parts = Vec::new();
        for fc in step_acc.function_calls {
            let name = fc["name"].as_str().unwrap_or("").to_string();
            let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));

            if let Some(app) = &app_handle {
                let _ = app.emit("agent-step", format!("Menjalankan tool: {name}..."));
            }

            let tool_out_opt = tokio::select! {
                _ = state.agent_cancel.notified() => {
                    was_cancelled = true;
                    None
                }
                out = providers::execute_any_tool(state, "LocalAgent", &name, args) => Some(out),
            };

            let Some(out) = tool_out_opt else {
                break;
            };

            let tokens_saved = {
                let logs = state.audit_logs.read().await;
                logs.last().map(|l| l.tokens_saved).unwrap_or(0)
            };

            steps.push(ToolStepTrace {
                tool_name: name.clone(),
                summary: out
                    .text
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect(),
                tokens_saved,
            });

            function_response_parts.push(json!({
                "functionResponse": {
                    "name": name,
                    "response": {
                        "isError": out.is_error,
                        "content": out.text
                    }
                }
            }));
        }

        if was_cancelled {
            break;
        }

        history.push(json!({
            "role": "user",
            "parts": function_response_parts
        }));

        if step_idx == 7 {
            final_reply = "Batas maksimum 8 langkah tool per giliran tercapai. Kirim pesan lanjutan untuk melanjutkan.".to_string();
        }
    }

    if was_cancelled {
        final_reply = if last_streamed_text.trim().is_empty() {
            "(Dibatalkan oleh user)".to_string()
        } else {
            format!("{}\n\n[Dibatalkan oleh user]", last_streamed_text.trim_end())
        };
    } else {
        prune_gemini_history(&mut history, 30);
        let mut cfg = state.gemini_config.write().await;
        cfg.history = history;
    }

    Ok(ProviderTurnOutcome {
        reply: final_reply,
        steps,
        usage: total_usage,
        cancelled: was_cancelled,
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
}
