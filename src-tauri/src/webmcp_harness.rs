use crate::config::WebMcpExecutionMode;
use crate::state::AppState;
use crate::tools::ToolOutput;
use crate::workspace::ResolvedWorkspace;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

const MAX_HARNESS_SESSIONS: usize = 16;
const HARNESS_SESSION_TTL: Duration = Duration::from_secs(60 * 60);
const CONTEXT_ROT_CALL_THRESHOLD: u32 = 12;
const CONTEXT_ROT_BYTE_THRESHOLD: usize = 45_000;
const TASK_CONTEXT_ROT_CALL_THRESHOLD: u32 = 30;
const TASK_CONTEXT_ROT_BYTE_THRESHOLD: usize = 120_000;
const MAX_HINTS_PER_CALL: usize = 2;
const MAX_TOTAL_HINT_CHARS: usize = 320;

#[derive(Debug, Clone, Default)]
struct WorkspaceHarnessState {
    anchor_emitted: bool,
    instructions_injected: bool,
    checkpoint_nudge_emitted: bool,
    calls_since_checkpoint: u32,
    bytes_since_checkpoint: usize,
}

#[derive(Debug, Clone)]
struct SessionHarnessState {
    last_used: Instant,
    seq: u64,
    workspaces: HashMap<String, WorkspaceHarnessState>,
    last_read_key: Option<String>,
    consecutive_read_count: u32,
    last_error_key: Option<String>,
    consecutive_error_count: u32,
}

impl SessionHarnessState {
    fn new(now: Instant, seq: u64) -> Self {
        Self {
            last_used: now,
            seq,
            workspaces: HashMap::new(),
            last_read_key: None,
            consecutive_read_count: 0,
            last_error_key: None,
            consecutive_error_count: 0,
        }
    }
}

/// Stateful per-`mcp-session-id` execution harness that counters context rot,
/// repetitive read loops, and retry loops for external WebMCP models.
#[derive(Default)]
pub struct WebMcpHarnessManager {
    inner: RwLock<HarnessStore>,
}

#[derive(Default)]
struct HarnessStore {
    next_seq: u64,
    sessions: HashMap<String, SessionHarnessState>,
}

fn single_line_clip(raw: &str, max_chars: usize) -> String {
    let cleaned = raw
        .replace(['\r', '\n', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let count = cleaned.chars().count();
    if count <= max_chars {
        cleaned
    } else if max_chars > 3 {
        let head: String = cleaned.chars().take(max_chars - 3).collect();
        format!("{head}...")
    } else {
        cleaned.chars().take(max_chars).collect()
    }
}

fn arg_str_norm(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

fn extract_nonzero_exit_code(text: &str) -> Option<i32> {
    for line in text.lines() {
        let trimmed = line.trim();
        let rest = trimmed
            .strip_prefix("EXIT CODE:")
            .or_else(|| trimmed.strip_prefix("EXIT_CODE:"));
        if let Some(after) = rest {
            if let Ok(code) = after.split_whitespace().next()?.parse::<i32>() {
                if code != 0 {
                    return Some(code);
                }
            }
        }
    }
    None
}

fn canonical_read_signature(
    tool_name: &str,
    ws_id: &str,
    args: &Value,
) -> Option<(String, String)> {
    match tool_name {
        "read_file_range" | "code_outline" | "read_symbol" => {
            let path = arg_str_norm(args, "path");
            if path.is_empty() {
                return None;
            }
            let symbol = arg_str_norm(args, "symbol");
            let range = format!(
                "{}:{}",
                args.get("start_line").and_then(|v| v.as_i64()).unwrap_or(0),
                args.get("end_line").and_then(|v| v.as_i64()).unwrap_or(0)
            );
            let label = if symbol.is_empty() {
                path.clone()
            } else {
                format!("{path}#{symbol}")
            };
            Some((format!("{tool_name}|{ws_id}|{path}|{symbol}|{range}"), label))
        }
        "smart_search" => {
            let query = arg_str_norm(args, "query");
            let path = arg_str_norm(args, "path");
            if query.is_empty() && path.is_empty() {
                return None;
            }
            let label = if query.is_empty() { path.clone() } else { query.clone() };
            Some((format!("{tool_name}|{ws_id}|{path}|{query}"), label))
        }
        "project_overview" | "find_files" => {
            let path = arg_str_norm(args, "path");
            let pattern = arg_str_norm(args, "pattern");
            let label = if !pattern.is_empty() {
                pattern.clone()
            } else if !path.is_empty() {
                path.clone()
            } else {
                "workspace".to_string()
            };
            Some((format!("{tool_name}|{ws_id}|{path}|{pattern}"), label))
        }
        "batch_inspect" => {
            let sig = serde_json::to_string(args).unwrap_or_default();
            Some((format!("batch_inspect|{ws_id}|{sig}"), "batch_inspect".to_string()))
        }
        _ => None,
    }
}

fn clears_read_loop(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "surgical_edit"
            | "multi_surgical_edit"
            | "patch_and_verify"
            | "write_file_jailed"
            | "file_ops"
            | "run_command"
            | "run_command_compressed"
            | "start_command_job"
            | "session_checkpoint"
    )
}

fn detect_blueprint_target(tool_name: &str, args: &Value) -> Option<String> {
    match tool_name {
        "multi_surgical_edit" => args
            .get("files")
            .and_then(|v| v.as_array())
            .and_then(|files| {
                files.iter().find_map(|item| {
                    let path = item.get("path").and_then(|v| v.as_str())?.trim();
                    if crate::checkpoints::is_blueprint_doc_path(path) {
                        Some(path.replace('\\', "/"))
                    } else {
                        None
                    }
                })
            }),
        "write_file_jailed" | "surgical_edit" | "patch_and_verify" => {
            let path = arg_str_norm(args, "path");
            if crate::checkpoints::is_blueprint_doc_path(&path) {
                Some(path.replace('\\', "/"))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn canonical_error_signature(tool_name: &str, ws_id: &str, args: &Value, err_text: &str) -> String {
    let target = match tool_name {
        "run_command" | "run_command_compressed" | "start_command_job" => {
            let cmd = arg_str_norm(args, "command");
            if !cmd.is_empty() {
                cmd
            } else {
                args.get("commands")
                    .and_then(|v| serde_json::to_string(v).ok())
                    .unwrap_or_default()
            }
        }
        _ => {
            let p = arg_str_norm(args, "path");
            if !p.is_empty() {
                p
            } else {
                arg_str_norm(args, "action")
            }
        }
    };
    let head_err = single_line_clip(err_text.lines().next().unwrap_or(""), 80);
    format!("{tool_name}|{ws_id}|{target}|{head_err}")
}

fn evict_stale_or_lru(store: &mut HarnessStore, now: Instant) {
    store
        .sessions
        .retain(|_, st| now.saturating_duration_since(st.last_used) <= HARNESS_SESSION_TTL);

    while store.sessions.len() > MAX_HARNESS_SESSIONS {
        if let Some(oldest_key) = store
            .sessions
            .iter()
            .min_by_key(|(_, st)| st.seq)
            .map(|(k, _)| k.clone())
        {
            store.sessions.remove(&oldest_key);
        } else {
            break;
        }
    }
}

impl WebMcpHarnessManager {
    pub async fn after_tool_call(
        &self,
        state: &AppState,
        session_id: Option<&str>,
        ws: Option<&ResolvedWorkspace>,
        tool_name: &str,
        args: &Value,
        out: &mut ToolOutput,
    ) {
        self.after_tool_call_with_mode(
            state,
            session_id,
            ws,
            tool_name,
            args,
            WebMcpExecutionMode::Chat,
            out,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn after_tool_call_with_mode(
        &self,
        state: &AppState,
        session_id: Option<&str>,
        ws: Option<&ResolvedWorkspace>,
        tool_name: &str,
        args: &Value,
        mode: WebMcpExecutionMode,
        out: &mut ToolOutput,
    ) {
        let enabled = state.config.read().await.webmcp_session_harness;
        if !enabled {
            return;
        }

        let Some(sid) = session_id.map(str::trim).filter(|s| !s.is_empty()) else {
            return;
        };

        let ws_key = ws
            .map(|w| w.id.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("_global");
        let is_failure = out.is_error || extract_nonzero_exit_code(&out.text).is_some();

        // Check outside the write lock whether we need a one-shot checkpoint anchor or workspace rules snippet.
        let (need_anchor, need_instructions) = if let Some(w) = ws {
            let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
            let ws_snap = guard.sessions.get(sid).and_then(|s| s.workspaces.get(ws_key));
            let need_anc =
                tool_name != "session_checkpoint" && ws_snap.map(|x| !x.anchor_emitted).unwrap_or(true);
            let explicit_instr = args
                .get("include_instructions")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let need_ins = tool_name == "batch_inspect"
                && !is_failure
                && !explicit_instr
                && !w.id.is_empty()
                && ws_snap.map(|x| !x.instructions_injected).unwrap_or(true);
            (need_anc, need_ins)
        } else {
            (false, false)
        };
        let checkpoint_anchor = if need_anchor {
            ws.and_then(|w| crate::checkpoints::peek_checkpoint_next_step(state, &w.id))
        } else {
            None
        };
        let workspace_rules_banner = if need_instructions {
            ws.and_then(|w| {
                crate::providers::load_combined_rules_snippet(
                    state.data_dir.as_deref(),
                    &w.canon_root,
                )
                .map(|raw| {
                    let trimmed = raw.trim();
                    let clipped = if trimmed.chars().count() > 1600 {
                        let head: String = trimmed.chars().take(1597).collect();
                        format!("{head}...")
                    } else {
                        trimmed.to_string()
                    };
                    format!("=== WORKSPACE RULES (@{}) ===\n{}", w.alias, clipped)
                })
            })
        } else {
            None
        };

        let now = Instant::now();
        let ws_id = ws.map(|w| w.id.as_str()).unwrap_or("");

        let (hints, inject_rules_prefix) = {
            let mut store = self.inner.write().unwrap_or_else(|e| e.into_inner());
            store.next_seq = store.next_seq.saturating_add(1);
            let seq = store.next_seq;
            if !store.sessions.contains_key(sid) {
                store
                    .sessions
                    .insert(sid.to_string(), SessionHarnessState::new(now, seq));
                evict_stale_or_lru(&mut store, now);
            }

            let sess = store
                .sessions
                .get_mut(sid)
                .expect("session inserted just above");
            sess.last_used = now;
            sess.seq = seq;

            // Saving or loading a session checkpoint anchors this workspace and resets its drift counters.
            if tool_name == "session_checkpoint" && !out.is_error {
                let op = args
                    .get("op")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_ascii_lowercase())
                    .unwrap_or_else(|| "load".to_string());
                if op == "save" || op == "load" {
                    let ws_state = sess.workspaces.entry(ws_key.to_string()).or_default();
                    ws_state.calls_since_checkpoint = 0;
                    ws_state.bytes_since_checkpoint = 0;
                    ws_state.checkpoint_nudge_emitted = false;
                    ws_state.anchor_emitted = true;
                    sess.last_read_key = None;
                    sess.consecutive_read_count = 0;
                    sess.last_error_key = None;
                    sess.consecutive_error_count = 0;
                }
                return;
            }

            let rules_prefix = {
                let ws_state = sess.workspaces.entry(ws_key.to_string()).or_default();
                ws_state.calls_since_checkpoint =
                    ws_state.calls_since_checkpoint.saturating_add(1);
                ws_state.bytes_since_checkpoint =
                    ws_state.bytes_since_checkpoint.saturating_add(out.text.len());
                if tool_name == "batch_inspect" && !is_failure && ws.is_some() {
                    let explicit_instr = args
                        .get("include_instructions")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if explicit_instr {
                        ws_state.instructions_injected = true;
                        None
                    } else if !ws_state.instructions_injected {
                        ws_state.instructions_injected = true;
                        workspace_rules_banner
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            // Update read-loop tracker
            let read_nudge: Option<String> = if clears_read_loop(tool_name) {
                sess.last_read_key = None;
                sess.consecutive_read_count = 0;
                None
            } else if !is_failure {
                if let Some((sig, label)) = canonical_read_signature(tool_name, ws_id, args) {
                    if sess.last_read_key.as_deref() == Some(sig.as_str()) {
                        sess.consecutive_read_count =
                            sess.consecutive_read_count.saturating_add(1);
                    } else {
                        sess.last_read_key = Some(sig);
                        sess.consecutive_read_count = 1;
                    }
                    if sess.consecutive_read_count >= 2 {
                        let clipped = single_line_clip(&label, 48);
                        Some(format!(
                            "[HARNESS_HINT: Inspected '{clipped}' {}x with no edit. Use batch_inspect, symbol edits, or advance.]",
                            sess.consecutive_read_count
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

            // Update error-loop tracker
            let error_nudge: Option<String> = if is_failure {
                let sig = canonical_error_signature(tool_name, ws_id, args, &out.text);
                if sess.last_error_key.as_deref() == Some(sig.as_str()) {
                    sess.consecutive_error_count = sess.consecutive_error_count.saturating_add(1);
                } else {
                    sess.last_error_key = Some(sig);
                    sess.consecutive_error_count = 1;
                }
                if sess.consecutive_error_count >= 2 {
                    Some(format!(
                        "[HARNESS_HINT: Repeated failure ({}x) on '{}'. Inspect lsp_query/logs or change approach before retrying.]",
                        sess.consecutive_error_count, tool_name
                    ))
                } else {
                    None
                }
            } else {
                sess.last_error_key = None;
                sess.consecutive_error_count = 0;
                None
            };

            // Nudge Antigravity handoff prompt when a blueprint/plan document is created or updated
            let blueprint_nudge: Option<String> = if !is_failure {
                detect_blueprint_target(tool_name, args).map(|bp_path| {
                    let clipped_bp = single_line_clip(&bp_path, 48);
                    match mode {
                        WebMcpExecutionMode::Chat => format!(
                            "[HARNESS_HINT: Blueprint '{clipped_bp}' saved (1/turn). Call session_checkpoint(op=\"save\", stage=\"blueprint\"), output WEBMCP_NEXT_SESSION_PROMPT & ANTIGRAVITY_HANDOFF_PROMPT verbatim, & offer 2-4 choices.]"
                        ),
                        WebMcpExecutionMode::Task => format!(
                            "[HARNESS_HINT: Blueprint '{clipped_bp}' saved. When the milestone finishes, call session_checkpoint(op=\"save\", stage=\"blueprint\") to update .airlock/HANDOFF.md.]"
                        ),
                    }
                })
            } else {
                None
            };

            // Check context-rot checkpoint threshold per workspace
            let (rot_call_threshold, rot_byte_threshold) = match mode {
                WebMcpExecutionMode::Chat => (CONTEXT_ROT_CALL_THRESHOLD, CONTEXT_ROT_BYTE_THRESHOLD),
                WebMcpExecutionMode::Task => (
                    TASK_CONTEXT_ROT_CALL_THRESHOLD,
                    TASK_CONTEXT_ROT_BYTE_THRESHOLD,
                ),
            };
            let rot_nudge: Option<String> = {
                let ws_state = sess.workspaces.entry(ws_key.to_string()).or_default();
                if ws.is_some()
                    && !ws_state.checkpoint_nudge_emitted
                    && (ws_state.calls_since_checkpoint >= rot_call_threshold
                        || ws_state.bytes_since_checkpoint >= rot_byte_threshold)
                {
                    ws_state.checkpoint_nudge_emitted = true;
                    Some(
                        "[HARNESS_HINT: Long session without checkpoint; call session_checkpoint(op=\"save\") to preserve progress.]"
                            .to_string(),
                    )
                } else {
                    None
                }
            };

            // One-shot task-contract anchor on first turn when checkpoint exists per workspace
            let anchor_nudge: Option<String> = {
                let ws_state = sess.workspaces.entry(ws_key.to_string()).or_default();
                if !ws_state.anchor_emitted && ws.is_some() {
                    ws_state.anchor_emitted = true;
                    checkpoint_anchor.map(|(alias, next)| {
                        let clipped_alias = single_line_clip(&alias, 24);
                        let clipped_next = single_line_clip(&next, 88);
                        format!(
                            "[HARNESS_HINT: Active checkpoint (@{clipped_alias}) next: {clipped_next}. Resume without re-exploring unless stale.]"
                        )
                    })
                } else {
                    None
                }
            };

            // Assemble prioritized hints bounded to <= 2 lines and <= 320 chars total
            let mut candidates: Vec<String> = Vec::with_capacity(5);
            for opt in [error_nudge, read_nudge, blueprint_nudge, rot_nudge, anchor_nudge] {
                if let Some(h) = opt {
                    candidates.push(h);
                }
            }
            let mut selected: Vec<String> = Vec::new();
            let mut total_chars = 0usize;
            for h in candidates.into_iter().take(MAX_HINTS_PER_CALL) {
                let len = h.chars().count();
                if total_chars + len + usize::from(!selected.is_empty()) <= MAX_TOTAL_HINT_CHARS {
                    total_chars += len + usize::from(!selected.is_empty());
                    selected.push(h);
                }
            }
            (selected, rules_prefix)
        };

        if let Some(prefix) = inject_rules_prefix {
            if out.text.is_empty() {
                out.text = prefix;
            } else {
                out.text = format!("{prefix}\n\n{}", out.text);
            }
        }

        if !hints.is_empty() {
            if !out.text.is_empty() && !out.text.ends_with('\n') {
                out.text.push('\n');
            }
            out.text.push_str(&hints.join("\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_webmcp_harness_triggers_context_rot_nudge_and_resets_on_checkpoint() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_harness_nudge_unit_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let state = AppState::new(temp_dir.clone());
        let ws = crate::workspace::resolve_workspace(&state, None, None)
            .await
            .unwrap();
        let mgr = WebMcpHarnessManager::default();

        for i in 0..11 {
            let mut out = ToolOutput {
                text: format!("ok_{i}"),
                is_error: false,
            };
            mgr.after_tool_call(
                &state,
                Some("sess-nudge"),
                Some(&ws),
                "list_workspaces",
                &json!({}),
                &mut out,
            )
            .await;
            assert!(
                !out.text.contains("session_checkpoint(op=\"save\")"),
                "Should not nudge before 12 calls (at call {i})"
            );
        }

        let mut twelfth = ToolOutput {
            text: "ok_12".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-nudge"),
            Some(&ws),
            "list_workspaces",
            &json!({}),
            &mut twelfth,
        )
        .await;
        assert!(
            twelfth.text.contains("[HARNESS_HINT:") && twelfth.text.contains("session_checkpoint"),
            "12th call must emit checkpoint nudge, got:\n{}",
            twelfth.text
        );

        let mut cp_save = ToolOutput {
            text: "CHECKPOINT_SAVED".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-nudge"),
            Some(&ws),
            "session_checkpoint",
            &json!({ "op": "save", "goal": "x" }),
            &mut cp_save,
        )
        .await;

        let mut post_save = ToolOutput {
            text: "ok_after".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-nudge"),
            Some(&ws),
            "list_workspaces",
            &json!({}),
            &mut post_save,
        )
        .await;
        assert!(!post_save.text.contains("session_checkpoint(op=\"save\")"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_webmcp_harness_nudges_handoff_on_blueprint_write_only() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_harness_blueprint_nudge_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let state = AppState::new(temp_dir.clone());
        let ws = crate::workspace::resolve_workspace(&state, None, None)
            .await
            .unwrap();
        let mgr = WebMcpHarnessManager::default();

        // 1. Writing a blueprint file triggers the Antigravity handoff prompt nudge
        let mut bp_out = ToolOutput {
            text: "Wrote .gemini/Blueprints/09-feature.md (120 bytes)".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-bp"),
            Some(&ws),
            "write_file_jailed",
            &json!({ "path": ".gemini/Blueprints/09-feature.md", "content": "# Idea" }),
            &mut bp_out,
        )
        .await;
        assert!(
            bp_out.text.contains("[HARNESS_HINT:")
                && bp_out.text.contains(".gemini/Blueprints/09-feature.md")
                && bp_out.text.contains("session_checkpoint")
                && bp_out.text.contains("WEBMCP_NEXT_SESSION_PROMPT")
                && bp_out.text.contains("ANTIGRAVITY_HANDOFF_PROMPT")
                && bp_out.text.contains("1/turn")
                && bp_out.text.contains("choices"),
            "Expected dual blueprint handoff hint, got:\n{}",
            bp_out.text
        );
        let hint_line = bp_out
            .text
            .lines()
            .find(|l| l.contains("[HARNESS_HINT:"))
            .unwrap();
        assert!(
            hint_line.chars().count() <= MAX_TOTAL_HINT_CHARS,
            "Harness hint exceeded budget: {}",
            hint_line.len()
        );

        // 2. Editing a regular code file must NOT emit the blueprint handoff hint
        let mut code_out = ToolOutput {
            text: "Applied atomic multi_surgical_edit".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-bp"),
            Some(&ws),
            "multi_surgical_edit",
            &json!({
                "files": [{ "path": "src/main.rs", "edits": [{ "search": "a", "replace": "b" }] }]
            }),
            &mut code_out,
        )
        .await;
        assert!(
            !code_out.text.contains("ANTIGRAVITY_HANDOFF_PROMPT"),
            "Regular code edit must not emit blueprint handoff hint:\n{}",
            code_out.text
        );

        // 3. multi_surgical_edit on a plan markdown file DOES trigger the blueprint handoff hint
        let mut plan_out = ToolOutput {
            text: "Applied atomic multi_surgical_edit".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-bp"),
            Some(&ws),
            "multi_surgical_edit",
            &json!({
                "files": [{ "path": "docs/auth-plan.md", "edits": [{ "search": "a", "replace": "b" }] }]
            }),
            &mut plan_out,
        )
        .await;
        assert!(
            plan_out.text.contains("[HARNESS_HINT:")
                && plan_out.text.contains("docs/auth-plan.md")
                && plan_out.text.contains("ANTIGRAVITY_HANDOFF_PROMPT"),
            "Expected blueprint handoff hint for multi_surgical_edit on plan doc, got:\n{}",
            plan_out.text
        );

        // 4. Failed write on a blueprint must NOT emit the handoff hint
        let mut err_out = ToolOutput {
            text: "Error writing blueprint".to_string(),
            is_error: true,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-bp"),
            Some(&ws),
            "write_file_jailed",
            &json!({ "path": ".gemini/Blueprints/fail.md", "content": "x" }),
            &mut err_out,
        )
        .await;
        assert!(
            !err_out.text.contains("ANTIGRAVITY_HANDOFF_PROMPT"),
            "Failed write must not emit blueprint handoff hint:\n{}",
            err_out.text
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_webmcp_harness_isolates_checkpoints_per_workspace() {
        let base_dir = std::env::temp_dir().join("smart_mcp_harness_ws_isolation_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        let ws_a_dir = base_dir.join("repo-alpha");
        let ws_b_dir = base_dir.join("repo-beta");
        std::fs::create_dir_all(&ws_a_dir).unwrap();
        std::fs::create_dir_all(&ws_b_dir).unwrap();

        let state = AppState::new(ws_a_dir.clone());
        {
            let mut cfg = state.config.write().await;
            cfg.workspaces = vec![
                crate::workspace::WorkspaceProfile {
                    id: "ws-alpha".to_string(),
                    alias: "repo-alpha".to_string(),
                    path: ws_a_dir.to_string_lossy().to_string(),
                    enabled: true,
                    guardrail_override: None,
                    allow_sensitive_override: None,
                    extra_sensitive_patterns: Vec::new(),
                    pinned: false,
                },
                crate::workspace::WorkspaceProfile {
                    id: "ws-beta".to_string(),
                    alias: "repo-beta".to_string(),
                    path: ws_b_dir.to_string_lossy().to_string(),
                    enabled: true,
                    guardrail_override: None,
                    allow_sensitive_override: None,
                    extra_sensitive_patterns: Vec::new(),
                    pinned: false,
                },
            ];
            cfg.default_workspace_id = Some("ws-alpha".to_string());
        }

        let ws_a = crate::workspace::resolve_workspace(&state, Some("@repo-alpha"), None)
            .await
            .unwrap();
        let ws_b = crate::workspace::resolve_workspace(&state, Some("@repo-beta"), None)
            .await
            .unwrap();

        let _ = crate::checkpoints::exec_session_checkpoint(
            &state,
            "WebMCP",
            &ws_a,
            &json!({ "op": "save", "goal": "Alpha goal", "next_steps": ["Alpha next step"] }),
        )
        .await
        .unwrap();
        let _ = crate::checkpoints::exec_session_checkpoint(
            &state,
            "WebMCP",
            &ws_b,
            &json!({ "op": "save", "goal": "Beta goal", "next_steps": ["Beta next step"] }),
        )
        .await
        .unwrap();

        let mgr = WebMcpHarnessManager::default();

        // 1. Global meta-call with ws=None must NOT consume either workspace's checkpoint anchor
        let mut meta_out = ToolOutput {
            text: "Mounted enabled workspaces (2)".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-iso"),
            None,
            "list_workspaces",
            &json!({}),
            &mut meta_out,
        )
        .await;
        assert!(!meta_out.text.contains("[HARNESS_HINT:"));

        // 2. First call targeting @repo-beta surfaces @repo-beta's checkpoint (NOT @repo-alpha's)
        let mut beta_out = ToolOutput {
            text: "ok beta".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-iso"),
            Some(&ws_b),
            "batch_inspect",
            &json!({ "workspace": "@repo-beta" }),
            &mut beta_out,
        )
        .await;
        assert!(
            beta_out.text.contains("@repo-beta") && beta_out.text.contains("Beta next step"),
            "Expected @repo-beta checkpoint anchor, got:\n{}",
            beta_out.text
        );

        // 3. Subsequent first call targeting @repo-alpha STILL surfaces @repo-alpha's own checkpoint
        let mut alpha_out = ToolOutput {
            text: "ok alpha".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-iso"),
            Some(&ws_a),
            "batch_inspect",
            &json!({ "workspace": "@repo-alpha" }),
            &mut alpha_out,
        )
        .await;
        assert!(
            alpha_out.text.contains("@repo-alpha") && alpha_out.text.contains("Alpha next step"),
            "Expected @repo-alpha checkpoint anchor, got:\n{}",
            alpha_out.text
        );

        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[tokio::test]
    async fn test_webmcp_harness_auto_injects_workspace_instructions_once_on_batch_inspect() {
        let base_dir = std::env::temp_dir().join("smart_mcp_harness_ws_rules_inject_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();
        std::fs::write(
            base_dir.join("GEMINI.md"),
            "Always run cargo check before committing workspace changes.",
        )
        .unwrap();

        let state = AppState::new(base_dir.clone());
        let ws = crate::workspace::resolve_workspace(&state, None, None)
            .await
            .unwrap();
        let mgr = WebMcpHarnessManager::default();

        // First batch_inspect injects GEMINI.md rules once
        let mut first_out = ToolOutput {
            text: "=== OVERVIEW ===\nok".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-rules"),
            Some(&ws),
            "batch_inspect",
            &json!({ "include_overview": true }),
            &mut first_out,
        )
        .await;
        assert!(
            first_out.text.contains("=== WORKSPACE RULES (@")
                && first_out.text.contains("Always run cargo check before committing"),
            "Expected auto-injected workspace rules on first batch_inspect, got:\n{}",
            first_out.text
        );

        // Second batch_inspect in same session & workspace does NOT repeat the rules banner
        let mut second_out = ToolOutput {
            text: "=== SEARCHES ===\nok".to_string(),
            is_error: false,
        };
        mgr.after_tool_call(
            &state,
            Some("sess-rules"),
            Some(&ws),
            "batch_inspect",
            &json!({ "searches": [{ "query": "foo" }] }),
            &mut second_out,
        )
        .await;
        assert!(
            !second_out.text.contains("=== WORKSPACE RULES (@"),
            "Did not expect repeated workspace rules banner, got:\n{}",
            second_out.text
        );

        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[tokio::test]
    async fn test_task_mode_uses_relaxed_rot_threshold_and_milestone_blueprint_nudge() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_harness_task_mode_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let state = AppState::new(temp_dir.clone());
        let ws = crate::workspace::resolve_workspace(&state, None, None)
            .await
            .unwrap();
        let mgr = WebMcpHarnessManager::default();

        for i in 0..29 {
            let mut out = ToolOutput {
                text: format!("ok_{i}"),
                is_error: false,
            };
            mgr.after_tool_call_with_mode(
                &state,
                Some("sess-task-rot"),
                Some(&ws),
                "list_workspaces",
                &json!({}),
                WebMcpExecutionMode::Task,
                &mut out,
            )
            .await;
            assert!(
                !out.text.contains("session_checkpoint(op=\"save\")"),
                "Task mode must not nudge before 30 calls (at call {i})"
            );
        }

        let mut thirtieth = ToolOutput {
            text: "ok_30".to_string(),
            is_error: false,
        };
        mgr.after_tool_call_with_mode(
            &state,
            Some("sess-task-rot"),
            Some(&ws),
            "list_workspaces",
            &json!({}),
            WebMcpExecutionMode::Task,
            &mut thirtieth,
        )
        .await;
        assert!(thirtieth.text.contains("session_checkpoint(op=\"save\")"));

        let mut bp_out = ToolOutput {
            text: "Wrote .gemini/blueprints/01-task.md".to_string(),
            is_error: false,
        };
        mgr.after_tool_call_with_mode(
            &state,
            Some("sess-task-bp"),
            Some(&ws),
            "write_file_jailed",
            &json!({ "path": ".gemini/blueprints/01-task.md", "content": "# Task BP" }),
            WebMcpExecutionMode::Task,
            &mut bp_out,
        )
        .await;
        assert!(bp_out.text.contains(".airlock/HANDOFF.md"));
        assert!(!bp_out.text.contains("(1/turn)"));
        assert!(!bp_out.text.contains("2-4 choices"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_task_mode_keeps_read_and_error_loop_guards_active() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_harness_task_loops_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let state = AppState::new(temp_dir.clone());
        let ws = crate::workspace::resolve_workspace(&state, None, None)
            .await
            .unwrap();
        let mgr = WebMcpHarnessManager::default();

        let read_args = json!({ "path": "src/main.rs", "start_line": 1, "end_line": 40 });
        for _ in 0..2 {
            let mut r_out = ToolOutput {
                text: "fn main() {}".to_string(),
                is_error: false,
            };
            mgr.after_tool_call_with_mode(
                &state,
                Some("sess-task-loops"),
                Some(&ws),
                "read_file_range",
                &read_args,
                WebMcpExecutionMode::Task,
                &mut r_out,
            )
            .await;
            if r_out.text.contains("[HARNESS_HINT:") {
                assert!(r_out.text.contains("2x with no edit"));
            }
        }

        let err_args = json!({ "action": "run", "command": "cargo check" });
        let mut e1 = ToolOutput {
            text: "error[E0425]: cannot find value".to_string(),
            is_error: true,
        };
        mgr.after_tool_call_with_mode(
            &state,
            Some("sess-task-loops"),
            Some(&ws),
            "run_command",
            &err_args,
            WebMcpExecutionMode::Task,
            &mut e1,
        )
        .await;
        let mut e2 = ToolOutput {
            text: "error[E0425]: cannot find value".to_string(),
            is_error: true,
        };
        mgr.after_tool_call_with_mode(
            &state,
            Some("sess-task-loops"),
            Some(&ws),
            "run_command",
            &err_args,
            WebMcpExecutionMode::Task,
            &mut e2,
        )
        .await;
        assert!(e2.text.contains("Repeated failure (2x)"), "got:\n{}", e2.text);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
