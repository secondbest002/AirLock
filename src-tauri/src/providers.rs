use crate::local_agent::{GeminiModelInfo, TokenUsage, ToolStepTrace};
use crate::security::resolve_jailed_path;
use crate::state::AppState;
use crate::tools::{check_sensitive_guard, execute_tool, mcp_tools_list, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::Path;
use tauri::Emitter;

fn is_local_url(raw: &str) -> bool {
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
}

fn truncate_str_chars(s: &str, max_chars: usize) -> String {
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

pub fn load_project_rules_snippet(root: &Path) -> Option<String> {
    const CANDIDATES: [&str; 5] = [
        "AGENTS.md",
        "CLAUDE.md",
        "GEMINI.md",
        ".cursorrules",
        ".github/copilot-instructions.md",
    ];
    let mut chunks = Vec::new();
    let mut remaining = 6000usize;

    for rel in CANDIDATES {
        if remaining == 0 {
            break;
        }
        let path = root.join(rel);
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let clipped = truncate_str_chars(trimmed, remaining);
        let used = clipped.chars().count();
        chunks.push(format!("### Project Rules ({rel})\n{clipped}"));
        remaining = remaining.saturating_sub(used);
    }

    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n\n"))
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
    let mut tools = mcp_tools_list().as_array().cloned().unwrap_or_default();
    let cfgs = state.config.read().await.downstream_mcps.clone();
    let aggregated = state.mcp_hub.list_aggregated_tools(&cfgs).await;
    tools.extend(aggregated);
    tools
}

pub async fn execute_any_tool(
    state: &AppState,
    source: &str,
    name: &str,
    args: Value,
) -> ToolOutput {
    if name.contains("__") {
        state
            .mcp_hub
            .call_downstream_tool(state, source, name, args)
            .await
    } else {
        execute_tool(state, source, name, args).await
    }
}

pub async fn build_attachment_text_context(
    state: &AppState,
    attachments: &[AgentAttachment],
) -> Result<String, String> {
    let root = state.workspace_root.read().await.clone();
    let mut sections = Vec::new();

    for att in attachments {
        if att.kind != "file" {
            continue;
        }
        let rel = att
            .path
            .as_deref()
            .unwrap_or(&att.name)
            .trim();
        if rel.is_empty() {
            continue;
        }
        let target = resolve_jailed_path(&root, rel)?;
        check_sensitive_guard(state, &root, &target, rel).await?;
        let bytes = std::fs::read(&target)
            .map_err(|e| format!("Gagal membaca lampiran '{rel}': {e}"))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("Lampiran '{rel}' bukan file teks UTF-8 yang valid."))?;
        let clipped = truncate_str_chars(&text, 12_000);
        sections.push(format!("### Attached File: {rel}\n```\n{clipped}\n```"));
    }

    Ok(sections.join("\n\n"))
}

pub async fn fetch_openai_compat_models(
    base_url: String,
    api_key: String,
) -> Result<Vec<GeminiModelInfo>, String> {
    let clean_base = base_url.trim().trim_end_matches('/');
    if clean_base.is_empty() {
        return Err("Base URL OpenAI-compatible belum diisi.".to_string());
    }
    let endpoint = format!("{clean_base}/models");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let mut req = client.get(&endpoint);
    if !api_key.trim().is_empty() {
        req = req.bearer_auth(api_key.trim());
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("Gagal menghubungi endpoint model ({endpoint}): {e}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Gagal mem-parsing daftar model: {e}"))?;

    if !status.is_success() {
        let msg = body["error"]["message"]
            .as_str()
            .unwrap_or("Unknown error");
        return Err(format!("Endpoint /models error ({status}): {msg}"));
    }

    let arr = body["data"]
        .as_array()
        .or_else(|| body["models"].as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for item in arr {
        let id = item
            .get("id")
            .or_else(|| item.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if id.is_empty() {
            continue;
        }
        let display_name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();
        let description = item
            .get("description")
            .or_else(|| item.get("owned_by"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let input_token_limit = item
            .get("context_length")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        out.push(GeminiModelInfo {
            id,
            display_name,
            description,
            input_token_limit,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

pub fn anthropic_known_models() -> Vec<GeminiModelInfo> {
    vec![
        GeminiModelInfo {
            id: "claude-sonnet-4-5".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            description: "Balanced coding and agentic reasoning model".to_string(),
            input_token_limit: 200_000,
        },
        GeminiModelInfo {
            id: "claude-opus-4-1".to_string(),
            display_name: "Claude Opus 4.1".to_string(),
            description: "High-capability deep reasoning model".to_string(),
            input_token_limit: 200_000,
        },
        GeminiModelInfo {
            id: "claude-3-7-sonnet-latest".to_string(),
            display_name: "Claude 3.7 Sonnet".to_string(),
            description: "Extended thinking coding model".to_string(),
            input_token_limit: 200_000,
        },
        GeminiModelInfo {
            id: "claude-3-5-haiku-latest".to_string(),
            display_name: "Claude 3.5 Haiku".to_string(),
            description: "Fast low-latency coding model".to_string(),
            input_token_limit: 200_000,
        },
    ]
}

// ============================================================================
// OpenAI-Compatible Chat Completions Streaming
// ============================================================================

#[derive(Debug, Default)]
pub struct OpenAiToolCallAcc {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Default)]
pub struct OpenAiSseAccumulator {
    pub text_buf: String,
    pub thought_buf: String,
    pub tool_calls: BTreeMap<usize, OpenAiToolCallAcc>,
    pub usage: TokenUsage,
}

pub fn process_openai_sse_data_line(
    acc: &mut OpenAiSseAccumulator,
    data_json: &str,
) -> Result<(String, String), String> {
    let trimmed = data_json.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok((String::new(), String::new()));
    }
    let val: Value =
        serde_json::from_str(trimmed).map_err(|e| format!("OpenAI SSE JSON parse error: {e}"))?;

    if let Some(u) = val.get("usage").filter(|v| !v.is_null()) {
        if let Some(v) = u["prompt_tokens"].as_u64() {
            if v > 0 {
                acc.usage.prompt_tokens = v;
            }
        }
        if let Some(v) = u["completion_tokens"].as_u64() {
            if v > 0 {
                acc.usage.output_tokens = v;
            }
        }
        if let Some(v) = u["prompt_tokens_details"]["cached_tokens"].as_u64() {
            if v > 0 {
                acc.usage.cached_tokens = v;
            }
        }
        if let Some(v) = u["total_tokens"].as_u64() {
            if v > 0 {
                acc.usage.total_tokens = v;
            }
        }
    }

    let mut text_delta = String::new();
    let mut thought_delta = String::new();

    if let Some(choice) = val["choices"].get(0) {
        let delta = &choice["delta"];
        if let Some(r) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(|v| v.as_str())
        {
            acc.thought_buf.push_str(r);
            thought_delta.push_str(r);
        }
        if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
            acc.text_buf.push_str(c);
            text_delta.push_str(c);
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let entry = acc.tool_calls.entry(idx).or_default();
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    if !id.is_empty() {
                        entry.id = id.to_string();
                    }
                }
                if let Some(func) = tc.get("function") {
                    if let Some(n) = func.get("name").and_then(|v| v.as_str()) {
                        entry.name.push_str(n);
                    }
                    if let Some(a) = func.get("arguments").and_then(|v| v.as_str()) {
                        entry.arguments.push_str(a);
                    }
                }
            }
        }
    }

    Ok((text_delta, thought_delta))
}

fn process_openai_sse_raw_line(
    acc: &mut OpenAiSseAccumulator,
    raw_line: &[u8],
    app_handle: &Option<tauri::AppHandle>,
) {
    let line_str = String::from_utf8_lossy(raw_line);
    let trimmed = line_str.trim_end_matches(&['\r', '\n'][..]);
    if let Some(data_payload) = trimmed.strip_prefix("data:") {
        let data_trimmed = data_payload.trim();
        if !data_trimmed.is_empty() && data_trimmed != "[DONE]" {
            if let Ok((text_delta, thought_delta)) = process_openai_sse_data_line(acc, data_trimmed)
            {
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

fn prune_openai_history(history: &mut Vec<Value>, max_entries: usize) {
    if history.len() <= max_entries || max_entries == 0 {
        return;
    }
    let min_cut = history.len() - max_entries;
    let is_user = |v: &Value| v.get("role").and_then(|r| r.as_str()) == Some("user");
    if let Some(idx) = (min_cut..history.len()).find(|&i| is_user(&history[i])) {
        history.drain(0..idx);
        return;
    }
    if let Some(idx) = (1..min_cut).rev().find(|&i| is_user(&history[i])) {
        history.drain(0..idx);
    }
}

pub async fn run_openai_compat_turn(
    state: &AppState,
    prompt: String,
    attachments: Vec<AgentAttachment>,
    system_prompt: String,
    tools: Vec<Value>,
    app_handle: Option<tauri::AppHandle>,
) -> Result<ProviderTurnOutcome, String> {
    let (base_url, model) = {
        let cfg = state.config.read().await;
        (
            cfg.openai_base_url.trim().trim_end_matches('/').to_string(),
            cfg.openai_model.trim().to_string(),
        )
    };
    let api_key = state.provider_secrets.read().await.openai_api_key.clone();

    if base_url.is_empty() {
        return Err("Base URL OpenAI-Compatible belum diisi.".to_string());
    }
    if model.is_empty() {
        return Err("Nama model OpenAI-Compatible belum diisi.".to_string());
    }
    if api_key.trim().is_empty() && !is_local_url(&base_url) {
        return Err("API Key untuk provider OpenAI-Compatible belum diisi.".to_string());
    }

    let attachment_text = build_attachment_text_context(state, &attachments).await?;
    let full_prompt = if attachment_text.is_empty() {
        prompt.clone()
    } else {
        format!("{prompt}\n\n{attachment_text}")
    };

    let image_parts: Vec<Value> = attachments
        .iter()
        .filter(|a| a.kind == "image")
        .filter_map(|a| {
            let b64 = a.data_base64.as_ref()?.trim();
            if b64.is_empty() {
                return None;
            }
            let mime = a.mime_type.as_deref().unwrap_or("image/png");
            Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime};base64,{b64}") }
            }))
        })
        .collect();

    let user_msg = if image_parts.is_empty() {
        json!({ "role": "user", "content": full_prompt })
    } else {
        let mut content = vec![json!({ "type": "text", "text": full_prompt })];
        content.extend(image_parts);
        json!({ "role": "user", "content": content })
    };

    let mut history = state.openai_history.read().await.clone();
    history.push(user_msg);

    let openai_tools: Vec<Value> = tools
        .into_iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t["name"],
                    "description": t["description"],
                    "parameters": t["inputSchema"]
                }
            })
        })
        .collect();

    let client = reqwest::Client::new();
    let url = format!("{base_url}/chat/completions");
    let mut steps = Vec::new();
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

        let mut messages_payload = vec![json!({ "role": "system", "content": system_prompt })];
        messages_payload.extend(history.clone());

        let body = json!({
            "model": model,
            "messages": messages_payload,
            "tools": openai_tools,
            "stream": true,
            "stream_options": { "include_usage": true }
        });

        let mut attempt = 0usize;
        let mut resp_opt: Option<reqwest::Response> = None;

        loop {
            let mut req = client.post(&url).json(&body);
            if !api_key.trim().is_empty() {
                req = req.bearer_auth(api_key.trim());
            }
            let send_res = tokio::select! {
                _ = state.agent_cancel.notified() => {
                    was_cancelled = true;
                    break;
                }
                res = req.send() => res,
            };

            let resp = send_res.map_err(|e| format!("Koneksi ke {url} gagal: {e}"))?;
            let status = resp.status();
            if status.is_success() {
                resp_opt = Some(resp);
                break;
            }

            let raw_text = resp
                .text()
                .await
                .map_err(|e| format!("Gagal membaca respons endpoint: {e}"))?;

            if (status.as_u16() == 429 || status.as_u16() == 503) && attempt < BACKOFF_SECS.len()
            {
                let wait_secs = BACKOFF_SECS[attempt];
                attempt += 1;
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
                .unwrap_or_else(|| truncate_str_chars(&raw_text, 300));
            return Err(format!("OpenAI-Compatible API error ({status}): {err_msg}"));
        }

        if was_cancelled {
            break;
        }

        let mut resp = resp_opt.expect("response present on success");
        let mut step_acc = OpenAiSseAccumulator::default();
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
                                process_openai_sse_raw_line(&mut step_acc, &raw_line, &app_handle);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => return Err(format!("Streaming terputus: {e}")),
                    }
                }
            }
        }

        if !was_cancelled && !byte_buf.is_empty() {
            process_openai_sse_raw_line(&mut step_acc, &byte_buf, &app_handle);
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

        if step_acc.tool_calls.is_empty() {
            let assistant_msg = json!({
                "role": "assistant",
                "content": step_acc.text_buf
            });
            history.push(assistant_msg);
            final_reply = if step_acc.text_buf.trim().is_empty() {
                "(Selesai tanpa teks tambahan)".to_string()
            } else {
                step_acc.text_buf
            };
            break;
        }

        let tool_calls_json: Vec<Value> = step_acc
            .tool_calls
            .iter()
            .map(|(idx, tc)| {
                let id = if tc.id.is_empty() {
                    format!("call_{step_idx}_{idx}")
                } else {
                    tc.id.clone()
                };
                json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": if tc.arguments.trim().is_empty() { "{}".to_string() } else { tc.arguments.clone() }
                    }
                })
            })
            .collect();

        history.push(json!({
            "role": "assistant",
            "content": if step_acc.text_buf.is_empty() { Value::Null } else { Value::String(step_acc.text_buf) },
            "tool_calls": tool_calls_json
        }));

        for tc_val in &tool_calls_json {
            let tc_id = tc_val["id"].as_str().unwrap_or("call_0");
            let tname = tc_val["function"]["name"].as_str().unwrap_or("");
            let args_raw = tc_val["function"]["arguments"].as_str().unwrap_or("{}");
            let args: Value = serde_json::from_str(args_raw).unwrap_or_else(|_| json!({}));

            if let Some(app) = &app_handle {
                let _ = app.emit("agent-step", format!("Menjalankan tool: {tname}..."));
            }

            let out_opt = tokio::select! {
                _ = state.agent_cancel.notified() => {
                    was_cancelled = true;
                    None
                }
                out = execute_any_tool(state, "LocalAgent", tname, args) => Some(out),
            };
            let Some(out) = out_opt else {
                break;
            };

            let tokens_saved = {
                let logs = state.audit_logs.read().await;
                logs.last().map(|l| l.tokens_saved).unwrap_or(0)
            };
            steps.push(ToolStepTrace {
                tool_name: tname.to_string(),
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

            history.push(json!({
                "role": "tool",
                "tool_call_id": tc_id,
                "content": out.text
            }));
        }

        if was_cancelled {
            break;
        }
        if step_idx == 7 {
            final_reply = "Batas maksimum 8 langkah tool per giliran tercapai. Kirim pesan lanjutan untuk melanjutkan.".to_string();
        }
    }

    if was_cancelled {
        final_reply = if last_streamed_text.trim().is_empty() {
            "(Dibatalkan oleh user)".to_string()
        } else {
            format!(
                "{}\n\n[Dibatalkan oleh user]",
                last_streamed_text.trim_end()
            )
        };
    } else {
        prune_openai_history(&mut history, 36);
        *state.openai_history.write().await = history;
    }

    Ok(ProviderTurnOutcome {
        reply: final_reply,
        steps,
        usage: total_usage,
        cancelled: was_cancelled,
    })
}

// ============================================================================
// Anthropic Messages API Streaming
// ============================================================================

#[derive(Debug, Clone)]
pub enum AnthropicBlockAcc {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
}

#[derive(Debug, Default)]
pub struct AnthropicSseAccumulator {
    pub blocks: BTreeMap<usize, AnthropicBlockAcc>,
    pub text_buf: String,
    pub thought_buf: String,
    pub usage: TokenUsage,
    pub api_error: Option<String>,
}

pub fn process_anthropic_sse_data_line(
    acc: &mut AnthropicSseAccumulator,
    data_json: &str,
) -> Result<(String, String), String> {
    let trimmed = data_json.trim();
    if trimmed.is_empty() {
        return Ok((String::new(), String::new()));
    }
    let val: Value =
        serde_json::from_str(trimmed).map_err(|e| format!("Anthropic SSE JSON parse error: {e}"))?;
    let evt_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut text_delta = String::new();
    let mut thought_delta = String::new();

    match evt_type {
        "message_start" => {
            if let Some(u) = val["message"].get("usage") {
                if let Some(inp) = u["input_tokens"].as_u64() {
                    acc.usage.prompt_tokens = inp;
                }
                if let Some(cached) = u["cache_read_input_tokens"].as_u64() {
                    acc.usage.cached_tokens = cached;
                }
            }
        }
        "content_block_start" => {
            let idx = val.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let cb = &val["content_block"];
            let ctype = cb.get("type").and_then(|v| v.as_str()).unwrap_or("text");
            let block = match ctype {
                "thinking" => {
                    let init_th = cb.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                    if !init_th.is_empty() {
                        acc.thought_buf.push_str(init_th);
                        thought_delta.push_str(init_th);
                    }
                    AnthropicBlockAcc::Thinking(init_th.to_string())
                }
                "tool_use" => {
                    let id = cb.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = cb
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let init_json = cb
                        .get("input")
                        .filter(|v| v.is_object() && !v.as_object().map(|m| m.is_empty()).unwrap_or(true))
                        .and_then(|v| serde_json::to_string(v).ok())
                        .unwrap_or_default();
                    AnthropicBlockAcc::ToolUse {
                        id,
                        name,
                        input_json: init_json,
                    }
                }
                _ => {
                    let init_txt = cb.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if !init_txt.is_empty() {
                        acc.text_buf.push_str(init_txt);
                        text_delta.push_str(init_txt);
                    }
                    AnthropicBlockAcc::Text(init_txt.to_string())
                }
            };
            acc.blocks.insert(idx, block);
        }
        "content_block_delta" => {
            let idx = val.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let delta = &val["delta"];
            let dtype = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match dtype {
                "text_delta" => {
                    if let Some(txt) = delta.get("text").and_then(|v| v.as_str()) {
                        acc.text_buf.push_str(txt);
                        text_delta.push_str(txt);
                        if let Some(AnthropicBlockAcc::Text(buf)) = acc.blocks.get_mut(&idx) {
                            buf.push_str(txt);
                        }
                    }
                }
                "thinking_delta" => {
                    if let Some(th) = delta.get("thinking").and_then(|v| v.as_str()) {
                        acc.thought_buf.push_str(th);
                        thought_delta.push_str(th);
                        if let Some(AnthropicBlockAcc::Thinking(buf)) = acc.blocks.get_mut(&idx) {
                            buf.push_str(th);
                        }
                    }
                }
                "input_json_delta" => {
                    if let Some(part) = delta.get("partial_json").and_then(|v| v.as_str()) {
                        if let Some(AnthropicBlockAcc::ToolUse { input_json, .. }) =
                            acc.blocks.get_mut(&idx)
                        {
                            input_json.push_str(part);
                        }
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            if let Some(out_tok) = val["usage"]["output_tokens"].as_u64() {
                acc.usage.output_tokens = out_tok;
                acc.usage.total_tokens = acc.usage.prompt_tokens + out_tok;
            }
        }
        "error" => {
            let msg = val["error"]["message"]
                .as_str()
                .unwrap_or("Unknown Anthropic stream error")
                .to_string();
            acc.api_error = Some(msg);
        }
        _ => {}
    }

    Ok((text_delta, thought_delta))
}

fn process_anthropic_sse_raw_line(
    acc: &mut AnthropicSseAccumulator,
    raw_line: &[u8],
    app_handle: &Option<tauri::AppHandle>,
) {
    let line_str = String::from_utf8_lossy(raw_line);
    let trimmed = line_str.trim_end_matches(&['\r', '\n'][..]);
    if let Some(data_payload) = trimmed.strip_prefix("data:") {
        let data_trimmed = data_payload.trim();
        if !data_trimmed.is_empty() {
            if let Ok((text_delta, thought_delta)) =
                process_anthropic_sse_data_line(acc, data_trimmed)
            {
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

fn is_clean_anthropic_user_turn(entry: &Value) -> bool {
    if entry.get("role").and_then(|v| v.as_str()) != Some("user") {
        return false;
    }
    if let Some(arr) = entry.get("content").and_then(|v| v.as_array()) {
        return !arr
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
    }
    true
}

fn prune_anthropic_history(history: &mut Vec<Value>, max_entries: usize) {
    if history.len() <= max_entries || max_entries == 0 {
        return;
    }
    let min_cut = history.len() - max_entries;
    if let Some(idx) = (min_cut..history.len()).find(|&i| is_clean_anthropic_user_turn(&history[i])) {
        history.drain(0..idx);
        return;
    }
    if let Some(idx) = (1..min_cut).rev().find(|&i| is_clean_anthropic_user_turn(&history[i])) {
        history.drain(0..idx);
    }
}

pub async fn run_anthropic_turn(
    state: &AppState,
    prompt: String,
    attachments: Vec<AgentAttachment>,
    system_prompt: String,
    tools: Vec<Value>,
    app_handle: Option<tauri::AppHandle>,
) -> Result<ProviderTurnOutcome, String> {
    let model = state.config.read().await.anthropic_model.trim().to_string();
    let api_key = state.provider_secrets.read().await.anthropic_api_key.clone();
    if api_key.trim().is_empty() {
        return Err("Anthropic API Key belum diisi.".to_string());
    }
    if model.is_empty() {
        return Err("Model Anthropic belum diisi.".to_string());
    }

    let attachment_text = build_attachment_text_context(state, &attachments).await?;
    let full_prompt = if attachment_text.is_empty() {
        prompt.clone()
    } else {
        format!("{prompt}\n\n{attachment_text}")
    };

    let mut content_blocks = vec![json!({ "type": "text", "text": full_prompt })];
    for att in &attachments {
        if att.kind == "image" {
            if let Some(b64) = att.data_base64.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                let mime = att.mime_type.as_deref().unwrap_or("image/png");
                content_blocks.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": mime,
                        "data": b64
                    }
                }));
            }
        }
    }

    let mut history = state.anthropic_history.read().await.clone();
    history.push(json!({
        "role": "user",
        "content": content_blocks
    }));

    let anthropic_tools: Vec<Value> = tools
        .into_iter()
        .map(|t| {
            json!({
                "name": t["name"],
                "description": t["description"],
                "input_schema": t["inputSchema"]
            })
        })
        .collect();

    let client = reqwest::Client::new();
    let url = "https://api.anthropic.com/v1/messages";
    let mut steps = Vec::new();
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
            "model": model,
            "max_tokens": 4096,
            "system": system_prompt,
            "tools": anthropic_tools,
            "messages": history,
            "stream": true
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
                    .post(url)
                    .header("x-api-key", api_key.trim())
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send() => res,
            };

            let resp = send_res.map_err(|e| format!("Koneksi ke Anthropic API gagal: {e}"))?;
            let status = resp.status();
            if status.is_success() {
                resp_opt = Some(resp);
                break;
            }

            let raw_text = resp
                .text()
                .await
                .map_err(|e| format!("Gagal membaca respons Anthropic API: {e}"))?;

            if (status.as_u16() == 429 || status.as_u16() == 529 || status.as_u16() == 503)
                && attempt < BACKOFF_SECS.len()
            {
                let wait_secs = BACKOFF_SECS[attempt];
                attempt += 1;
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
                .unwrap_or_else(|| truncate_str_chars(&raw_text, 300));
            return Err(format!("Anthropic API error ({status}): {err_msg}"));
        }

        if was_cancelled {
            break;
        }

        let mut resp = resp_opt.expect("response present on success");
        let mut step_acc = AnthropicSseAccumulator::default();
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
                                process_anthropic_sse_raw_line(&mut step_acc, &raw_line, &app_handle);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => return Err(format!("Streaming Anthropic terputus: {e}")),
                    }
                }
            }
        }

        if !was_cancelled && !byte_buf.is_empty() {
            process_anthropic_sse_raw_line(&mut step_acc, &byte_buf, &app_handle);
        }
        if let Some(err) = step_acc.api_error {
            return Err(format!("Anthropic API stream error: {err}"));
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

        let mut assistant_content = Vec::new();
        let mut tool_uses: Vec<(String, String, Value)> = Vec::new();
        for (_, block) in &step_acc.blocks {
            match block {
                AnthropicBlockAcc::Text(txt) if !txt.is_empty() => {
                    assistant_content.push(json!({ "type": "text", "text": txt }));
                }
                AnthropicBlockAcc::ToolUse {
                    id,
                    name,
                    input_json,
                } => {
                    let parsed_input: Value = if input_json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(input_json).unwrap_or_else(|_| json!({}))
                    };
                    assistant_content.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": parsed_input
                    }));
                    tool_uses.push((id.clone(), name.clone(), parsed_input));
                }
                _ => {}
            }
        }

        if !assistant_content.is_empty() {
            history.push(json!({
                "role": "assistant",
                "content": assistant_content
            }));
        }

        if tool_uses.is_empty() {
            final_reply = if step_acc.text_buf.trim().is_empty() {
                "(Selesai tanpa teks tambahan)".to_string()
            } else {
                step_acc.text_buf
            };
            break;
        }

        let mut tool_result_blocks = Vec::new();
        for (tu_id, tname, args) in tool_uses {
            if let Some(app) = &app_handle {
                let _ = app.emit("agent-step", format!("Menjalankan tool: {tname}..."));
            }
            let out_opt = tokio::select! {
                _ = state.agent_cancel.notified() => {
                    was_cancelled = true;
                    None
                }
                out = execute_any_tool(state, "LocalAgent", &tname, args) => Some(out),
            };
            let Some(out) = out_opt else {
                break;
            };

            let tokens_saved = {
                let logs = state.audit_logs.read().await;
                logs.last().map(|l| l.tokens_saved).unwrap_or(0)
            };
            steps.push(ToolStepTrace {
                tool_name: tname,
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
            tool_result_blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": tu_id,
                "is_error": out.is_error,
                "content": out.text
            }));
        }

        if was_cancelled {
            break;
        }
        history.push(json!({
            "role": "user",
            "content": tool_result_blocks
        }));

        if step_idx == 7 {
            final_reply = "Batas maksimum 8 langkah tool per giliran tercapai. Kirim pesan lanjutan untuk melanjutkan.".to_string();
        }
    }

    if was_cancelled {
        final_reply = if last_streamed_text.trim().is_empty() {
            "(Dibatalkan oleh user)".to_string()
        } else {
            format!(
                "{}\n\n[Dibatalkan oleh user]",
                last_streamed_text.trim_end()
            )
        };
    } else {
        prune_anthropic_history(&mut history, 30);
        *state.anthropic_history.write().await = history;
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

    #[test]
    fn test_sanitize_schema_for_gemini() {
        let raw = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Demo",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": ["string", "null"],
                    "default": "abc",
                    "description": "Query string"
                }
            }
        });
        let cleaned = sanitize_schema_for_gemini(&raw);
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned.get("additionalProperties").is_none());
        assert!(cleaned.get("title").is_none());
        assert_eq!(cleaned["properties"]["query"]["type"], "string");
        assert!(cleaned["properties"]["query"].get("default").is_none());
    }

    #[test]
    fn test_openai_sse_delta_accumulation() {
        let mut acc = OpenAiSseAccumulator::default();
        let c1 = r#"{"choices":[{"index":0,"delta":{"reasoning_content":"Check ","content":""}}]}"#;
        let (t1, th1) = process_openai_sse_data_line(&mut acc, c1).unwrap();
        assert_eq!(t1, "");
        assert_eq!(th1, "Check ");

        let c2 = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_99","function":{"name":"smart_","arguments":"{\"query\":"}}]}}]}"#;
        process_openai_sse_data_line(&mut acc, c2).unwrap();

        let c3 = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"search","arguments":"\"main\"}"}}]}}],"usage":{"prompt_tokens":90,"completion_tokens":15,"total_tokens":105}}"#;
        process_openai_sse_data_line(&mut acc, c3).unwrap();

        assert_eq!(acc.thought_buf, "Check ");
        assert_eq!(acc.tool_calls.len(), 1);
        assert_eq!(acc.tool_calls[&0].id, "call_99");
        assert_eq!(acc.tool_calls[&0].name, "smart_search");
        assert_eq!(acc.tool_calls[&0].arguments, r#"{"query":"main"}"#);
        assert_eq!(acc.usage.total_tokens, 105);
    }

    #[test]
    fn test_anthropic_sse_block_parsing() {
        let mut acc = AnthropicSseAccumulator::default();
        process_anthropic_sse_data_line(
            &mut acc,
            r#"{"type":"message_start","message":{"usage":{"input_tokens":140,"cache_read_input_tokens":40}}}"#,
        )
        .unwrap();
        process_anthropic_sse_data_line(
            &mut acc,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        )
        .unwrap();
        let (_, th) = process_anthropic_sse_data_line(
            &mut acc,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Plan..."}}"#,
        )
        .unwrap();
        assert_eq!(th, "Plan...");

        process_anthropic_sse_data_line(
            &mut acc,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file_range","input":{}}}"#,
        )
        .unwrap();
        process_anthropic_sse_data_line(
            &mut acc,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/main.rs\"}"}}"#,
        )
        .unwrap();
        process_anthropic_sse_data_line(
            &mut acc,
            r#"{"type":"message_delta","usage":{"output_tokens":32}}"#,
        )
        .unwrap();

        assert_eq!(acc.thought_buf, "Plan...");
        assert_eq!(acc.usage.prompt_tokens, 140);
        assert_eq!(acc.usage.cached_tokens, 40);
        assert_eq!(acc.usage.output_tokens, 32);
        assert_eq!(acc.usage.total_tokens, 172);
        match acc.blocks.get(&1).unwrap() {
            AnthropicBlockAcc::ToolUse {
                id,
                name,
                input_json,
            } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "read_file_range");
                assert_eq!(input_json, r#"{"path":"src/main.rs"}"#);
            }
            _ => panic!("Expected ToolUse block"),
        }
    }
}
