use super::common::{
    await_with_watchdog, build_attachment_text_context, emit_stream_chunk, emit_stream_step,
    emit_stream_thought, execute_tool_calls_hybrid, finalize_provider_turn,
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
    run_ctx: &RunContext,
) {
    let line_str = String::from_utf8_lossy(raw_line);
    let trimmed = line_str.trim_end_matches(&['\r', '\n'][..]);
    if let Some(data_payload) = trimmed.strip_prefix("data:") {
        let data_trimmed = data_payload.trim();
        if !data_trimmed.is_empty() {
            if let Ok((text_delta, thought_delta)) =
                process_anthropic_sse_data_line(acc, data_trimmed)
            {
                emit_stream_thought(app_handle, run_ctx, &thought_delta);
                emit_stream_chunk(app_handle, run_ctx, &text_delta);
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
    let model = state.config.read().await.anthropic_model.trim().to_string();
    let api_key = state.provider_secrets.read().await.anthropic_api_key.clone();
    if api_key.trim().is_empty() {
        return Err("Anthropic API Key belum diisi.".to_string());
    }
    if model.is_empty() {
        return Err("Model Anthropic belum diisi.".to_string());
    }

    let mut history = if run_ctx.depth > 0 {
        Vec::new()
    } else {
        let per_sess = state
            .agent_manager
            .get_history("anthropic", &run_ctx.session_id)
            .await;
        if per_sess.is_empty() && run_ctx.session_id == "default" {
            state.anthropic_history.read().await.clone()
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
                "content": [{ "type": "text", "text": "Continue from the last completed step and finish the task." }]
            }));
        }
    } else {
        let attachment_text = build_attachment_text_context(&state, &attachments).await?;
        let full_prompt = if attachment_text.is_empty() {
            prompt.clone()
        } else {
            format!("{prompt}\n\n{attachment_text}")
        };

        let mut content_blocks = vec![json!({ "type": "text", "text": full_prompt })];
        for att in &attachments {
            if att.kind == "image" {
                if let Some(b64) = att
                    .data_base64
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
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
        history.push(json!({
            "role": "user",
            "content": content_blocks
        }));
    }

    let mut last_clean_history = history.clone();
    let watchdog = load_watchdog_settings(&state).await;
    let max_steps = if run_ctx.depth > 0 {
        watchdog.max_steps.min(10)
    } else if run_ctx.execution_mode.starts_with("delegated_") {
        state
            .agent_manager
            .get_run_max_steps(&run_ctx.run_id)
            .await
            .unwrap_or_else(|| watchdog.max_steps.clamp(1, 20))
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

    let client = &state.http_client;
    let url = "https://api.anthropic.com/v1/messages";
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
            let send_fut = client
                .post(url)
                .header("x-api-key", api_key.trim())
                .header("anthropic-version", "2023-06-01")
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
            return Err(format!("Anthropic API error ({status}): {err_msg}"));
        }

        if interrupt_opt.is_some() {
            break;
        }

        let mut resp = resp_opt.expect("response present on success");
        let mut step_acc = AnthropicSseAccumulator::default();
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
                        process_anthropic_sse_raw_line(
                            &mut step_acc,
                            &raw_line,
                            &app_handle,
                            &run_ctx,
                        );
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => return Err(format!("Streaming Anthropic terputus: {e}")),
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
            process_anthropic_sse_raw_line(&mut step_acc, &byte_buf, &app_handle, &run_ctx);
        }
        if let Some(err) = step_acc.api_error {
            return Err(format!("Anthropic API stream error: {err}"));
        }

        let mut assistant_content = Vec::new();
        let mut pending_calls: Vec<PendingToolCall> = Vec::new();
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
                    pending_calls.push(PendingToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        args: parsed_input,
                    });
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

        if pending_calls.is_empty() {
            last_clean_history = history.clone();
            final_reply = if step_acc.text_buf.trim().is_empty() {
                "(Selesai tanpa teks tambahan)".to_string()
            } else {
                step_acc.text_buf
            };
            break;
        }

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
                let mut tool_result_blocks = Vec::with_capacity(exec_results.len());
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
                    tool_result_blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": item.id,
                        "is_error": item.output.is_error,
                        "content": item.output.text
                    }));
                }
                history.push(json!({
                    "role": "user",
                    "content": tool_result_blocks
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

    if run_ctx.depth == 0 {
        prune_anthropic_history(&mut last_clean_history, 30);
    }

    Ok(finalize_provider_turn(TurnFinalizeParams {
        state: &state,
        provider_key: "anthropic",
        legacy_history: &state.anthropic_history,
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
