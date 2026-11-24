use crate::jobs;
use crate::security::{compress_output, resolve_jailed_path};
use crate::state::AppState;
use crate::tools::{check_sensitive_guard, overview_text_for_resource, run_shell_raw};
use serde_json::{json, Value};
use std::fs;

pub fn resources_list() -> Value {
    json!([
        {
            "uri": "workspace://overview",
            "name": "Workspace Overview",
            "description": "Jailed directory tree and git working tree status for the active workspace.",
            "mimeType": "text/plain"
        },
        {
            "uri": "workspace://git/diff",
            "name": "Git Working Tree Diff",
            "description": "Compressed git status and unstaged working tree diff.",
            "mimeType": "text/plain"
        },
        {
            "uri": "workspace://audit/recent",
            "name": "Recent Audit Trail",
            "description": "Last 30 tool executions and rollback metadata from the current hub.",
            "mimeType": "application/json"
        },
        {
            "uri": "workspace://jobs/active",
            "name": "Background Command Jobs",
            "description": "Current and recent async background jobs managed by the hub.",
            "mimeType": "application/json"
        }
    ])
}

pub fn resource_templates_list() -> Value {
    json!([
        {
            "uriTemplate": "workspace://file/{rel_path}",
            "name": "Jailed Workspace File",
            "description": "Reads a UTF-8 file strictly inside the active jailed workspace (enforces Sensitive File Guard).",
            "mimeType": "text/plain"
        }
    ])
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub async fn read_resource(state: &AppState, uri: &str) -> Result<Value, String> {
    let root = state.workspace_root.read().await.clone();
    match uri.trim() {
        "workspace://overview" => {
            let text = overview_text_for_resource(&root).await?;
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": text
                }]
            }))
        }
        "workspace://git/diff" => {
            let (code, stdout, stderr) = run_shell_raw(&root, "git diff", 8).await?;
            let (compressed, _) = compress_output(&stdout, &stderr, 120);
            let text = format!("EXIT CODE: {code}\n{compressed}");
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": text
                }]
            }))
        }
        "workspace://audit/recent" => {
            let logs = state.audit_logs.read().await;
            let start = logs.len().saturating_sub(30);
            let text = serde_json::to_string_pretty(&logs[start..])
                .unwrap_or_else(|_| "[]".to_string());
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": text
                }]
            }))
        }
        "workspace://jobs/active" => {
            let jobs_list = jobs::list_jobs(state).await;
            let text = serde_json::to_string_pretty(&jobs_list)
                .unwrap_or_else(|_| "[]".to_string());
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": text
                }]
            }))
        }
        other => {
            if let Some(raw_rel) = other.strip_prefix("workspace://file/") {
                let rel = percent_decode(raw_rel);
                let target = resolve_jailed_path(&root, &rel)?;
                check_sensitive_guard(state, &root, &target, &rel).await?;
                let meta = fs::metadata(&target)
                    .map_err(|e| format!("Gagal membaca metadata '{rel}': {e}"))?;
                if meta.len() > 512_000 {
                    return Err(format!(
                        "File '{rel}' melebihi batas 512 KB untuk resource read. Gunakan read_file_range."
                    ));
                }
                let text = fs::read_to_string(&target)
                    .map_err(|e| format!("Gagal membaca file '{rel}': {e}"))?;
                Ok(json!({
                    "contents": [{
                        "uri": other,
                        "mimeType": "text/plain",
                        "text": text
                    }]
                }))
            } else {
                Err(format!("Resource URI tidak dikenal: {other}"))
            }
        }
    }
}

pub fn prompts_list() -> Value {
    json!([
        {
            "name": "code_review",
            "description": "Review uncommitted git changes or a specific area using token-efficient MCP tools.",
            "arguments": [
                { "name": "focus", "description": "Optional subfolder or focus area", "required": false }
            ]
        },
        {
            "name": "debug_error",
            "description": "Investigate a build/runtime failure systematically using smart_search, code_outline, and read_file_range.",
            "arguments": [
                { "name": "error_message", "description": "Error output or symptom", "required": true },
                { "name": "file_hint", "description": "Optional suspected file", "required": false }
            ]
        },
        {
            "name": "architecture_overview",
            "description": "Map the workspace structure, entry points, and module boundaries with minimal tokens.",
            "arguments": []
        },
        {
            "name": "safe_refactor",
            "description": "Plan and apply a surgical, verified refactor with rollback safety.",
            "arguments": [
                { "name": "target", "description": "Target file or module", "required": true },
                { "name": "goal", "description": "Desired refactoring goal", "required": true }
            ]
        }
    ])
}

pub async fn get_prompt(state: &AppState, name: &str, args: &Value) -> Result<Value, String> {
    let raw_root = state.workspace_root.read().await.display().to_string();
    let workspace = raw_root.strip_prefix(r"\\?\").unwrap_or(&raw_root).to_string();

    match name {
        "code_review" => {
            let focus = args
                .get("focus")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("entire working tree");
            let text = format!(
                "Review recent code changes in workspace `{workspace}` (focus: {focus}). \
                Start with `git_ops(op=\"status\")` and `git_ops(op=\"diff\")`. \
                Use `code_outline` and `read_file_range` only on touched sections, and report concrete bugs, security risks, and missing tests."
            );
            Ok(json!({
                "description": "Token-efficient code review workflow",
                "messages": [{ "role": "user", "content": { "type": "text", "text": text } }]
            }))
        }
        "debug_error" => {
            let err_msg = args
                .get("error_message")
                .and_then(|v| v.as_str())
                .unwrap_or("(no error message provided)");
            let hint = args
                .get("file_hint")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let hint_part = if hint.trim().is_empty() {
                String::new()
            } else {
                format!(" Suspect file hint: `{hint}`.")
            };
            let text = format!(
                "Debug the following issue in `{workspace}`:{hint_part}\n```\n{err_msg}\n```\n\
                Use `smart_search` and `code_outline` before reading slices via `read_file_range`. \
                Apply minimal fixes with `surgical_edit` or `patch_and_verify` (or `start_command_job` + `poll_command_job` for long test suites)."
            );
            Ok(json!({
                "description": "Systematic error debugging workflow",
                "messages": [{ "role": "user", "content": { "type": "text", "text": text } }]
            }))
        }
        "architecture_overview" => {
            let text = format!(
                "Explore `{workspace}` with minimal token usage: call `project_overview` and `find_files`, \
                then inspect key entry points with `code_outline` before slicing with `read_file_range`. \
                Summarize the architecture, data flow, and main dependencies."
            );
            Ok(json!({
                "description": "Minimal-token architecture mapping",
                "messages": [{ "role": "user", "content": { "type": "text", "text": text } }]
            }))
        }
        "safe_refactor" => {
            let target = args
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("(unspecified target)");
            let goal = args
                .get("goal")
                .and_then(|v| v.as_str())
                .unwrap_or("(unspecified goal)");
            let text = format!(
                "Refactor `{target}` in `{workspace}` with goal: {goal}. \
                First inspect symbol layout using `code_outline`, read needed ranges via `read_file_range`, \
                apply incremental edits with `surgical_edit` or `patch_and_verify`, and verify all tests pass."
            );
            Ok(json!({
                "description": "Safe verified refactor workflow",
                "messages": [{ "role": "user", "content": { "type": "text", "text": text } }]
            }))
        }
        other => Err(format!("Prompt tidak dikenal: {other}")),
    }
}
