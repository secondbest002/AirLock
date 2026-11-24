use crate::jobs;
use crate::security::{compress_output, resolve_jailed_path};
use crate::state::AppState;
use crate::tools::{check_sensitive_guard_for_workspace, overview_text_for_resource};
use crate::workspace::{build_workspace_summaries, resolve_workspace, ResolvedWorkspace};
use serde_json::{json, Value};
use std::fs;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub fn resources_list() -> Value {
    json!([
        {
            "uri": "workspace://list",
            "name": "Mounted Workspaces",
            "description": "JSON list of mounted enabled workspaces with aliases and guardrail settings.",
            "mimeType": "application/json"
        },
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
        },
        {
            "uriTemplate": "workspace://{alias}/overview",
            "name": "Mounted Workspace Overview",
            "description": "Jailed directory tree for a specific mounted workspace @alias.",
            "mimeType": "text/plain"
        },
        {
            "uriTemplate": "workspace://{alias}/git/diff",
            "name": "Mounted Workspace Git Diff",
            "description": "Unstaged git diff for a specific mounted workspace @alias.",
            "mimeType": "text/plain"
        },
        {
            "uriTemplate": "workspace://{alias}/file/{rel_path}",
            "name": "Mounted Workspace File",
            "description": "Reads a UTF-8 file inside a specific mounted workspace @alias (enforces Sensitive File Guard).",
            "mimeType": "text/plain"
        },
        {
            "uriTemplate": "skill://{name}",
            "name": "Agent Skill Instructions (SKILL.md)",
            "description": "Reads the full SKILL.md instructions for an enabled skill.",
            "mimeType": "text/markdown"
        },
        {
            "uriTemplate": "skill://{name}/{rel_path}",
            "name": "Agent Skill Reference Resource",
            "description": "Reads a jailed reference or asset file inside an enabled skill folder.",
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

async fn read_ws_git_diff(ws: &ResolvedWorkspace) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("diff")
        .current_dir(&ws.canon_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    let out = tokio::time::timeout(Duration::from_secs(8), cmd.output())
        .await
        .map_err(|_| "Perintah git diff timeout setelah 8 detik.".to_string())?
        .map_err(|e| format!("Gagal menjalankan git diff: {e}"))?;

    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (compressed, _) = compress_output(&stdout, &stderr, 120);
    Ok(format!("EXIT CODE: {code}\n{compressed}"))
}

async fn read_ws_file(
    state: &AppState,
    ws: &ResolvedWorkspace,
    raw_rel: &str,
) -> Result<String, String> {
    let rel = percent_decode(raw_rel);
    let target = resolve_jailed_path(&ws.canon_root, &rel)?;
    check_sensitive_guard_for_workspace(state, ws, &target, &rel).await?;
    let meta = fs::metadata(&target)
        .map_err(|e| format!("Gagal membaca metadata '{rel}': {e}"))?;
    if meta.len() > 512_000 {
        return Err(format!(
            "File '{rel}' melebihi batas 512 KB untuk resource read. Gunakan read_file_range."
        ));
    }
    fs::read_to_string(&target).map_err(|e| format!("Gagal membaca file '{rel}': {e}"))
}

#[allow(dead_code)]
pub async fn read_resource(state: &AppState, uri: &str) -> Result<Value, String> {
    read_resource_with_context(state, uri, None).await
}

pub async fn read_resource_with_context(
    state: &AppState,
    uri: &str,
    pinned_ws: Option<&str>,
) -> Result<Value, String> {
    let trimmed = uri.trim();
    match trimmed {
        "workspace://list" => {
            let text = if let Some(pin) = pinned_ws.map(str::trim).filter(|s| !s.is_empty()) {
                let ws = resolve_workspace(state, None, Some(pin)).await?;
                serde_json::to_string_pretty(&json!([{
                    "id": ws.id,
                    "alias": ws.alias,
                    "path": ws.clean_path,
                    "is_default": true,
                    "effective_guardrail": ws.effective_guardrail,
                    "pinned_connection": true
                }]))
                .unwrap_or_else(|_| "[]".to_string())
            } else {
                let (raw, cfg_def) = {
                    let cfg = state.config.read().await;
                    (cfg.workspaces.clone(), cfg.default_workspace_id.clone())
                };
                let state_def = state.default_workspace_id.read().await.clone();
                let def_id = if !state_def.is_empty() {
                    state_def
                } else {
                    cfg_def.unwrap_or_default()
                };
                let global_guard = state.guardrail_mode.read().await.clone();
                let enabled: Vec<_> = build_workspace_summaries(&raw, &def_id, &global_guard)
                    .into_iter()
                    .filter(|w| w.enabled)
                    .collect();
                serde_json::to_string_pretty(&enabled).unwrap_or_else(|_| "[]".to_string())
            };
            Ok(json!({
                "contents": [{
                    "uri": trimmed,
                    "mimeType": "application/json",
                    "text": text
                }]
            }))
        }
        "workspace://overview" => {
            let ws = resolve_workspace(state, None, pinned_ws).await?;
            let text = overview_text_for_resource(&ws.canon_root).await?;
            Ok(json!({
                "contents": [{
                    "uri": trimmed,
                    "mimeType": "text/plain",
                    "text": text
                }]
            }))
        }
        "workspace://git/diff" => {
            let ws = resolve_workspace(state, None, pinned_ws).await?;
            let text = read_ws_git_diff(&ws).await?;
            Ok(json!({
                "contents": [{
                    "uri": trimmed,
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
                    "uri": trimmed,
                    "mimeType": "application/json",
                    "text": text
                }]
            }))
        }
        "workspace://jobs/active" => {
            let mut jobs_list = jobs::list_jobs(state).await;
            if let Some(pin) = pinned_ws.map(str::trim).filter(|s| !s.is_empty()) {
                let ws = resolve_workspace(state, None, Some(pin)).await?;
                jobs_list.retain(|j| {
                    j.workspace_alias
                        .as_deref()
                        .map(|a| a.eq_ignore_ascii_case(&ws.alias))
                        .unwrap_or(false)
                });
            }
            let text = serde_json::to_string_pretty(&jobs_list)
                .unwrap_or_else(|_| "[]".to_string());
            Ok(json!({
                "contents": [{
                    "uri": trimmed,
                    "mimeType": "application/json",
                    "text": text
                }]
            }))
        }
        other => {
            if let Some(raw_rel) = other.strip_prefix("workspace://file/") {
                let ws = resolve_workspace(state, None, pinned_ws).await?;
                let text = read_ws_file(state, &ws, raw_rel).await?;
                Ok(json!({
                    "contents": [{
                        "uri": other,
                        "mimeType": "text/plain",
                        "text": text
                    }]
                }))
            } else if let Some(after_scheme) = other.strip_prefix("workspace://") {
                let Some((raw_alias, rest)) = after_scheme.split_once('/') else {
                    return Err(format!("Resource URI tidak dikenal: {other}"));
                };
                if raw_alias.is_empty() || matches!(raw_alias, "audit" | "jobs" | "file") {
                    return Err(format!("Resource URI tidak dikenal: {other}"));
                }
                let ws = resolve_workspace(state, Some(raw_alias), pinned_ws).await?;
                if rest == "overview" {
                    let text = overview_text_for_resource(&ws.canon_root).await?;
                    Ok(json!({
                        "contents": [{
                            "uri": other,
                            "mimeType": "text/plain",
                            "text": text
                        }]
                    }))
                } else if rest == "git/diff" {
                    let text = read_ws_git_diff(&ws).await?;
                    Ok(json!({
                        "contents": [{
                            "uri": other,
                            "mimeType": "text/plain",
                            "text": text
                        }]
                    }))
                } else if let Some(raw_rel) = rest.strip_prefix("file/") {
                    let text = read_ws_file(state, &ws, raw_rel).await?;
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
            } else if let Some(after_skill) = other.strip_prefix("skill://") {
                let (raw_name, rel_opt) = match after_skill.split_once('/') {
                    Some((n, r)) if !r.trim().is_empty() => (n.trim(), Some(r.trim())),
                    Some((n, _)) => (n.trim(), None),
                    None => (after_skill.trim(), None),
                };
                if raw_name.is_empty() {
                    return Err(format!("Resource URI skill tidak valid: {other}"));
                }
                let skills = state.skills_for_context(pinned_ws).await;
                let skill = skills
                    .into_iter()
                    .find(|s| {
                        s.summary.enabled
                            && s.summary.parse_error.is_none()
                            && s.summary.name.eq_ignore_ascii_case(raw_name)
                    })
                    .ok_or_else(|| format!("Skill '{raw_name}' tidak ditemukan atau nonaktif."))?;

                if let Some(raw_rel) = rel_opt {
                    let rel = percent_decode(raw_rel);
                    let text = crate::skills::read_skill_resource_file(&skill, &rel)?;
                    Ok(json!({
                        "contents": [{
                            "uri": other,
                            "mimeType": "text/plain",
                            "text": text
                        }]
                    }))
                } else {
                    Ok(json!({
                        "contents": [{
                            "uri": other,
                            "mimeType": "text/markdown",
                            "text": skill.raw_md
                        }]
                    }))
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{normalize_clean_path, WorkspaceProfile};

    #[tokio::test]
    async fn test_aliased_resource_uris() {
        let base = std::env::temp_dir().join("smart_mcp_aliased_resources_test");
        let _ = fs::remove_dir_all(&base);
        let dir_api = base.join("api");
        let dir_web = base.join("web");
        fs::create_dir_all(&dir_api).unwrap();
        fs::create_dir_all(&dir_web).unwrap();
        fs::write(dir_api.join("api_entry.rs"), "// api entry\n").unwrap();
        fs::write(dir_web.join("web_entry.ts"), "export const ui = true;\n").unwrap();
        fs::write(dir_web.join(".env"), "SECRET=123\n").unwrap();

        let (_, clean_api) = normalize_clean_path(&dir_api).unwrap();
        let (_, clean_web) = normalize_clean_path(&dir_web).unwrap();

        let state = AppState::new(dir_api.clone());
        {
            let mut cfg = state.config.write().await;
            cfg.workspaces = vec![
                WorkspaceProfile {
                    id: "ws-api".to_string(),
                    alias: "api".to_string(),
                    path: clean_api,
                    enabled: true,
                    guardrail_override: None,
                    allow_sensitive_override: None,
                    extra_sensitive_patterns: vec![],
                    pinned: false,
                },
                WorkspaceProfile {
                    id: "ws-web".to_string(),
                    alias: "web".to_string(),
                    path: clean_web,
                    enabled: true,
                    guardrail_override: None,
                    allow_sensitive_override: None,
                    extra_sensitive_patterns: vec![],
                    pinned: false,
                },
            ];
            cfg.default_workspace_id = Some("ws-api".to_string());
        }
        state.sync_default_workspace_root().await;

        // 1. workspace://list lists both workspaces
        let list_val = read_resource_with_context(&state, "workspace://list", None)
            .await
            .unwrap();
        let list_txt = list_val["contents"][0]["text"].as_str().unwrap();
        assert!(list_txt.contains("\"alias\": \"api\""));
        assert!(list_txt.contains("\"alias\": \"web\""));

        // 2. Aliased overview & file read work for @web even when default is @api
        let ov_web = read_resource_with_context(&state, "workspace://web/overview", None)
            .await
            .unwrap();
        assert!(ov_web["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("web_entry.ts"));

        let file_web =
            read_resource_with_context(&state, "workspace://web/file/web_entry.ts", None)
                .await
                .unwrap();
        assert!(file_web["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("export const ui"));

        // 3. Sensitive guard enforced on aliased file URI
        let env_err = read_resource_with_context(&state, "workspace://web/file/.env", None)
            .await
            .unwrap_err();
        assert!(env_err.contains("Sensitive File Guard"));

        // 4. Connection pinning isolates workspace://list and rejects cross-workspace URIs
        let pin_list = read_resource_with_context(&state, "workspace://list", Some("web"))
            .await
            .unwrap();
        let pin_list_txt = pin_list["contents"][0]["text"].as_str().unwrap();
        assert!(pin_list_txt.contains("\"alias\": \"web\""));
        assert!(!pin_list_txt.contains("\"alias\": \"api\""));

        let cross_err =
            read_resource_with_context(&state, "workspace://api/file/api_entry.rs", Some("web"))
                .await
                .unwrap_err();
        assert!(cross_err.contains("ditolak"));

        let _ = fs::remove_dir_all(&base);
    }
}
