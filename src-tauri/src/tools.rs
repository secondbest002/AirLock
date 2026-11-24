use crate::security::{compress_output, is_sensitive_path, resolve_jailed_path, validate_command};
use crate::state::{AppState, AuditEntry, PendingToolApproval, ToolApprovalDecision};
use crate::tools_extra;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use similar::TextDiff;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{Emitter, Manager};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::oneshot;

#[derive(Debug, Deserialize)]
struct EditOp {
    search: String,
    replace: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Debug, Deserialize)]
struct BatchReadItem {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

/// Shared tool metadata used by both MCP `tools/list` and Gemini `functionDeclarations`.
pub fn mcp_tools_list() -> Value {
    json!([
        {
            "name": "project_overview",
            "description": "Returns the jailed project directory tree (ignoring node_modules/.git/target) and short git status in 1 call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_depth": { "type": "integer", "description": "Max folder depth (default 3, max 6)" }
                }
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "smart_search",
            "description": "Fast ripgrep-style text/regex search across project files returning file paths, line numbers, and 2 lines of context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search text or regex pattern" },
                    "path": { "type": "string", "description": "Optional relative subfolder or file" },
                    "case_insensitive": { "type": "boolean", "description": "Ignore case (default false)" },
                    "max_results": { "type": "integer", "description": "Max matches (default 30)" }
                },
                "required": ["query"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "read_file_range",
            "description": "Reads specific line ranges from one file or multiple files in batch_paths with line numbers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative file path" },
                    "start_line": { "type": "integer", "description": "1-based start line (default 1)" },
                    "end_line": { "type": "integer", "description": "1-based end line (default start_line + 120)" },
                    "batch_paths": {
                        "type": "array",
                        "description": "Optional array of { path, start_line, end_line } to read multiple slices in 1 turn",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "start_line": { "type": "integer" },
                                "end_line": { "type": "integer" }
                            },
                            "required": ["path"]
                        }
                    }
                }
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "surgical_edit",
            "description": "Replaces exact search blocks inside a file without rewriting the whole file. Saves 75-85% tokens and saves a snapshot for 1-click revert.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Target relative file path" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "search": { "type": "string", "description": "Exact existing text block" },
                                "replace": { "type": "string", "description": "Replacement text block" },
                                "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)" }
                            },
                            "required": ["search", "replace"]
                        }
                    }
                },
                "required": ["path", "edits"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        },
        {
            "name": "patch_and_verify",
            "description": "Macro tool: applies surgical_edit blocks AND runs a verification command (e.g., npm test / cargo check) in a single turn.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "search": { "type": "string" },
                                "replace": { "type": "string" },
                                "replace_all": { "type": "boolean" }
                            },
                            "required": ["search", "replace"]
                        }
                    },
                    "verify_command": { "type": "string", "description": "Verification shell command to run right after patching" }
                },
                "required": ["path", "edits", "verify_command"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "run_command_compressed",
            "description": "Executes a shell command in workspace_root and compresses long logs via Token Guard, keeping error/warning lines and summary.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 18; max 22 for WebMCP, max 60 for LocalAgent/Simulator)" },
                    "max_lines": { "type": "integer", "description": "Max compressed lines (default 60)" }
                },
                "required": ["command"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "write_file_jailed",
            "description": "Creates or overwrites a file strictly inside workspace_root while saving a rollback snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "find_files",
            "description": "Fast glob-based file path discovery ignoring node_modules/.git/target/dist without reading file contents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Wildcard glob pattern (e.g. *.rs, *auth*, src/*.ts)" },
                    "path": { "type": "string", "description": "Optional relative subfolder" },
                    "max_results": { "type": "integer", "description": "Max files returned (default 80, max 300)" }
                }
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "code_outline",
            "description": "Extracts symbol declarations (functions, structs, classes, interfaces, headings) with line numbers before reading slices.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Target relative file path" },
                    "max_symbols": { "type": "integer", "description": "Max symbols (default 120, max 400)" }
                },
                "required": ["path"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "git_ops",
            "description": "Safe structured Git operations with compressed output: status, diff, diff_staged, log, branch, show, blame, stash_list, add, commit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["status", "diff", "diff_staged", "log", "branch", "show", "blame", "stash_list", "add", "commit"]
                    },
                    "path": { "type": "string", "description": "Optional relative file/folder path" },
                    "message": { "type": "string", "description": "Commit message (required for op=commit)" },
                    "target": { "type": "string", "description": "Commit/ref for op=show" },
                    "limit": { "type": "integer", "description": "Max commits for op=log (default 15, max 50)" }
                },
                "required": ["op"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        },
        {
            "name": "file_ops",
            "description": "Jailed file management (mkdir, move, delete) with snapshot backup for 1-click revert on delete/move.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["mkdir", "move", "delete"] },
                    "path": { "type": "string", "description": "Target relative path" },
                    "destination": { "type": "string", "description": "Destination relative path (required for action=move)" }
                },
                "required": ["action", "path"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": false
            }
        },
        {
            "name": "start_command_job",
            "description": "Starts a long-running shell command asynchronously in background and returns job_id immediately (<50ms) to avoid WebMCP timeouts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run in background" },
                    "label": { "type": "string", "description": "Optional short label" },
                    "timeout_secs": { "type": "integer", "description": "Max runtime seconds (default 300, max 1800)" }
                },
                "required": ["command"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "poll_command_job",
            "description": "Polls status and compressed incremental output from a background job started with start_command_job.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" },
                    "from_line": { "type": "integer", "description": "0-based line offset (default 0)" },
                    "max_lines": { "type": "integer", "description": "Max compressed lines (default 80)" }
                },
                "required": ["job_id"]
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "stop_command_job",
            "description": "Stops a running background command job and terminates its process tree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string" }
                },
                "required": ["job_id"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }
    ])
}

fn is_known_builtin_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "project_overview"
            | "smart_search"
            | "read_file_range"
            | "surgical_edit"
            | "patch_and_verify"
            | "run_command_compressed"
            | "write_file_jailed"
            | "find_files"
            | "code_outline"
            | "git_ops"
            | "file_ops"
            | "start_command_job"
            | "poll_command_job"
            | "stop_command_job"
    )
}

pub fn is_tool_read_only(tool_name: &str, args: &Value) -> bool {
    match tool_name {
        "project_overview"
        | "smart_search"
        | "read_file_range"
        | "find_files"
        | "code_outline"
        | "poll_command_job" => true,
        "git_ops" => {
            let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
            matches!(
                op,
                "status" | "diff" | "diff_staged" | "log" | "branch" | "show" | "blame" | "stash_list"
            )
        }
        _ => false,
    }
}

fn requires_hitl_in_safe_edit(tool_name: &str, args: &Value, is_read_only: bool) -> bool {
    if tool_name.contains("__") {
        return !is_read_only;
    }
    match tool_name {
        "run_command_compressed" | "patch_and_verify" | "start_command_job" => true,
        "file_ops" => args.get("action").and_then(|v| v.as_str()) == Some("delete"),
        "git_ops" => args.get("op").and_then(|v| v.as_str()) == Some("commit"),
        _ => false,
    }
}

pub async fn resolve_tool_approval(
    state: &AppState,
    request_id: &str,
    decision: ToolApprovalDecision,
) -> Result<(), String> {
    let entry = {
        let mut pending = state.pending_tool_approvals.write().await;
        pending.remove(request_id)
    };
    let Some((_, tx)) = entry else {
        return Err("Permintaan konfirmasi tool sudah kadaluarsa atau tidak ditemukan.".to_string());
    };
    let _ = tx.send(decision);
    Ok(())
}

pub async fn enforce_guardrail_gate(
    state: &AppState,
    source: &str,
    tool_name: &str,
    args: &Value,
    is_read_only: bool,
) -> Result<(), String> {
    let mode = state.guardrail_mode.read().await.clone();
    if mode == "read_only" && !is_read_only {
        return Err(format!(
            "Diblokir oleh Guardrail Mode (Read-Only): tool '{tool_name}' bersifat mutatif."
        ));
    }

    if mode == "safe_edit"
        && source != "Simulator"
        && requires_hitl_in_safe_edit(tool_name, args, is_read_only)
    {
        if state
            .session_approved_tools
            .read()
            .await
            .contains(tool_name)
        {
            return Ok(());
        }

        let request_id = format!("hitl_{}", AppState::generate_token());
        let args_pretty = serde_json::to_string_pretty(args).unwrap_or_else(|_| "{}".to_string());
        let args_preview: String = args_pretty.chars().take(1200).collect();
        let summary = format!("{source} ingin menjalankan '{tool_name}' dalam mode Safe-Edit.");

        let info = PendingToolApproval {
            request_id: request_id.clone(),
            source: source.to_string(),
            tool_name: tool_name.to_string(),
            summary,
            args_preview,
            created_at: AppState::now_timestamp(),
        };

        let (tx, rx) = oneshot::channel::<ToolApprovalDecision>();
        {
            let mut pending = state.pending_tool_approvals.write().await;
            pending.insert(request_id.clone(), (info.clone(), tx));
        }

        if let Some(app) = state.app_handle.read().await.clone() {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
            }
            let _ = app.emit("tool-approval-request", &info);
        }

        let res = tokio::time::timeout(Duration::from_secs(90), rx).await;

        {
            let mut pending = state.pending_tool_approvals.write().await;
            pending.remove(&request_id);
        }
        if let Some(app) = state.app_handle.read().await.clone() {
            let _ = app.emit("tool-approval-resolved", &request_id);
        }

        match res {
            Ok(Ok(ToolApprovalDecision::ApproveOnce)) => Ok(()),
            Ok(Ok(ToolApprovalDecision::ApproveForSession)) => {
                state
                    .session_approved_tools
                    .write()
                    .await
                    .insert(tool_name.to_string());
                Ok(())
            }
            Ok(Ok(ToolApprovalDecision::Deny)) => {
                Err(format!("Eksekusi tool '{tool_name}' ditolak oleh user (HITL)."))
            }
            Err(_) | Ok(Err(_)) => Err(format!(
                "Eksekusi tool '{tool_name}' timeout pada konfirmasi HITL (90s)."
            )),
        }
    } else {
        Ok(())
    }
}

pub async fn execute_tool(
    state: &AppState,
    source: &str,
    tool_name: &str,
    args: Value,
) -> ToolOutput {
    if !is_known_builtin_tool(tool_name) {
        let err = format!("Tool tidak dikenal: {tool_name}");
        state
            .record_audit(
                AuditEntry {
                    id: AppState::next_audit_id(),
                    timestamp: AppState::now_timestamp(),
                    source: source.to_string(),
                    tool_name: tool_name.to_string(),
                    target_path: None,
                    summary: format!("Error: {err}"),
                    diff_text: None,
                    can_revert: false,
                    reverted: false,
                    tokens_saved: 0,
                    original_content: None,
                    original_blob_hash: None,
                    file_existed_before: false,
                    post_edit_hash: None,
                    group_id: None,
                    move_from_path: None,
                },
                0,
            )
            .await;
        return ToolOutput {
            text: err,
            is_error: true,
        };
    }

    let is_ro = is_tool_read_only(tool_name, &args);
    let result = match enforce_guardrail_gate(state, source, tool_name, &args, is_ro).await {
        Err(err) => Err(err),
        Ok(()) => {
            let root = state.workspace_root.read().await.clone();
            match tool_name {
                "project_overview" => exec_project_overview(state, source, &root, &args).await,
                "smart_search" => exec_smart_search(state, source, &root, &args).await,
                "read_file_range" => exec_read_file_range(state, source, &root, &args).await,
                "surgical_edit" => exec_surgical_edit(state, source, &root, &args).await,
                "patch_and_verify" => exec_patch_and_verify(state, source, &root, &args).await,
                "run_command_compressed" => exec_run_command(state, source, &root, &args).await,
                "write_file_jailed" => exec_write_file(state, source, &root, &args).await,
                "find_files" => tools_extra::exec_find_files(state, source, &root, &args).await,
                "code_outline" => tools_extra::exec_code_outline(state, source, &root, &args).await,
                "git_ops" => tools_extra::exec_git_ops(state, source, &root, &args).await,
                "file_ops" => tools_extra::exec_file_ops(state, source, &root, &args).await,
                "start_command_job" => {
                    tools_extra::exec_start_command_job(state, source, &root, &args).await
                }
                "poll_command_job" => tools_extra::exec_poll_command_job(state, &args).await,
                "stop_command_job" => {
                    tools_extra::exec_stop_command_job(state, source, &args).await
                }
                other => Err(format!("Tool tidak dikenal: {other}")),
            }
        }
    };

    match result {
        Ok(text) => ToolOutput {
            text,
            is_error: false,
        },
        Err(err) => {
            state
                .record_audit(
                    AuditEntry {
                        id: AppState::next_audit_id(),
                        timestamp: AppState::now_timestamp(),
                        source: source.to_string(),
                        tool_name: tool_name.to_string(),
                        target_path: args
                            .get("path")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        summary: format!("Error: {err}"),
                        diff_text: None,
                        can_revert: false,
                        reverted: false,
                        tokens_saved: 0,
                        original_content: None,
                        original_blob_hash: None,
                        file_existed_before: false,
                        post_edit_hash: None,
                        group_id: None,
                        move_from_path: None,
                    },
                    0,
                )
                .await;
            ToolOutput {
                text: err,
                is_error: true,
            }
        }
    }
}

pub(crate) async fn current_turn_group_id(state: &AppState) -> Option<String> {
    state.active_turn_group_id.read().await.clone()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) async fn check_sensitive_guard(
    state: &AppState,
    root: &Path,
    target: &Path,
    rel_label: &str,
) -> Result<(), String> {
    if state.allow_sensitive_reads.load(Ordering::SeqCst) {
        return Ok(());
    }
    let patterns = state.config.read().await.sensitive_deny_patterns.clone();
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let rel = target.strip_prefix(&canon_root).unwrap_or(target);
    if let Some(pat) = is_sensitive_path(rel, &patterns) {
        return Err(format!(
            "Akses ditolak oleh Sensitive File Guard ('{rel_label}' cocok pola '{pat}'). Aktifkan 'Allow Sensitive Files' di dashboard jika memang diperlukan."
        ));
    }
    let full_slash = target.to_string_lossy().replace('\\', "/").to_lowercase();
    if full_slash.ends_with(".smart-tauri-hub/auth.json") {
        return Err(format!(
            "Akses ditolak oleh Sensitive File Guard ('{rel_label}' mengarah ke .smart-tauri-hub/auth.json)."
        ));
    }
    Ok(())
}

fn is_binary_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "wav"
            | "mp3"
            | "ogg"
            | "flac"
            | "mp4"
            | "avi"
            | "mkv"
            | "mov"
            | "mdl"
            | "spr"
            | "bsp"
            | "wad"
            | "pak"
            | "vpk"
            | "gcf"
            | "dem"
            | "dll"
            | "exe"
            | "so"
            | "dylib"
            | "sys"
            | "o"
            | "obj"
            | "lib"
            | "a"
            | "pdb"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "bmp"
            | "tga"
            | "ico"
            | "webp"
            | "psd"
            | "dds"
            | "zip"
            | "rar"
            | "7z"
            | "tar"
            | "gz"
            | "bz2"
            | "xz"
            | "pdf"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "bin"
            | "dat"
            | "db"
            | "sqlite"
            | "class"
            | "jar"
            | "pyc"
            | "wasm"
            | "ttf"
            | "otf"
            | "woff"
            | "woff2"
    )
}

async fn exec_project_overview(
    state: &AppState,
    source: &str,
    root: &Path,
    args: &Value,
) -> Result<String, String> {
    let max_depth = args
        .get("max_depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(3)
        .clamp(1, 6) as usize;

    let root_owned = root.to_path_buf();
    let entries = tokio::task::spawn_blocking(move || {
        use std::collections::HashMap;
        let start = std::time::Instant::now();
        let mut out = Vec::new();
        let mut dir_file_counts: HashMap<PathBuf, usize> = HashMap::new();
        let mut dir_omitted_counts: HashMap<PathBuf, usize> = HashMap::new();
        let mut walked = 0usize;
        let mut stopped_reason: Option<&'static str> = None;

        let walker = WalkBuilder::new(&root_owned)
            .max_depth(Some(max_depth))
            .hidden(false)
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                name != ".git" && name != "node_modules" && name != "target" && name != "dist"
            })
            .build();

        for entry in walker.flatten() {
            walked += 1;
            if walked > 2500 {
                stopped_reason = Some("2500 entry scan cap");
                break;
            }
            if walked % 128 == 0 && start.elapsed() > Duration::from_secs(5) {
                stopped_reason = Some("5s scan time budget");
                break;
            }

            let path = entry.path();
            if path == root_owned {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(&root_owned) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if path.is_dir() {
                    out.push(format!("{rel_str}/"));
                } else {
                    let parent = path.parent().unwrap_or(&root_owned).to_path_buf();
                    let count = dir_file_counts.entry(parent.clone()).or_insert(0);
                    *count += 1;
                    if *count <= 25 {
                        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                        out.push(format!("{rel_str} ({size} B)"));
                    } else {
                        *dir_omitted_counts.entry(parent).or_insert(0) += 1;
                    }
                }
            }
            if out.len() >= 200 {
                stopped_reason = Some("200 displayed entries cap");
                break;
            }
        }

        for (dir, omitted) in dir_omitted_counts {
            if omitted > 0 {
                let rel_dir = dir
                    .strip_prefix(&root_owned)
                    .unwrap_or(&dir)
                    .to_string_lossy()
                    .replace('\\', "/");
                let label = if rel_dir.is_empty() { "." } else { &rel_dir };
                out.push(format!("... (+{omitted} more files in {label})"));
            }
        }
        if let Some(reason) = stopped_reason {
            out.push(format!("... [overview capped at {reason}]"));
        }
        out
    })
    .await
    .map_err(|e| format!("Gagal memindai direktori: {e}"))?;

    let git_status = run_shell_raw(root, "git status -s", 5)
        .await
        .map(|(code, out, _)| {
            if code == 0 && !out.trim().is_empty() {
                out.trim().to_string()
            } else if code == 0 {
                "(clean working tree)".to_string()
            } else {
                "(not a git repository)".to_string()
            }
        })
        .unwrap_or_else(|_| "(git unavailable)".to_string());

    let display_root = root.display().to_string();
    let clean_root = display_root.strip_prefix(r"\\?\").unwrap_or(&display_root);
    let output = format!(
        "WORKSPACE: {}\nGIT STATUS:\n{}\n\nTREE ({} entries):\n{}",
        clean_root,
        git_status,
        entries.len(),
        entries.join("\n")
    );

    let tokens_saved = 350;
    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "project_overview".to_string(),
                target_path: None,
                summary: format!("Listed {} project entries + git status", entries.len()),
                diff_text: None,
                can_revert: false,
                reverted: false,
                tokens_saved,
                original_content: None,
                original_blob_hash: None,
                file_existed_before: false,
                post_edit_hash: None,
                group_id: None,
                move_from_path: None,
            },
            2,
        )
        .await;

    Ok(output)
}

pub(crate) async fn overview_text_for_resource(root: &Path) -> Result<String, String> {
    let root_owned = root.to_path_buf();
    let entries = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let walker = WalkBuilder::new(&root_owned)
            .max_depth(Some(3))
            .hidden(false)
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                name != ".git" && name != "node_modules" && name != "target" && name != "dist"
            })
            .build();
        for entry in walker.flatten() {
            let path = entry.path();
            if path == root_owned {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(&root_owned) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if path.is_dir() {
                    out.push(format!("{rel_str}/"));
                } else {
                    out.push(rel_str);
                }
            }
            if out.len() >= 200 {
                break;
            }
        }
        out
    })
    .await
    .map_err(|e| format!("Gagal memindai overview: {e}"))?;

    let git_status = run_shell_raw(root, "git status -s", 5)
        .await
        .map(|(code, out, _)| {
            if code == 0 && !out.trim().is_empty() {
                out.trim().to_string()
            } else if code == 0 {
                "(clean working tree)".to_string()
            } else {
                "(not a git repository)".to_string()
            }
        })
        .unwrap_or_else(|_| "(git unavailable)".to_string());

    let display_root = root.display().to_string();
    let clean_root = display_root.strip_prefix(r"\\?\").unwrap_or(&display_root);
    Ok(format!(
        "WORKSPACE: {clean_root}\nGIT STATUS:\n{git_status}\n\nTREE ({} entries):\n{}",
        entries.len(),
        entries.join("\n")
    ))
}

async fn exec_smart_search(
    state: &AppState,
    source: &str,
    root: &Path,
    args: &Value,
) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'query' wajib diisi.")?;
    if query.trim().is_empty() {
        return Err("Query pencarian tidak boleh kosong.".to_string());
    }

    let search_root = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => resolve_jailed_path(root, p)?,
        _ => resolve_jailed_path(root, ".")?,
    };
    let case_insensitive = args
        .get("case_insensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .clamp(1, 100) as usize;

    let regex = RegexBuilder::new(query)
        .case_insensitive(case_insensitive)
        .build()
        .or_else(|_| {
            RegexBuilder::new(&regex::escape(query))
                .case_insensitive(case_insensitive)
                .build()
        })
        .map_err(|e| format!("Regex tidak valid: {e}"))?;

    let allow_sensitive = state.allow_sensitive_reads.load(Ordering::SeqCst);
    let sensitive_patterns = state.config.read().await.sensitive_deny_patterns.clone();
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let (matches_out, files_scanned, total_scanned_bytes, was_capped, sensitive_skipped) =
        tokio::task::spawn_blocking(move || {
            let start = std::time::Instant::now();
            let mut matches_out = Vec::new();
            let mut files_scanned = 0usize;
            let mut total_scanned_bytes = 0usize;
            let mut was_capped = false;
            let mut sensitive_skipped = 0usize;

            let walker = WalkBuilder::new(&search_root)
                .filter_entry(|e| {
                    let name = e.file_name().to_string_lossy();
                    name != ".git" && name != "node_modules" && name != "target" && name != "dist"
                })
                .build();

            for entry in walker.flatten() {
                if files_scanned >= 3000 {
                    was_capped = true;
                    break;
                }
                if files_scanned % 64 == 0 && start.elapsed() > Duration::from_secs(8) {
                    was_capped = true;
                    break;
                }

                let path = entry.path();
                if !path.is_file() || is_binary_extension(path) {
                    continue;
                }
                let rel_path_obj = path.strip_prefix(&canon_root).unwrap_or(path);
                if !allow_sensitive && is_sensitive_path(rel_path_obj, &sensitive_patterns).is_some()
                {
                    sensitive_skipped += 1;
                    continue;
                }

                let meta = match entry.metadata() {
                    Ok(m) if m.len() <= 512_000 => m,
                    _ => continue,
                };

                let raw_bytes = match fs::read(path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let probe_len = raw_bytes.len().min(512);
                if raw_bytes[..probe_len].contains(&0u8) {
                    continue;
                }
                let content = match String::from_utf8(raw_bytes) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                files_scanned += 1;
                total_scanned_bytes += meta.len() as usize;
                let lines: Vec<&str> = content.lines().collect();
                let rel = rel_path_obj.to_string_lossy().replace('\\', "/");

                for (idx, line) in lines.iter().enumerate() {
                    if regex.is_match(line) {
                        let start_l = idx.saturating_sub(2);
                        let end_l = (idx + 3).min(lines.len());
                        let mut snippet = vec![format!("--- {} (line {}) ---", rel, idx + 1)];
                        for l_idx in start_l..end_l {
                            let marker = if l_idx == idx { ">" } else { " " };
                            snippet.push(format!("{marker} {:>4} | {}", l_idx + 1, lines[l_idx]));
                        }
                        matches_out.push(snippet.join("\n"));
                        if matches_out.len() >= max_results {
                            break;
                        }
                    }
                }
                if matches_out.len() >= max_results {
                    break;
                }
            }

            (
                matches_out,
                files_scanned,
                total_scanned_bytes,
                was_capped,
                sensitive_skipped,
            )
        })
        .await
        .map_err(|e| format!("Gagal memindai teks: {e}"))?;

    let cap_note = if was_capped {
        "\n\n[Scan capped for responsiveness — specify 'path' subfolder to search deeper]"
    } else {
        ""
    };
    let guard_note = if sensitive_skipped > 0 {
        format!("\n\n[Sensitive File Guard: {sensitive_skipped} file sensitif dilewati]")
    } else {
        String::new()
    };

    let output = if matches_out.is_empty() {
        format!(
            "Tidak ditemukan kecocokan untuk '{query}' ({files_scanned} file teks dipindai).{cap_note}{guard_note}"
        )
    } else {
        format!(
            "Ditemukan {} kecocokan dari {} file teks:\n\n{}{}{}",
            matches_out.len(),
            files_scanned,
            matches_out.join("\n\n"),
            cap_note,
            guard_note
        )
    };

    let tokens_saved = (total_scanned_bytes / 4).saturating_sub(output.len() / 4);
    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "smart_search".to_string(),
                target_path: args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                summary: format!(
                    "Query '{}' -> {} match(es) across {} files",
                    query,
                    matches_out.len(),
                    files_scanned
                ),
                diff_text: None,
                can_revert: false,
                reverted: false,
                tokens_saved,
                original_content: None,
                original_blob_hash: None,
                file_existed_before: false,
                post_edit_hash: None,
                group_id: None,
                move_from_path: None,
            },
            1,
        )
        .await;

    Ok(output)
}

async fn exec_read_file_range(
    state: &AppState,
    source: &str,
    root: &Path,
    args: &Value,
) -> Result<String, String> {
    let mut items: Vec<BatchReadItem> = Vec::new();
    if let Some(batch) = args.get("batch_paths") {
        items = serde_json::from_value(batch.clone())
            .map_err(|e| format!("Format batch_paths tidak valid: {e}"))?;
    } else if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        items.push(BatchReadItem {
            path: path.to_string(),
            start_line: args
                .get("start_line")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize),
            end_line: args
                .get("end_line")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize),
        });
    } else {
        return Err("Wajib mengisi 'path' atau 'batch_paths'.".to_string());
    }

    let mut sections = Vec::new();
    let mut total_file_bytes = 0usize;

    for item in &items {
        let target = resolve_jailed_path(root, &item.path)?;
        check_sensitive_guard(state, root, &target, &item.path).await?;
        let content = fs::read_to_string(&target)
            .map_err(|e| format!("Gagal membaca '{}': {e}", item.path))?;
        total_file_bytes += content.len();

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let start = item.start_line.unwrap_or(1).max(1);
        let end = item
            .end_line
            .unwrap_or(start + 119)
            .min(total_lines)
            .max(start.saturating_sub(1));

        let mut slice_lines = Vec::new();
        slice_lines.push(format!(
            "FILE: {} (lines {}-{} of {})",
            item.path, start, end, total_lines
        ));
        if start <= total_lines {
            for idx in (start - 1)..end {
                slice_lines.push(format!("{:>4} | {}", idx + 1, lines[idx]));
            }
        }
        sections.push(slice_lines.join("\n"));
    }

    let output = sections.join("\n\n");
    let tokens_saved = (total_file_bytes / 4).saturating_sub(output.len() / 4);
    let turns_saved = items.len().saturating_sub(1);

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "read_file_range".to_string(),
                target_path: items.first().map(|i| i.path.clone()),
                summary: format!("Read {} range slice(s)", items.len()),
                diff_text: None,
                can_revert: false,
                reverted: false,
                tokens_saved,
                original_content: None,
                original_blob_hash: None,
                file_existed_before: false,
                post_edit_hash: None,
                group_id: None,
                move_from_path: None,
            },
            turns_saved,
        )
        .await;

    Ok(output)
}

fn apply_surgical_edits_to_file(
    root: &Path,
    rel_path: &str,
    edits_val: &Value,
) -> Result<(String, String, String, usize, String), String> {
    let target = resolve_jailed_path(root, rel_path)?;
    let original = fs::read_to_string(&target)
        .map_err(|e| format!("Gagal membaca file target '{rel_path}': {e}"))?;

    let edits: Vec<EditOp> = serde_json::from_value(edits_val.clone())
        .map_err(|e| format!("Format 'edits' tidak valid: {e}"))?;
    if edits.is_empty() {
        return Err("Array 'edits' tidak boleh kosong.".to_string());
    }

    let uses_crlf = original.contains("\r\n");
    let original_lf = original.replace("\r\n", "\n");
    let mut updated_lf = original_lf.clone();

    for (idx, op) in edits.iter().enumerate() {
        let search_lf = op.search.replace("\r\n", "\n");
        let replace_lf = op.replace.replace("\r\n", "\n");
        if search_lf.is_empty() {
            return Err(format!("Edit #{idx}: field 'search' tidak boleh kosong."));
        }
        let match_count = updated_lf.matches(&search_lf).count();
        if match_count == 0 {
            return Err(format!(
                "Edit #{idx}: blok 'search' tidak ditemukan di '{rel_path}'."
            ));
        }
        if match_count > 1 && !op.replace_all {
            return Err(format!(
                "Edit #{idx}: blok 'search' cocok {match_count} kali di '{rel_path}'. Persempit konteks atau set replace_all=true."
            ));
        }
        if op.replace_all {
            updated_lf = updated_lf.replace(&search_lf, &replace_lf);
        } else {
            updated_lf = updated_lf.replacen(&search_lf, &replace_lf, 1);
        }
    }

    let final_content = if uses_crlf {
        updated_lf.replace('\n', "\r\n")
    } else {
        updated_lf.clone()
    };

    fs::write(&target, &final_content)
        .map_err(|e| format!("Gagal menulis hasil edit ke '{rel_path}': {e}"))?;

    let diff_text = TextDiff::from_lines(&original_lf, &updated_lf)
        .unified_diff()
        .context_radius(3)
        .header(rel_path, rel_path)
        .to_string();

    let edit_payload_len: usize = edits
        .iter()
        .map(|e| e.search.len() + e.replace.len())
        .sum();
    let tokens_saved = (original.len() / 4).saturating_sub(edit_payload_len / 4);
    let post_edit_hash = sha256_hex(final_content.as_bytes());

    Ok((
        original,
        diff_text,
        format!("Applied {} surgical edit(s)", edits.len()),
        tokens_saved,
        post_edit_hash,
    ))
}

async fn exec_surgical_edit(
    state: &AppState,
    source: &str,
    root: &Path,
    args: &Value,
) -> Result<String, String> {
    let rel_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'path' wajib diisi.")?;
    let edits_val = args.get("edits").ok_or("Argumen 'edits' wajib diisi.")?;

    let target = resolve_jailed_path(root, rel_path)?;
    check_sensitive_guard(state, root, &target, rel_path).await?;

    let (original, diff_text, summary, tokens_saved, post_edit_hash) =
        apply_surgical_edits_to_file(root, rel_path, edits_val)?;
    let blob_hash = state.snapshot_store.save_blob(&original);
    let group_id = current_turn_group_id(state).await;

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "surgical_edit".to_string(),
                target_path: Some(rel_path.to_string()),
                summary: summary.clone(),
                diff_text: Some(diff_text.clone()),
                can_revert: true,
                reverted: false,
                tokens_saved,
                original_content: Some(original),
                original_blob_hash: blob_hash,
                file_existed_before: true,
                post_edit_hash: Some(post_edit_hash),
                group_id,
                move_from_path: None,
            },
            0,
        )
        .await;

    Ok(format!("{summary} on {rel_path}\n\n{diff_text}"))
}

async fn exec_patch_and_verify(
    state: &AppState,
    source: &str,
    root: &Path,
    args: &Value,
) -> Result<String, String> {
    let rel_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'path' wajib diisi.")?;
    let edits_val = args.get("edits").ok_or("Argumen 'edits' wajib diisi.")?;
    let verify_cmd = args
        .get("verify_command")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'verify_command' wajib diisi.")?;

    let target = resolve_jailed_path(root, rel_path)?;
    check_sensitive_guard(state, root, &target, rel_path).await?;
    validate_command(verify_cmd)?;

    let (original, diff_text, edit_summary, edit_saved, post_edit_hash) =
        apply_surgical_edits_to_file(root, rel_path, edits_val)?;
    let blob_hash = state.snapshot_store.save_blob(&original);
    let group_id = current_turn_group_id(state).await;

    let (exit_code, stdout, stderr) = run_shell_raw(root, verify_cmd, 20).await?;
    let (compressed, cmd_saved_bytes) = compress_output(&stdout, &stderr, 60);
    let total_tokens_saved = edit_saved + (cmd_saved_bytes / 4);

    let full_summary = format!("{edit_summary} + verify '{verify_cmd}' (exit {exit_code})");

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "patch_and_verify".to_string(),
                target_path: Some(rel_path.to_string()),
                summary: full_summary.clone(),
                diff_text: Some(diff_text.clone()),
                can_revert: true,
                reverted: false,
                tokens_saved: total_tokens_saved,
                original_content: Some(original),
                original_blob_hash: blob_hash,
                file_existed_before: true,
                post_edit_hash: Some(post_edit_hash),
                group_id,
                move_from_path: None,
            },
            2,
        )
        .await;

    Ok(format!(
        "PATCH DIFF ({rel_path}):\n{diff_text}\nVERIFY COMMAND: {verify_cmd}\nEXIT CODE: {exit_code}\nOUTPUT:\n{compressed}"
    ))
}

struct ChildTreeGuard {
    child: Option<tokio::process::Child>,
}

impl ChildTreeGuard {
    fn kill_tree_now(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(target_os = "windows")]
            if let Some(pid) = child.id() {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .creation_flags(0x08000000)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.start_kill();
        }
    }
}

impl Drop for ChildTreeGuard {
    fn drop(&mut self) {
        self.kill_tree_now();
    }
}

pub(crate) async fn run_shell_raw(
    root: &Path,
    command: &str,
    timeout_secs: u64,
) -> Result<(i32, String, String), String> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd.exe");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };

    cmd.current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Gagal menjalankan perintah '{command}': {e}"))?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });

    let mut guard = ChildTreeGuard { child: Some(child) };
    let wait_res = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        guard.child.as_mut().expect("child present").wait(),
    )
    .await;

    match wait_res {
        Ok(Ok(status)) => {
            guard.child = None;
            let out_bytes = tokio::time::timeout(Duration::from_secs(2), stdout_task)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let err_bytes = tokio::time::timeout(Duration::from_secs(2), stderr_task)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();

            let exit_code = status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&out_bytes).to_string();
            let stderr = String::from_utf8_lossy(&err_bytes).to_string();
            Ok((exit_code, stdout, stderr))
        }
        Ok(Err(e)) => {
            guard.kill_tree_now();
            Err(format!("Gagal menunggu output perintah: {e}"))
        }
        Err(_) => {
            guard.kill_tree_now();
            Err(format!("Perintah timeout setelah {timeout_secs} detik."))
        }
    }
}

async fn exec_run_command(
    state: &AppState,
    source: &str,
    root: &Path,
    args: &Value,
) -> Result<String, String> {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'command' wajib diisi.")?;
    validate_command(command)?;

    let max_timeout = if source == "WebMCP" { 22 } else { 60 };
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(18)
        .clamp(1, max_timeout);
    let max_lines = args
        .get("max_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(60)
        .clamp(12, 200) as usize;

    let (exit_code, stdout, stderr) = run_shell_raw(root, command, timeout_secs).await?;
    let (compressed, saved_bytes) = compress_output(&stdout, &stderr, max_lines);
    let tokens_saved = saved_bytes / 4;

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "run_command_compressed".to_string(),
                target_path: None,
                summary: format!("$ {command} (exit {exit_code})"),
                diff_text: Some(compressed.clone()),
                can_revert: false,
                reverted: false,
                tokens_saved,
                original_content: None,
                original_blob_hash: None,
                file_existed_before: false,
                post_edit_hash: None,
                group_id: None,
                move_from_path: None,
            },
            0,
        )
        .await;

    Ok(format!("EXIT CODE: {exit_code}\n{compressed}"))
}

async fn exec_write_file(
    state: &AppState,
    source: &str,
    root: &Path,
    args: &Value,
) -> Result<String, String> {
    let rel_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'path' wajib diisi.")?;
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'content' wajib diisi.")?;

    let target = resolve_jailed_path(root, rel_path)?;
    check_sensitive_guard(state, root, &target, rel_path).await?;

    let existed = target.exists();
    let original = if existed {
        fs::read_to_string(&target).ok()
    } else {
        None
    };
    let blob_hash = original
        .as_deref()
        .and_then(|txt| state.snapshot_store.save_blob(txt));
    let group_id = current_turn_group_id(state).await;

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Gagal membuat direktori induk '{rel_path}': {e}"))?;
    }

    fs::write(&target, content)
        .map_err(|e| format!("Gagal menulis file '{rel_path}': {e}"))?;

    let post_edit_hash = sha256_hex(content.as_bytes());
    let diff_text = TextDiff::from_lines(original.as_deref().unwrap_or(""), content)
        .unified_diff()
        .context_radius(3)
        .header(rel_path, rel_path)
        .to_string();

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "write_file_jailed".to_string(),
                target_path: Some(rel_path.to_string()),
                summary: if existed {
                    format!("Overwrote {} ({} bytes)", rel_path, content.len())
                } else {
                    format!("Created {} ({} bytes)", rel_path, content.len())
                },
                diff_text: Some(diff_text.clone()),
                can_revert: true,
                reverted: false,
                tokens_saved: 0,
                original_content: original,
                original_blob_hash: blob_hash,
                file_existed_before: existed,
                post_edit_hash: Some(post_edit_hash),
                group_id,
                move_from_path: None,
            },
            0,
        )
        .await;

    Ok(format!("Wrote {rel_path} ({} bytes)\n\n{diff_text}", content.len()))
}

pub async fn revert_audit_entry(
    state: &AppState,
    audit_id: &str,
    force: bool,
) -> Result<String, String> {
    let root = state.workspace_root.read().await.clone();
    let mut logs = state.audit_logs.write().await;

    let entry = logs
        .iter_mut()
        .find(|e| e.id == audit_id)
        .ok_or_else(|| format!("Entri audit '{audit_id}' tidak ditemukan."))?;

    if !entry.can_revert {
        return Err("Entri ini tidak mendukung rollback.".to_string());
    }
    if entry.reverted {
        return Err("Perubahan ini sudah pernah di-revert.".to_string());
    }

    let rel_path = entry
        .target_path
        .as_deref()
        .ok_or("Target path tidak ditemukan.")?;
    let target = resolve_jailed_path(&root, rel_path)?;

    if !force {
        if let Some(expected_hash) = entry.post_edit_hash.as_deref() {
            if target.exists() {
                let current_bytes = fs::read(&target)
                    .map_err(|e| format!("Gagal memverifikasi hash file '{rel_path}': {e}"))?;
                let current_hash = sha256_hex(&current_bytes);
                if current_hash != expected_hash {
                    return Err(format!(
                        "CONFLICT: File '{rel_path}' telah berubah sejak entri audit ini ({}). Gunakan Force Revert jika yakin ingin menimpa perubahan terbaru.",
                        entry.timestamp
                    ));
                }
            } else {
                return Err(format!(
                    "CONFLICT: File '{rel_path}' telah dihapus secara eksternal sejak entri audit ini ({}). Gunakan Force Revert untuk memulihkan versi sebelumnya.",
                    entry.timestamp
                ));
            }
        } else if entry.tool_name == "file_ops" && entry.move_from_path.is_none() && target.exists() {
            return Err(format!(
                "CONFLICT: File '{rel_path}' telah dibuat ulang setelah dihapus pada ({}). Gunakan Force Revert jika ingin menimpa.",
                entry.timestamp
            ));
        }
    }

    if let Some(orig_rel) = entry.move_from_path.as_deref() {
        let orig_target = resolve_jailed_path(&root, orig_rel)?;
        if !target.exists() {
            return Err(format!("File '{rel_path}' tidak ditemukan untuk dipindahkan kembali."));
        }
        if orig_target.exists() && !force {
            return Err(format!(
                "CONFLICT: Lokasi asal '{orig_rel}' sudah ditempati file lain. Gunakan Force Revert untuk menimpa."
            ));
        }
        if let Some(parent) = orig_target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Gagal membuat direktori asal '{orig_rel}': {e}"))?;
        }
        if orig_target.exists() && force {
            let _ = fs::remove_file(&orig_target);
        }
        fs::rename(&target, &orig_target)
            .map_err(|e| format!("Gagal mengembalikan '{rel_path}' ke '{orig_rel}': {e}"))?;
    } else if entry.file_existed_before {
        let orig = entry
            .original_content
            .clone()
            .or_else(|| {
                entry
                    .original_blob_hash
                    .as_deref()
                    .and_then(|h| state.snapshot_store.load_blob(h))
            })
            .ok_or("Snapshot konten asli tidak tersedia.")?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Gagal membuat direktori '{rel_path}': {e}"))?;
        }
        fs::write(&target, &orig)
            .map_err(|e| format!("Gagal memulihkan '{rel_path}': {e}"))?;
    } else if target.exists() {
        fs::remove_file(&target)
            .map_err(|e| format!("Gagal menghapus file baru '{rel_path}': {e}"))?;
    }

    entry.reverted = true;
    let msg = format!("Berhasil revert perubahan pada {rel_path}.");
    state.snapshot_store.save_audit_entries(&logs);
    drop(logs);
    state.emit_dashboard().await;
    Ok(msg)
}

pub async fn revert_audit_group(
    state: &AppState,
    group_id: &str,
    force: bool,
) -> Result<String, String> {
    let ids: Vec<String> = {
        let logs = state.audit_logs.read().await;
        logs.iter()
            .rev()
            .filter(|e| {
                e.group_id.as_deref() == Some(group_id) && e.can_revert && !e.reverted
            })
            .map(|e| e.id.clone())
            .collect()
    };

    if ids.is_empty() {
        return Err(format!(
            "Tidak ada perubahan yang dapat di-revert untuk grup '{group_id}'."
        ));
    }

    let count = ids.len();
    for id in ids {
        revert_audit_entry(state, &id, force).await?;
    }

    Ok(format!(
        "Berhasil revert {count} perubahan pada grup '{group_id}'."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn test_surgical_edit_diff_and_revert() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_tools_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let original_code = (1..=40)
            .map(|i| {
                if i == 20 {
                    "    let status = \"OLD_VALUE\";".to_string()
                } else {
                    format!("    let line_{i} = {i};")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let file_path = temp_dir.join("sample.rs");
        fs::write(&file_path, &original_code).unwrap();

        let state = AppState::new(temp_dir.clone());
        let args = json!({
            "path": "sample.rs",
            "edits": [
                {
                    "search": "let status = \"OLD_VALUE\";",
                    "replace": "let status = \"NEW_VALUE\";"
                }
            ]
        });

        let out = execute_tool(&state, "Test", "surgical_edit", args).await;
        assert!(!out.is_error, "Surgical edit failed: {}", out.text);
        assert!(out.text.contains("-    let status = \"OLD_VALUE\";"));
        assert!(out.text.contains("+    let status = \"NEW_VALUE\";"));

        let modified = fs::read_to_string(&file_path).unwrap();
        assert!(modified.contains("NEW_VALUE"));

        let logs = state.audit_logs.read().await;
        assert_eq!(logs.len(), 1);
        assert!(logs[0].tokens_saved > 50);
        let audit_id = logs[0].id.clone();
        drop(logs);

        let revert_msg = revert_audit_entry(&state, &audit_id, false).await.unwrap();
        assert!(revert_msg.contains("sample.rs"));
        let restored = fs::read_to_string(&file_path).unwrap();
        assert_eq!(restored, original_code);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_surgical_edit_preserves_crlf_line_endings() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_crlf_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let original_crlf = "fn greet() {\r\n    let x = 1;\r\n    let y = 2;\r\n}\r\n";
        let file_path = temp_dir.join("win.rs");
        fs::write(&file_path, original_crlf).unwrap();

        let state = AppState::new(temp_dir.clone());
        let args = json!({
            "path": "win.rs",
            "edits": [
                {
                    "search": "    let x = 1;\n    let y = 2;",
                    "replace": "    let x = 10;\n    let y = 20;"
                }
            ]
        });

        let out = execute_tool(&state, "Test", "surgical_edit", args).await;
        assert!(!out.is_error, "CRLF surgical edit failed: {}", out.text);

        let updated = fs::read_to_string(&file_path).unwrap();
        assert!(updated.contains("\r\n"), "CRLF line endings should be preserved");
        assert!(!updated.replace("\r\n", "").contains('\n'), "No stray LF expected");
        assert!(updated.contains("let x = 10;\r\n    let y = 20;"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_revert_conflict_detection_and_force() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_revert_conflict_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let file_path = temp_dir.join("doc.txt");
        fs::write(&file_path, "v1\n").unwrap();

        let state = AppState::new(temp_dir.clone());
        let out = execute_tool(
            &state,
            "Test",
            "surgical_edit",
            json!({
                "path": "doc.txt",
                "edits": [{ "search": "v1", "replace": "v2" }]
            }),
        )
        .await;
        assert!(!out.is_error);

        let audit_id = state.audit_logs.read().await[0].id.clone();

        // Simulate external modification after edit
        fs::write(&file_path, "v3_external_edit\n").unwrap();

        let conflict_res = revert_audit_entry(&state, &audit_id, false).await;
        assert!(conflict_res.is_err());
        assert!(conflict_res.unwrap_err().starts_with("CONFLICT:"));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "v3_external_edit\n");

        let force_res = revert_audit_entry(&state, &audit_id, true).await;
        assert!(force_res.is_ok());
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "v1\n");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_sensitive_file_blocked_in_read_and_search() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_sensitive_guard_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join(".env"), "API_SECRET=super_secret_token_123\n").unwrap();
        fs::write(
            temp_dir.join("secrets.json"),
            "{\"secret\":\"super_secret_token_123\"}\n",
        )
        .unwrap();
        fs::write(
            temp_dir.join("app.ts"),
            "const tokenRef = \"super_secret_token_123\";\n",
        )
        .unwrap();

        let state = AppState::new(temp_dir.clone());

        let read_env = execute_tool(
            &state,
            "Test",
            "read_file_range",
            json!({ "path": ".env" }),
        )
        .await;
        assert!(read_env.is_error);
        assert!(read_env.text.contains("Sensitive File Guard"));

        let read_secrets = execute_tool(
            &state,
            "Test",
            "read_file_range",
            json!({ "path": "secrets.json" }),
        )
        .await;
        assert!(read_secrets.is_error);
        assert!(read_secrets.text.contains("Sensitive File Guard"));

        let search_out = execute_tool(
            &state,
            "Test",
            "smart_search",
            json!({ "query": "super_secret_token_123" }),
        )
        .await;
        assert!(!search_out.is_error);
        assert!(search_out.text.contains("app.ts"));
        assert!(!search_out.text.contains(".env"));
        assert!(!search_out.text.contains("secrets.json"));
        assert!(search_out.text.contains("Sensitive File Guard"));

        let direct_env_search = execute_tool(
            &state,
            "Test",
            "smart_search",
            json!({ "query": "super_secret_token_123", "path": ".env" }),
        )
        .await;
        assert!(!direct_env_search.is_error);
        assert!(!direct_env_search.text.contains("API_SECRET"));
        assert!(direct_env_search.text.contains("Sensitive File Guard"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_smart_search_skips_binary_game_assets() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_game_assets_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(temp_dir.join("cstrike")).unwrap();
        // Fake binary assets containing matching text or NUL bytes
        fs::write(temp_dir.join("cstrike").join("player.mdl"), b"IDST\x00sv_cheats\x00").unwrap();
        fs::write(temp_dir.join("cstrike").join("sound.wav"), b"RIFFsv_cheats").unwrap();
        fs::write(temp_dir.join("cstrike").join("client.dll"), b"MZ\x00sv_cheats").unwrap();
        // Text config file
        fs::write(
            temp_dir.join("cstrike").join("server.cfg"),
            "hostname \"CS Server\"\nsv_cheats 0\nmp_freezetime 3\n",
        )
        .unwrap();

        let state = AppState::new(temp_dir.clone());
        let out = execute_tool(
            &state,
            "Test",
            "smart_search",
            json!({ "query": "sv_cheats" }),
        )
        .await;
        assert!(!out.is_error);
        assert!(out.text.contains("server.cfg"));
        assert!(!out.text.contains("player.mdl"));
        assert!(!out.text.contains("sound.wav"));
        assert!(!out.text.contains("client.dll"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_find_files_and_code_outline() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_find_outline_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(temp_dir.join("src")).unwrap();
        fs::write(
            temp_dir.join("src").join("auth.rs"),
            "pub struct AuthUser {\n    id: u64,\n}\n\npub async fn verify_token(tok: &str) -> bool {\n    !tok.is_empty()\n}\n",
        )
        .unwrap();
        fs::write(temp_dir.join("README.md"), "# Demo\n").unwrap();

        let state = AppState::new(temp_dir.clone());

        let find_out = execute_tool(
            &state,
            "Test",
            "find_files",
            json!({ "pattern": "*.rs" }),
        )
        .await;
        assert!(!find_out.is_error, "find_files failed: {}", find_out.text);
        assert!(find_out.text.contains("src/auth.rs"));
        assert!(!find_out.text.contains("README.md"));

        let outline_out = execute_tool(
            &state,
            "Test",
            "code_outline",
            json!({ "path": "src/auth.rs" }),
        )
        .await;
        assert!(!outline_out.is_error, "code_outline failed: {}", outline_out.text);
        assert!(outline_out.text.contains("pub struct AuthUser"));
        assert!(outline_out.text.contains("pub async fn verify_token"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_file_ops_move_delete_and_revert() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_file_ops_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("a.txt"), "hello move and delete\n").unwrap();

        let state = AppState::new(temp_dir.clone());

        // 1. Move a.txt -> sub/b.txt
        let mv = execute_tool(
            &state,
            "Test",
            "file_ops",
            json!({ "action": "move", "path": "a.txt", "destination": "sub/b.txt" }),
        )
        .await;
        assert!(!mv.is_error, "move failed: {}", mv.text);
        assert!(!temp_dir.join("a.txt").exists());
        assert!(temp_dir.join("sub").join("b.txt").exists());

        let move_audit_id = state.audit_logs.read().await.last().unwrap().id.clone();
        revert_audit_entry(&state, &move_audit_id, false)
            .await
            .unwrap();
        assert!(temp_dir.join("a.txt").exists());
        assert!(!temp_dir.join("sub").join("b.txt").exists());

        // 2. Delete a.txt and revert back
        let del = execute_tool(
            &state,
            "Test",
            "file_ops",
            json!({ "action": "delete", "path": "a.txt" }),
        )
        .await;
        assert!(!del.is_error, "delete failed: {}", del.text);
        assert!(!temp_dir.join("a.txt").exists());

        let del_audit_id = state.audit_logs.read().await.last().unwrap().id.clone();
        revert_audit_entry(&state, &del_audit_id, false)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(temp_dir.join("a.txt")).unwrap(),
            "hello move and delete\n"
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_guardrail_read_only_and_safe_edit_hitl() {
        use std::sync::Arc;
        let temp_dir = std::env::temp_dir().join("smart_mcp_guardrail_hitl_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("notes.txt"), "initial\n").unwrap();

        let state = Arc::new(AppState::new(temp_dir.clone()));

        // 1. Read-Only blocks write_file_jailed but allows read_file_range
        *state.guardrail_mode.write().await = "read_only".to_string();
        let blocked = execute_tool(
            &state,
            "WebMCP",
            "write_file_jailed",
            json!({ "path": "notes.txt", "content": "hacked" }),
        )
        .await;
        assert!(blocked.is_error);
        assert!(blocked.text.contains("Read-Only"));

        let allowed_ro = execute_tool(
            &state,
            "WebMCP",
            "read_file_range",
            json!({ "path": "notes.txt" }),
        )
        .await;
        assert!(!allowed_ro.is_error);

        // 2. Safe-Edit allows surgical_edit immediately without HITL, but prompts HITL for run_command_compressed
        *state.guardrail_mode.write().await = "safe_edit".to_string();
        let edit_ok = execute_tool(
            &state,
            "WebMCP",
            "surgical_edit",
            json!({ "path": "notes.txt", "edits": [{ "search": "initial", "replace": "updated" }] }),
        )
        .await;
        assert!(!edit_ok.is_error);

        // Deny first HITL command
        let st_deny = state.clone();
        let deny_task = tokio::spawn(async move {
            for _ in 0..80 {
                let req_id = st_deny
                    .pending_tool_approvals
                    .read()
                    .await
                    .keys()
                    .next()
                    .cloned();
                if let Some(id) = req_id {
                    resolve_tool_approval(&st_deny, &id, ToolApprovalDecision::Deny)
                        .await
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("Timed out waiting for HITL approval");
        });

        let denied_cmd = execute_tool(
            &state,
            "WebMCP",
            "run_command_compressed",
            json!({ "command": "echo should_be_denied" }),
        )
        .await;
        deny_task.await.unwrap();
        assert!(denied_cmd.is_error);
        assert!(denied_cmd.text.contains("ditolak"));

        // ApproveForSession once, then second call runs immediately without waiting
        let st_allow = state.clone();
        let allow_task = tokio::spawn(async move {
            for _ in 0..80 {
                let req_id = st_allow
                    .pending_tool_approvals
                    .read()
                    .await
                    .keys()
                    .next()
                    .cloned();
                if let Some(id) = req_id {
                    resolve_tool_approval(&st_allow, &id, ToolApprovalDecision::ApproveForSession)
                        .await
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("Timed out waiting for HITL approval");
        });

        let ok1 = execute_tool(
            &state,
            "WebMCP",
            "run_command_compressed",
            json!({ "command": "echo first_ok" }),
        )
        .await;
        allow_task.await.unwrap();
        assert!(!ok1.is_error);

        let ok2 = execute_tool(
            &state,
            "WebMCP",
            "run_command_compressed",
            json!({ "command": "echo cached_session_ok" }),
        )
        .await;
        assert!(!ok2.is_error);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_revert_audit_group_reverse_order() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_group_revert_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("counter.txt"), "step0\n").unwrap();

        let state = AppState::new(temp_dir.clone());
        *state.active_turn_group_id.write().await = Some("turn-abc1".to_string());

        let e1 = execute_tool(
            &state,
            "LocalAgent",
            "surgical_edit",
            json!({ "path": "counter.txt", "edits": [{ "search": "step0", "replace": "step1" }] }),
        )
        .await;
        assert!(!e1.is_error);

        let e2 = execute_tool(
            &state,
            "LocalAgent",
            "surgical_edit",
            json!({ "path": "counter.txt", "edits": [{ "search": "step1", "replace": "step2" }] }),
        )
        .await;
        assert!(!e2.is_error);

        *state.active_turn_group_id.write().await = None;

        // Reverting in reverse order (e2 then e1) restores step2 -> step1 -> step0 cleanly with zero conflicts!
        let msg = revert_audit_group(&state, "turn-abc1", false).await.unwrap();
        assert!(msg.contains("2 perubahan"));
        assert_eq!(fs::read_to_string(temp_dir.join("counter.txt")).unwrap(), "step0\n");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
