use crate::jobs;
use crate::security::{compress_output, resolve_jailed_path, wildcard_match};
use crate::state::{AppState, AuditEntry};
use crate::tools::{
    check_sensitive_guard_for_workspace, check_sensitive_guard_parts, current_turn_group_id,
    protected_internal_files, sha256_hex,
};
use crate::workspace::{resolve_workspace, ResolvedWorkspace};
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::Value;
use std::fs;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;

static OUTLINE_RE_RS: OnceLock<Regex> = OnceLock::new();
static OUTLINE_RE_JS: OnceLock<Regex> = OnceLock::new();
static OUTLINE_RE_PY: OnceLock<Regex> = OnceLock::new();
static OUTLINE_RE_GO: OnceLock<Regex> = OnceLock::new();
static OUTLINE_RE_C_FAMILY: OnceLock<Regex> = OnceLock::new();
static OUTLINE_RE_MD: OnceLock<Regex> = OnceLock::new();
static OUTLINE_RE_GENERIC: OnceLock<Regex> = OnceLock::new();

pub(crate) fn outline_regex_for_ext(ext: &str) -> &'static Regex {
    match ext {
        "rs" => OUTLINE_RE_RS.get_or_init(|| {
            Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\x22[^\x22]+\x22\s+)?(?:fn|struct|enum|trait|impl\b|mod\b|type\b|const\b|static\b|macro_rules!)").expect("valid rs outline regex")
        }),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => OUTLINE_RE_JS.get_or_init(|| {
            Regex::new(r"^\s*(?:export\s+(?:default\s+)?)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(?:function\b|class\b|interface\b|type\b|enum\b|(?:const|let)\s+[A-Za-z0-9_$]+\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z0-9_$]+)\s*=>)").expect("valid js outline regex")
        }),
        "py" => OUTLINE_RE_PY.get_or_init(|| {
            Regex::new(r"^\s*(?:async\s+def|def|class)\s+[A-Za-z0-9_]+").expect("valid py outline regex")
        }),
        "go" => OUTLINE_RE_GO.get_or_init(|| {
            Regex::new(r"^\s*(?:func\b|type\s+[A-Za-z0-9_]+\s+(?:struct|interface)\b)").expect("valid go outline regex")
        }),
        "java" | "cs" | "cpp" | "cc" | "c" | "h" | "hpp" => OUTLINE_RE_C_FAMILY.get_or_init(|| {
            Regex::new(r"^\s*(?:public|private|protected|internal|static|virtual|override|async|template)?[^{;]*\b(?:class|struct|interface|enum|namespace)\b|^\s*(?:public|private|protected|internal|static|async|virtual|inline)[^=;{}]*\([^;]*\)\s*(?:const\s*)?\{?\s*$").expect("valid c-family outline regex")
        }),
        "md" | "markdown" => OUTLINE_RE_MD.get_or_init(|| {
            Regex::new(r"^#{1,4}\s+.+").expect("valid md outline regex")
        }),
        _ => OUTLINE_RE_GENERIC.get_or_init(|| {
            Regex::new(r"^\s*(?:export\s+)?(?:pub\s+)?(?:async\s+)?(?:fn|function|def|class|struct|interface|enum|trait|impl|type)\b").expect("valid generic outline regex")
        }),
    }
}

pub(crate) fn extract_outline_text(rel_path: &str, ext: &str, content: &str, max_symbols: usize) -> (String, usize) {
    if let Some(syms) = crate::semantic::extract_ast_symbols(ext, content) {
        if !syms.is_empty() {
            return crate::semantic::format_outline_text(rel_path, &syms, max_symbols);
        }
    }

    let re = outline_regex_for_ext(ext);
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            let trimmed = line.trim();
            let clean: String = trimmed.chars().take(140).collect();
            symbols.push(format!("L{:>4} | {}", idx + 1, clean));
            if symbols.len() >= max_symbols {
                break;
            }
        }
    }

    let sym_len = symbols.len();
    let output = if symbols.is_empty() {
        format!("OUTLINE: {rel_path} ({total_lines} baris) — tidak ada deklarasi simbol yang terdeteksi.")
    } else {
        format!(
            "OUTLINE: {} ({} baris, {} simbol ditampilkan)\n\n{}",
            rel_path,
            total_lines,
            sym_len,
            symbols.join("\n")
        )
    };
    (output, sym_len)
}

pub(crate) fn run_find_files_blocking(
    search_root: std::path::PathBuf,
    canon_root: std::path::PathBuf,
    protected_files: Vec<std::path::PathBuf>,
    allow_sensitive: bool,
    sensitive_patterns: Vec<String>,
    pattern: String,
    max_results: usize,
) -> (Vec<String>, usize, usize, bool) {
    let start = std::time::Instant::now();
    let mut found = Vec::new();
    let mut total_walked = 0usize;
    let mut sensitive_skipped = 0usize;
    let mut was_capped = false;
    let pat_has_slash = pattern.contains('/') || pattern.contains('\\');
    let pat_norm = pattern.replace('\\', "/");

    let search_rel = search_root
        .strip_prefix(&canon_root)
        .unwrap_or(&search_root)
        .to_string_lossy()
        .replace('\\', "/");
    let searching_inside_handoff = crate::handoff::is_handoff_ignored_rel(&search_rel);
    let root_for_filter = canon_root.clone();

    let walker = WalkBuilder::new(&search_root)
        .filter_entry(move |e| {
            let name = e.file_name().to_string_lossy();
            if name == ".git"
                || name == "node_modules"
                || name == "target"
                || name == "dist"
                || name.eq_ignore_ascii_case(".smart-tauri-hub")
            {
                return false;
            }
            if !searching_inside_handoff {
                if let Ok(rel) = e.path().strip_prefix(&root_for_filter) {
                    let rel_str = rel.to_string_lossy();
                    if crate::handoff::is_handoff_ignored_rel(&rel_str) {
                        return false;
                    }
                }
            }
            true
        })
        .build();

    for entry in walker.flatten() {
        total_walked += 1;
        if total_walked > 5000 {
            was_capped = true;
            break;
        }
        if total_walked % 128 == 0 && start.elapsed() > Duration::from_secs(5) {
            was_capped = true;
            break;
        }

        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel_obj = path.strip_prefix(&canon_root).unwrap_or(path);
        let rel_str = rel_obj.to_string_lossy().replace('\\', "/");
        if check_sensitive_guard_parts(
            &protected_files,
            &canon_root,
            allow_sensitive,
            &sensitive_patterns,
            path,
            &rel_str,
        )
        .is_err()
        {
            sensitive_skipped += 1;
            continue;
        }

        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let matched = if pat_has_slash {
            wildcard_match(&pat_norm, &rel_str)
        } else {
            wildcard_match(&pat_norm, fname) || wildcard_match(&pat_norm, &rel_str)
        };
        if matched {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            found.push(format!("{rel_str} ({size} B)"));
            if found.len() >= max_results {
                break;
            }
        }
    }

    (found, total_walked, sensitive_skipped, was_capped)
}

pub(crate) fn format_find_files_output(
    pattern: &str,
    found: &[String],
    total_walked: usize,
    sensitive_skipped: usize,
    was_capped: bool,
) -> String {
    let mut notes = Vec::new();
    if was_capped {
        notes.push("[Scan dibatasi untuk menjaga responsivitas — persempit parameter 'path']".to_string());
    }
    if sensitive_skipped > 0 {
        notes.push(format!(
            "[Sensitive File Guard: {sensitive_skipped} file sensitif dilewati]"
        ));
    }
    let notes_suffix = if notes.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", notes.join("\n"))
    };

    if found.is_empty() {
        format!("Tidak ada file yang cocok dengan pola '{pattern}' ({total_walked} entri dipindai).{notes_suffix}")
    } else {
        format!(
            "Ditemukan {} file cocok pola '{}':\n{}{}",
            found.len(),
            pattern,
            found.join("\n"),
            notes_suffix
        )
    }
}

pub async fn exec_find_files(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let root = &ws.canon_root;
    let pattern = args
        .get("pattern")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("*")
        .to_string();
    let search_root = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => resolve_jailed_path(root, p)?,
        _ => resolve_jailed_path(root, ".")?,
    };
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(80)
        .clamp(1, 300) as usize;

    let allow_sensitive = ws.effective_allow_sensitive;
    let sensitive_patterns = ws.effective_sensitive_patterns.clone();
    let canon_root = ws.canon_root.clone();
    let protected_files = protected_internal_files(state);
    let pat_clone = pattern.clone();

    let (found, total_walked, sensitive_skipped, was_capped) =
        tokio::task::spawn_blocking(move || {
            run_find_files_blocking(
                search_root,
                canon_root,
                protected_files,
                allow_sensitive,
                sensitive_patterns,
                pat_clone,
                max_results,
            )
        })
        .await
        .map_err(|e| format!("Gagal memindai file: {e}"))?;

    let output =
        format_find_files_output(&pattern, &found, total_walked, sensitive_skipped, was_capped);

    let tokens_saved = 200;
    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "find_files".to_string(),
                target_path: args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                summary: format!("Pattern '{}' -> {} file(s)", pattern, found.len()),
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
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            1,
        )
        .await;

    Ok(output)
}

pub async fn exec_code_outline(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let rel_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'path' wajib diisi.")?;
    let max_symbols = args
        .get("max_symbols")
        .and_then(|v| v.as_u64())
        .unwrap_or(120)
        .clamp(10, 400) as usize;

    let target = resolve_jailed_path(&ws.canon_root, rel_path)?;
    check_sensitive_guard_for_workspace(state, ws, &target, rel_path).await?;
    let content = fs::read_to_string(&target)
        .map_err(|e| format!("Gagal membaca file '{rel_path}': {e}"))?;

    let ext = target
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let (output, sym_count) = extract_outline_text(rel_path, &ext, &content, max_symbols);

    let tokens_saved = (content.len() / 4).saturating_sub(output.len() / 4);
    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "code_outline".to_string(),
                target_path: Some(rel_path.to_string()),
                summary: format!("Extracted {} symbol(s) from {}", sym_count, rel_path),
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
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            1,
        )
        .await;

    Ok(output)
}

pub async fn exec_read_symbol(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let rel_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("Argumen 'path' wajib diisi.")?;
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("Argumen 'symbol' wajib diisi.")?;
    let include_docs = args
        .get("include_docs")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let max_lines = args
        .get("max_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(200)
        .clamp(5, 500) as usize;

    let target = resolve_jailed_path(&ws.canon_root, rel_path)?;
    check_sensitive_guard_for_workspace(state, ws, &target, rel_path).await?;
    let content = fs::read_to_string(&target)
        .map_err(|e| format!("Gagal membaca file '{rel_path}': {e}"))?;

    let ext = target
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let output = crate::semantic::read_symbol_from_content(
        rel_path,
        &ext,
        &content,
        symbol,
        include_docs,
        max_lines,
    )?;

    let tokens_saved = (content.len() / 4).saturating_sub(output.len() / 4);
    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "read_symbol".to_string(),
                target_path: Some(rel_path.to_string()),
                summary: format!("Read symbol '{}' from {}", symbol, rel_path),
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
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            1,
        )
        .await;

    Ok(output)
}

pub async fn exec_git_ops(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let root = &ws.canon_root;
    let op = args
        .get("op")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'op' wajib diisi.")?
        .trim();

    let rel_path_opt = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());

    let clean_rel_path = if let Some(p) = rel_path_opt {
        let jailed = resolve_jailed_path(root, p)?;
        let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let rel = jailed
            .strip_prefix(&canon_root)
            .unwrap_or(&jailed)
            .to_string_lossy()
            .replace('\\', "/");
        if rel.is_empty() {
            ".".to_string()
        } else {
            rel
        }
    } else {
        String::new()
    };

    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(15)
        .clamp(1, 50);

    let mut git_args: Vec<String> = Vec::new();
    match op {
        "status" => {
            git_args.push("status".to_string());
            git_args.push("-sb".to_string());
        }
        "diff" => {
            git_args.push("diff".to_string());
            if !clean_rel_path.is_empty() {
                git_args.push("--".to_string());
                git_args.push(clean_rel_path.clone());
            }
        }
        "diff_staged" => {
            git_args.push("diff".to_string());
            git_args.push("--cached".to_string());
            if !clean_rel_path.is_empty() {
                git_args.push("--".to_string());
                git_args.push(clean_rel_path.clone());
            }
        }
        "log" => {
            git_args.push("log".to_string());
            git_args.push(format!("-n{limit}"));
            git_args.push("--oneline".to_string());
            git_args.push("--decorate".to_string());
            if !clean_rel_path.is_empty() {
                git_args.push("--".to_string());
                git_args.push(clean_rel_path.clone());
            }
        }
        "branch" => {
            git_args.push("branch".to_string());
            git_args.push("-vv".to_string());
        }
        "show" => {
            let target = args
                .get("target")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("HEAD");
            if target.starts_with('-') {
                return Err("Target git show tidak valid.".to_string());
            }
            git_args.push("show".to_string());
            git_args.push("--stat".to_string());
            git_args.push(target.to_string());
        }
        "blame" => {
            if clean_rel_path.is_empty() {
                return Err("Argumen 'path' wajib diisi untuk git_ops blame.".to_string());
            }
            git_args.push("blame".to_string());
            git_args.push("-L".to_string());
            git_args.push("1,120".to_string());
            git_args.push("--".to_string());
            git_args.push(clean_rel_path.clone());
        }
        "stash_list" => {
            git_args.push("stash".to_string());
            git_args.push("list".to_string());
        }
        "add" => {
            git_args.push("add".to_string());
            git_args.push("--".to_string());
            git_args.push(if clean_rel_path.is_empty() {
                ".".to_string()
            } else {
                clean_rel_path.clone()
            });
        }
        "commit" => {
            let msg = args
                .get("message")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .ok_or("Argumen 'message' wajib diisi untuk git_ops commit.")?;
            let pre_diag = crate::hooks::run_pre_commit_hooks(state, ws).await?;
            if let Some(diag) = pre_diag {
                git_args.push("commit".to_string());
                git_args.push("-m".to_string());
                git_args.push(msg.to_string());
                let mut cmd = Command::new("git");
                cmd.args(&git_args)
                    .current_dir(root)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                #[cfg(target_os = "windows")]
                cmd.creation_flags(0x08000000);
                let out = tokio::time::timeout(Duration::from_secs(15), cmd.output())
                    .await
                    .map_err(|_| "Perintah git timeout setelah 15 detik.".to_string())?
                    .map_err(|e| format!("Gagal menjalankan git: {e}"))?;
                let exit_code = out.status.code().unwrap_or(-1);
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                let (compressed, saved_bytes) = compress_output(&stdout, &stderr, 80);
                state
                    .record_audit(
                        AuditEntry {
                            id: AppState::next_audit_id(),
                            timestamp: AppState::now_timestamp(),
                            source: source.to_string(),
                            tool_name: "git_ops".to_string(),
                            target_path: None,
                            summary: format!("git commit (exit {exit_code})"),
                            diff_text: Some(compressed.clone()),
                            can_revert: false,
                            reverted: false,
                            tokens_saved: saved_bytes / 4,
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
                return Ok(format!("GIT OP: commit (exit {exit_code})\n{compressed}\n\n{diag}"));
            }
            git_args.push("commit".to_string());
            git_args.push("-m".to_string());
            git_args.push(msg.to_string());
        }
        other => return Err(format!("Operasi git_ops tidak dikenal: {other}")),
    }

    let mut cmd = Command::new("git");
    cmd.args(&git_args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    let out = tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .map_err(|_| "Perintah git timeout setelah 15 detik.".to_string())?
        .map_err(|e| format!("Gagal menjalankan git: {e}"))?;

    let exit_code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let (compressed, saved_bytes) = compress_output(&stdout, &stderr, 80);
    let tokens_saved = saved_bytes / 4;

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "git_ops".to_string(),
                target_path: if clean_rel_path.is_empty() {
                    None
                } else {
                    Some(clean_rel_path)
                },
                summary: format!("git {op} (exit {exit_code})"),
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
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            0,
        )
        .await;

    Ok(format!("GIT OP: {op} (exit {exit_code})\n{compressed}"))
}

pub async fn exec_file_ops(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let root = &ws.canon_root;
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'action' wajib diisi (mkdir | move | delete).")?
        .trim();
    let rel_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'path' wajib diisi.")?;

    let target = resolve_jailed_path(root, rel_path)?;
    check_sensitive_guard_for_workspace(state, ws, &target, rel_path).await?;
    if target == *root {
        return Err("Tidak dapat memodifikasi workspace root secara langsung.".to_string());
    }

    let group_id = current_turn_group_id(state).await;

    match action {
        "mkdir" => {
            fs::create_dir_all(&target)
                .map_err(|e| format!("Gagal membuat direktori '{rel_path}': {e}"))?;
            state
                .record_audit(
                    AuditEntry {
                        id: AppState::next_audit_id(),
                        timestamp: AppState::now_timestamp(),
                        source: source.to_string(),
                        tool_name: "file_ops".to_string(),
                        target_path: Some(rel_path.to_string()),
                        summary: format!("mkdir {rel_path}"),
                        diff_text: None,
                        can_revert: false,
                        reverted: false,
                        tokens_saved: 0,
                        original_content: None,
                        original_blob_hash: None,
                        file_existed_before: false,
                        post_edit_hash: None,
                        group_id,
                        move_from_path: None,
                        workspace_id: Some(ws.id.clone()),
                        workspace_alias: Some(ws.alias.clone()),
                        workspace_root: Some(ws.clean_path.clone()),
                    },
                    0,
                )
                .await;
            Ok(format!("Direktori '{rel_path}' dibuat."))
        }
        "delete" => {
            if !target.exists() {
                return Err(format!("File '{rel_path}' tidak ditemukan."));
            }
            if !target.is_file() {
                return Err(format!(
                    "'{rel_path}' adalah direktori; file_ops delete hanya mengizinkan penghapusan file tunggal demi keamanan."
                ));
            }
            let original = fs::read_to_string(&target).map_err(|_| {
                format!("'{rel_path}' bukan file teks UTF-8 sehingga tidak dapat di-snapshot untuk rollback aman.")
            })?;
            let blob_hash = state.snapshot_store.save_blob(&original);
            fs::remove_file(&target)
                .map_err(|e| format!("Gagal menghapus '{rel_path}': {e}"))?;

            let summary = format!("Deleted {} ({} bytes)", rel_path, original.len());
            state
                .record_audit(
                    AuditEntry {
                        id: AppState::next_audit_id(),
                        timestamp: AppState::now_timestamp(),
                        source: source.to_string(),
                        tool_name: "file_ops".to_string(),
                        target_path: Some(rel_path.to_string()),
                        summary: summary.clone(),
                        diff_text: Some(format!("Deleted file {rel_path} ({} lines)", original.lines().count())),
                        can_revert: true,
                        reverted: false,
                        tokens_saved: 0,
                        original_content: Some(original),
                        original_blob_hash: blob_hash,
                        file_existed_before: true,
                        post_edit_hash: None,
                        group_id,
                        move_from_path: None,
                        workspace_id: Some(ws.id.clone()),
                        workspace_alias: Some(ws.alias.clone()),
                        workspace_root: Some(ws.clean_path.clone()),
                    },
                    0,
                )
                .await;
            Ok(summary)
        }
        "move" => {
            let dest_rel = args
                .get("destination")
                .and_then(|v| v.as_str())
                .ok_or("Argumen 'destination' wajib diisi untuk action 'move'.")?;
            let dest = resolve_jailed_path(root, dest_rel)?;
            check_sensitive_guard_for_workspace(state, ws, &dest, dest_rel).await?;
            if !target.exists() {
                return Err(format!("Sumber '{rel_path}' tidak ditemukan."));
            }
            if dest.exists() {
                return Err(format!("Tujuan '{dest_rel}' sudah ada."));
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Gagal membuat direktori tujuan '{dest_rel}': {e}"))?;
            }
            fs::rename(&target, &dest)
                .map_err(|e| format!("Gagal memindahkan '{rel_path}' ke '{dest_rel}': {e}"))?;

            let post_edit_hash = if dest.is_file() {
                fs::read(&dest).ok().map(|b| sha256_hex(&b))
            } else {
                None
            };
            let summary = format!("Moved {rel_path} -> {dest_rel}");
            state
                .record_audit(
                    AuditEntry {
                        id: AppState::next_audit_id(),
                        timestamp: AppState::now_timestamp(),
                        source: source.to_string(),
                        tool_name: "file_ops".to_string(),
                        target_path: Some(dest_rel.to_string()),
                        summary: summary.clone(),
                        diff_text: Some(format!("RENAME\n- {rel_path}\n+ {dest_rel}")),
                        can_revert: true,
                        reverted: false,
                        tokens_saved: 0,
                        original_content: None,
                        original_blob_hash: None,
                        file_existed_before: true,
                        post_edit_hash,
                        group_id,
                        move_from_path: Some(rel_path.to_string()),
                        workspace_id: Some(ws.id.clone()),
                        workspace_alias: Some(ws.alias.clone()),
                        workspace_root: Some(ws.clean_path.clone()),
                    },
                    0,
                )
                .await;
            Ok(summary)
        }
        other => Err(format!("Action file_ops tidak dikenal: {other}")),
    }
}

pub async fn exec_start_command_job(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'command' wajib diisi.")?;
    let label = args
        .get("label")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(300);

    let (summary, _rx) = jobs::start_job_for_source(
        state,
        &ws.canon_root,
        command,
        label,
        timeout_secs,
        Some(ws.alias.clone()),
        Some(source.to_string()),
    )
    .await?;

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "start_command_job".to_string(),
                target_path: None,
                summary: format!("Started job {}: {}", summary.job_id, command),
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

    let poll_wait_secs = state.config.read().await.effective_webmcp_poll_wait_secs();
    Ok(format!(
        "JOB_STARTED: {}\nWORKSPACE: @{}\nCOMMAND: {}\nSTATUS: running\nGunakan poll_command_job(job_id=\"{}\", from_line=0) — tool ini otomatis menunggu s.d. {} detik hingga selesai tanpa perlu perintah sleep.",
        summary.job_id, ws.alias, summary.command, summary.job_id, poll_wait_secs
    ))
}

async fn verify_job_pin_access(
    state: &AppState,
    job_id: &str,
    pinned_ws: Option<&str>,
) -> Result<Option<ResolvedWorkspace>, String> {
    let Some(pin) = pinned_ws.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let pin_ws = resolve_workspace(state, None, Some(pin)).await?;
    if let Some(job_alias) = jobs::get_job_workspace_alias(state, job_id).await {
        if !job_alias.eq_ignore_ascii_case(&pin_ws.alias) {
            return Err(format!(
                "Koneksi ini di-pin ke workspace '@{}'; job '{}' milik workspace '@{}' ditolak.",
                pin_ws.alias, job_id, job_alias
            ));
        }
    }
    Ok(Some(pin_ws))
}

pub async fn exec_poll_command_job(
    state: &AppState,
    source: &str,
    args: &Value,
    pinned_ws: Option<&str>,
) -> Result<String, String> {
    let job_id = args
        .get("job_id")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'job_id' wajib diisi.")?;
    verify_job_pin_access(state, job_id, pinned_ws).await?;
    let from_line = args.get("from_line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let max_lines = args
        .get("max_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(80) as usize;
    let default_wait = if source == "WebMCP" {
        state.config.read().await.effective_webmcp_poll_wait_secs()
    } else {
        0
    };
    let wait_secs = args
        .get("wait_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(default_wait);
    jobs::poll_job_with_wait(state, job_id, from_line, max_lines, wait_secs).await
}

pub async fn exec_stop_command_job(
    state: &AppState,
    source: &str,
    args: &Value,
    pinned_ws: Option<&str>,
) -> Result<String, String> {
    let job_id = args
        .get("job_id")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'job_id' wajib diisi.")?;
    let pin_ws_opt = verify_job_pin_access(state, job_id, pinned_ws).await?;
    let summary = jobs::stop_job(state, job_id).await?;

    let ws_meta = match pin_ws_opt {
        Some(w) => Some(w),
        None => match summary.workspace_alias.as_deref() {
            Some(alias) => resolve_workspace(state, Some(alias), None).await.ok(),
            None => None,
        },
    };

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "stop_command_job".to_string(),
                target_path: None,
                summary: format!("Stopped job {}", summary.job_id),
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
                workspace_id: ws_meta.as_ref().map(|w| w.id.clone()),
                workspace_alias: summary.workspace_alias.clone().or_else(|| ws_meta.as_ref().map(|w| w.alias.clone())),
                workspace_root: ws_meta.as_ref().map(|w| w.clean_path.clone()),
            },
            0,
        )
        .await;

    Ok(format!("Job {} dihentikan.", summary.job_id))
}
