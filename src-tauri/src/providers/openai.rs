use super::common::{
    await_with_watchdog, build_attachment_text_context, emit_stream_chunk, emit_stream_step,
    emit_stream_thought, execute_tool_calls_hybrid, finalize_provider_turn, is_local_url,
    load_watchdog_settings, truncate_str_chars, wait_retry_backoff, AgentAttachment,
    PendingToolCall, ProviderTurnOutcome, TurnFinalizeParams, WatchdogInterrupt,
};
use crate::agent_manager::RunContext;
use crate::local_agent::{GeminiModelInfo, TokenUsage, ToolStepTrace};
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;

pub async fn fetch_openai_compat_models(
    client: &reqwest::Client,
    base_url: String,
    api_key: String,
) -> Result<Vec<GeminiModelInfo>, String> {
    let clean_base = base_url.trim().trim_end_matches('/');
    if clean_base.is_empty() {
        return Err("Base URL OpenAI-compatible belum diisi.".to_string());
    }
    let endpoint = format!("{clean_base}/models");

    let mut req = client
        .get(&endpoint)
        .timeout(std::time::Duration::from_secs(15));
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
    run_ctx: &RunContext,
) {
    let line_str = String::from_utf8_lossy(raw_line);
    let trimmed = line_str.trim_end_matches(&['\r', '\n'][..]);
    if let Some(data_payload) = trimmed.strip_prefix("data:") {
        let data_trimmed = data_payload.trim();
        if !data_trimmed.is_empty() && data_trimmed != "[DONE]" {
            if let Ok((text_delta, thought_delta)) = process_openai_sse_data_line(acc, data_trimmed)
            {
                emit_stream_thought(app_handle, run_ctx, &thought_delta);
                emit_stream_chunk(app_handle, run_ctx, &text_delta);
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

    let mut history = if run_ctx.depth > 0 {
        Vec::new()
    } else {
        let per_sess = state
            .agent_manager
            .get_history("openai_compat", &run_ctx.session_id)
            .await;
        if per_sess.is_empty() && run_ctx.session_id == "default" {
            state.openai_history.read().await.clone()
        } else {
            per_sess
        }
    };
    if is_resume {
        let needs_continue_prompt = history
            .last()
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            == Some("assistant")
            || history.is_empty();
        if needs_continue_prompt {
            history.push(json!({
                "role": "user",
                "content": "Continue from the last completed step and finish the task."
            }));
        }
    } else {
        let attachment_text = build_attachment_text_context(&state, &attachments).await?;
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
        history.push(user_msg);
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
    let mut awaiting_approval = false;

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

    let client = &state.http_client;
    let url = format!("{base_url}/chat/completions");
    let mut steps: Vec<ToolStepTrace> = Vec::new();
    let mut total_usage = TokenUsage::default();
    let mut final_reply = String::new();
    let mut last_streamed_text = String::new();
    let mut interrupt_opt: Option<WatchdogInterrupt> = None;
    let mut hit_max_steps = false;
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
            let send_res = match await_with_watchdog(
                req.send(),
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
                if let Err(kind) =
                    wait_retry_backoff(wait_secs, &mut cancel_rx, turn_start, turn_budget_secs)
                        .await
                {
                    interrupt_opt = Some(kind);
                    break;
                }
                continue;
            }

            let err_msg = serde_json::from_str::<Value>(&raw_text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| truncate_str_chars(&raw_text, 300));
            return Err(format!("OpenAI-Compatible API error ({status}): {err_msg}"));
        }

        if interrupt_opt.is_some() {
            break;
        }

        let mut resp = resp_opt.expect("response present on success");
        let mut step_acc = OpenAiSseAccumulator::default();
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
                        process_openai_sse_raw_line(&mut step_acc, &raw_line, &app_handle, &run_ctx);
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => return Err(format!("Streaming terputus: {e}")),
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
            process_openai_sse_raw_line(&mut step_acc, &byte_buf, &app_handle, &run_ctx);
        }

        if step_acc.tool_calls.is_empty() {
            let assistant_msg = json!({
                "role": "assistant",
                "content": step_acc.text_buf
            });
            history.push(assistant_msg);
            last_clean_history = history.clone();
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

        let pending_calls: Vec<PendingToolCall> = tool_calls_json
            .iter()
            .map(|tc_val| {
                let id = tc_val["id"].as_str().unwrap_or("call_0").to_string();
                let name = tc_val["function"]["name"].as_str().unwrap_or("").to_string();
                let args_raw = tc_val["function"]["arguments"].as_str().unwrap_or("{}");
                let args: Value = serde_json::from_str(args_raw).unwrap_or_else(|_| json!({}));
                PendingToolCall { id, name, args }
            })
            .collect();

        match execute_tool_calls_hybrid(
            state.clone(),
            "LocalAgent",
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
                for item in exec_results {
                    if item.name == "request_plan_approval" && !item.output.is_error {
                        requested_approval_msg = Some(item.output.text.clone());
                    }
                    steps.push(ToolStepTrace {
                        tool_name: item.name,
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
                    history.push(json!({
                        "role": "tool",
                        "tool_call_id": item.id,
                        "content": item.output.text
                    }));
                }
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

    if run_ctx.depth == 0 {
        prune_openai_history(&mut last_clean_history, 36);
    }

    Ok(finalize_provider_turn(TurnFinalizeParams {
        state: &state,
        provider_key: "openai_compat",
        legacy_history: &state.openai_history,
        run_ctx: &run_ctx,
        clean_history: last_clean_history,
        steps,
        total_usage,
        last_streamed_text: &last_streamed_text,
        final_reply,
        interrupt_opt,
        hit_max_steps,
        awaiting_approval,
        turn_budget_secs,
        stream_idle_secs,
    })
    .await)
}
