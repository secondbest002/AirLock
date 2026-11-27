use crate::agent_manager::DelegatedRunResult;
use crate::artifacts::ArtifactDocument;
use crate::checkpoints::{load_checkpoint_struct, SessionCheckpoint};
use crate::jobs::{JobStatus, ManagedJob};
use crate::security::{compress_output, resolve_jailed_path};
use crate::state::{AppState, AuditEntry};
use crate::tools::check_sensitive_guard_for_workspace;
use crate::workspace::{paths_equal, ResolvedWorkspace};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};

pub(crate) const HANDOFF_MD_REL: &str = ".airlock/HANDOFF.md";
pub(crate) const HANDOFF_DIR_REL: &str = ".airlock/handoff";
const MAX_HANDOFF_MD_CHARS: usize = 6144;
const MAX_LATEST_DIFF_BYTES: usize = 32 * 1024;
const MAX_UNTRACKED_FILE_BYTES: u64 = 16 * 1024;
const MAX_AUDIT_LINES: usize = 50;

#[derive(Debug, Clone)]
pub(crate) enum HandoffTrigger {
    ToolMutation(String),
    CommandFinished(String),
    JobFinished(String),
    DelegatedFinished(String),
    CheckpointSaved,
    CheckpointCleared,
    ArtifactUpdated(String),
    AuditReverted,
    ManualSync,
}

impl HandoffTrigger {
    fn label(&self) -> String {
        match self {
            Self::ToolMutation(tool) => format!("WebMCP tool ({tool})"),
            Self::CommandFinished(tool) => format!("WebMCP command ({tool})"),
            Self::JobFinished(jid) => format!("Job finished ({jid})"),
            Self::DelegatedFinished(rid) => format!("Delegated run ({rid})"),
            Self::CheckpointSaved => "Checkpoint saved".to_string(),
            Self::CheckpointCleared => "Checkpoint cleared".to_string(),
            Self::ArtifactUpdated(sid) => format!("Artifact updated ({sid})"),
            Self::AuditReverted => "Audit reverted".to_string(),
            Self::ManualSync => "Manual sync".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandoffAuditRecord {
    id: String,
    timestamp: String,
    source: String,
    tool_name: String,
    target_path: Option<String>,
    summary: String,
    can_revert: bool,
    reverted: bool,
    group_id: Option<String>,
    post_edit_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedJobCommandSnap {
    job_id: String,
    command: String,
    status: JobStatus,
    exit_code: Option<i32>,
    elapsed_secs: u64,
    started_at: String,
    tail_compressed: String,
}

#[derive(Debug, Clone)]
struct CachedHandoffContext {
    secrets_to_redact: Vec<String>,
    audit_records: Vec<HandoffAuditRecord>,
    touched_files_status: Vec<(String, bool)>,
    last_group_id: Option<String>,
    artifacts: Vec<ArtifactDocument>,
    checkpoint: Option<SessionCheckpoint>,
    delegated: Option<DelegatedRunResult>,
}

#[derive(Default)]
pub(crate) struct HandoffCoordinator {
    ws_locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
    exclude_ensured: RwLock<HashSet<String>>,
    latest_delegated: RwLock<HashMap<String, DelegatedRunResult>>,
    cached_ctx: RwLock<HashMap<String, CachedHandoffContext>>,
}

#[derive(Clone, Default)]
pub struct WorkspaceHandoffManager {
    pub(crate) inner: Arc<HandoffCoordinator>,
}

pub(crate) fn is_webmcp_source(source: &str) -> bool {
    matches!(source.trim(), "WebMCP" | "WebMCP:delegate")
}

pub(crate) fn is_handoff_ignored_rel(rel: &str) -> bool {
    let norm = rel
        .trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_ascii_lowercase();
    norm == ".airlock/handoff.md"
        || norm == ".airlock/handoff"
        || norm.starts_with(".airlock/handoff/")
}

pub(crate) fn workspace_has_handoff(ws: &ResolvedWorkspace) -> bool {
    ws.canon_root.join(".airlock").join("HANDOFF.md").is_file()
}

fn clip_chars(raw: &str, max_chars: usize) -> String {
    let trimmed = raw.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else if max_chars > 3 {
        let head: String = trimmed.chars().take(max_chars - 3).collect();
        format!("{head}...")
    } else {
        trimmed.chars().take(max_chars).collect()
    }
}

fn write_text_atomic(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Gagal membuat direktori '{}': {e}", parent.display()))?;
    }
    let tmp_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => format!("{name}.tmp"),
        None => "handoff.tmp".to_string(),
    };
    let tmp_path = path.with_file_name(tmp_name);
    fs::write(&tmp_path, content)
        .map_err(|e| format!("Gagal menulis file sementara '{}': {e}", tmp_path.display()))?;
    if fs::rename(&tmp_path, path).is_err() {
        fs::write(path, content)
            .map_err(|e| format!("Gagal menyimpan '{}': {e}", path.display()))?;
        let _ = fs::remove_file(&tmp_path);
    }
    Ok(())
}

async fn run_git_bounded(root: &Path, args: &[&str], timeout_secs: u64) -> Option<(i32, String)> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    let out = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
        .await
        .ok()?
        .ok()?;
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    Some((code, stdout))
}

async fn collect_secrets_to_redact(state: &AppState) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    let tok = state.auth_token.read().await.trim().to_string();
    if tok.len() >= 6 {
        raw.push(tok);
    }
    {
        let secs = state.provider_secrets.read().await;
        for s in [
            &secs.gemini_api_key,
            &secs.openai_api_key,
            &secs.anthropic_api_key,
            &secs.auth_token,
            &secs.cloudflare_tunnel_token,
        ] {
            let t = s.trim();
            if t.len() >= 6 {
                raw.push(t.to_string());
            }
        }
        for v in secs.downstream_bearer_tokens.values() {
            let t = v.trim();
            if t.len() >= 6 {
                raw.push(t.to_string());
            }
        }
        for map in secs.downstream_envs.values() {
            for v in map.values() {
                let t = v.trim();
                if t.len() >= 6 {
                    raw.push(t.to_string());
                }
            }
        }
        for v in secs.oauth_client_secrets.values() {
            let t = v.trim();
            if t.len() >= 6 {
                raw.push(t.to_string());
            }
        }
        for map in secs.plugin_secrets.values() {
            for v in map.values() {
                let t = v.trim();
                if t.len() >= 6 {
                    raw.push(t.to_string());
                }
            }
        }
    }
    raw.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    raw.dedup();
    raw
}

fn redact_secrets(secrets: &[String], input: &str) -> String {
    if secrets.is_empty() || input.is_empty() {
        return input.to_string();
    }
    let mut out = input.to_string();
    for s in secrets {
        if out.contains(s.as_str()) {
            out = out.replace(s.as_str(), "<redacted>");
        }
    }
    out
}

async fn ensure_git_info_exclude(
    coord: &HandoffCoordinator,
    ws_canon_root: &Path,
) -> Result<(), String> {
    let cache_key = crate::workspace::clean_path_string(ws_canon_root).to_ascii_lowercase();
    if coord.exclude_ensured.read().await.contains(&cache_key) {
        return Ok(());
    }

    let dot_git = ws_canon_root.join(".git");
    let exclude_path = if dot_git.is_dir() {
        dot_git.join("info").join("exclude")
    } else if dot_git.is_file() {
        let Some((0, rel_or_abs)) =
            run_git_bounded(ws_canon_root, &["rev-parse", "--git-path", "info/exclude"], 2).await
        else {
            return Ok(());
        };
        let trimmed = rel_or_abs.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let p = PathBuf::from(trimmed);
        if p.is_absolute() {
            p
        } else {
            ws_canon_root.join(p)
        }
    } else {
        return Ok(());
    };

    let existing = fs::read_to_string(&exclude_path).unwrap_or_default();
    let has_md = existing
        .lines()
        .any(|l| l.trim() == "/.airlock/HANDOFF.md");
    let has_dir = existing.lines().any(|l| l.trim() == "/.airlock/handoff/");
    if has_md && has_dir {
        coord.exclude_ensured.write().await.insert(cache_key);
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("# >>> Airlock local handoff (auto-managed)\n");
    if !has_md {
        updated.push_str("/.airlock/HANDOFF.md\n");
    }
    if !has_dir {
        updated.push_str("/.airlock/handoff/\n");
    }
    updated.push_str("# <<< Airlock local handoff\n");

    write_text_atomic(&exclude_path, &updated)?;
    coord.exclude_ensured.write().await.insert(cache_key);
    Ok(())
}

fn entry_belongs_to_workspace(entry: &AuditEntry, ws: &ResolvedWorkspace) -> bool {
    if let Some(ref wid) = entry.workspace_id {
        return wid == &ws.id;
    }
    if let Some(ref wroot) = entry.workspace_root {
        return paths_equal(wroot, &ws.clean_path);
    }
    if let Some(ref walias) = entry.workspace_alias {
        return walias.eq_ignore_ascii_case(&ws.alias);
    }
    false
}

fn is_actionable_handoff_audit(entry: &AuditEntry, ws: &ResolvedWorkspace) -> bool {
    if !is_webmcp_source(&entry.source) || !entry_belongs_to_workspace(entry, ws) {
        return false;
    }
    match entry.tool_name.as_str() {
        "batch_inspect"
        | "read_file_range"
        | "smart_search"
        | "project_overview"
        | "find_files"
        | "code_outline"
        | "read_symbol"
        | "lsp_diagnostics"
        | "lsp_definition"
        | "lsp_references"
        | "lsp_hover"
        | "lsp_query"
        | "list_workspaces"
        | "list_skills"
        | "load_skill"
        | "read_skill_resource"
        | "poll_command_job" => false,
        "session_checkpoint" => !entry.summary.starts_with("Loaded "),
        "git_ops" => {
            entry.summary.starts_with("git add") || entry.summary.starts_with("git commit")
        }
        _ => true,
    }
}

fn extract_status_path(porcelain_line: &str) -> Option<&str> {
    if porcelain_line.len() < 4 {
        return None;
    }
    let raw = porcelain_line[3..].trim();
    let candidate = raw.rsplit(" -> ").next().unwrap_or(raw).trim();
    Some(candidate.trim_matches('"'))
}

async fn collect_git_snapshot(root: &Path) -> (bool, Option<String>, Option<String>, Vec<String>, bool) {
    let dot_git = root.join(".git");
    if !dot_git.exists() {
        return (false, AppState::read_git_branch(root), None, Vec::new(), false);
    }
    let branch = AppState::read_git_branch(root);
    let has_head_res = run_git_bounded(root, &["rev-parse", "--short", "HEAD"], 2).await;
    let (has_head, head_short) = match has_head_res {
        Some((0, txt)) => {
            let t = txt.trim().to_string();
            if t.is_empty() {
                (false, None)
            } else {
                (true, Some(t))
            }
        }
        _ => (false, None),
    };

    let status_lines = match run_git_bounded(root, &["status", "--porcelain", "--untracked-files=all"], 2).await {
        Some((0, txt)) => txt
            .lines()
            .map(|l| l.trim_end().to_string())
            .filter(|l| {
                if l.trim().is_empty() {
                    return false;
                }
                if let Some(p) = extract_status_path(l) {
                    !is_handoff_ignored_rel(p)
                } else {
                    true
                }
            })
            .collect(),
        _ => Vec::new(),
    };

    (true, branch, head_short, status_lines, has_head)
}

fn truncate_diff_budget(diff: &str) -> String {
    if diff.len() <= MAX_LATEST_DIFF_BYTES {
        return diff.to_string();
    }
    let marker = "\n... [truncated at 32 KB — run git diff or inspect files directly] ...\n";
    let budget = MAX_LATEST_DIFF_BYTES.saturating_sub(marker.len());
    let mut end = budget.min(diff.len());
    while end > 0 && !diff.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = diff[..end].to_string();
    out.push_str(marker);
    out
}

fn synthetic_new_file_diff(rel_path: &str, content: &str) -> String {
    let norm = rel_path.replace('\\', "/");
    let normalized_lf = content.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized_lf.lines().collect();
    let count = lines.len().max(1);
    let mut out = format!(
        "diff --git a/{norm} b/{norm}\nnew file mode 100644\n--- /dev/null\n+++ b/{norm}\n@@ -0,0 +1,{count} @@\n"
    );
    for line in &lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

async fn collect_latest_diff(
    state_opt: Option<&AppState>,
    ws: &ResolvedWorkspace,
    audit_entries: &[AuditEntry],
    status_lines: &[String],
    is_git: bool,
    has_head: bool,
    secrets: &[String],
) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let mut total_bytes = 0usize;

    if is_git {
        if has_head {
            if let Some((0, txt)) = run_git_bounded(
                &ws.canon_root,
                &[
                    "diff",
                    "HEAD",
                    "--no-color",
                    "--",
                    ".",
                    ":(exclude).airlock/HANDOFF.md",
                    ":(exclude).airlock/handoff",
                ],
                2,
            )
            .await
            {
                let trimmed = txt.trim_end();
                if !trimmed.is_empty() {
                    total_bytes += trimmed.len();
                    chunks.push(trimmed.to_string());
                }
            }
        } else {
            for args in [
                vec![
                    "diff",
                    "--cached",
                    "--no-color",
                    "--",
                    ".",
                    ":(exclude).airlock/HANDOFF.md",
                    ":(exclude).airlock/handoff",
                ],
                vec![
                    "diff",
                    "--no-color",
                    "--",
                    ".",
                    ":(exclude).airlock/HANDOFF.md",
                    ":(exclude).airlock/handoff",
                ],
            ] {
                if let Some((0, txt)) = run_git_bounded(&ws.canon_root, &args, 2).await {
                    let trimmed = txt.trim_end();
                    if !trimmed.is_empty() {
                        total_bytes += trimmed.len();
                        chunks.push(trimmed.to_string());
                    }
                }
            }
        }

        // Render full synthetic diffs for untracked new files touched by WebMCP
        let active_webmcp_paths: HashSet<String> = audit_entries
            .iter()
            .filter(|e| {
                is_actionable_handoff_audit(e, ws) && e.can_revert && !e.reverted
            })
            .filter_map(|e| e.target_path.as_ref())
            .map(|p| p.trim().replace('\\', "/"))
            .collect();

        for line in status_lines {
            if total_bytes >= MAX_LATEST_DIFF_BYTES {
                break;
            }
            if !line.starts_with("?? ") {
                continue;
            }
            let Some(rel) = extract_status_path(line) else {
                continue;
            };
            let rel_norm = rel.replace('\\', "/");
            if is_handoff_ignored_rel(&rel_norm) {
                continue;
            }
            if !active_webmcp_paths.contains(&rel_norm) {
                continue;
            }
            let Ok(target) = resolve_jailed_path(&ws.canon_root, &rel_norm) else {
                continue;
            };
            if let Some(st) = state_opt {
                if check_sensitive_guard_for_workspace(st, ws, &target, &rel_norm)
                    .await
                    .is_err()
                {
                    continue;
                }
            }
            let Ok(meta) = fs::metadata(&target) else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_UNTRACKED_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = fs::read(&target) else {
                continue;
            };
            if bytes.iter().take(512).any(|&b| b == 0) {
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            let syn = synthetic_new_file_diff(&rel_norm, &text);
            total_bytes += syn.len();
            chunks.push(syn.trim_end().to_string());
        }
    } else {
        // Non-git workspace fallback: aggregate latest non-reverted WebMCP AuditEntry diff_text
        let mut seen_paths: HashSet<String> = HashSet::new();
        for entry in audit_entries.iter().rev() {
            if total_bytes >= MAX_LATEST_DIFF_BYTES {
                break;
            }
            if !is_actionable_handoff_audit(entry, ws) || entry.reverted {
                continue;
            }
            let Some(ref tp) = entry.target_path else {
                continue;
            };
            if !seen_paths.insert(tp.clone()) {
                continue;
            }
            if let Some(st) = state_opt {
                if let Ok(target) = resolve_jailed_path(&ws.canon_root, tp) {
                    if check_sensitive_guard_for_workspace(st, ws, &target, tp)
                        .await
                        .is_err()
                    {
                        continue;
                    }
                }
            }
            if let Some(ref diff) = entry.diff_text {
                if !diff.trim().is_empty() {
                    total_bytes += diff.len();
                    chunks.push(diff.trim_end().to_string());
                }
            }
        }
    }

    let joined = chunks.join("\n\n");
    let redacted = redact_secrets(secrets, &joined);
    truncate_diff_budget(&redacted)
}

fn job_status_label(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Killed => "killed",
        JobStatus::TimedOut => "timed_out",
    }
}

async fn collect_webmcp_job_snapshots(
    jobs_map: &Arc<RwLock<HashMap<String, ManagedJob>>>,
    ws_alias: &str,
    secrets: &[String],
    max_jobs: usize,
    tail_lines: usize,
) -> Vec<CachedJobCommandSnap> {
    let guard = jobs_map.read().await;
    let mut matching: Vec<&ManagedJob> = guard
        .values()
        .filter(|j| {
            j.source.as_deref().is_some_and(is_webmcp_source)
                && j.workspace_alias
                    .as_deref()
                    .map(|a| a.eq_ignore_ascii_case(ws_alias))
                    .unwrap_or(false)
        })
        .collect();
    matching.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| b.job_id.cmp(&a.job_id))
    });

    matching
        .into_iter()
        .take(max_jobs)
        .map(|j| {
            let raw = j.lines.join("\n");
            let (compressed, _) = compress_output(&raw, "", tail_lines.max(12));
            let redacted = redact_secrets(secrets, &compressed);
            let elapsed = j
                .finished_elapsed_secs
                .unwrap_or_else(|| j.started_instant.elapsed().as_secs());
            CachedJobCommandSnap {
                job_id: j.job_id.clone(),
                command: redact_secrets(secrets, &j.command),
                status: j.status,
                exit_code: j.exit_code,
                elapsed_secs: elapsed,
                started_at: j.started_at.clone(),
                tail_compressed: redacted,
            }
        })
        .collect()
}

fn build_touched_files_status(
    checkpoint: Option<&SessionCheckpoint>,
    audit_records: &[HandoffAuditRecord],
) -> (Vec<(String, bool)>, Option<String>) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut list: Vec<(String, bool)> = Vec::new();
    let mut last_group: Option<String> = checkpoint.and_then(|c| c.last_audit_group_id.clone());

    for rec in audit_records.iter().rev() {
        if last_group.is_none() {
            if let Some(ref gid) = rec.group_id {
                last_group = Some(gid.clone());
            }
        }
        if rec.can_revert {
            if let Some(ref p) = rec.target_path {
                let norm = p.replace('\\', "/");
                if seen.insert(norm.clone()) {
                    list.push((norm, rec.reverted));
                    if list.len() >= 15 {
                        break;
                    }
                }
            }
        }
    }

    if let Some(cp) = checkpoint {
        for p in &cp.recent_touched_files {
            if list.len() >= 15 {
                break;
            }
            let norm = p.replace('\\', "/");
            if seen.insert(norm.clone()) {
                list.push((norm, false));
            }
        }
    }

    (list, last_group)
}

#[allow(clippy::too_many_arguments)]
fn render_handoff_md(
    ws: &ResolvedWorkspace,
    updated_at: &str,
    trigger_label: &str,
    branch: Option<&str>,
    head_short: Option<&str>,
    status_lines: &[String],
    checkpoint: Option<&SessionCheckpoint>,
    cleared_notice: Option<&str>,
    touched_files: &[(String, bool)],
    last_group_id: Option<&str>,
    jobs: &[CachedJobCommandSnap],
    audit_records: &[HandoffAuditRecord],
    artifacts: &[ArtifactDocument],
    delegated: Option<&DelegatedRunResult>,
    secrets: &[String],
) -> String {
    let mut sections: Vec<String> = Vec::new();

    let branch_disp = branch.unwrap_or("no-git");
    let head_disp = head_short.unwrap_or("none");
    let gid_disp = last_group_id.unwrap_or("none");
    sections.push(format!(
        "# Airlock WebMCP Handoff — @{}\n\n\
         - **Workspace:** `{}`\n\
         - **Git:** `{}` (HEAD `{}`)\n\
         - **Updated:** `{}` · **Trigger:** `{}`\n\
         - **Last Audit Group:** `{}`",
        ws.alias, ws.clean_path, branch_disp, head_disp, updated_at, trigger_label, gid_disp
    ));

    // Goal & Status
    if let Some(cp) = checkpoint {
        let mut goal_sec = format!(
            "## Goal & Status\n\n- **Status:** `{}`\n- **Goal:** {}",
            cp.status.as_deref().unwrap_or("in_progress"),
            clip_chars(&redact_secrets(secrets, &cp.goal), 320)
        );
        if let Some(ref bp) = cp.blueprint_path {
            goal_sec.push_str(&format!("\n- **Blueprint:** `{}`", bp));
        }
        if !cp.decisions.is_empty() {
            goal_sec.push_str("\n\n**Decisions:**\n");
            for d in cp.decisions.iter().take(5) {
                goal_sec.push_str(&format!("- {}\n", clip_chars(&redact_secrets(secrets, d), 140)));
            }
        }
        if !cp.completed.is_empty() {
            goal_sec.push_str("\n**Completed:**\n");
            for c in cp.completed.iter().take(5) {
                goal_sec.push_str(&format!("- {}\n", clip_chars(&redact_secrets(secrets, c), 140)));
            }
        }
        if !cp.next_steps.is_empty() {
            goal_sec.push_str("\n**Next Steps:**\n");
            for (i, n) in cp.next_steps.iter().take(5).enumerate() {
                goal_sec.push_str(&format!(
                    "{}. {}\n",
                    i + 1,
                    clip_chars(&redact_secrets(secrets, n), 140)
                ));
            }
        }
        if !cp.open_issues.is_empty() {
            goal_sec.push_str("\n**Open Issues / Risks:**\n");
            for o in cp.open_issues.iter().take(4) {
                goal_sec.push_str(&format!("- {}\n", clip_chars(&redact_secrets(secrets, o), 140)));
            }
        }
        if !cp.model_notes.trim().is_empty() {
            goal_sec.push_str(&format!(
                "\n**Notes:** {}\n",
                clip_chars(&redact_secrets(secrets, &cp.model_notes), 320)
            ));
        }
        sections.push(goal_sec.trim_end().to_string());
    } else {
        let mut syn = String::from("## Goal & Status\n\n");
        if let Some(msg) = cleared_notice {
            syn.push_str(&format!("- **Status:** `{msg}`\n"));
        } else {
            syn.push_str("- **Status:** `Active WebMCP session (no explicit session_checkpoint saved yet)`\n");
        }
        if let Some(last) = audit_records.last() {
            syn.push_str(&format!(
                "- **Last WebMCP Action:** `{}` — {}\n",
                last.tool_name,
                clip_chars(&redact_secrets(secrets, &last.summary), 180)
            ));
        }
        sections.push(syn.trim_end().to_string());
    }

    // Recent Touched Files
    let mut files_sec = String::from("## Recent Touched Files\n\n");
    if touched_files.is_empty() {
        files_sec.push_str("- *(none recorded yet)*");
    } else {
        for (path, reverted) in touched_files.iter().take(15) {
            if *reverted {
                files_sec.push_str(&format!("- `{path}` [reverted]\n"));
            } else {
                files_sec.push_str(&format!("- `{path}`\n"));
            }
        }
    }
    sections.push(files_sec.trim_end().to_string());

    // Latest Verification & Commands (up to 3 WebMCP jobs or command audit entries)
    let mut cmd_sec = String::from("## Latest Verification & Commands\n\n");
    if !jobs.is_empty() {
        for j in jobs.iter().take(3) {
            let exit_disp = j
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string());
            cmd_sec.push_str(&format!(
                "- `{}` (`{}` · exit `{}` · {}s)\n",
                clip_chars(&j.command, 100),
                job_status_label(j.status),
                exit_disp,
                j.elapsed_secs
            ));
            if !j.tail_compressed.trim().is_empty() {
                let tail: Vec<&str> = j.tail_compressed.lines().rev().take(12).collect();
                let ordered: Vec<&str> = tail.into_iter().rev().collect();
                cmd_sec.push_str("  ```text\n");
                for l in ordered {
                    cmd_sec.push_str("  ");
                    cmd_sec.push_str(&clip_chars(l, 160));
                    cmd_sec.push('\n');
                }
                cmd_sec.push_str("  ```\n");
            }
        }
    } else {
        let sync_cmds: Vec<&HandoffAuditRecord> = audit_records
            .iter()
            .rev()
            .filter(|r| {
                matches!(
                    r.tool_name.as_str(),
                    "run_command" | "run_command_compressed" | "patch_and_verify" | "multi_surgical_edit"
                )
            })
            .take(3)
            .collect();
        if sync_cmds.is_empty() {
            cmd_sec.push_str("- *(no commands recorded yet)*");
        } else {
            for r in sync_cmds {
                cmd_sec.push_str(&format!(
                    "- `{}` ({}) — {}\n",
                    r.tool_name,
                    r.timestamp,
                    clip_chars(&redact_secrets(secrets, &r.summary), 160)
                ));
            }
        }
    }
    if let Some(del) = delegated {
        cmd_sec.push_str(&format!(
            "\n- **Delegated Run `{}`** ({:?}, {}s): {}\n",
            del.run_id,
            del.status,
            del.elapsed_secs,
            clip_chars(&redact_secrets(secrets, &del.summary_text), 220)
        ));
    }
    sections.push(cmd_sec.trim_end().to_string());

    // Live Git Working Tree
    let mut git_sec = String::from("## Live Git Working Tree\n\n");
    if status_lines.is_empty() {
        git_sec.push_str("(clean working tree or non-git)");
    } else {
        git_sec.push_str("```text\n");
        for line in status_lines.iter().take(20) {
            git_sec.push_str(line);
            git_sec.push('\n');
        }
        if status_lines.len() > 20 {
            git_sec.push_str(&format!("... (+{} more)\n", status_lines.len() - 20));
        }
        git_sec.push_str("```");
    }
    sections.push(git_sec);

    // Supporting Files & Artifacts
    let mut ptrs = String::from(
        "## Supporting Files & MCP Resources\n\n\
         - Progress & command logs: `.airlock/handoff/progress.md`\n\
         - Latest diff (staged + unstaged + new files): `.airlock/handoff/latest.diff`\n\
         - WebMCP audit trail: `.airlock/handoff/session-audit.jsonl`\n\
         - MCP resources: `airlock://workspace/handoff` · `airlock://workspace/checkpoint`",
    );
    if checkpoint.is_some() {
        ptrs.push_str("\n- Checkpoint JSON: `.airlock/handoff/checkpoint.json`");
        ptrs.push_str("\n- Next-session prompts (WebMCP / Antigravity): `.airlock/handoff/prompts.md`");
    }
    if !artifacts.is_empty() {
        let names: Vec<String> = artifacts
            .iter()
            .map(|a| format!("`.airlock/handoff/artifacts/{}`", a.kind.filename()))
            .collect();
        ptrs.push_str(&format!("\n- Artifacts: {}", names.join(", ")));
    }
    sections.push(ptrs);

    let full = sections.join("\n\n");
    if full.chars().count() <= MAX_HANDOFF_MD_CHARS {
        full
    } else {
        let suffix = "\n\n... [HANDOFF.md capped at 6 KB — see .airlock/handoff/progress.md & latest.diff]";
        let budget = MAX_HANDOFF_MD_CHARS.saturating_sub(suffix.chars().count());
        let head: String = full.chars().take(budget).collect();
        format!("{head}{suffix}")
    }
}

fn render_progress_md(
    ws: &ResolvedWorkspace,
    updated_at: &str,
    checkpoint: Option<&SessionCheckpoint>,
    cleared_notice: Option<&str>,
    artifacts: &[ArtifactDocument],
    jobs: &[CachedJobCommandSnap],
    delegated: Option<&DelegatedRunResult>,
    secrets: &[String],
) -> String {
    let mut out = vec![format!(
        "# Airlock WebMCP Progress — @{}\n\nUpdated: `{updated_at}`",
        ws.alias
    )];

    if let Some(cp) = checkpoint {
        let mut sec = format!("## Checkpoint (`{}`)\n\nGoal: {}", cp.updated_at, redact_secrets(secrets, &cp.goal));
        if !cp.completed.is_empty() {
            sec.push_str("\n\n### Completed\n");
            for c in &cp.completed {
                sec.push_str(&format!("- [x] {}\n", redact_secrets(secrets, c)));
            }
        }
        if !cp.next_steps.is_empty() {
            sec.push_str("\n### Next Steps\n");
            for n in &cp.next_steps {
                sec.push_str(&format!("- [ ] {}\n", redact_secrets(secrets, n)));
            }
        }
        if !cp.open_issues.is_empty() {
            sec.push_str("\n### Open Issues\n");
            for o in &cp.open_issues {
                sec.push_str(&format!("- {}\n", redact_secrets(secrets, o)));
            }
        }
        out.push(sec.trim_end().to_string());
    } else if let Some(msg) = cleared_notice {
        out.push(format!("## Checkpoint\n\n{msg}"));
    }

    if let Some(task_doc) = artifacts
        .iter()
        .find(|a| a.kind == crate::artifacts::ArtifactKind::TaskList)
    {
        out.push(format!(
            "## Task List Artifact (`{}` · v{})\n\n{}",
            task_doc.title,
            task_doc.version,
            redact_secrets(secrets, &task_doc.content_md)
        ));
    }

    let mut jobs_sec = String::from("## Recent Command & Job Runs\n\n");
    if jobs.is_empty() {
        jobs_sec.push_str("*(no WebMCP background/sync command jobs recorded)*");
    } else {
        for j in jobs.iter().take(5) {
            let exit_disp = j
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string());
            jobs_sec.push_str(&format!(
                "### `{}` (`{}`)\n- Command: `{}`\n- Status: `{}` · Exit: `{}` · Elapsed: `{}s` · Started: `{}`\n",
                j.job_id,
                job_status_label(j.status),
                j.command,
                job_status_label(j.status),
                exit_disp,
                j.elapsed_secs,
                j.started_at
            ));
            if !j.tail_compressed.trim().is_empty() {
                let tail: Vec<&str> = j.tail_compressed.lines().rev().take(30).collect();
                let ordered: Vec<&str> = tail.into_iter().rev().collect();
                jobs_sec.push_str("```text\n");
                for l in ordered {
                    jobs_sec.push_str(l);
                    jobs_sec.push('\n');
                }
                jobs_sec.push_str("```\n");
            }
        }
    }
    out.push(jobs_sec.trim_end().to_string());

    if let Some(del) = delegated {
        let touched = if del.touched_files.is_empty() {
            "(none)".to_string()
        } else {
            del.touched_files.join(", ")
        };
        out.push(format!(
            "## Latest Delegated Local Agent Run\n\n- Run ID: `{}`\n- Mode: `{}` · Status: `{:?}` · Steps: `{}` · Elapsed: `{}s`\n- Touched Files: {}\n\n### Summary\n{}",
            del.run_id,
            del.mode,
            del.status,
            del.step_count,
            del.elapsed_secs,
            touched,
            redact_secrets(secrets, &del.summary_text)
        ));
    }

    out.join("\n\n")
}

async fn write_handoff_bundle(
    ws: &ResolvedWorkspace,
    updated_at: &str,
    trigger_label: &str,
    branch: Option<&str>,
    head_short: Option<&str>,
    status_lines: &[String],
    checkpoint: Option<&SessionCheckpoint>,
    cleared_notice: Option<&str>,
    touched_files: &[(String, bool)],
    last_group_id: Option<&str>,
    jobs: &[CachedJobCommandSnap],
    audit_records: &[HandoffAuditRecord],
    artifacts: &[ArtifactDocument],
    delegated: Option<&DelegatedRunResult>,
    latest_diff: &str,
    secrets: &[String],
) -> Result<String, String> {
    let cp_path = resolve_jailed_path(&ws.canon_root, &format!("{HANDOFF_DIR_REL}/checkpoint.json"))?;
    let prompts_path = resolve_jailed_path(&ws.canon_root, &format!("{HANDOFF_DIR_REL}/prompts.md"))?;
    if let Some(cp) = checkpoint {
        let cp_json = serde_json::to_string_pretty(cp)
            .map_err(|e| format!("Gagal serialisasi checkpoint.json: {e}"))?;
        write_text_atomic(&cp_path, &redact_secrets(secrets, &cp_json))?;

        let stage = crate::checkpoints::effective_checkpoint_stage(cp);
        let webmcp_prompt = crate::checkpoints::build_webmcp_next_session_prompt(cp);
        let antigravity_prompt = crate::checkpoints::build_antigravity_handoff_prompt(cp);
        let prompts_md = format!(
            "# Next-Session Handoff Prompts — @{}\n\n\
             Updated: `{updated_at}` | Stage: `{stage}`\n\n\
             ## Option 1: Continue in Gemini WebMCP (Multi-Turn Self-Pacing)\n\n\
             ```text\n{}\n```\n\n\
             ## Option 2: Handoff to Google Antigravity (Field Audit + Full Execution)\n\n\
             ```text\n{}\n```\n",
            ws.alias,
            webmcp_prompt,
            antigravity_prompt
        );
        write_text_atomic(&prompts_path, &redact_secrets(secrets, &prompts_md))?;
    } else {
        if cp_path.exists() {
            let _ = fs::remove_file(&cp_path);
        }
        if prompts_path.exists() {
            let _ = fs::remove_file(&prompts_path);
        }
    }

    let audit_path =
        resolve_jailed_path(&ws.canon_root, &format!("{HANDOFF_DIR_REL}/session-audit.jsonl"))?;
    let audit_lines: Vec<String> = audit_records
        .iter()
        .filter_map(|r| serde_json::to_string(r).ok())
        .collect();
    let audit_body = if audit_lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", audit_lines.join("\n"))
    };
    write_text_atomic(&audit_path, &audit_body)?;

    let diff_path = resolve_jailed_path(&ws.canon_root, &format!("{HANDOFF_DIR_REL}/latest.diff"))?;
    write_text_atomic(&diff_path, latest_diff)?;

    let artifacts_dir =
        resolve_jailed_path(&ws.canon_root, &format!("{HANDOFF_DIR_REL}/artifacts"))?;
    fs::create_dir_all(&artifacts_dir)
        .map_err(|e| format!("Gagal membuat {HANDOFF_DIR_REL}/artifacts: {e}"))?;
    let mut kept_filenames: HashSet<&str> = HashSet::new();
    for doc in artifacts {
        let fname = doc.kind.filename();
        kept_filenames.insert(fname);
        let rel = format!("{HANDOFF_DIR_REL}/artifacts/{fname}");
        let dest = resolve_jailed_path(&ws.canon_root, &rel)?;
        write_text_atomic(&dest, &redact_secrets(secrets, &doc.content_md))?;
    }
    if let Ok(entries) = fs::read_dir(&artifacts_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if !kept_filenames.contains(name) {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
        }
    }

    let progress_path =
        resolve_jailed_path(&ws.canon_root, &format!("{HANDOFF_DIR_REL}/progress.md"))?;
    let progress_md = render_progress_md(
        ws,
        updated_at,
        checkpoint,
        cleared_notice,
        artifacts,
        jobs,
        delegated,
        secrets,
    );
    write_text_atomic(&progress_path, &progress_md)?;

    let handoff_path = resolve_jailed_path(&ws.canon_root, HANDOFF_MD_REL)?;
    let handoff_md = render_handoff_md(
        ws,
        updated_at,
        trigger_label,
        branch,
        head_short,
        status_lines,
        checkpoint,
        cleared_notice,
        touched_files,
        last_group_id,
        jobs,
        audit_records,
        artifacts,
        delegated,
        secrets,
    );
    write_text_atomic(&handoff_path, &handoff_md)?;

    Ok(crate::workspace::clean_path_string(&handoff_path))
}

async fn workspace_lock(coord: &HandoffCoordinator, ws_id: &str) -> Arc<Mutex<()>> {
    {
        let guard = coord.ws_locks.read().await;
        if let Some(lk) = guard.get(ws_id) {
            return Arc::clone(lk);
        }
    }
    let mut guard = coord.ws_locks.write().await;
    Arc::clone(
        guard
            .entry(ws_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

async fn gather_audit_records_for_ws(
    state: &AppState,
    ws: &ResolvedWorkspace,
    secrets: &[String],
) -> (Vec<AuditEntry>, Vec<HandoffAuditRecord>) {
    let logs = state.audit_logs.read().await;
    let mut raw_entries: Vec<AuditEntry> = Vec::new();
    for entry in logs.iter() {
        if is_actionable_handoff_audit(entry, ws) {
            raw_entries.push(entry.clone());
        }
    }
    if raw_entries.len() > MAX_AUDIT_LINES {
        let start = raw_entries.len() - MAX_AUDIT_LINES;
        raw_entries.drain(0..start);
    }

    let mut records = Vec::with_capacity(raw_entries.len());
    for entry in &raw_entries {
        let is_sensitive_target = if let Some(ref tp) = entry.target_path {
            resolve_jailed_path(&ws.canon_root, tp)
                .ok()
                .map(|target| {
                    crate::tools::check_sensitive_guard_parts(
                        &[],
                        &ws.canon_root,
                        ws.effective_allow_sensitive,
                        &ws.effective_sensitive_patterns,
                        &target,
                        tp,
                    )
                    .is_err()
                })
                .unwrap_or(false)
        } else {
            false
        };
        let summary = if is_sensitive_target {
            "[sensitive path redacted]".to_string()
        } else {
            clip_chars(&redact_secrets(secrets, &entry.summary), 220)
        };
        records.push(HandoffAuditRecord {
            id: entry.id.clone(),
            timestamp: entry.timestamp.clone(),
            source: entry.source.clone(),
            tool_name: entry.tool_name.clone(),
            target_path: if is_sensitive_target {
                None
            } else {
                entry.target_path.clone()
            },
            summary,
            can_revert: entry.can_revert,
            reverted: entry.reverted,
            group_id: entry.group_id.clone(),
            post_edit_hash: entry.post_edit_hash.clone(),
        });
    }

    (raw_entries, records)
}

pub(crate) async fn record_delegated_result(
    state: &AppState,
    ws_id: &str,
    res: DelegatedRunResult,
) {
    state
        .handoff_manager
        .inner
        .latest_delegated
        .write()
        .await
        .insert(ws_id.to_string(), res);
}

pub(crate) async fn prime_job_handoff_context(state: &AppState, ws: &ResolvedWorkspace) {
    let secrets = collect_secrets_to_redact(state).await;
    let (_, audit_records) = gather_audit_records_for_ws(state, ws, &secrets).await;
    let checkpoint = load_checkpoint_struct(state, &ws.id);
    let artifacts = state
        .artifact_store
        .list_workspace_latest_by_kind(&ws.clean_path, &["webmcp-latest", "delegated_"])
        .await;
    let delegated = state
        .handoff_manager
        .inner
        .latest_delegated
        .read()
        .await
        .get(&ws.id)
        .cloned();
    let (touched_files_status, last_group_id) =
        build_touched_files_status(checkpoint.as_ref(), &audit_records);

    let ctx = CachedHandoffContext {
        secrets_to_redact: secrets,
        audit_records,
        touched_files_status,
        last_group_id,
        artifacts,
        checkpoint,
        delegated,
    };
    state
        .handoff_manager
        .inner
        .cached_ctx
        .write()
        .await
        .insert(ws.id.clone(), ctx);
}

pub(crate) async fn sync_from_state(
    state: &AppState,
    ws: &ResolvedWorkspace,
    trigger: HandoffTrigger,
    force_manual: bool,
) -> Result<String, String> {
    if ws.effective_guardrail == "read_only" {
        if force_manual {
            return Err(format!(
                "Workspace '@{}' berada dalam mode read_only; sinkronisasi .airlock/HANDOFF.md ditolak.",
                ws.alias
            ));
        }
        return Ok(String::new());
    }

    if !force_manual && !state.config.read().await.webmcp_auto_handoff_dir {
        return Ok(String::new());
    }

    let coord = &state.handoff_manager.inner;
    let lock = workspace_lock(coord, &ws.id).await;
    let _guard = lock.lock().await;

    ensure_git_info_exclude(coord, &ws.canon_root).await?;

    let secrets = collect_secrets_to_redact(state).await;
    let (raw_entries, audit_records) = gather_audit_records_for_ws(state, ws, &secrets).await;
    let checkpoint = load_checkpoint_struct(state, &ws.id);
    let artifacts = state
        .artifact_store
        .list_workspace_latest_by_kind(&ws.clean_path, &["webmcp-latest", "delegated_"])
        .await;
    let delegated = coord.latest_delegated.read().await.get(&ws.id).cloned();
    let jobs = collect_webmcp_job_snapshots(&state.jobs.jobs, &ws.alias, &secrets, 5, 30).await;
    let (is_git, branch, head_short, status_lines, has_head) =
        collect_git_snapshot(&ws.canon_root).await;
    let latest_diff = collect_latest_diff(
        Some(state),
        ws,
        &raw_entries,
        &status_lines,
        is_git,
        has_head,
        &secrets,
    )
    .await;

    let (touched_files, last_group_id) =
        build_touched_files_status(checkpoint.as_ref(), &audit_records);
    let updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let cleared_notice = if matches!(trigger, HandoffTrigger::CheckpointCleared) {
        Some(format!("Checkpoint cleared at {updated_at}"))
    } else {
        None
    };

    coord.cached_ctx.write().await.insert(
        ws.id.clone(),
        CachedHandoffContext {
            secrets_to_redact: secrets.clone(),
            audit_records: audit_records.clone(),
            touched_files_status: touched_files.clone(),
            last_group_id: last_group_id.clone(),
            artifacts: artifacts.clone(),
            checkpoint: checkpoint.clone(),
            delegated: delegated.clone(),
        },
    );

    write_handoff_bundle(
        ws,
        &updated_at,
        &trigger.label(),
        branch.as_deref(),
        head_short.as_deref(),
        &status_lines,
        checkpoint.as_ref(),
        cleared_notice.as_deref(),
        &touched_files,
        last_group_id.as_deref(),
        &jobs,
        &audit_records,
        &artifacts,
        delegated.as_ref(),
        &latest_diff,
        &secrets,
    )
    .await
}

pub(crate) async fn sync_after_job_finished(
    coordinator: Arc<HandoffCoordinator>,
    jobs_map: Arc<RwLock<HashMap<String, ManagedJob>>>,
    ws: ResolvedWorkspace,
    job_id: String,
) {
    if ws.effective_guardrail == "read_only" {
        return;
    }

    let lock = workspace_lock(&coordinator, &ws.id).await;
    let _guard = lock.lock().await;

    if ensure_git_info_exclude(&coordinator, &ws.canon_root)
        .await
        .is_err()
    {
        return;
    }

    let cached_opt = coordinator.cached_ctx.read().await.get(&ws.id).cloned();
    let (secrets, audit_records, touched_files, last_group_id, artifacts, checkpoint, delegated) =
        match cached_opt {
            Some(c) => (
                c.secrets_to_redact,
                c.audit_records,
                c.touched_files_status,
                c.last_group_id,
                c.artifacts,
                c.checkpoint,
                c.delegated,
            ),
            None => (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
                Vec::new(),
                None,
                None,
            ),
        };

    let jobs = collect_webmcp_job_snapshots(&jobs_map, &ws.alias, &secrets, 5, 30).await;
    let (is_git, branch, head_short, status_lines, has_head) =
        collect_git_snapshot(&ws.canon_root).await;
    let latest_diff = if is_git {
        collect_latest_diff(None, &ws, &[], &status_lines, true, has_head, &secrets).await
    } else {
        fs::read_to_string(ws.canon_root.join(".airlock/handoff/latest.diff")).unwrap_or_default()
    };

    let updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let trigger_label = HandoffTrigger::JobFinished(job_id).label();

    let _ = write_handoff_bundle(
        &ws,
        &updated_at,
        &trigger_label,
        branch.as_deref(),
        head_short.as_deref(),
        &status_lines,
        checkpoint.as_ref(),
        None,
        &touched_files,
        last_group_id.as_deref(),
        &jobs,
        &audit_records,
        &artifacts,
        delegated.as_ref(),
        &latest_diff,
        &secrets,
    )
    .await;
}

pub(crate) async fn render_handoff_preview_readonly(
    state: &AppState,
    ws: &ResolvedWorkspace,
) -> String {
    let handoff_file = ws.canon_root.join(".airlock").join("HANDOFF.md");
    if handoff_file.is_file() {
        if let Ok(content) = fs::read_to_string(&handoff_file) {
            return content;
        }
    }

    let secrets = collect_secrets_to_redact(state).await;
    let (_, audit_records) = gather_audit_records_for_ws(state, ws, &secrets).await;
    let checkpoint = load_checkpoint_struct(state, &ws.id);
    let artifacts = state
        .artifact_store
        .list_workspace_latest_by_kind(&ws.clean_path, &["webmcp-latest", "delegated_"])
        .await;
    let delegated = state
        .handoff_manager
        .inner
        .latest_delegated
        .read()
        .await
        .get(&ws.id)
        .cloned();
    let jobs = collect_webmcp_job_snapshots(&state.jobs.jobs, &ws.alias, &secrets, 3, 20).await;
    let (_, branch, head_short, status_lines, _) = collect_git_snapshot(&ws.canon_root).await;
    let (touched_files, last_group_id) =
        build_touched_files_status(checkpoint.as_ref(), &audit_records);
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    render_handoff_md(
        ws,
        &now,
        "Live read-only preview",
        branch.as_deref(),
        head_short.as_deref(),
        &status_lines,
        checkpoint.as_ref(),
        None,
        &touched_files,
        last_group_id.as_deref(),
        &jobs,
        &audit_records,
        &artifacts,
        delegated.as_ref(),
        &secrets,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_manager::RunContext;
    use crate::workspace::WorkspaceProfile;
    use crate::mcp_resources::read_resource_with_context;
    use crate::tools::{execute_tool, execute_tool_with_run_context, revert_audit_entry};
    use serde_json::json;
    use std::process::Command as StdCommand;
    use std::sync::atomic::AtomicUsize;

    fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
        StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command should run in test")
    }

    fn init_git_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        assert!(run_git(dir, &["init"]).status.success());
        assert!(
            run_git(dir, &["config", "user.email", "test@example.com"])
                .status
                .success()
        );
        assert!(
            run_git(dir, &["config", "user.name", "Test User"])
                .status
                .success()
        );
        fs::write(dir.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        fs::write(dir.join("README.md"), "# Sample Repo\n").unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src").join("lib.rs"), "pub fn base() -> i32 { 1 }\n").unwrap();
        assert!(run_git(dir, &["add", "."]).status.success());
        assert!(
            run_git(dir, &["commit", "-m", "init commit"])
                .status
                .success()
        );
    }

    async fn setup_isolated_state(
        test_name: &str,
        init_git: bool,
    ) -> (PathBuf, PathBuf, Arc<AppState>, ResolvedWorkspace) {
        let base = std::env::temp_dir().join(format!("smart_mcp_handoff_{test_name}"));
        let _ = fs::remove_dir_all(&base);
        let data_dir = base.join("_appdata");
        let ws_dir = base.join("workspace");
        fs::create_dir_all(&data_dir).unwrap();
        if init_git {
            init_git_repo(&ws_dir);
        } else {
            fs::create_dir_all(&ws_dir).unwrap();
        }

        let state = Arc::new(AppState::new_persistent(
            ws_dir.clone(),
            data_dir.join("auth.json"),
            data_dir.join("config.json"),
        ));
        let clean_ws = crate::workspace::clean_path_string(&ws_dir);
        {
            let mut cfg = state.config.write().await;
            cfg.workspaces = vec![WorkspaceProfile {
                id: "ws-main".to_string(),
                alias: "repo".to_string(),
                path: clean_ws,
                enabled: true,
                guardrail_override: Some("autonomous".to_string()),
                allow_sensitive_override: None,
                extra_sensitive_patterns: Vec::new(),
                pinned: false,
            }];
            cfg.default_workspace_id = Some("ws-main".to_string());
        }
        *state.default_workspace_id.write().await = "ws-main".to_string();
        let ws = crate::workspace::resolve_workspace(&state, None, None)
            .await
            .unwrap();
        (base, ws.canon_root.clone(), state, ws)
    }

    #[tokio::test]
    async fn test_auto_handoff_creates_files_and_updates_git_info_exclude() {
        let (base, ws_dir, state, _ws) = setup_isolated_state("creates_and_exclude", true).await;
        let gitignore_before = fs::read_to_string(ws_dir.join(".gitignore")).unwrap();

        let res = execute_tool(
            &state,
            "WebMCP",
            "write_file_jailed",
            json!({
                "path": "src/new_mod.rs",
                "content": "pub fn greet() -> &'static str { \"hello\" }\n"
            }),
        )
        .await;
        assert!(!res.is_error, "write failed: {}", res.text);

        let handoff_md = ws_dir.join(".airlock").join("HANDOFF.md");
        let progress_md = ws_dir.join(".airlock").join("handoff").join("progress.md");
        let latest_diff = ws_dir.join(".airlock").join("handoff").join("latest.diff");
        let audit_jsonl = ws_dir
            .join(".airlock")
            .join("handoff")
            .join("session-audit.jsonl");

        assert!(handoff_md.is_file());
        assert!(progress_md.is_file());
        assert!(latest_diff.is_file());
        assert!(audit_jsonl.is_file());

        // Second WebMCP write must not duplicate .git/info/exclude lines
        let res2 = execute_tool(
            &state,
            "WebMCP",
            "surgical_edit",
            json!({
                "path": "src/lib.rs",
                "edits": [{
                    "search": "pub fn base() -> i32 { 1 }",
                    "replace": "pub fn base() -> i32 { 42 }"
                }]
            }),
        )
        .await;
        assert!(!res2.is_error, "edit failed: {}", res2.text);

        let exclude_text =
            fs::read_to_string(ws_dir.join(".git").join("info").join("exclude")).unwrap();
        assert_eq!(
            exclude_text
                .lines()
                .filter(|l| l.trim() == "/.airlock/HANDOFF.md")
                .count(),
            1
        );
        assert_eq!(
            exclude_text
                .lines()
                .filter(|l| l.trim() == "/.airlock/handoff/")
                .count(),
            1
        );
        assert_eq!(
            fs::read_to_string(ws_dir.join(".gitignore")).unwrap(),
            gitignore_before
        );

        let status_out = run_git(&ws_dir, &["status", "--porcelain"]);
        let status_str = String::from_utf8_lossy(&status_out.stdout);
        assert!(
            !status_str.contains(".airlock/HANDOFF.md") && !status_str.contains(".airlock/handoff"),
            "Handoff files must be excluded from git status:\n{status_str}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_auto_handoff_strict_webmcp_only_ignores_other_sources() {
        let (base, ws_dir, state, ws) = setup_isolated_state("strict_webmcp_only", false).await;

        for src in ["LocalAgent", "ACP", "MCP-Stdio", "Simulator", "Test"] {
            let w = execute_tool(
                &state,
                src,
                "write_file_jailed",
                json!({
                    "path": format!("non_webmcp_{}.txt", src.to_lowercase()),
                    "content": format!("written by {src}\n")
                }),
            )
            .await;
            assert!(!w.is_error, "write by {src} failed: {}", w.text);

            let cp = execute_tool(
                &state,
                src,
                "session_checkpoint",
                json!({
                    "op": "save",
                    "goal": format!("Goal from {src}"),
                    "next_steps": ["step 1"]
                }),
            )
            .await;
            assert!(!cp.is_error, "checkpoint by {src} failed: {}", cp.text);
        }

        assert!(
            !ws_dir.join(".airlock").exists(),
            ".airlock/ must NOT be created by LocalAgent/ACP/Stdio/Simulator/Test"
        );

        // Now a WebMCP edit creates .airlock/HANDOFF.md, containing ONLY WebMCP audit records
        let web_w = execute_tool(
            &state,
            "WebMCP",
            "write_file_jailed",
            json!({
                "path": "webmcp_only.txt",
                "content": "created by WebMCP\n"
            }),
        )
        .await;
        assert!(!web_w.is_error, "WebMCP write failed: {}", web_w.text);

        let audit_text = fs::read_to_string(
            ws_dir
                .join(".airlock")
                .join("handoff")
                .join("session-audit.jsonl"),
        )
        .unwrap();
        assert!(audit_text.contains("\"source\":\"WebMCP\""));
        for forbidden in ["LocalAgent", "ACP", "MCP-Stdio", "Simulator"] {
            assert!(
                !audit_text.contains(&format!("\"source\":\"{forbidden}\"")),
                "Unexpected non-WebMCP audit source leaked into session-audit.jsonl:\n{audit_text}"
            );
        }

        // Delegated run originated from WebMCP executes as "WebMCP:delegate" and syncs handoff
        let run_ctx = RunContext {
            run_id: "run-del-1".to_string(),
            parent_run_id: None,
            session_id: "sess-del".to_string(),
            workspace_id: ws.id.clone(),
            workspace_alias: ws.alias.clone(),
            workspace_clean_path: ws.clean_path.clone(),
            execution_mode: "delegated_execute".to_string(),
            depth: 1,
            turn_group_id: "grp-del-1".to_string(),
            subagent_counter: Arc::new(AtomicUsize::new(0)),
            acp_event_tx: None,
        };
        let del_w = execute_tool_with_run_context(
            Arc::clone(&state),
            "LocalAgent",
            "write_file_jailed",
            json!({
                "path": "delegated_output.txt",
                "content": "written inside delegated run\n"
            }),
            None,
            Some(&run_ctx),
        )
        .await;
        assert!(!del_w.is_error, "delegated write failed: {}", del_w.text);

        let audit_after_del = fs::read_to_string(
            ws_dir
                .join(".airlock")
                .join("handoff")
                .join("session-audit.jsonl"),
        )
        .unwrap();
        assert!(
            audit_after_del.contains("\"source\":\"WebMCP:delegate\""),
            "Expected WebMCP:delegate audit entry:\n{audit_after_del}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_latest_diff_includes_staged_and_full_untracked_new_files() {
        let (base, ws_dir, state, _ws) = setup_isolated_state("latest_diff_all_states", true).await;

        // 1. Staged modification on tracked README.md
        fs::write(ws_dir.join("README.md"), "# Sample Repo\nStaged line\n").unwrap();
        assert!(run_git(&ws_dir, &["add", "README.md"]).status.success());

        // 2. Unstaged modification on tracked src/lib.rs via WebMCP
        let edit_res = execute_tool(
            &state,
            "WebMCP",
            "surgical_edit",
            json!({
                "path": "src/lib.rs",
                "edits": [{
                    "search": "pub fn base() -> i32 { 1 }",
                    "replace": "pub fn base() -> i32 { 99 }"
                }]
            }),
        )
        .await;
        assert!(!edit_res.is_error, "surgical_edit failed: {}", edit_res.text);

        // 3. New untracked file created via WebMCP
        let new_res = execute_tool(
            &state,
            "WebMCP",
            "write_file_jailed",
            json!({
                "path": "src/fresh.rs",
                "content": "pub const FRESH_VAL: usize = 7;\n"
            }),
        )
        .await;
        assert!(!new_res.is_error);

        let diff_text =
            fs::read_to_string(ws_dir.join(".airlock").join("handoff").join("latest.diff")).unwrap();
        assert!(
            diff_text.contains("Staged line"),
            "Staged diff missing:\n{diff_text}"
        );
        assert!(
            diff_text.contains("pub fn base() -> i32 { 99 }"),
            "Unstaged diff missing:\n{diff_text}"
        );
        assert!(
            diff_text.contains("--- /dev/null")
                && diff_text.contains("+++ b/src/fresh.rs")
                && diff_text.contains("+pub const FRESH_VAL: usize = 7;"),
            "Full untracked new file diff missing:\n{diff_text}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_auto_handoff_handles_git_worktree_gitdir_file() {
        let base = std::env::temp_dir().join("smart_mcp_handoff_worktree_gitdir");
        let _ = fs::remove_dir_all(&base);
        let main_repo = base.join("main_repo");
        let ws_dir = base.join("wt1_checkout");
        init_git_repo(&main_repo);
        let wt_out = run_git(
            &main_repo,
            &[
                "worktree",
                "add",
                "-b",
                "wt1",
                ws_dir.to_string_lossy().as_ref(),
                "HEAD",
            ],
        );
        assert!(
            wt_out.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&wt_out.stderr)
        );
        assert!(ws_dir.join(".git").is_file());

        let state = Arc::new(AppState::new(base.join("_appdata")));
        let clean_ws = crate::workspace::clean_path_string(&ws_dir);
        state.config.write().await.workspaces = vec![WorkspaceProfile {
            id: "ws-wt".to_string(),
            alias: "wt".to_string(),
            path: clean_ws,
            enabled: true,
            guardrail_override: Some("autonomous".to_string()),
            allow_sensitive_override: None,
            extra_sensitive_patterns: Vec::new(),
            pinned: false,
        }];
        *state.default_workspace_id.write().await = "ws-wt".to_string();

        let res = execute_tool(
            &state,
            "WebMCP",
            "write_file_jailed",
            json!({ "path": "wt.txt", "content": "worktree content\n" }),
        )
        .await;
        assert!(!res.is_error, "write in worktree failed: {}", res.text);

        let common_git = main_repo.join(".git");
        let exclude_text = fs::read_to_string(common_git.join("info").join("exclude")).unwrap();
        assert!(exclude_text.contains("/.airlock/HANDOFF.md"));
        assert!(exclude_text.contains("/.airlock/handoff/"));
        assert!(ws_dir.join(".airlock").join("HANDOFF.md").is_file());

        let status_out = run_git(&ws_dir, &["status", "--porcelain"]);
        let status_str = String::from_utf8_lossy(&status_out.stdout);
        assert!(
            !status_str.contains(".airlock/HANDOFF.md") && !status_str.contains(".airlock/handoff"),
            "Worktree git status should ignore handoff:\n{status_str}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_auto_handoff_respects_read_only_guardrail_and_config_toggle() {
        let (base, ws_dir, state, ws) = setup_isolated_state("guardrail_and_toggle", false).await;

        // 1. Auto-sync disabled via webmcp_auto_handoff_dir = false
        state.config.write().await.webmcp_auto_handoff_dir = false;
        let w1 = execute_tool(
            &state,
            "WebMCP",
            "write_file_jailed",
            json!({ "path": "toggle_off.txt", "content": "no handoff yet\n" }),
        )
        .await;
        assert!(!w1.is_error);
        assert!(
            !ws_dir.join(".airlock").exists(),
            ".airlock/ should not be auto-created when webmcp_auto_handoff_dir=false"
        );

        // Re-enable toggle, set workspace guardrail to read_only -> both auto and manual reject/skip
        {
            let mut cfg = state.config.write().await;
            cfg.webmcp_auto_handoff_dir = true;
            cfg.workspaces[0].guardrail_override = Some("read_only".to_string());
        }
        let ro_ws = crate::workspace::resolve_workspace(&state, None, None)
            .await
            .unwrap();
        let auto_skip = sync_from_state(&state, &ro_ws, HandoffTrigger::ManualSync, false)
            .await
            .unwrap();
        assert!(auto_skip.is_empty());
        assert!(!ws_dir.join(".airlock").exists());

        let manual_err = sync_from_state(&state, &ro_ws, HandoffTrigger::ManualSync, true).await;
        assert!(manual_err.is_err());
        assert!(!ws_dir.join(".airlock").exists());

        // Restore autonomous guardrail -> manual sync creates handoff cleanly
        let created = sync_from_state(&state, &ws, HandoffTrigger::ManualSync, true)
            .await
            .unwrap();
        assert!(!created.is_empty());
        assert!(ws_dir.join(".airlock").join("HANDOFF.md").is_file());

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_auto_handoff_redacts_secrets_and_filters_readonly_audit() {
        let (base, ws_dir, state, ws) = setup_isolated_state("redact_and_filter", false).await;
        let auth_tok = state.auth_token.read().await.clone();
        let gemini_secret = "AIzaSyFakeSecretValueForTesting123456";
        state.provider_secrets.write().await.gemini_api_key = gemini_secret.to_string();

        fs::write(ws_dir.join("note.txt"), "hello world\n").unwrap();
        let ro = execute_tool(
            &state,
            "WebMCP",
            "read_file_range",
            json!({ "path": "note.txt" }),
        )
        .await;
        assert!(!ro.is_error);

        let cp = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({
                "op": "save",
                "goal": format!("Do not leak {auth_tok} or {gemini_secret}"),
                "notes": format!("Auth={auth_tok}"),
                "next_steps": ["Verify secret scrubbing"]
            }),
        )
        .await;
        assert!(!cp.is_error);
        let _ = sync_from_state(&state, &ws, HandoffTrigger::ManualSync, true)
            .await
            .unwrap();

        let handoff_txt = fs::read_to_string(ws_dir.join(".airlock").join("HANDOFF.md")).unwrap();
        let cp_json_txt = fs::read_to_string(
            ws_dir
                .join(".airlock")
                .join("handoff")
                .join("checkpoint.json"),
        )
        .unwrap();
        let audit_txt = fs::read_to_string(
            ws_dir
                .join(".airlock")
                .join("handoff")
                .join("session-audit.jsonl"),
        )
        .unwrap();

        for text in [&handoff_txt, &cp_json_txt] {
            assert!(!text.contains(&auth_tok), "Auth token leaked in:\n{text}");
            assert!(
                !text.contains(gemini_secret),
                "Gemini API key leaked in:\n{text}"
            );
            assert!(text.contains("<redacted>"));
        }
        assert!(
            !audit_txt.contains("read_file_range"),
            "Read-only audit entry must be filtered from session-audit.jsonl:\n{audit_txt}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_auto_handoff_updates_after_background_job_checkpoint_and_revert() {
        let (base, ws_dir, state, _ws) = setup_isolated_state("job_cp_revert", false).await;

        // 1. Checkpoint save
        let cp_save = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({
                "op": "save",
                "goal": "Complete backend sync",
                "completed": ["Added config flag"],
                "next_steps": ["Run verification job"]
            }),
        )
        .await;
        assert!(!cp_save.is_error);
        assert!(cp_save.text.contains(".airlock/HANDOFF.md"));
        assert!(
            ws_dir
                .join(".airlock")
                .join("handoff")
                .join("checkpoint.json")
                .is_file()
        );

        // 2. WebMCP edit so we have a revertible entry
        let ed = execute_tool(
            &state,
            "WebMCP",
            "write_file_jailed",
            json!({ "path": "work.txt", "content": "v1\n" }),
        )
        .await;
        assert!(!ed.is_error);
        let audit_id = {
            let logs = state.audit_logs.read().await;
            logs.iter()
                .rev()
                .find(|e| e.target_path.as_deref() == Some("work.txt") && e.can_revert)
                .unwrap()
                .id
                .clone()
        };

        // 3. Background job started via WebMCP updates HANDOFF.md & progress.md when done_rx fires
        let job_res = execute_tool(
            &state,
            "WebMCP",
            "start_command_job",
            json!({
                "command": "echo handoff_job_output_marker",
                "label": "Verify Marker"
            }),
        )
        .await;
        assert!(!job_res.is_error, "start_command_job failed: {}", job_res.text);

        let mut done_rx = {
            let map = state.jobs.jobs.read().await;
            let job = map.values().next().expect("job registered");
            job.done_rx.clone()
        };
        while !*done_rx.borrow() {
            done_rx.changed().await.unwrap();
        }
        let handoff_after_job =
            fs::read_to_string(ws_dir.join(".airlock").join("HANDOFF.md")).unwrap();
        let progress_after_job = fs::read_to_string(
            ws_dir
                .join(".airlock")
                .join("handoff")
                .join("progress.md"),
        )
        .unwrap();
        assert!(
            handoff_after_job.contains("Verify Marker")
                || handoff_after_job.contains("echo handoff_job_output_marker"),
            "HANDOFF.md missing job summary:\n{handoff_after_job}"
        );
        assert!(
            progress_after_job.contains("handoff_job_output_marker"),
            "progress.md missing job output:\n{progress_after_job}"
        );

        // 4. Revert audit entry updates session-audit.jsonl (reverted: true)
        revert_audit_entry(&state, &audit_id, false).await.unwrap();
        let audit_after_revert = fs::read_to_string(
            ws_dir
                .join(".airlock")
                .join("handoff")
                .join("session-audit.jsonl"),
        )
        .unwrap();
        assert!(
            audit_after_revert.contains("\"reverted\":true"),
            "Expected reverted=true in session-audit.jsonl:\n{audit_after_revert}"
        );

        // 5. Checkpoint clear removes checkpoint.json & records notice in progress.md
        let cp_clear = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({ "op": "clear" }),
        )
        .await;
        assert!(!cp_clear.is_error);
        assert!(
            !ws_dir
                .join(".airlock")
                .join("handoff")
                .join("checkpoint.json")
                .exists()
        );
        let progress_cleared = fs::read_to_string(
            ws_dir
                .join(".airlock")
                .join("handoff")
                .join("progress.md"),
        )
        .unwrap();
        assert!(progress_cleared.contains("Checkpoint cleared at"));

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_artifacts_merge_newest_per_kind_and_mcp_resource_preview() {
        let (base, ws_dir, state, ws) = setup_isolated_state("artifacts_and_resource", false).await;

        // Before any WebMCP write, reading resource MUST return markdown preview WITHOUT writing .airlock/
        let preview_val = read_resource_with_context(&state, "airlock://workspace/handoff", None)
            .await
            .unwrap();
        let preview_txt = preview_val["contents"][0]["text"].as_str().unwrap();
        assert!(preview_txt.contains("# Airlock WebMCP Handoff — @repo"));
        assert!(preview_txt.contains("Live read-only preview"));
        assert!(
            !ws_dir.join(".airlock").exists(),
            "Reading MCP resource must remain strictly read-only"
        );

        let aliased_val =
            read_resource_with_context(&state, &format!("workspace://@{}/handoff", ws.alias), None)
                .await
                .unwrap();
        assert!(aliased_val["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("# Airlock WebMCP Handoff"));
        assert!(!ws_dir.join(".airlock").exists());

        // Add delegated plan artifact and newer webmcp task artifact, plus older webmcp plan artifact
        state
            .artifact_store
            .upsert_artifact(
                &ws.clean_path,
                &ws.alias,
                "webmcp-latest",
                crate::artifacts::ArtifactKind::ImplementationPlan,
                "Old Plan",
                "old plan body",
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        state
            .artifact_store
            .upsert_artifact(
                &ws.clean_path,
                &ws.alias,
                "delegated_run1",
                crate::artifacts::ArtifactKind::ImplementationPlan,
                "Delegated Plan",
                "# Delegated Plan Body\nFresh step",
            )
            .await
            .unwrap();

        let task_art = execute_tool(
            &state,
            "WebMCP",
            "upsert_artifact",
            json!({
                "kind": "task",
                "title": "Main Task",
                "content_md": "- [x] Implement handoff\n- [ ] Verify suite\n"
            }),
        )
        .await;
        assert!(!task_art.is_error, "upsert_artifact failed: {}", task_art.text);

        let plan_md = fs::read_to_string(
            ws_dir
                .join(".airlock")
                .join("handoff")
                .join("artifacts")
                .join("implementation_plan.md"),
        )
        .unwrap();
        let task_md = fs::read_to_string(
            ws_dir
                .join(".airlock")
                .join("handoff")
                .join("artifacts")
                .join("task.md"),
        )
        .unwrap();
        assert!(plan_md.contains("Delegated Plan Body"));
        assert!(task_md.contains("Implement handoff"));

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_scanners_ignore_handoff_dir_while_direct_read_works() {
        let (base, ws_dir, state, _ws) = setup_isolated_state("scanners_ignore_handoff", false).await;

        fs::create_dir_all(ws_dir.join(".airlock")).unwrap();
        fs::write(ws_dir.join(".airlock").join("hooks.json"), "{\"hooks\":{}}\n").unwrap();
        fs::create_dir_all(ws_dir.join("src")).unwrap();
        fs::write(ws_dir.join("src").join("app.rs"), "pub fn UNIQUE_APP_TOKEN() {}\n").unwrap();

        let w = execute_tool(
            &state,
            "WebMCP",
            "write_file_jailed",
            json!({
                "path": "src/helper.rs",
                "content": "// HANDOFF_SCANNER_SENTINEL\npub fn helper() {}\n"
            }),
        )
        .await;
        assert!(!w.is_error);
        assert!(ws_dir.join(".airlock").join("HANDOFF.md").is_file());

        let ov = execute_tool(&state, "WebMCP", "project_overview", json!({ "depth": 4 })).await;
        assert!(!ov.is_error);
        assert!(!ov.text.contains("HANDOFF.md"));
        assert!(!ov.text.contains(".airlock/handoff"));

        let ff = execute_tool(&state, "WebMCP", "find_files", json!({})).await;
        assert!(!ff.is_error);
        assert!(ff.text.contains("src/app.rs"));
        assert!(ff.text.contains("src/helper.rs"));
        assert!(!ff.text.contains("HANDOFF.md"));
        assert!(!ff.text.contains("session-audit.jsonl"));

        let ss = execute_tool(
            &state,
            "WebMCP",
            "smart_search",
            json!({ "query": "HANDOFF_SCANNER_SENTINEL" }),
        )
        .await;
        assert!(!ss.is_error);
        assert!(ss.text.contains("src/helper.rs") || ss.text.contains("src\\helper.rs"));
        assert!(!ss.text.contains(".airlock/HANDOFF.md"));
        assert!(!ss.text.contains(".airlock/handoff"));

        // Direct read_file_range of both .airlock/HANDOFF.md and .airlock/hooks.json still works cleanly
        let direct = execute_tool(
            &state,
            "WebMCP",
            "read_file_range",
            json!({ "path": ".airlock/HANDOFF.md", "start_line": 1, "end_line": 20 }),
        )
        .await;
        assert!(!direct.is_error, "direct read failed: {}", direct.text);
        assert!(direct.text.contains("# Airlock WebMCP Handoff"));

        let direct_hooks = execute_tool(
            &state,
            "WebMCP",
            "read_file_range",
            json!({ "path": ".airlock/hooks.json", "start_line": 1, "end_line": 5 }),
        )
        .await;
        assert!(!direct_hooks.is_error, "hooks read failed: {}", direct_hooks.text);
        assert!(direct_hooks.text.contains("hooks"));

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_handoff_writes_redacted_prompts_md_and_clears_on_reset() {
        let (base, ws_dir, state, _ws) = setup_isolated_state("prompts_md_mirror", false).await;
        let auth_tok = state.auth_token.read().await.clone();

        let save_out = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({
                "op": "save",
                "stage": "blueprint",
                "blueprint_path": ".gemini/blueprints/09-workflow.md",
                "recommended_skills": ["test-driven-development"],
                "goal": format!("Build workflow mirror without leaking {auth_tok}"),
                "next_steps": ["Implement Phase 1"]
            }),
        )
        .await;
        assert!(!save_out.is_error, "checkpoint save failed: {}", save_out.text);

        let prompts_file = ws_dir.join(".airlock").join("handoff").join("prompts.md");
        assert!(prompts_file.is_file(), "prompts.md should be written on save");
        let prompts_txt = fs::read_to_string(&prompts_file).unwrap();
        assert!(prompts_txt.contains("Stage: `blueprint`"));
        assert!(prompts_txt.contains("Option 1: Continue in Gemini WebMCP"));
        assert!(prompts_txt.contains("Option 2: Handoff to Google Antigravity"));
        assert!(prompts_txt.contains("PRE-FLIGHT CHECKUP"));
        assert!(prompts_txt.contains("PRE-FLIGHT & FIELD AUDIT"));
        assert!(!prompts_txt.contains(&auth_tok), "Secret leaked into prompts.md");
        assert!(prompts_txt.contains("<redacted>"));

        let handoff_txt = fs::read_to_string(ws_dir.join(".airlock").join("HANDOFF.md")).unwrap();
        assert!(handoff_txt.contains(".airlock/handoff/prompts.md"));

        let clear_out = execute_tool(
            &state,
            "WebMCP",
            "session_checkpoint",
            json!({ "op": "clear" }),
        )
        .await;
        assert!(!clear_out.is_error);
        assert!(
            !prompts_file.exists(),
            "prompts.md must be removed after checkpoint clear"
        );

        let _ = fs::remove_dir_all(&base);
    }
}
