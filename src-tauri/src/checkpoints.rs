use crate::config::WebMcpExecutionMode;
use crate::jobs::{self, JobStatus};
use crate::state::{AppState, AuditEntry};
use crate::workspace::ResolvedWorkspace;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_CHECKPOINT_JSON_BYTES: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCheckpoint {
    pub workspace_id: String,
    pub workspace_alias: String,
    pub updated_at: String,
    pub goal: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub completed: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    #[serde(default)]
    pub open_issues: Vec<String>,
    #[serde(default)]
    pub model_notes: String,
    #[serde(default)]
    pub git_head_short: Option<String>,
    #[serde(default)]
    pub git_status_short: Vec<String>,
    #[serde(default)]
    pub recent_touched_files: Vec<String>,
    #[serde(default)]
    pub running_job_ids: Vec<String>,
    #[serde(default)]
    pub last_audit_group_id: Option<String>,
    #[serde(default)]
    pub blueprint_path: Option<String>,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub blueprint_paths: Vec<String>,
    #[serde(default)]
    pub recommended_skills: Vec<String>,
}

pub(crate) fn normalize_checkpoint_stage(raw: Option<&str>, bp_paths: &[String]) -> String {
    match raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("inspect") => "inspect".to_string(),
        Some("blueprint") => "blueprint".to_string(),
        Some("implement") => "implement".to_string(),
        _ => {
            if !bp_paths.is_empty() {
                "blueprint".to_string()
            } else {
                "implement".to_string()
            }
        }
    }
}

pub(crate) fn collected_blueprint_paths(cp: &SessionCheckpoint) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(ref single) = cp.blueprint_path {
        let norm = single.trim().replace('\\', "/");
        if !norm.is_empty() && seen.insert(norm.clone()) {
            out.push(norm);
        }
    }
    for p in &cp.blueprint_paths {
        let norm = p.trim().replace('\\', "/");
        if !norm.is_empty() && seen.insert(norm.clone()) {
            out.push(norm);
        }
    }
    out
}

pub(crate) fn effective_checkpoint_stage(cp: &SessionCheckpoint) -> String {
    let bps = collected_blueprint_paths(cp);
    normalize_checkpoint_stage(cp.stage.as_deref(), &bps)
}

fn format_checkpoint_metadata_lines(cp: &SessionCheckpoint) -> Vec<String> {
    let mut details: Vec<String> = vec![format!("Goal: {}", clip_str(&cp.goal, 240))];
    if !cp.decisions.is_empty() {
        let decs: Vec<String> = cp.decisions.iter().take(3).map(|d| clip_str(d, 100)).collect();
        details.push(format!("Decisions: {}", decs.join("; ")));
    }
    if !cp.next_steps.is_empty() {
        let steps: Vec<String> = cp
            .next_steps
            .iter()
            .take(3)
            .enumerate()
            .map(|(i, s)| format!("{}. {}", i + 1, clip_str(s, 100)))
            .collect();
        details.push(format!("Next steps: {}", steps.join(" ")));
    }
    details
}

fn format_skills_hint(cp: &SessionCheckpoint) -> String {
    if cp.recommended_skills.is_empty() {
        "load matching task/architecture/verification skills via `load_skill`".to_string()
    } else {
        let list: Vec<String> = cp
            .recommended_skills
            .iter()
            .take(6)
            .map(|s| format!("`{s}`"))
            .collect();
        format!("load {}", list.join(", "))
    }
}

pub(crate) fn build_webmcp_next_session_prompt(cp: &SessionCheckpoint) -> String {
    build_webmcp_next_session_prompt_for_mode(cp, WebMcpExecutionMode::Chat)
}

pub(crate) fn build_webmcp_next_session_prompt_for_mode(
    cp: &SessionCheckpoint,
    mode: WebMcpExecutionMode,
) -> String {
    let stage = effective_checkpoint_stage(cp);
    let bps = collected_blueprint_paths(cp);
    let bp_ref = bps.first().cloned().unwrap_or_default();
    let bp_list_note = if bps.len() > 1 {
        format!(" (also tracked: {})", bps[1..].join(", "))
    } else {
        String::new()
    };
    let skills_hint = format_skills_hint(cp);
    let details = format_checkpoint_metadata_lines(cp);

    let body = match (mode, stage.as_str()) {
        (WebMcpExecutionMode::Task, "inspect") => format!(
            "At workspace '@{}', resume Task Mode from checkpoint (`session_checkpoint(op=\"load\", workspace=\"{}\")` or `airlock://workspace/checkpoint`) and read `.airlock/HANDOFF.md`.\n\
             1. SKILLS: Call `list_skills`, load `airlock-task-workflow`, and {skills_hint}.\n\
             2. PRE-FLIGHT CHECKUP: Verify inspected files/symbols against the live codebase via `batch_inspect` + `lsp_query`.\n\
             3. AUTONOMOUS MILESTONE: Complete the research & full blueprint deliverable under `.gemini/blueprints/` (or complete the adversarial review) in this run without pausing per file.\n\
             4. CHECKPOINT: Save `session_checkpoint(op=\"save\", stage=\"blueprint\"|\"inspect\", ...)` at milestone completion so `.airlock/HANDOFF.md` stays ready for Antigravity.",
            cp.workspace_alias,
            cp.workspace_alias,
        ),
        (WebMcpExecutionMode::Task, "blueprint") => {
            let bp_target = if bp_ref.is_empty() {
                "the target blueprint in '.gemini/blueprints/'".to_string()
            } else {
                format!("blueprint '{bp_ref}'{bp_list_note}")
            };
            format!(
                "At workspace '@{}', execute Task Mode autonomous milestone: read `.airlock/HANDOFF.md`, {bp_target}, and `airlock://workspace/checkpoint` (or `session_checkpoint(op=\"load\", workspace=\"{}\")`).\n\
                 1. SKILLS: Call `list_skills`, load `airlock-task-workflow`, and {skills_hint}.\n\
                 2. PRE-FLIGHT CHECKUP: Run `batch_inspect` to verify target files, symbols, and invariants in {bp_target} match the live codebase BEFORE editing.\n\
                 3. CLOSED-LOOP EXECUTION: Implement {bp_target} and run verification (`multi_surgical_edit` with `post_check_lsp: true` + `run_command` tests/build) until green; pause only on design ambiguity or destructive risk.\n\
                 4. CHECKPOINT: Save `session_checkpoint(op=\"save\", stage=\"implement\", ...)` when the milestone finishes.",
                cp.workspace_alias,
                cp.workspace_alias,
            )
        }
        (WebMcpExecutionMode::Task, _) => {
            let bp_clause = if bp_ref.is_empty() {
                String::new()
            } else {
                format!(" and blueprint '{bp_ref}'{bp_list_note}")
            };
            format!(
                "At workspace '@{}', continue Task Mode autonomous execution: read `.airlock/HANDOFF.md`{bp_clause} and `airlock://workspace/checkpoint` (or `session_checkpoint(op=\"load\", workspace=\"{}\")`).\n\
                 1. SKILLS: Ensure `airlock-task-workflow` is loaded via `list_skills` and {skills_hint}.\n\
                 2. PRE-FLIGHT CHECKUP: Inspect live drift vs remaining work via `batch_inspect` before editing.\n\
                 3. FINISH & VERIFY: Complete remaining milestone steps, iterate on diagnostics/test failures until green, then save `session_checkpoint(op=\"save\", stage=\"implement\", ...)`.",
                cp.workspace_alias,
                cp.workspace_alias,
            )
        }
        (WebMcpExecutionMode::Chat, "inspect") => format!(
            "At workspace '@{}', resume from checkpoint (`session_checkpoint(op=\"load\", workspace=\"{}\")` or `airlock://workspace/checkpoint`) and read `.airlock/HANDOFF.md`.\n\
             1. SKILLS: Call `list_skills`, load `airlock-blueprint-workflow`, and {skills_hint}.\n\
             2. PRE-FLIGHT CHECKUP: Verify the inspected files/symbols and `.airlock/HANDOFF.md` against the live codebase via `batch_inspect` before drafting.\n\
             3. BLUEPRINT MODE (MULTI-TURN): Propose the blueprint list if not finalized, then write AT MOST 1 complete blueprint file per turn into `.gemini/blueprints/` (NO implementation code yet).\n\
             4. STOP & OFFER CHOICES: Stop after 1 blueprint and end with 1 short follow-up question + 2-4 concise actionable choices.",
            cp.workspace_alias,
            cp.workspace_alias,
        ),
        (WebMcpExecutionMode::Chat, "blueprint") => {
            let bp_target = if bp_ref.is_empty() {
                "the target blueprint in '.gemini/blueprints/'".to_string()
            } else {
                format!("blueprint '{bp_ref}'{bp_list_note}")
            };
            format!(
                "At workspace '@{}', implement 1 blueprint in self-pacing multi-turn mode: read `.airlock/HANDOFF.md`, {bp_target}, and load `airlock://workspace/checkpoint` (or `session_checkpoint(op=\"load\", workspace=\"{}\")`).\n\
                 1. SKILLS: Call `list_skills`, load `airlock-blueprint-workflow`, and {skills_hint}.\n\
                 2. PRE-FLIGHT CHECKUP: Run `batch_inspect` to verify target files, symbols, and invariants in {bp_target} match the live codebase BEFORE editing. Report any mismatch first.\n\
                 3. MULTI-TURN (1 BLUEPRINT): Break execution into small phases and complete ONLY ONE phase + its verification this turn. Do NOT attempt the whole blueprint in one turn.\n\
                 4. STOP & OFFER CHOICES: Update `session_checkpoint` if needed, then STOP and end with 1 short follow-up question + 2-4 concise choices (next phase / re-check / handoff to Antigravity).",
                cp.workspace_alias,
                cp.workspace_alias,
            )
        }
        (WebMcpExecutionMode::Chat, _) => {
            let bp_clause = if bp_ref.is_empty() {
                String::new()
            } else {
                format!(" and blueprint '{bp_ref}'{bp_list_note}")
            };
            format!(
                "At workspace '@{}', continue self-pacing multi-turn implementation: read `.airlock/HANDOFF.md`{bp_clause} and load `airlock://workspace/checkpoint` (or `session_checkpoint(op=\"load\", workspace=\"{}\")`).\n\
                 1. SKILLS: Ensure `airlock-blueprint-workflow` is active via `list_skills` and {skills_hint}.\n\
                 2. PRE-FLIGHT CHECKUP: Inspect live drift vs the next unchecked phase via `batch_inspect` before editing.\n\
                 3. MULTI-TURN: Implement and verify ONLY the next single phase of this 1 blueprint, then STOP.\n\
                 4. OFFER CHOICES: End with 1 short follow-up question + 2-4 concise choices (continue next phase / debug / finalize or handoff to Antigravity).",
                cp.workspace_alias,
                cp.workspace_alias,
            )
        }
    };

    format!("{body}\n{}", details.join("\n"))
}

pub(crate) fn build_fallback_antigravity_handoff_prompt(ws_alias: &str) -> String {
    format!(
        "At workspace '@{ws_alias}', read '.airlock/HANDOFF.md' (and any files under '.airlock/handoff/' or '.gemini/blueprints/').\n\
         1. SKILLS: Call `list_skills` and load matching task/architecture/verification skills via `load_skill`.\n\
         2. PRE-FLIGHT & FIELD AUDIT: Inspect live codebase state and recent diffs against the handoff summary before editing.\n\
         3. EXECUTE & VERIFY: Continue/implement remaining steps and run full tests/build to verify."
    )
}

pub(crate) fn is_blueprint_doc_path(path: &str) -> bool {
    let norm = path.trim().replace('\\', "/").to_ascii_lowercase();
    if !(norm.ends_with(".md") || norm.ends_with(".markdown")) {
        return false;
    }
    if norm.contains("/blueprints/") || norm.starts_with("blueprints/") {
        return true;
    }
    let filename = norm.rsplit('/').next().unwrap_or(&norm);
    let stem = filename
        .strip_suffix(".markdown")
        .or_else(|| filename.strip_suffix(".md"))
        .unwrap_or(filename);
    stem == "plan"
        || stem == "spec"
        || stem == "handoff"
        || stem.contains("blueprint")
        || stem.contains("implementation_plan")
        || stem.starts_with("plan-")
        || stem.starts_with("plan_")
        || stem.ends_with("-plan")
        || stem.ends_with("_plan")
        || stem.starts_with("spec-")
        || stem.starts_with("spec_")
        || stem.ends_with("-spec")
        || stem.ends_with("_spec")
}

pub(crate) fn build_antigravity_handoff_prompt(cp: &SessionCheckpoint) -> String {
    let stage = effective_checkpoint_stage(cp);
    let bps = collected_blueprint_paths(cp);
    let bp_clause = match bps.as_slice() {
        [] => String::new(),
        [only] => format!(" and read blueprint '{only}'"),
        [first, rest @ ..] => {
            format!(" and read blueprint '{first}' (see also: {})", rest.join(", "))
        }
    };
    let skills_hint = format_skills_hint(cp);
    let details = format_checkpoint_metadata_lines(cp);

    let body = match stage.as_str() {
        "inspect" => format!(
            "At workspace '@{}', read '.airlock/HANDOFF.md' or load `airlock://workspace/checkpoint` (or call `session_checkpoint(op=\"load\", workspace=\"{}\")`){bp_clause}.\n\
             1. SKILLS: Check and {skills_hint} before designing or editing.\n\
             2. PRE-FLIGHT & FIELD AUDIT: Audit live codebase conditions against inspection findings to confirm all symbols, invariants, and gaps.\n\
             3. EXECUTE: Draft/finalize blueprint(s) or implement step-by-step and run tests/build to verify.",
            cp.workspace_alias,
            cp.workspace_alias,
        ),
        "implement" => format!(
            "At workspace '@{}', take over/verify implementation: read '.airlock/HANDOFF.md', '.airlock/handoff/progress.md', '.airlock/handoff/latest.diff', or load `airlock://workspace/checkpoint` (or call `session_checkpoint(op=\"load\", workspace=\"{}\")`){bp_clause}.\n\
             1. SKILLS: Check and {skills_hint}.\n\
             2. PRE-FLIGHT & FIELD AUDIT: Audit partial WebMCP edits and live codebase state against the blueprint to spot drift, regressions, or gaps before continuing.\n\
             3. FINISH & VERIFY: Complete/fix remaining steps and run full tests/build to verify.",
            cp.workspace_alias,
            cp.workspace_alias,
        ),
        _ => format!(
            "At workspace '@{}', read '.airlock/HANDOFF.md' or load `airlock://workspace/checkpoint` (or call `session_checkpoint(op=\"load\", workspace=\"{}\")`){bp_clause}.\n\
             1. SKILLS: Check and {skills_hint}.\n\
             2. PRE-FLIGHT & FIELD AUDIT: Inspect the live codebase against the blueprint (check moved paths, changed signatures, missing assumptions, or conflicts) and adapt/report mismatches before editing.\n\
             3. IMPLEMENT & VERIFY: Verify alignment with the current codebase, then implement step-by-step and run tests/build to verify.",
            cp.workspace_alias,
            cp.workspace_alias,
        ),
    };

    format!("{body}\n{}", details.join("\n"))
}

fn checkpoint_file_path(state: &AppState, workspace_id: &str) -> PathBuf {
    let safe_id: String = workspace_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let file_stem = if safe_id.is_empty() {
        "default".to_string()
    } else {
        safe_id
    };
    let base = state
        .data_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("airlock_ephemeral_checkpoints"));
    base.join("checkpoints")
        .join(format!("{file_stem}.json"))
}

pub(crate) fn load_checkpoint_struct(
    state: &AppState,
    workspace_id: &str,
) -> Option<SessionCheckpoint> {
    let path = checkpoint_file_path(state, workspace_id);
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<SessionCheckpoint>(&raw).ok()
}

/// Fast, git-free peek into saved checkpoint next steps for the WebMCP session harness.
pub(crate) fn peek_checkpoint_next_step(
    state: &AppState,
    workspace_id: &str,
) -> Option<(String, String)> {
    let path = checkpoint_file_path(state, workspace_id);
    let raw = fs::read_to_string(path).ok()?;
    let cp: SessionCheckpoint = serde_json::from_str(&raw).ok()?;
    let next_hint = cp
        .next_steps
        .iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .or_else(|| {
            if cp.status.as_deref() == Some("completed") {
                None
            } else {
                let goal = cp.goal.trim();
                if goal.is_empty() {
                    None
                } else {
                    Some(goal)
                }
            }
        })?;
    Some((cp.workspace_alias, next_hint.to_string()))
}

async fn run_git_capture(root: &Path, args: &[&str]) -> Option<(i32, String)> {
    crate::process_util::run_git_command_bounded(root, args, true, 6)
        .await
        .ok()
        .map(|(code, stdout, _)| (code, stdout))
}

async fn collect_git_state(root: &Path) -> (Option<String>, Vec<String>) {
    let head = match run_git_capture(root, &["rev-parse", "--short", "HEAD"]).await {
        Some((0, txt)) => {
            let trimmed = txt.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        _ => None,
    };

    let status_lines = match run_git_capture(root, &["status", "--short"]).await {
        Some((0, txt)) => txt
            .lines()
            .map(|l| l.trim_end().to_string())
            .filter(|l| {
                if l.trim().is_empty() {
                    return false;
                }
                let raw_path = l.get(3..).unwrap_or("").trim();
                let candidate = raw_path.rsplit(" -> ").next().unwrap_or(raw_path).trim().trim_matches('"');
                !crate::handoff::is_handoff_ignored_rel(candidate)
            })
            .take(20)
            .collect(),
        _ => Vec::new(),
    };

    (head, status_lines)
}

fn parse_string_list(args: &Value, key: &str, max_items: usize, max_item_chars: usize) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .take(max_items)
                .map(|s| s.chars().take(max_item_chars).collect())
                .collect()
        })
        .unwrap_or_default()
}

fn clip_str(raw: &str, max_chars: usize) -> String {
    let trimmed = raw.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{head}...")
    }
}

fn save_checkpoint_atomic(path: &Path, cp: &mut SessionCheckpoint) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Gagal membuat direktori checkpoint: {e}"))?;
    }

    let mut json_str = serde_json::to_string_pretty(cp)
        .map_err(|e| format!("Gagal serialisasi session checkpoint: {e}"))?;

    if json_str.len() > MAX_CHECKPOINT_JSON_BYTES {
        cp.model_notes = clip_str(&cp.model_notes, 800);
        cp.decisions.truncate(8);
        cp.completed.truncate(8);
        cp.next_steps.truncate(8);
        cp.open_issues.truncate(8);
        json_str = serde_json::to_string_pretty(cp)
            .map_err(|e| format!("Gagal serialisasi session checkpoint: {e}"))?;
    }
    if json_str.len() > MAX_CHECKPOINT_JSON_BYTES {
        cp.model_notes = clip_str(&cp.model_notes, 200);
        cp.goal = clip_str(&cp.goal, 600);
        json_str = serde_json::to_string_pretty(cp)
            .map_err(|e| format!("Gagal serialisasi session checkpoint: {e}"))?;
    }
    if json_str.len() > MAX_CHECKPOINT_JSON_BYTES {
        return Err(format!(
            "Session checkpoint melebihi batas maksimum {} bytes ({} bytes). Ringkas teks input terlebih dahulu.",
            MAX_CHECKPOINT_JSON_BYTES,
            json_str.len()
        ));
    }

    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, &json_str)
        .map_err(|e| format!("Gagal menulis file sementara checkpoint: {e}"))?;
    if fs::rename(&tmp_path, path).is_err() {
        fs::write(path, &json_str)
            .map_err(|e| format!("Gagal menyimpan checkpoint: {e}"))?;
        let _ = fs::remove_file(&tmp_path);
    }
    Ok(())
}

pub(crate) async fn format_checkpoint_with_drift(
    state: &AppState,
    ws: &ResolvedWorkspace,
) -> Result<Option<String>, String> {
    let path = checkpoint_file_path(state, &ws.id);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("Gagal membaca checkpoint untuk @{}: {e}", ws.alias))?;
    let cp: SessionCheckpoint = serde_json::from_str(&raw)
        .map_err(|e| format!("Format checkpoint untuk @{} tidak valid: {e}", ws.alias))?;

    let (cur_head, cur_status) = collect_git_state(&ws.canon_root).await;
    let live_running_jobs: Vec<String> = jobs::list_jobs(state)
        .await
        .into_iter()
        .filter(|j| {
            j.status == JobStatus::Running
                && j.workspace_alias
                    .as_deref()
                    .map(|a| a.eq_ignore_ascii_case(&ws.alias))
                    .unwrap_or(true)
        })
        .map(|j| format!("{} ({})", j.job_id, j.command.chars().take(48).collect::<String>()))
        .collect();

    let mut out = Vec::new();
    out.push(format!(
        "SESSION_CHECKPOINT (@{} · updated {})",
        cp.workspace_alias, cp.updated_at
    ));
    out.push(format!("GOAL: {}", cp.goal));
    out.push(format!("STAGE: {}", effective_checkpoint_stage(&cp)));
    if let Some(ref status) = cp.status {
        if !status.trim().is_empty() {
            out.push(format!("STATUS: {}", status.trim()));
        }
    }
    if let Some(ref bp) = cp.blueprint_path {
        if !bp.trim().is_empty() {
            out.push(format!("BLUEPRINT_PATH: {}", bp.trim()));
        }
    }
    let all_bps = collected_blueprint_paths(&cp);
    if all_bps.len() > 1 {
        out.push(format!("BLUEPRINT_PATHS: {}", all_bps.join(", ")));
    }
    if !cp.recommended_skills.is_empty() {
        out.push(format!(
            "RECOMMENDED_SKILLS: {}",
            cp.recommended_skills.join(", ")
        ));
    }

    if !cp.decisions.is_empty() {
        out.push(format!("DECISIONS:\n- {}", cp.decisions.join("\n- ")));
    }
    if !cp.completed.is_empty() {
        out.push(format!("COMPLETED:\n- {}", cp.completed.join("\n- ")));
    }
    if !cp.next_steps.is_empty() {
        let numbered: Vec<String> = cp
            .next_steps
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {}", i + 1, s))
            .collect();
        out.push(format!("NEXT_STEPS:\n{}", numbered.join("\n")));
    }
    if !cp.open_issues.is_empty() {
        out.push(format!("OPEN_ISSUES:\n- {}", cp.open_issues.join("\n- ")));
    }
    if !cp.model_notes.trim().is_empty() {
        out.push(format!("NOTES: {}", cp.model_notes.trim()));
    }
    if let Some(ref gid) = cp.last_audit_group_id {
        out.push(format!("LAST_AUDIT_GROUP_ID: {gid}"));
    }
    if !cp.recent_touched_files.is_empty() {
        out.push(format!(
            "RECENT_TOUCHED_FILES: {}",
            cp.recent_touched_files.join(", ")
        ));
    }
    if let Some(ref saved_head) = cp.git_head_short {
        out.push(format!("SAVED_GIT_HEAD: {saved_head}"));
    }
    if !live_running_jobs.is_empty() {
        out.push(format!("LIVE_RUNNING_JOBS: {}", live_running_jobs.join("; ")));
    } else if !cp.running_job_ids.is_empty() {
        out.push(format!(
            "PREVIOUSLY_RUNNING_JOBS: {}",
            cp.running_job_ids.join(", ")
        ));
    }

    let saved_set: HashSet<&str> = cp.git_status_short.iter().map(|s| s.as_str()).collect();
    let cur_set: HashSet<&str> = cur_status.iter().map(|s| s.as_str()).collect();
    let mut drift_lines = Vec::new();
    if cur_head != cp.git_head_short {
        drift_lines.push(format!(
            "- HEAD changed: {} -> {}",
            cp.git_head_short.as_deref().unwrap_or("(none)"),
            cur_head.as_deref().unwrap_or("(none)")
        ));
    }
    for line in &cur_status {
        if !saved_set.contains(line.as_str()) {
            drift_lines.push(format!("- [new/changed] {line}"));
        }
    }
    for line in &cp.git_status_short {
        if !cur_set.contains(line.as_str()) {
            drift_lines.push(format!("- [cleared/committed] {line}"));
        }
    }
    if !drift_lines.is_empty() {
        out.push(format!(
            "LIVE_DRIFT_SINCE_CHECKPOINT:\n{}",
            drift_lines.join("\n")
        ));
    } else if !cp.git_status_short.is_empty() {
        out.push(format!(
            "GIT_STATUS_SHORT (no drift):\n{}",
            cp.git_status_short.join("\n")
        ));
    }

    Ok(Some(out.join("\n")))
}

pub(crate) async fn exec_session_checkpoint(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    exec_session_checkpoint_with_mode(state, source, ws, args, WebMcpExecutionMode::Chat).await
}

pub(crate) async fn exec_session_checkpoint_with_mode(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
    mode: WebMcpExecutionMode,
) -> Result<String, String> {
    let op = args
        .get("op")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "load".to_string());

    match op.as_str() {
        "save" => {
            let goal_raw = args
                .get("goal")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("Argumen 'goal' wajib diisi saat op='save'.")?;
            let goal = clip_str(goal_raw, 800);
            let status = args
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| clip_str(s, 80))
                .filter(|s| !s.is_empty());
            let decisions = parse_string_list(args, "decisions", 12, 260);
            let completed = parse_string_list(args, "completed", 12, 260);
            let next_steps = parse_string_list(args, "next_steps", 12, 260);
            let open_issues = parse_string_list(args, "open_issues", 12, 260);
            let explicit_touched = parse_string_list(args, "touched_files", 15, 220);
            let explicit_blueprint_path = args
                .get("blueprint_path")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().replace('\\', "/"))
                .filter(|s| !s.is_empty())
                .map(|s| clip_str(&s, 220));
            let explicit_blueprint_paths = parse_string_list(args, "blueprint_paths", 8, 220);
            let recommended_skills = parse_string_list(args, "recommended_skills", 10, 80);
            let explicit_stage = args
                .get("stage")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let model_notes = args
                .get("notes")
                .or_else(|| args.get("model_notes"))
                .and_then(|v| v.as_str())
                .map(|s| clip_str(s, 1800))
                .unwrap_or_default();

            let (git_head_short, git_status_short) = collect_git_state(&ws.canon_root).await;

            let (recent_touched_files, last_audit_group_id) = {
                let logs = state.audit_logs.read().await;
                let mut seen = HashSet::new();
                let mut touched = Vec::new();
                let mut last_gid: Option<String> = None;

                for entry in logs.iter().rev() {
                    let same_ws = entry.workspace_id.as_deref() == Some(ws.id.as_str())
                        || entry
                            .workspace_alias
                            .as_deref()
                            .map(|a| a.eq_ignore_ascii_case(&ws.alias))
                            .unwrap_or(false);
                    if !same_ws {
                        continue;
                    }
                    if last_gid.is_none() {
                        if let Some(ref gid) = entry.group_id {
                            last_gid = Some(gid.clone());
                        }
                    }
                    if entry.can_revert {
                        if let Some(ref p) = entry.target_path {
                            if seen.insert(p.clone()) {
                                touched.push(p.clone());
                                if touched.len() >= 10 {
                                    break;
                                }
                            }
                        }
                    }
                }
                for explicit in &explicit_touched {
                    if seen.insert(explicit.clone()) {
                        touched.push(explicit.clone());
                        if touched.len() >= 15 {
                            break;
                        }
                    }
                }
                (touched, last_gid)
            };

            let mut blueprint_paths: Vec<String> = Vec::new();
            let mut bp_seen: HashSet<String> = HashSet::new();
            if let Some(ref primary) = explicit_blueprint_path {
                if bp_seen.insert(primary.clone()) {
                    blueprint_paths.push(primary.clone());
                }
            }
            for raw in explicit_blueprint_paths {
                let norm = clip_str(&raw.replace('\\', "/"), 220);
                if !norm.is_empty() && bp_seen.insert(norm.clone()) {
                    blueprint_paths.push(norm);
                }
            }
            if blueprint_paths.is_empty() {
                if let Some(detected) = explicit_touched
                    .iter()
                    .find(|p| is_blueprint_doc_path(p))
                    .or_else(|| recent_touched_files.iter().find(|p| is_blueprint_doc_path(p)))
                {
                    let norm = clip_str(&detected.replace('\\', "/"), 220);
                    if !norm.is_empty() {
                        blueprint_paths.push(norm);
                    }
                }
            }
            let blueprint_path = blueprint_paths.first().cloned();
            let stage = normalize_checkpoint_stage(explicit_stage, &blueprint_paths);

            let running_job_ids: Vec<String> = jobs::list_jobs(state)
                .await
                .into_iter()
                .filter(|j| {
                    j.status == JobStatus::Running
                        && j.workspace_alias
                            .as_deref()
                            .map(|a| a.eq_ignore_ascii_case(&ws.alias))
                            .unwrap_or(true)
                })
                .map(|j| j.job_id)
                .collect();

            let updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let mut cp = SessionCheckpoint {
                workspace_id: ws.id.clone(),
                workspace_alias: ws.alias.clone(),
                updated_at: updated_at.clone(),
                goal,
                status,
                decisions,
                completed,
                next_steps,
                open_issues,
                model_notes,
                git_head_short,
                git_status_short,
                recent_touched_files,
                running_job_ids,
                last_audit_group_id,
                blueprint_path,
                stage: Some(stage.clone()),
                blueprint_paths,
                recommended_skills,
            };

            let path = checkpoint_file_path(state, &ws.id);
            save_checkpoint_atomic(&path, &mut cp)?;

            state
                .record_audit(
                    AuditEntry {
                        id: AppState::next_audit_id(),
                        timestamp: AppState::now_timestamp(),
                        source: source.to_string(),
                        tool_name: "session_checkpoint".to_string(),
                        target_path: None,
                        summary: format!("Saved session checkpoint for @{}", ws.alias),
                        diff_text: None,
                        can_revert: false,
                        reverted: false,
                        tokens_saved: 200,
                        original_content: None,
                        original_blob_hash: None,
                        file_existed_before: false,
                        post_edit_hash: None,
                        group_id: None,
                        move_from_path: None,
                        workspace_id: Some(ws.id.clone()),
                        workspace_alias: Some(ws.alias.clone()),
                        workspace_root: Some(ws.clean_path.clone()),
                    },
                    1,
                )
                .await;

            let mut handoff_line = String::new();
            if crate::handoff::is_webmcp_source(source) {
                match crate::handoff::sync_from_state(
                    state,
                    ws,
                    crate::handoff::HandoffTrigger::CheckpointSaved,
                    false,
                )
                .await
                {
                    Ok(p) if !p.is_empty() => {
                        handoff_line = "\nHANDOFF_PATH: .airlock/HANDOFF.md".to_string();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        handoff_line = format!("\nHANDOFF_SYNC_WARN: {e}");
                    }
                }
            }

            let webmcp_prompt = build_webmcp_next_session_prompt_for_mode(&cp, mode);
            let handoff_prompt = build_antigravity_handoff_prompt(&cp);
            Ok(format!(
                "CHECKPOINT_SAVED: workspace=@{} stage={} updated_at={} git_head={} touched={} running_jobs={}{}\nResume in any new chat with: session_checkpoint(op=\"load\", workspace=\"{}\") or resource airlock://workspace/checkpoint.\nWEBMCP_NEXT_SESSION_PROMPT:\n```text\n{}\n```\nANTIGRAVITY_HANDOFF_PROMPT:\n```text\n{}\n```",
                ws.alias,
                stage,
                updated_at,
                cp.git_head_short.as_deref().unwrap_or("none"),
                cp.recent_touched_files.len(),
                cp.running_job_ids.len(),
                handoff_line,
                ws.alias,
                webmcp_prompt,
                handoff_prompt
            ))
        }
        "load" => {
            let msg = match format_checkpoint_with_drift(state, ws).await? {
                Some(txt) => txt,
                None => format!(
                    "Belum ada session_checkpoint tersimpan untuk @{}. Panggil session_checkpoint(op=\"save\", goal=...) sebelum berpindah chat.",
                    ws.alias
                ),
            };
            state
                .record_audit(
                    AuditEntry {
                        id: AppState::next_audit_id(),
                        timestamp: AppState::now_timestamp(),
                        source: source.to_string(),
                        tool_name: "session_checkpoint".to_string(),
                        target_path: None,
                        summary: format!("Loaded session checkpoint for @{}", ws.alias),
                        diff_text: None,
                        can_revert: false,
                        reverted: false,
                        tokens_saved: 350,
                        original_content: None,
                        original_blob_hash: None,
                        file_existed_before: false,
                        post_edit_hash: None,
                        group_id: None,
                        move_from_path: None,
                        workspace_id: Some(ws.id.clone()),
                        workspace_alias: Some(ws.alias.clone()),
                        workspace_root: Some(ws.clean_path.clone()),
                    },
                    2,
                )
                .await;
            Ok(msg)
        }
        "clear" => {
            let path = checkpoint_file_path(state, &ws.id);
            let existed = path.is_file();
            if existed {
                fs::remove_file(&path)
                    .map_err(|e| format!("Gagal menghapus checkpoint @{}: {e}", ws.alias))?;
            }
            if crate::handoff::is_webmcp_source(source) && crate::handoff::workspace_has_handoff(ws) {
                let _ = crate::handoff::sync_from_state(
                    state,
                    ws,
                    crate::handoff::HandoffTrigger::CheckpointCleared,
                    false,
                )
                .await;
            }
            state
                .record_audit(
                    AuditEntry {
                        id: AppState::next_audit_id(),
                        timestamp: AppState::now_timestamp(),
                        source: source.to_string(),
                        tool_name: "session_checkpoint".to_string(),
                        target_path: None,
                        summary: format!("Cleared session checkpoint for @{}", ws.alias),
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
                        workspace_id: Some(ws.id.clone()),
                        workspace_alias: Some(ws.alias.clone()),
                        workspace_root: Some(ws.clean_path.clone()),
                    },
                    0,
                )
                .await;
            Ok(if existed {
                format!("CHECKPOINT_CLEARED: checkpoint untuk @{} telah dihapus.", ws.alias)
            } else {
                format!("Tidak ada checkpoint aktif untuk @{}.", ws.alias)
            })
        }
        other => Err(format!(
            "Operasi session_checkpoint '{other}' tidak dikenal (gunakan: save | load | clear)."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::execute_tool;
    use serde_json::json;

    fn run_git_sync(root: &Path, args: &[&str]) {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(root);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        let out = cmd.output().expect("git command should run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn test_session_checkpoint_save_load_and_drift_detection() {
        let base = std::env::temp_dir().join("smart_mcp_session_checkpoint_test");
        let _ = fs::remove_dir_all(&base);
        let ws_dir = base.join("repo");
        let data_dir = base.join("data");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        fs::write(ws_dir.join("main.rs"), "fn main() { println!(\"v1\"); }\n").unwrap();
        run_git_sync(&ws_dir, &["init"]);
        run_git_sync(&ws_dir, &["config", "user.email", "test@airlock.local"]);
        run_git_sync(&ws_dir, &["config", "user.name", "Airlock Test"]);
        run_git_sync(&ws_dir, &["add", "."]);
        run_git_sync(&ws_dir, &["commit", "-m", "initial"]);

        let state = AppState::new_persistent(
            ws_dir.clone(),
            data_dir.join("auth.json"),
            data_dir.join("config.json"),
        );

        let edit_out = execute_tool(
            &state,
            "WebMCP",
            "surgical_edit",
            json!({
                "path": "main.rs",
                "edits": [{ "search": "v1", "replace": "v2" }]
            }),
        )
        .await;
        assert!(!edit_out.is_error, "edit failed: {}", edit_out.text);

        let ws = crate::workspace::resolve_workspace(&state, None, None)
            .await
            .unwrap();

        let saved = exec_session_checkpoint(
            &state,
            "WebMCP",
            &ws,
            &json!({
                "op": "save",
                "goal": "Implement checkpoint parity",
                "decisions": ["Store checkpoints in ~/.smart-tauri-hub/checkpoints/"],
                "completed": ["Added config fields"],
                "next_steps": ["Add automated drift test"],
                "open_issues": [],
                "model_notes": "Ready for verification"
            }),
        )
        .await
        .unwrap();
        assert!(saved.contains("CHECKPOINT_SAVED"));

        // Verify checkpoint file lives in data_dir/checkpoints, NEVER inside ws_dir!
        let cp_file = data_dir
            .join("checkpoints")
            .join(format!("{}.json", ws.id));
        assert!(cp_file.is_file(), "Expected checkpoint file at {:?}", cp_file);
        assert!(!ws_dir.join(".smart-tauri-hub").exists());
        let cp_raw = fs::read_to_string(&cp_file).unwrap();
        assert!(cp_raw.contains("main.rs"));

        // Modify/create a new file in workspace AFTER checkpoint save to trigger live drift
        fs::write(ws_dir.join("new_drift.txt"), "drifted after checkpoint\n").unwrap();

        let loaded = exec_session_checkpoint(&state, "WebMCP", &ws, &json!({ "op": "load" }))
            .await
            .unwrap();
        assert!(loaded.contains("GOAL: Implement checkpoint parity"));
        assert!(loaded.contains("1. Add automated drift test"));
        assert!(loaded.contains("RECENT_TOUCHED_FILES: main.rs"));
        assert!(loaded.contains("LIVE_DRIFT_SINCE_CHECKPOINT:"));
        assert!(loaded.contains("new_drift.txt"));

        let cleared = exec_session_checkpoint(&state, "WebMCP", &ws, &json!({ "op": "clear" }))
            .await
            .unwrap();
        assert!(cleared.contains("CHECKPOINT_CLEARED"));
        assert!(!cp_file.exists());

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_session_checkpoint_schema_fields_and_default_op_load() {
        let base = std::env::temp_dir().join("smart_mcp_session_checkpoint_schema_test");
        let _ = fs::remove_dir_all(&base);
        let ws_dir = base.join("repo");
        let data_dir = base.join("data");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(ws_dir.join("notes.txt"), "hello\n").unwrap();

        let state = AppState::new_persistent(
            ws_dir.clone(),
            data_dir.join("auth.json"),
            data_dir.join("config.json"),
        );

        let save_out = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({
                "op": "SAVE",
                "goal": "Schema alignment test",
                "status": "ready_to_verify",
                "decisions": ["Keep checkpoints outside repo"],
                "next_steps": ["Run cargo test"],
                "touched_files": ["src/main.ts", "notes.txt"],
                "notes": "Handoff note via schema field"
            }),
        )
        .await;
        assert!(!save_out.is_error, "save failed: {}", save_out.text);

        // Omitting 'op' must default to 'load' per inputSchema, and preserve status/touched_files/notes
        let load_out = execute_tool(&state, "WebMCP", "session_checkpoint", json!({})).await;
        assert!(!load_out.is_error, "default load failed: {}", load_out.text);
        assert!(load_out.text.contains("GOAL: Schema alignment test"));
        assert!(
            load_out.text.contains("STATUS: ready_to_verify"),
            "Missing status in:\n{}",
            load_out.text
        );
        assert!(
            load_out.text.contains("src/main.ts") && load_out.text.contains("notes.txt"),
            "Missing touched_files in:\n{}",
            load_out.text
        );
        assert!(
            load_out.text.contains("NOTES: Handoff note via schema field"),
            "Missing notes in:\n{}",
            load_out.text
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_session_checkpoint_save_generates_antigravity_handoff_prompt() {
        let base = std::env::temp_dir().join("smart_mcp_checkpoint_handoff_test");
        let _ = fs::remove_dir_all(&base);
        let ws_dir = base.join("repo");
        let data_dir = base.join("data");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        let state = AppState::new_persistent(
            ws_dir.clone(),
            data_dir.join("auth.json"),
            data_dir.join("config.json"),
        );

        // Write a non-blueprint code file first, then a blueprint doc so auto-detection picks the blueprint
        let code_out = execute_tool(
            &state,
            "WebMCP",
            "write_file_jailed",
            json!({
                "path": "src/helper.rs",
                "content": "pub fn help() {}\n"
            }),
        )
        .await;
        assert!(!code_out.is_error, "code write failed: {}", code_out.text);

        let bp_out = execute_tool(
            &state,
            "WebMCP",
            "write_file_jailed",
            json!({
                "path": ".gemini/Blueprints/08-handoff.md",
                "content": "# Blueprint 08\nImplement automatic handoff prompt.\n"
            }),
        )
        .await;
        assert!(!bp_out.is_error, "blueprint write failed: {}", bp_out.text);

        let save_out = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({
                "op": "save",
                "goal": "Add automatic Antigravity handoff prompt",
                "decisions": ["Reuse session_checkpoint"],
                "next_steps": ["Wire harness nudge", "Run cargo test"]
            }),
        )
        .await;
        assert!(!save_out.is_error, "save failed: {}", save_out.text);
        assert!(
            save_out.text.contains("ANTIGRAVITY_HANDOFF_PROMPT:"),
            "Missing handoff prompt block:\n{}",
            save_out.text
        );
        assert!(
            save_out.text.contains(".gemini/Blueprints/08-handoff.md"),
            "Missing auto-detected blueprint path:\n{}",
            save_out.text
        );
        assert!(
            save_out.text.contains("Add automatic Antigravity handoff prompt"),
            "Missing goal in handoff prompt:\n{}",
            save_out.text
        );
        assert!(
            save_out.text.contains("airlock://workspace/checkpoint"),
            "Missing checkpoint resource ref in handoff prompt:\n{}",
            save_out.text
        );

        let load_out = execute_tool(&state, "WebMCP", "session_checkpoint", json!({})).await;
        assert!(!load_out.is_error, "load failed: {}", load_out.text);
        assert!(
            load_out
                .text
                .contains("BLUEPRINT_PATH: .gemini/Blueprints/08-handoff.md"),
            "Missing BLUEPRINT_PATH in loaded checkpoint:\n{}",
            load_out.text
        );

        // Explicit blueprint_path overrides auto-detected audit path
        let explicit_save = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({
                "op": "save",
                "goal": "Override blueprint path",
                "blueprint_path": "docs/custom-spec.md",
                "next_steps": ["Implement custom spec"]
            }),
        )
        .await;
        assert!(!explicit_save.is_error);
        assert!(
            explicit_save.text.contains("docs/custom-spec.md"),
            "Missing explicit blueprint_path override:\n{}",
            explicit_save.text
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_session_checkpoint_stage_aware_dual_prompts() {
        let base = std::env::temp_dir().join("smart_mcp_checkpoint_stage_dual_test");
        let _ = fs::remove_dir_all(&base);
        let ws_dir = base.join("repo");
        let data_dir = base.join("data");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        let state = AppState::new_persistent(
            ws_dir.clone(),
            data_dir.join("auth.json"),
            data_dir.join("config.json"),
        );

        // 1. Stage = inspect -> guides into multi-turn blueprint drafting & field audit
        let inspect_out = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({
                "op": "save",
                "stage": "inspect",
                "recommended_skills": ["lean-audit", "design-system-gen"],
                "goal": "Inspect auth & session pipeline",
                "next_steps": ["Draft 01-auth-flow-blueprint.md"]
            }),
        )
        .await;
        assert!(!inspect_out.is_error, "inspect save failed: {}", inspect_out.text);
        assert!(inspect_out.text.contains("stage=inspect"));
        assert!(inspect_out.text.contains("WEBMCP_NEXT_SESSION_PROMPT:"));
        assert!(inspect_out.text.contains("ANTIGRAVITY_HANDOFF_PROMPT:"));
        assert!(inspect_out.text.contains("BLUEPRINT MODE (MULTI-TURN)"));
        assert!(inspect_out.text.contains("AT MOST 1 complete blueprint"));
        assert!(inspect_out.text.contains("PRE-FLIGHT & FIELD AUDIT"));
        assert!(inspect_out.text.contains("`lean-audit`"));

        // 2. Stage omitted + multiple blueprint_paths -> auto-infers "blueprint" and syncs blueprint_path
        let bp_out = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({
                "op": "save",
                "blueprint_paths": [".gemini/blueprints/01-auth.md", ".gemini/blueprints/02-ui.md"],
                "recommended_skills": ["test-driven-development"],
                "goal": "Implement JWT refresh rotation",
                "decisions": ["Use 15m TTL"],
                "next_steps": ["Implement Phase 1 token store"]
            }),
        )
        .await;
        assert!(!bp_out.is_error, "blueprint save failed: {}", bp_out.text);
        assert!(bp_out.text.contains("stage=blueprint"));
        assert!(bp_out.text.contains("MULTI-TURN (1 BLUEPRINT)"));
        assert!(bp_out.text.contains("PRE-FLIGHT CHECKUP"));
        assert!(bp_out.text.contains("2-4 concise choices"));
        assert!(bp_out.text.contains(".gemini/blueprints/01-auth.md"));
        assert!(bp_out.text.contains(".gemini/blueprints/02-ui.md"));

        let load_out = execute_tool(&state, "WebMCP", "session_checkpoint", json!({})).await;
        assert!(!load_out.is_error);
        assert!(load_out.text.contains("STAGE: blueprint"));
        assert!(load_out.text.contains("BLUEPRINT_PATH: .gemini/blueprints/01-auth.md"));
        assert!(load_out.text.contains("BLUEPRINT_PATHS:"));
        assert!(load_out.text.contains("RECOMMENDED_SKILLS: test-driven-development"));

        // 3. Stage = implement -> includes progress/diff context for Antigravity takeover
        let impl_out = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({
                "op": "save",
                "stage": "implement",
                "blueprint_path": ".gemini/blueprints/01-auth.md",
                "goal": "Finish Phase 2 middleware",
                "next_steps": ["Add token middleware", "Run cargo test"]
            }),
        )
        .await;
        assert!(!impl_out.is_error);
        assert!(impl_out.text.contains("stage=implement"));
        assert!(impl_out.text.contains(".airlock/handoff/latest.diff"));
        assert!(impl_out.text.contains(".airlock/handoff/progress.md"));

        // 4. Legacy JSON without new optional fields deserializes cleanly via #[serde(default)]
        let legacy_json = json!({
            "workspace_id": "ws1",
            "workspace_alias": "repo",
            "updated_at": "2026-01-01 00:00:00",
            "goal": "Legacy goal",
            "blueprint_path": ".gemini/blueprints/legacy.md"
        });
        let legacy_cp: SessionCheckpoint = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(effective_checkpoint_stage(&legacy_cp), "blueprint");
        assert!(build_webmcp_next_session_prompt(&legacy_cp).contains("legacy.md"));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_task_mode_next_session_prompt_omits_single_phase_cap() {
        let cp = SessionCheckpoint {
            workspace_id: "ws1".to_string(),
            workspace_alias: "repo".to_string(),
            updated_at: "2026-11-28 00:00:00".to_string(),
            goal: "Ship dual mode".to_string(),
            status: None,
            decisions: vec!["Keep Chat mode intact".to_string()],
            completed: vec![],
            next_steps: vec!["Verify tests".to_string()],
            open_issues: vec![],
            model_notes: String::new(),
            git_head_short: None,
            git_status_short: vec![],
            recent_touched_files: vec![],
            running_job_ids: vec![],
            last_audit_group_id: None,
            blueprint_path: Some(".gemini/blueprints/01-dual.md".to_string()),
            stage: Some("blueprint".to_string()),
            blueprint_paths: vec![".gemini/blueprints/01-dual.md".to_string()],
            recommended_skills: vec!["test-driven-development".to_string()],
        };

        let chat_prompt = build_webmcp_next_session_prompt_for_mode(&cp, WebMcpExecutionMode::Chat);
        assert!(chat_prompt.contains("airlock-blueprint-workflow"));
        assert!(chat_prompt.contains("ONLY ONE phase"));
        assert!(chat_prompt.contains("2-4 concise choices"));

        let task_prompt = build_webmcp_next_session_prompt_for_mode(&cp, WebMcpExecutionMode::Task);
        assert!(task_prompt.contains("airlock-task-workflow"));
        assert!(!task_prompt.contains("ONLY ONE phase"));
        assert!(!task_prompt.contains("2-4 concise choices"));
    }

    #[test]
    fn test_fallback_antigravity_handoff_prompt_references_workspace_and_handoff_md() {
        let prompt = build_fallback_antigravity_handoff_prompt("my-app");
        assert!(prompt.contains("@my-app"));
        assert!(prompt.contains(".airlock/HANDOFF.md"));
        assert!(prompt.contains("list_skills"));
    }
}
