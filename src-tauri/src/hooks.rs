use crate::security::{compress_output, resolve_jailed_path, validate_command, wildcard_match};
use crate::state::AppState;
use crate::tools::{run_shell_raw, sha256_hex};
use crate::workspace::ResolvedWorkspace;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::Emitter;

const MAX_WORKFLOW_BYTES: u64 = 64 * 1024;
const MAX_HOOKS_JSON_BYTES: u64 = 64 * 1024;

fn default_hooks_version() -> u32 {
    1
}

fn default_timeout_secs() -> u64 {
    15
}

fn default_max_output_lines() -> usize {
    30
}

fn default_on_failure() -> String {
    "append_to_tool_output".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSummary {
    pub name: String,
    pub slash: String,
    pub title: String,
    pub description: String,
    pub source: String,
    pub execution_mode: Option<String>,
    pub prompt_template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDefinition {
    pub id: String,
    pub event: String,
    #[serde(default)]
    pub file_glob: Option<String>,
    pub command: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_output_lines")]
    pub max_output_lines: usize,
    #[serde(default = "default_on_failure")]
    pub on_failure: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HooksFile {
    #[serde(default = "default_hooks_version")]
    pub version: u32,
    #[serde(default)]
    pub hooks: Vec<HookDefinition>,
}

impl Default for HooksFile {
    fn default() -> Self {
        Self {
            version: default_hooks_version(),
            hooks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HooksStatusSummary {
    pub workspace_alias: String,
    pub workspace_path: String,
    pub hooks_file_exists: bool,
    pub current_sha256: Option<String>,
    pub trusted: bool,
    pub hash_changed: bool,
    pub hooks: Vec<HookDefinition>,
    pub parse_error: Option<String>,
}

pub fn normalize_trust_key(clean_ws_path: &str) -> String {
    let trimmed = clean_ws_path.trim();
    if cfg!(target_os = "windows") {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed.to_string()
    }
}

fn builtin_workflows() -> Vec<WorkflowSummary> {
    vec![
        WorkflowSummary {
            name: "plan".to_string(),
            slash: "/plan".to_string(),
            title: "Plan First & Architecture Review".to_string(),
            description: "Teliti codebase read-only, tulis Implementation Plan + Task List, dan tunggu approval sebelum edit.".to_string(),
            source: "builtin".to_string(),
            execution_mode: Some("plan_first".to_string()),
            prompt_template: "Investigasi codebase secara read-only menggunakan batch_inspect / smart_search / read_file_range. Susun Implementation Plan lewat upsert_artifact(kind=\"implementation_plan\") dan task checklist via upsert_artifact(kind=\"task_list\"), lalu panggil request_plan_approval sebelum mengedit kode.".to_string(),
        }
        .into(),
        WorkflowSummary {
            name: "review".to_string(),
            slash: "/review".to_string(),
            title: "Code & Security Review".to_string(),
            description: "Tinjau git diff & perubahan terbaru untuk bug, race condition, keamanan, dan edge case.".to_string(),
            source: "builtin".to_string(),
            execution_mode: None,
            prompt_template: "Jalankan git_ops(op=\"status\") dan git_ops(op=\"diff\"), inspeksi simbol terkait via code_outline & read_file_range, lalu laporkan bug nyata, celah keamanan, dan gap test secara ringkas.".to_string(),
        },
        WorkflowSummary {
            name: "debug".to_string(),
            slash: "/debug".to_string(),
            title: "Root-Cause Debugging".to_string(),
            description: "Telusuri akar masalah error build/runtime dengan smart_search & surgical verification.".to_string(),
            source: "builtin".to_string(),
            execution_mode: None,
            prompt_template: "Lakukan analisis akar masalah (root-cause): cari call sites dengan smart_search, baca irisan kode dengan read_file_range, lalu perbaiki minimal dengan surgical_edit atau patch_and_verify.".to_string(),
        },
        WorkflowSummary {
            name: "refactor".to_string(),
            slash: "/refactor".to_string(),
            title: "Safe Verified Refactor".to_string(),
            description: "Rencanakan dan terapkan refactor bedah multi-file dengan rollback safety.".to_string(),
            source: "builtin".to_string(),
            execution_mode: None,
            prompt_template: "Petakan struktur simbol modul target dengan code_outline, lakukan refactor bedah yang aman via multi_surgical_edit/surgical_edit tanpa duplikasi, dan verifikasi build/test.".to_string(),
        },
        WorkflowSummary {
            name: "architecture".to_string(),
            slash: "/architecture".to_string(),
            title: "Architecture Mapping".to_string(),
            description: "Petakan arsitektur repo, entry point, aliran data, dan batasan modul hemat token.".to_string(),
            source: "builtin".to_string(),
            execution_mode: None,
            prompt_template: "Panggil project_overview & find_files, inspeksi entry point dengan code_outline dan read_file_range terbatas, lalu rangkum arsitektur dan modul inti secara padat.".to_string(),
        },
        WorkflowSummary {
            name: "test".to_string(),
            slash: "/test".to_string(),
            title: "Run & Expand Verification Tests".to_string(),
            description: "Jalankan test suite (start_command_job untuk test panjang) dan tambahkan unit test yang kurang.".to_string(),
            source: "builtin".to_string(),
            execution_mode: None,
            prompt_template: "Identifikasi test runner proyek dan jalankan verifikasi (gunakan run_command_compressed atau start_command_job + poll_command_job bila lama), lalu tangani kegagalan atau tambah unit test.".to_string(),
        },
    ]
}

fn parse_workflow_file(file_stem: &str, raw: &str) -> WorkflowSummary {
    let cleaned = raw
        .strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n");
    let lines: Vec<&str> = cleaned.lines().collect();

    let mut title = String::new();
    let mut description = String::new();
    let mut mode_opt: Option<String> = None;
    let mut body_start = 0usize;

    if lines.first().map(|l| l.trim()) == Some("---") {
        if let Some(end_idx) = lines.iter().enumerate().skip(1).find_map(|(idx, l)| {
            if l.trim() == "---" {
                Some(idx)
            } else {
                None
            }
        }) {
            for fm in &lines[1..end_idx] {
                if let Some((k, v)) = fm.split_once(':') {
                    let key = k.trim().to_ascii_lowercase();
                    let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
                    match key.as_str() {
                        "title" | "name" if !val.is_empty() => title = val,
                        "description" | "desc" if !val.is_empty() => description = val,
                        "mode" | "execution_mode" if val == "plan_first" || val == "direct" => {
                            mode_opt = Some(val);
                        }
                        _ => {}
                    }
                }
            }
            body_start = end_idx + 1;
        }
    }

    let body = lines[body_start..]
        .join("\n")
        .trim()
        .to_string();

    if title.is_empty() {
        title = lines[body_start..]
            .iter()
            .find_map(|l| {
                let t = l.trim();
                t.strip_prefix("# ").map(|s| s.trim().to_string())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("Workflow /{file_stem}"));
    }

    if description.is_empty() {
        description = lines[body_start..]
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.chars().take(120).collect())
            .unwrap_or_else(|| "Custom workspace workflow".to_string());
    }

    let name = file_stem
        .trim()
        .to_ascii_lowercase()
        .replace(' ', "-")
        .replace('_', "-");

    WorkflowSummary {
        slash: format!("/{name}"),
        name,
        title,
        description,
        source: "workspace".to_string(),
        execution_mode: mode_opt,
        prompt_template: body,
    }
}

pub fn list_workflows(ws_root: &Path) -> Vec<WorkflowSummary> {
    let mut order = Vec::new();
    let mut map: BTreeMap<String, WorkflowSummary> = BTreeMap::new();

    for item in builtin_workflows() {
        order.push(item.name.clone());
        map.insert(item.name.clone(), item);
    }

    let mut custom_names = Vec::new();
    for rel_dir in [".airlock/workflows", ".agents/workflows"] {
        let Ok(canon_dir) = resolve_jailed_path(ws_root, rel_dir) else {
            continue;
        };
        if !canon_dir.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&canon_dir) else {
            continue;
        };
        let mut md_files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("md"))
                    .unwrap_or(false)
            })
            .collect();
        md_files.sort();

        for path in md_files {
            let Ok(canon_file) = fs::canonicalize(&path) else {
                continue;
            };
            let canon_root = fs::canonicalize(ws_root).unwrap_or_else(|_| ws_root.to_path_buf());
            if !canon_file.starts_with(&canon_root) {
                continue;
            }
            let Ok(meta) = fs::metadata(&canon_file) else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_WORKFLOW_BYTES {
                continue;
            }
            let Some(stem) = canon_file.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(raw) = fs::read_to_string(&canon_file) else {
                continue;
            };
            let parsed = parse_workflow_file(stem, &raw);
            if !map.contains_key(&parsed.name) {
                custom_names.push(parsed.name.clone());
            }
            map.insert(parsed.name.clone(), parsed);
        }
    }

    custom_names.sort();
    custom_names.dedup();
    order.extend(custom_names);

    let mut out = Vec::with_capacity(order.len());
    for key in order {
        if let Some(wf) = map.remove(&key) {
            out.push(wf);
        }
    }
    out
}

fn read_workspace_hooks_raw(
    ws: &ResolvedWorkspace,
) -> (bool, Option<String>, Vec<HookDefinition>, Option<String>) {
    let hooks_path = ws.canon_root.join(".airlock").join("hooks.json");
    if !hooks_path.is_file() {
        return (false, None, Vec::new(), None);
    }
    let Ok(canon_hooks) = fs::canonicalize(&hooks_path) else {
        return (true, None, Vec::new(), Some("Gagal kanonikalisasi .airlock/hooks.json.".to_string()));
    };
    if !canon_hooks.starts_with(&ws.canon_root) {
        return (
            true,
            None,
            Vec::new(),
            Some("Symlink .airlock/hooks.json berada di luar workspace root.".to_string()),
        );
    }
    let Ok(meta) = fs::metadata(&canon_hooks) else {
        return (true, None, Vec::new(), Some("Gagal membaca metadata .airlock/hooks.json.".to_string()));
    };
    if meta.len() > MAX_HOOKS_JSON_BYTES {
        return (
            true,
            None,
            Vec::new(),
            Some("File .airlock/hooks.json melebihi batas 64 KB.".to_string()),
        );
    }
    let Ok(bytes) = fs::read(&canon_hooks) else {
        return (true, None, Vec::new(), Some("Gagal membaca .airlock/hooks.json.".to_string()));
    };
    let current_sha = sha256_hex(&bytes);
    match serde_json::from_slice::<HooksFile>(&bytes) {
        Ok(parsed) => (true, Some(current_sha), parsed.hooks, None),
        Err(err) => (
            true,
            Some(current_sha),
            Vec::new(),
            Some(format!("Format .airlock/hooks.json tidak valid: {err}")),
        ),
    }
}

pub async fn get_hooks_status(
    state: &AppState,
    ws: &ResolvedWorkspace,
) -> HooksStatusSummary {
    let (exists, current_sha, hooks, parse_error) = read_workspace_hooks_raw(ws);
    let key = normalize_trust_key(&ws.clean_path);
    let saved_hash = state
        .config
        .read()
        .await
        .trusted_hook_hashes
        .get(&key)
        .cloned();

    let trusted = match (&current_sha, &saved_hash, &parse_error) {
        (Some(cur), Some(saved), None) => cur == saved,
        _ => false,
    };
    let hash_changed = match (&current_sha, &saved_hash) {
        (Some(cur), Some(saved)) => cur != saved,
        _ => false,
    };

    HooksStatusSummary {
        workspace_alias: ws.alias.clone(),
        workspace_path: ws.clean_path.clone(),
        hooks_file_exists: exists,
        current_sha256: current_sha,
        trusted,
        hash_changed,
        hooks,
        parse_error,
    }
}

pub async fn trust_workspace_hooks(
    state: &AppState,
    ws: &ResolvedWorkspace,
    expected_sha256: &str,
) -> Result<HooksStatusSummary, String> {
    let (exists, current_sha_opt, _hooks, parse_err) = read_workspace_hooks_raw(ws);
    if !exists {
        return Err(format!(
            "Workspace @{} tidak memiliki file .airlock/hooks.json.",
            ws.alias
        ));
    }
    if let Some(err) = parse_err {
        return Err(err);
    }
    let current_sha = current_sha_opt
        .ok_or_else(|| "Gagal menghitung SHA-256 .airlock/hooks.json.".to_string())?;
    if !expected_sha256.trim().eq_ignore_ascii_case(&current_sha) {
        return Err(
            "Isi .airlock/hooks.json berubah sejak terakhir diinspeksi (hash mismatch). Muat ulang dialog dan tinjau ulang perintahnya."
                .to_string(),
        );
    }

    let key = normalize_trust_key(&ws.clean_path);
    {
        let mut cfg = state.config.write().await;
        cfg.trusted_hook_hashes.insert(key, current_sha);
    }
    state.save_config_state().await;
    Ok(get_hooks_status(state, ws).await)
}

async fn collect_executable_hooks(
    state: &AppState,
    ws: &ResolvedWorkspace,
    event: &str,
) -> Vec<HookDefinition> {
    if ws.effective_guardrail == "read_only" {
        return Vec::new();
    }

    let (global_hooks, saved_hash) = {
        let cfg = state.config.read().await;
        let key = normalize_trust_key(&ws.clean_path);
        (cfg.hooks.clone(), cfg.trusted_hook_hashes.get(&key).cloned())
    };

    let mut out: Vec<HookDefinition> = global_hooks
        .into_iter()
        .filter(|h| h.enabled && h.event.eq_ignore_ascii_case(event))
        .collect();

    let (exists, current_sha_opt, ws_hooks, parse_err) = read_workspace_hooks_raw(ws);
    if exists && parse_err.is_none() {
        let is_trusted = match (&current_sha_opt, &saved_hash) {
            (Some(cur), Some(saved)) => cur == saved,
            _ => false,
        };
        if is_trusted {
            out.extend(
                ws_hooks
                    .into_iter()
                    .filter(|h| h.enabled && h.event.eq_ignore_ascii_case(event)),
            );
        } else if ws_hooks
            .iter()
            .any(|h| h.enabled && h.event.eq_ignore_ascii_case(event))
        {
            if let Some(app) = state.app_handle.read().await.clone() {
                let status = get_hooks_status(state, ws).await;
                let _ = app.emit("hooks-trust-required", status);
            }
        }
    }

    out
}

fn hook_matches_any_path(glob_opt: Option<&str>, edited_rel_paths: &[String]) -> bool {
    let Some(glob_raw) = glob_opt.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let patterns: Vec<&str> = glob_raw
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if patterns.is_empty() {
        return true;
    }

    for rel in edited_rel_paths {
        let clean_rel = rel.trim().replace('\\', "/");
        let file_name = clean_rel.rsplit('/').next().unwrap_or(&clean_rel);
        for pat in &patterns {
            if wildcard_match(pat, &clean_rel) || wildcard_match(pat, file_name) {
                return true;
            }
        }
    }
    false
}

pub async fn run_post_edit_hooks(
    state: &AppState,
    ws: &ResolvedWorkspace,
    edited_rel_paths: &[String],
) -> Option<String> {
    if edited_rel_paths.is_empty() {
        return None;
    }
    let hooks = collect_executable_hooks(state, ws, "post_edit").await;
    if hooks.is_empty() {
        return None;
    }

    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut blocks: Vec<String> = Vec::new();

    for hook in hooks {
        let hook_key = hook.id.trim().to_ascii_lowercase();
        if hook_key.is_empty() || !seen_ids.insert(hook_key) {
            continue;
        }
        if !hook_matches_any_path(hook.file_glob.as_deref(), edited_rel_paths) {
            continue;
        }
        if validate_command(&hook.command).is_err() {
            continue;
        }
        let timeout = hook.timeout_secs.clamp(1, 20);
        match run_shell_raw(&ws.canon_root, &hook.command, timeout).await {
            Ok((code, stdout, stderr)) => {
                if code != 0 && !hook.on_failure.eq_ignore_ascii_case("ignore") {
                    let max_lines = hook.max_output_lines.clamp(5, 80);
                    let (compressed, _) = compress_output(&stdout, &stderr, max_lines);
                    blocks.push(format!(
                        "--- HOOK DIAGNOSTICS ({}, exit {code}) ---\n{compressed}",
                        hook.id
                    ));
                }
            }
            Err(err) => {
                if !hook.on_failure.eq_ignore_ascii_case("ignore") {
                    blocks.push(format!(
                        "--- HOOK DIAGNOSTICS ({}, error) ---\n{err}",
                        hook.id
                    ));
                }
            }
        }
    }

    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n\n"))
    }
}

pub async fn run_pre_commit_hooks(
    state: &AppState,
    ws: &ResolvedWorkspace,
) -> Result<Option<String>, String> {
    let hooks = collect_executable_hooks(state, ws, "pre_commit").await;
    if hooks.is_empty() {
        return Ok(None);
    }

    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut warnings: Vec<String> = Vec::new();

    for hook in hooks {
        let hook_key = hook.id.trim().to_ascii_lowercase();
        if hook_key.is_empty() || !seen_ids.insert(hook_key) {
            continue;
        }
        validate_command(&hook.command)?;
        let timeout = hook.timeout_secs.clamp(1, 20);
        let (code, stdout, stderr) = run_shell_raw(&ws.canon_root, &hook.command, timeout).await?;
        if code != 0 {
            let max_lines = hook.max_output_lines.clamp(5, 80);
            let (compressed, _) = compress_output(&stdout, &stderr, max_lines);
            if hook.on_failure.eq_ignore_ascii_case("block") {
                return Err(format!(
                    "Commit dibatalkan oleh pre_commit hook '{}' (exit {code}):\n{compressed}",
                    hook.id
                ));
            }
            if !hook.on_failure.eq_ignore_ascii_case("ignore") {
                warnings.push(format!(
                    "--- HOOK DIAGNOSTICS ({}, exit {code}) ---\n{compressed}",
                    hook.id
                ));
            }
        }
    }

    if warnings.is_empty() {
        Ok(None)
    } else {
        Ok(Some(warnings.join("\n\n")))
    }
}

pub async fn run_post_turn_hooks(state: &AppState, ws: &ResolvedWorkspace) -> Option<String> {
    let hooks = collect_executable_hooks(state, ws, "post_turn").await;
    if hooks.is_empty() {
        return None;
    }

    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut outputs: Vec<String> = Vec::new();
    for hook in hooks {
        let hook_key = hook.id.trim().to_ascii_lowercase();
        if hook_key.is_empty() || !seen_ids.insert(hook_key) {
            continue;
        }
        if validate_command(&hook.command).is_err() {
            continue;
        }
        let timeout = hook.timeout_secs.clamp(1, 20);
        if let Ok((code, stdout, stderr)) = run_shell_raw(&ws.canon_root, &hook.command, timeout).await {
            if code != 0 && !hook.on_failure.eq_ignore_ascii_case("ignore") {
                let (compressed, _) = compress_output(&stdout, &stderr, hook.max_output_lines.clamp(5, 80));
                outputs.push(format!(
                    "--- POST-TURN HOOK ({}, exit {code}) ---\n{compressed}",
                    hook.id
                ));
            }
        }
    }
    if outputs.is_empty() {
        None
    } else {
        Some(outputs.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::execute_tool;
    use crate::workspace::{clean_path_string, resolve_workspace};
    use serde_json::json;

    #[tokio::test]
    async fn test_untrusted_workspace_hooks_never_execute_and_hash_change_revokes_trust() {
        let base = std::env::temp_dir().join("smart_mcp_hooks_trust_test");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join(".airlock")).unwrap();
        fs::write(base.join("demo.txt"), "hello world\n").unwrap();

        let cmd_write_marker = if cfg!(target_os = "windows") {
            "echo RAN>hook_executed.marker"
        } else {
            "echo RAN > hook_executed.marker"
        };

        let hooks_json_v1 = format!(
            r#"{{
              "version": 1,
              "hooks": [
                {{
                  "id": "marker-hook",
                  "event": "post_edit",
                  "file_glob": "*.txt",
                  "command": "{cmd_write_marker}",
                  "timeout_secs": 10
                }}
              ]
            }}"#
        );
        let hooks_file = base.join(".airlock").join("hooks.json");
        fs::write(&hooks_file, &hooks_json_v1).unwrap();

        let state = AppState::new(base.clone());
        let ws = resolve_workspace(&state, None, None).await.unwrap();

        // 1. Untrusted hooks MUST NEVER execute during surgical_edit
        let edit1 = execute_tool(
            &state,
            "LocalAgent",
            "surgical_edit",
            json!({
                "path": "demo.txt",
                "edits": [{ "search": "hello world", "replace": "hello v1" }]
            }),
        )
        .await;
        assert!(!edit1.is_error, "Edit should succeed: {}", edit1.text);
        assert!(!base.join("hook_executed.marker").exists());

        // 2. Trust current SHA-256 -> hook executes on matching edit
        let status = get_hooks_status(&state, &ws).await;
        assert!(status.hooks_file_exists);
        assert!(!status.trusted);
        let sha1 = status.current_sha256.unwrap();
        trust_workspace_hooks(&state, &ws, &sha1).await.unwrap();

        let edit2 = execute_tool(
            &state,
            "LocalAgent",
            "surgical_edit",
            json!({
                "path": "demo.txt",
                "edits": [{ "search": "hello v1", "replace": "hello v2" }]
            }),
        )
        .await;
        assert!(!edit2.is_error);
        assert!(base.join("hook_executed.marker").is_file());

        // 3. Modify 1 byte in .airlock/hooks.json -> trust automatically revoked before execution!
        let _ = fs::remove_file(base.join("hook_executed.marker"));
        let modified = format!("{hooks_json_v1}\n ");
        fs::write(&hooks_file, modified).unwrap();

        let status_after = get_hooks_status(&state, &ws).await;
        assert!(!status_after.trusted);
        assert!(status_after.hash_changed);

        let edit3 = execute_tool(
            &state,
            "LocalAgent",
            "surgical_edit",
            json!({
                "path": "demo.txt",
                "edits": [{ "search": "hello v2", "replace": "hello v3" }]
            }),
        )
        .await;
        assert!(!edit3.is_error);
        assert!(
            !base.join("hook_executed.marker").exists(),
            "Modified hooks.json must never execute without re-approval"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_post_edit_hook_appends_compressed_diagnostics_and_dedups_batch() {
        let base = std::env::temp_dir().join("smart_mcp_hooks_dedup_diag_test");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join(".airlock")).unwrap();
        fs::create_dir_all(base.join("src")).unwrap();
        fs::write(base.join("src/a.rs"), "fn a() -> i32 { 1 }\n").unwrap();
        fs::write(base.join("src/b.rs"), "fn b() -> i32 { 2 }\n").unwrap();

        let fail_and_log_cmd = if cfg!(target_os = "windows") {
            "echo HIT>>hook_hits.txt & echo error[E0425]: sample compiler diag 1>&2 & exit /b 1"
        } else {
            "echo HIT >> hook_hits.txt; echo error[E0425]: sample compiler diag >&2; exit 1"
        };

        let hooks_json = serde_json::json!({
            "version": 1,
            "hooks": [{
                "id": "rust-check-after-edit",
                "event": "post_edit",
                "file_glob": "*.rs",
                "command": fail_and_log_cmd,
                "timeout_secs": 10,
                "max_output_lines": 20,
                "on_failure": "append_to_tool_output",
                "enabled": true
            }]
        });
        fs::write(
            base.join(".airlock/hooks.json"),
            serde_json::to_string_pretty(&hooks_json).unwrap(),
        )
        .unwrap();

        let state = AppState::new(base.clone());
        let ws = resolve_workspace(&state, None, None).await.unwrap();
        let sha = get_hooks_status(&state, &ws)
            .await
            .current_sha256
            .unwrap();
        trust_workspace_hooks(&state, &ws, &sha).await.unwrap();

        // Batch edit 2 .rs files -> hook runs ONCE (deduplicated) and appends diagnostics to ToolOutput.text
        let out = execute_tool(
            &state,
            "LocalAgent",
            "multi_surgical_edit",
            json!({
                "files": [
                    { "path": "src/a.rs", "edits": [{ "search": "{ 1 }", "replace": "{ 10 }" }] },
                    { "path": "src/b.rs", "edits": [{ "search": "{ 2 }", "replace": "{ 20 }" }] }
                ]
            }),
        )
        .await;

        assert!(!out.is_error, "multi_surgical_edit should succeed even if post_edit diag fails: {}", out.text);
        assert!(
            out.text.contains("HOOK DIAGNOSTICS (rust-check-after-edit, exit 1)"),
            "Missing hook header in:\n{}",
            out.text
        );
        assert!(
            out.text.contains("sample compiler diag"),
            "Missing diag text in:\n{}",
            out.text
        );

        let hits = fs::read_to_string(base.join("hook_hits.txt")).unwrap();
        assert_eq!(
            hits.lines().count(),
            1,
            "Batch multi_surgical_edit must run matching post_edit hook exactly once, got:\n{hits}"
        );
        let _ = clean_path_string(&base);

        let _ = fs::remove_dir_all(&base);
    }
}
