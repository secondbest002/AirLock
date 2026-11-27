use super::guardrail::{
    check_sensitive_guard_for_workspace, check_sensitive_guard_parts, current_turn_group_id,
    protected_internal_files, sha256_hex,
};
use super::shell::run_shell_raw;
use crate::jobs;
use crate::security::{compress_output, resolve_jailed_path, validate_command_with_policy};
use crate::state::{AppState, AuditEntry};
use crate::tools_extra::extract_outline_text;
use crate::workspace::{
    build_workspace_summaries, resolve_workspace, ResolvedWorkspace,
};
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::Value;
use similar::TextDiff;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::Emitter;
use tokio::task::JoinSet;

#[derive(Debug, Deserialize)]
struct EditOp {
    search: String,
    replace: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Debug, Deserialize)]
struct BatchReadItem {
    #[serde(default)]
    workspace: Option<String>,
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct BatchSearchSpec {
    query: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    max_results: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct BatchFindSpec {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct BatchSymbolSpec {
    path: String,
    query: String,
    #[serde(default)]
    include_docs: Option<bool>,
    #[serde(default)]
    max_lines: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct MultiEditFile {
    path: String,
    edits: Value,
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

fn scan_project_overview_blocking(root_owned: PathBuf, max_depth: usize) -> Vec<String> {
    use std::collections::HashMap;
    let start = std::time::Instant::now();
    let mut out = Vec::new();
    let mut dir_file_counts: HashMap<PathBuf, usize> = HashMap::new();
    let mut dir_omitted_counts: HashMap<PathBuf, usize> = HashMap::new();
    let mut walked = 0usize;
    let mut stopped_reason: Option<&'static str> = None;

    let root_for_filter = root_owned.clone();
    let walker = WalkBuilder::new(&root_owned)
        .max_depth(Some(max_depth))
        .hidden(false)
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
            if let Ok(rel) = e.path().strip_prefix(&root_for_filter) {
                if crate::handoff::is_handoff_ignored_rel(&rel.to_string_lossy()) {
                    return false;
                }
            }
            true
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
}

async fn collect_overview_text(
    root: &Path,
    alias: &str,
    max_depth: usize,
) -> Result<(String, usize), String> {
    let root_owned = root.to_path_buf();
    let entries = tokio::task::spawn_blocking(move || scan_project_overview_blocking(root_owned, max_depth))
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
    let len = entries.len();
    let output = format!(
        "WORKSPACE: @{} ({})\nGIT STATUS:\n{}\n\nTREE ({} entries):\n{}",
        alias,
        clean_root,
        git_status,
        len,
        entries.join("\n")
    );
    Ok((output, len))
}

pub(crate) async fn exec_project_overview(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let max_depth = args
        .get("max_depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(3)
        .clamp(1, 6) as usize;

    let (output, entry_len) = collect_overview_text(&ws.canon_root, &ws.alias, max_depth).await?;

    let tokens_saved = 350;
    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "project_overview".to_string(),
                target_path: None,
                summary: format!("Listed {} project entries + git status", entry_len),
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
            2,
        )
        .await;

    Ok(output)
}

pub(crate) async fn overview_text_for_resource(root: &Path) -> Result<String, String> {
    let root_owned = root.to_path_buf();
    let entries = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let root_for_filter = root_owned.clone();
        let walker = WalkBuilder::new(&root_owned)
            .max_depth(Some(3))
            .hidden(false)
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
                if let Ok(rel) = e.path().strip_prefix(&root_for_filter) {
                    if crate::handoff::is_handoff_ignored_rel(&rel.to_string_lossy()) {
                        return false;
                    }
                }
                true
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

fn build_search_regex(query: &str, case_insensitive: bool) -> Result<Regex, String> {
    if query.trim().is_empty() {
        return Err("Query pencarian tidak boleh kosong.".to_string());
    }
    RegexBuilder::new(query)
        .case_insensitive(case_insensitive)
        .build()
        .or_else(|_| {
            RegexBuilder::new(&regex::escape(query))
                .case_insensitive(case_insensitive)
                .build()
        })
        .map_err(|e| format!("Regex tidak valid: {e}"))
}

fn run_smart_search_blocking(
    search_root: PathBuf,
    canon_root: PathBuf,
    protected_files: Vec<PathBuf>,
    allow_sensitive: bool,
    sensitive_patterns: Vec<String>,
    regex: Regex,
    max_results: usize,
) -> (Vec<String>, usize, usize, bool, usize) {
    let start = std::time::Instant::now();
    let mut matches_out = Vec::new();
    let mut files_scanned = 0usize;
    let mut total_scanned_bytes = 0usize;
    let mut was_capped = false;
    let mut sensitive_skipped = 0usize;

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
                    if crate::handoff::is_handoff_ignored_rel(&rel.to_string_lossy()) {
                        return false;
                    }
                }
            }
            true
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
        let rel = rel_path_obj.to_string_lossy().replace('\\', "/");
        if check_sensitive_guard_parts(
            &protected_files,
            &canon_root,
            allow_sensitive,
            &sensitive_patterns,
            path,
            &rel,
        )
        .is_err()
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
}

fn format_smart_search_output(
    query: &str,
    matches_out: &[String],
    files_scanned: usize,
    was_capped: bool,
    sensitive_skipped: usize,
) -> String {
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

    if matches_out.is_empty() {
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
    }
}

pub(crate) async fn exec_smart_search(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'query' wajib diisi.")?;
    let case_insensitive = args
        .get("case_insensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .clamp(1, 100) as usize;

    let search_root = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => resolve_jailed_path(&ws.canon_root, p)?,
        _ => resolve_jailed_path(&ws.canon_root, ".")?,
    };
    let regex = build_search_regex(query, case_insensitive)?;
    let protected_files = protected_internal_files(state);
    let canon_root = ws.canon_root.clone();
    let allow_sensitive = ws.effective_allow_sensitive;
    let sensitive_patterns = ws.effective_sensitive_patterns.clone();

    let (matches_out, files_scanned, total_scanned_bytes, was_capped, sensitive_skipped) =
        tokio::task::spawn_blocking(move || {
            run_smart_search_blocking(
                search_root,
                canon_root,
                protected_files,
                allow_sensitive,
                sensitive_patterns,
                regex,
                max_results,
            )
        })
        .await
        .map_err(|e| format!("Gagal memindai teks: {e}"))?;

    let output = format_smart_search_output(
        query,
        &matches_out,
        files_scanned,
        was_capped,
        sensitive_skipped,
    );

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
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            1,
        )
        .await;

    Ok(output)
}

pub(crate) async fn exec_read_file_range(
    state: &AppState,
    source: &str,
    default_ws: &ResolvedWorkspace,
    top_level_ws: Option<&str>,
    pinned_ws: Option<&str>,
    args: &Value,
) -> Result<String, String> {
    let mut items: Vec<BatchReadItem> = Vec::new();
    if let Some(batch) = args.get("batch_paths") {
        items = serde_json::from_value(batch.clone())
            .map_err(|e| format!("Format batch_paths tidak valid: {e}"))?;
        if items.is_empty() {
            return Err("Array 'batch_paths' tidak boleh kosong.".to_string());
        }
    } else if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        items.push(BatchReadItem {
            workspace: None,
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
    let has_item_ws_override = items
        .iter()
        .any(|i| i.workspace.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false));

    for item in &items {
        let item_ws = match item.workspace.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(ws_req) => {
                resolve_workspace(state, Some(ws_req), pinned_ws).await?
            }
            None => {
                if let Some(top_ws) = top_level_ws {
                    resolve_workspace(state, Some(top_ws), pinned_ws).await?
                } else {
                    default_ws.clone()
                }
            }
        };

        let target = resolve_jailed_path(&item_ws.canon_root, &item.path)?;
        check_sensitive_guard_for_workspace(state, &item_ws, &target, &item.path).await?;
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

        let label = if has_item_ws_override {
            format!("@{}:{}", item_ws.alias, item.path)
        } else {
            item.path.clone()
        };
        let mut slice_lines = Vec::new();
        slice_lines.push(format!(
            "FILE: {} (lines {}-{} of {})",
            label, start, end, total_lines
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
                workspace_id: Some(default_ws.id.clone()),
                workspace_alias: Some(default_ws.alias.clone()),
                workspace_root: Some(default_ws.clean_path.clone()),
            },
            turns_saved,
        )
        .await;

    Ok(output)
}

struct PreparedEdit {
    rel_path: String,
    target: PathBuf,
    original: String,
    final_content: String,
    diff_text: String,
    summary: String,
    tokens_saved: usize,
    post_edit_hash: String,
}

fn prepare_surgical_edits(
    rel_path: &str,
    target: PathBuf,
    edits_val: &Value,
) -> Result<PreparedEdit, String> {
    let original = fs::read_to_string(&target)
        .map_err(|e| format!("Gagal membaca file target '{rel_path}': {e}"))?;

    let edits: Vec<EditOp> = serde_json::from_value(edits_val.clone())
        .map_err(|e| format!("Format 'edits' pada '{rel_path}' tidak valid: {e}"))?;
    if edits.is_empty() {
        return Err(format!("Array 'edits' pada '{rel_path}' tidak boleh kosong."));
    }

    let uses_crlf = original.contains("\r\n");
    let original_lf = original.replace("\r\n", "\n");
    let mut updated_lf = original_lf.clone();

    for (idx, op) in edits.iter().enumerate() {
        let search_lf = op.search.replace("\r\n", "\n");
        let replace_lf = op.replace.replace("\r\n", "\n");
        if search_lf.is_empty() {
            return Err(format!(
                "'{rel_path}' Edit #{idx}: field 'search' tidak boleh kosong."
            ));
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

    Ok(PreparedEdit {
        rel_path: rel_path.to_string(),
        target,
        original,
        final_content,
        diff_text,
        summary: format!("Applied {} surgical edit(s)", edits.len()),
        tokens_saved,
        post_edit_hash,
    })
}

fn apply_prepared_edit(prep: &PreparedEdit) -> Result<(), String> {
    fs::write(&prep.target, &prep.final_content)
        .map_err(|e| format!("Gagal menulis hasil edit ke '{}': {e}", prep.rel_path))
}

async fn append_edit_post_checks(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
    edited_paths: &[String],
    text: &mut String,
) {
    if let Some(diag) = crate::hooks::run_post_edit_hooks(state, ws, edited_paths).await {
        text.push_str("\n\n");
        text.push_str(&diag);
    }

    let explicit_lsp = args.get("post_check_lsp").and_then(|v| v.as_bool());
    let run_lsp = match explicit_lsp {
        Some(v) => v,
        None => source == "WebMCP" && state.config.read().await.webmcp_auto_verify_on_edit,
    };
    if run_lsp {
        if let Some(lsp_note) = crate::lsp_bridge::quick_diagnostics_for_files(
            state,
            ws,
            edited_paths,
            Duration::from_millis(2500),
        )
        .await
        {
            text.push_str("\n\n");
            text.push_str(&lsp_note);
        }
    }
}

pub(crate) async fn exec_surgical_edit(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let rel_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'path' wajib diisi.")?;
    let edits_val = args.get("edits").ok_or("Argumen 'edits' wajib diisi.")?;

    let target = resolve_jailed_path(&ws.canon_root, rel_path)?;
    check_sensitive_guard_for_workspace(state, ws, &target, rel_path).await?;

    let prep = prepare_surgical_edits(rel_path, target, edits_val)?;
    apply_prepared_edit(&prep)?;
    let blob_hash = state.snapshot_store.save_blob(&prep.original);
    let group_id = current_turn_group_id(state).await;

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "surgical_edit".to_string(),
                target_path: Some(rel_path.to_string()),
                summary: prep.summary.clone(),
                diff_text: Some(prep.diff_text.clone()),
                can_revert: true,
                reverted: false,
                tokens_saved: prep.tokens_saved,
                original_content: Some(prep.original),
                original_blob_hash: blob_hash,
                file_existed_before: true,
                post_edit_hash: Some(prep.post_edit_hash),
                group_id,
                move_from_path: None,
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            0,
        )
        .await;

    let mut text = format!("{} on {rel_path}\n\n{}", prep.summary, prep.diff_text);
    append_edit_post_checks(state, source, ws, args, &[rel_path.to_string()], &mut text).await;
    Ok(text)
}

async fn acquire_ws_write_lock(
    state: &AppState,
    ws: &ResolvedWorkspace,
) -> tokio::sync::OwnedMutexGuard<()> {
    state
        .agent_manager
        .workspace_write_lock(&ws.id)
        .await
        .lock_owned()
        .await
}

pub(crate) async fn exec_patch_and_verify(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
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

    let target = resolve_jailed_path(&ws.canon_root, rel_path)?;
    check_sensitive_guard_for_workspace(state, ws, &target, rel_path).await?;
    let (cmd_policy, cmd_allowlist) = state.command_policy_and_allowlist().await;
    validate_command_with_policy(verify_cmd, &cmd_policy, &cmd_allowlist)?;

    let write_guard = acquire_ws_write_lock(state, ws).await;
    let prep = prepare_surgical_edits(rel_path, target, edits_val)?;
    apply_prepared_edit(&prep)?;

    // CRITICAL: Record rollback snapshot IMMEDIATELY after write and BEFORE verification starts!
    let blob_hash = state.snapshot_store.save_blob(&prep.original);
    let group_id = current_turn_group_id(state).await;
    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "patch_and_verify".to_string(),
                target_path: Some(rel_path.to_string()),
                summary: format!("{} + verify '{}'", prep.summary, verify_cmd),
                diff_text: Some(prep.diff_text.clone()),
                can_revert: true,
                reverted: false,
                tokens_saved: prep.tokens_saved,
                original_content: Some(prep.original),
                original_blob_hash: blob_hash,
                file_existed_before: true,
                post_edit_hash: Some(prep.post_edit_hash),
                group_id,
                move_from_path: None,
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            2,
        )
        .await;
    drop(write_guard);

    let (default_promote_secs, poll_wait_secs) = {
        let cfg = state.config.read().await;
        (
            cfg.effective_webmcp_auto_promote_secs(),
            cfg.effective_webmcp_poll_wait_secs(),
        )
    };
    let explicit_promote = args
        .get("auto_promote_secs")
        .and_then(|v| v.as_u64())
        .map(|s| s.clamp(1, 25));
    let promote_secs =
        explicit_promote.or_else(|| if source == "WebMCP" { Some(default_promote_secs) } else { None });

    if let Some(p_secs) = promote_secs {
        let (summary, mut done_rx) = jobs::start_job_for_source(
            state,
            &ws.canon_root,
            verify_cmd,
            Some(format!("Verify: {rel_path}")),
            300,
            Some(ws.alias.clone()),
            Some(source.to_string()),
        )
        .await?;

        let finished_in_time =
            tokio::time::timeout(Duration::from_secs(p_secs), done_rx.wait_for(|&d| d))
                .await
                .is_ok();

        if finished_in_time {
            let (code, compressed) = if let Some((_, exit_opt, _, out, _)) =
                jobs::get_job_snapshot_output(state, &summary.job_id, 60).await
            {
                (exit_opt.unwrap_or(0), out)
            } else {
                (0, String::new())
            };
            return Ok(format!(
                "PATCH DIFF ({rel_path}):\n{}\nVERIFY COMMAND: {verify_cmd}\nEXIT CODE: {code}\nOUTPUT:\n{compressed}",
                prep.diff_text
            ));
        }

        let (elapsed, recent, total_lines) = if let Some((_, _, el, out, tot)) =
            jobs::get_job_snapshot_output(state, &summary.job_id, 20).await
        {
            (el, out, tot)
        } else {
            (p_secs, String::new(), 0)
        };

        return Ok(format!(
            "PATCH DIFF ({rel_path}):\n{}\nAUTO_PROMOTED_VERIFY_JOB\nJOB_ID: {}\nWORKSPACE: @{}\nSTATUS: running\nELAPSED: {}s\nNEXT_FROM_LINE: {}\nHINT: Patch was applied and recorded for rollback. Verification exceeded {}s sync window and continues in background; call poll_command_job(job_id=\"{}\", from_line={}) (it automatically long-polls up to {}s; do NOT run sleep).\nRECENT OUTPUT:\n{}",
            prep.diff_text,
            summary.job_id,
            ws.alias,
            elapsed,
            total_lines,
            p_secs,
            summary.job_id,
            total_lines,
            poll_wait_secs,
            if recent.is_empty() {
                "(waiting for output...)"
            } else {
                recent.as_str()
            }
        ));
    }

    match run_shell_raw(&ws.canon_root, verify_cmd, 20).await {
        Ok((exit_code, stdout, stderr)) => {
            let (compressed, _) = compress_output(&stdout, &stderr, 60);
            Ok(format!(
                "PATCH DIFF ({rel_path}):\n{}\nVERIFY COMMAND: {verify_cmd}\nEXIT CODE: {exit_code}\nOUTPUT:\n{compressed}",
                prep.diff_text
            ))
        }
        Err(err) => Ok(format!(
            "PATCH DIFF ({rel_path}):\n{}\nVERIFY COMMAND: {verify_cmd}\nVERIFY ERROR: {err}\n(Rollback snapshot for '{rel_path}' is safely recorded in audit log.)",
            prep.diff_text
        )),
    }
}

pub(crate) async fn exec_write_file(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
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

    let target = resolve_jailed_path(&ws.canon_root, rel_path)?;
    check_sensitive_guard_for_workspace(state, ws, &target, rel_path).await?;

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
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            0,
        )
        .await;

    let mut text = format!("Wrote {rel_path} ({} bytes)\n\n{diff_text}", content.len());
    append_edit_post_checks(state, source, ws, args, &[rel_path.to_string()], &mut text).await;
    Ok(text)
}

pub(crate) async fn exec_list_workspaces(
    state: &AppState,
    source: &str,
    pinned_ws: Option<&str>,
) -> Result<String, String> {
    let output = if let Some(pin) = pinned_ws.map(str::trim).filter(|s| !s.is_empty()) {
        let resolved = resolve_workspace(state, None, Some(pin)).await?;
        format!(
            "CONNECTION PINNED: @{}\nMounted workspace accessible in this session (1):\n- @{} (default, guardrail={}) -> {}",
            resolved.alias, resolved.alias, resolved.effective_guardrail, resolved.clean_path
        )
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
        let summaries = build_workspace_summaries(&raw, &def_id, &global_guard);
        let enabled: Vec<_> = summaries.into_iter().filter(|w| w.enabled).collect();
        if enabled.is_empty() {
            let root = state.workspace_root.read().await.clone();
            format!("ACTIVE WORKSPACE (default): {}", crate::workspace::clean_path_string(&root))
        } else {
            let lines: Vec<String> = enabled
                .iter()
                .map(|w| {
                    let def_tag = if w.is_default { "default, " } else { "" };
                    let missing = if w.exists { "" } else { " [MISSING]" };
                    format!(
                        "- @{} ({def_tag}guardrail={}, id={}){} -> {}",
                        w.alias, w.effective_guardrail, w.id, missing, w.path
                    )
                })
                .collect();
            format!(
                "Mounted enabled workspaces ({}):\n{}\n\nPass workspace: \"<alias>\" to any workspace tool to operate across repos.",
                enabled.len(),
                lines.join("\n")
            )
        }
    };

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "list_workspaces".to_string(),
                target_path: None,
                summary: "Listed mounted workspaces".to_string(),
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
                workspace_id: None,
                workspace_alias: None,
                workspace_root: None,
            },
            0,
        )
        .await;

    Ok(output)
}

pub(crate) async fn exec_batch_inspect(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    top_level_ws: Option<&str>,
    pinned_ws: Option<&str>,
    args: &Value,
) -> Result<String, String> {
    let include_instructions = args
        .get("include_instructions")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let include_overview = args
        .get("include_overview")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let overview_depth = args
        .get("overview_depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(2)
        .clamp(1, 4) as usize;

    let find_patterns: Vec<BatchFindSpec> = match args.get("find_patterns") {
        Some(v) if !v.is_null() => serde_json::from_value::<Vec<BatchFindSpec>>(v.clone())
            .map_err(|e| format!("Format 'find_patterns' tidak valid: {e}"))?
            .into_iter()
            .take(3)
            .collect(),
        _ => Vec::new(),
    };

    let outline_paths: Vec<String> = match args.get("outline_paths") {
        Some(v) if !v.is_null() => serde_json::from_value::<Vec<String>>(v.clone())
            .map_err(|e| format!("Format 'outline_paths' tidak valid: {e}"))?
            .into_iter()
            .take(8)
            .collect(),
        _ => Vec::new(),
    };

    let searches: Vec<BatchSearchSpec> = match args.get("searches") {
        Some(v) if !v.is_null() => serde_json::from_value::<Vec<BatchSearchSpec>>(v.clone())
            .map_err(|e| format!("Format 'searches' tidak valid: {e}"))?
            .into_iter()
            .take(4)
            .collect(),
        _ => Vec::new(),
    };

    let read_slices: Vec<BatchReadItem> = match args.get("read_slices") {
        Some(v) if !v.is_null() => serde_json::from_value::<Vec<BatchReadItem>>(v.clone())
            .map_err(|e| format!("Format 'read_slices' tidak valid: {e}"))?
            .into_iter()
            .take(6)
            .collect(),
        _ => Vec::new(),
    };

    let symbol_reads: Vec<BatchSymbolSpec> = match args.get("symbol_reads") {
        Some(v) if !v.is_null() => serde_json::from_value::<Vec<BatchSymbolSpec>>(v.clone())
            .map_err(|e| format!("Format 'symbol_reads' tidak valid: {e}"))?
            .into_iter()
            .take(6)
            .collect(),
        _ => Vec::new(),
    };

    if !include_instructions
        && !include_overview
        && find_patterns.is_empty()
        && outline_paths.is_empty()
        && searches.is_empty()
        && read_slices.is_empty()
        && symbol_reads.is_empty()
    {
        return Err(
            "batch_inspect memerlukan minimal satu dari: include_instructions=true, include_overview=true, find_patterns, outline_paths, searches, read_slices, atau symbol_reads."
                .to_string(),
        );
    }

    // Resolve slice workspaces upfront so pinned_ws isolation is enforced immediately.
    let mut resolved_slices: Vec<(ResolvedWorkspace, BatchReadItem)> =
        Vec::with_capacity(read_slices.len());
    let has_slice_ws_override = read_slices
        .iter()
        .any(|i| i.workspace.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false));
    for item in read_slices {
        let item_ws = match item.workspace.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(ws_req) => resolve_workspace(state, Some(ws_req), pinned_ws).await?,
            None => match top_level_ws {
                Some(top_ws) => resolve_workspace(state, Some(top_ws), pinned_ws).await?,
                None => ws.clone(),
            },
        };
        resolved_slices.push((item_ws, item));
    }

    let protected_files = protected_internal_files(state);
    let is_webmcp = source == "WebMCP";

    let overview_fut = async {
        if include_overview {
            Some(
                collect_overview_text(&ws.canon_root, &ws.alias, overview_depth)
                    .await
                    .map(|(txt, _)| txt),
            )
        } else {
            None
        }
    };

    let finds_fut = async {
        let mut set: JoinSet<(usize, String)> = JoinSet::new();
        for (idx, spec) in find_patterns.into_iter().enumerate() {
            let ws_clone = ws.clone();
            let prot = protected_files.clone();
            let pat_raw = spec.pattern;
            let pat = pat_raw.trim().to_string();
            let sub = spec.path;
            let max_r = spec.max_results.unwrap_or(40).clamp(1, 100) as usize;
            set.spawn_blocking(move || {
                let res = (|| -> Result<String, String> {
                    if pat.is_empty() {
                        return Err("Parameter 'pattern' tidak boleh kosong.".to_string());
                    }
                    let search_root = match sub.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        Some(p) => resolve_jailed_path(&ws_clone.canon_root, p)?,
                        None => resolve_jailed_path(&ws_clone.canon_root, ".")?,
                    };
                    let (found, walked, skipped, capped) = crate::tools_extra::run_find_files_blocking(
                        search_root,
                        ws_clone.canon_root.clone(),
                        prot,
                        ws_clone.effective_allow_sensitive,
                        ws_clone.effective_sensitive_patterns.clone(),
                        pat.clone(),
                        max_r,
                    );
                    Ok(format!(
                        "[Find: '{}']\n{}",
                        pat,
                        crate::tools_extra::format_find_files_output(
                            &pat, &found, walked, skipped, capped
                        )
                    ))
                })();
                match res {
                    Ok(s) => (idx, s),
                    Err(e) => (idx, format!("FIND ERROR ('{pat_raw}'): {e}")),
                }
            });
        }
        let mut results = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(item) = joined {
                results.push(item);
            }
        }
        results.sort_by_key(|(idx, _)| *idx);
        results.into_iter().map(|(_, s)| s).collect::<Vec<_>>()
    };

    let outlines_fut = async {
        let mut set: JoinSet<(usize, String)> = JoinSet::new();
        for (idx, rel_path) in outline_paths.iter().enumerate() {
            let ws_clone = ws.clone();
            let prot = protected_files.clone();
            let rel_owned = rel_path.clone();
            set.spawn_blocking(move || {
                let res = (|| -> Result<String, String> {
                    let target = resolve_jailed_path(&ws_clone.canon_root, &rel_owned)?;
                    check_sensitive_guard_parts(
                        &prot,
                        &ws_clone.canon_root,
                        ws_clone.effective_allow_sensitive,
                        &ws_clone.effective_sensitive_patterns,
                        &target,
                        &rel_owned,
                    )?;
                    let content = fs::read_to_string(&target)
                        .map_err(|e| format!("Gagal membaca '{}': {e}", rel_owned))?;
                    let ext = target
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    Ok(extract_outline_text(&rel_owned, &ext, &content, 120).0)
                })();
                match res {
                    Ok(s) => (idx, s),
                    Err(e) => (idx, format!("OUTLINE ERROR ({rel_owned}): {e}")),
                }
            });
        }
        let mut results = Vec::with_capacity(outline_paths.len());
        while let Some(joined) = set.join_next().await {
            if let Ok(item) = joined {
                results.push(item);
            }
        }
        results.sort_by_key(|(idx, _)| *idx);
        results.into_iter().map(|(_, s)| s).collect::<Vec<_>>()
    };

    let searches_fut = async {
        let mut set: JoinSet<(usize, String)> = JoinSet::new();
        for (idx, spec) in searches.iter().enumerate() {
            let ws_clone = ws.clone();
            let prot = protected_files.clone();
            let q = spec.query.clone();
            let sub = spec.path.clone();
            let ci = spec.case_insensitive;
            let max_r = spec.max_results.unwrap_or(20).clamp(1, 50) as usize;
            set.spawn_blocking(move || {
                let res = (|| -> Result<String, String> {
                    let search_root = match sub.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        Some(p) => resolve_jailed_path(&ws_clone.canon_root, p)?,
                        None => resolve_jailed_path(&ws_clone.canon_root, ".")?,
                    };
                    let re = build_search_regex(&q, ci)?;
                    let (matches, scanned, _, capped, skipped) = run_smart_search_blocking(
                        search_root,
                        ws_clone.canon_root.clone(),
                        prot,
                        ws_clone.effective_allow_sensitive,
                        ws_clone.effective_sensitive_patterns.clone(),
                        re,
                        max_r,
                    );
                    Ok(format!(
                        "[Search: '{}']\n{}",
                        q,
                        format_smart_search_output(&q, &matches, scanned, capped, skipped)
                    ))
                })();
                match res {
                    Ok(s) => (idx, s),
                    Err(e) => (idx, format!("SEARCH ERROR ('{q}'): {e}")),
                }
            });
        }
        let mut results = Vec::with_capacity(searches.len());
        while let Some(joined) = set.join_next().await {
            if let Ok(item) = joined {
                results.push(item);
            }
        }
        results.sort_by_key(|(idx, _)| *idx);
        results.into_iter().map(|(_, s)| s).collect::<Vec<_>>()
    };

    let slices_fut = async {
        let mut set: JoinSet<(usize, String)> = JoinSet::new();
        for (idx, (item_ws, item)) in resolved_slices.into_iter().enumerate() {
            let prot = protected_files.clone();
            set.spawn_blocking(move || {
                let path_err_label = item.path.clone();
                let res = (|| -> Result<String, String> {
                    let target = resolve_jailed_path(&item_ws.canon_root, &item.path)?;
                    check_sensitive_guard_parts(
                        &prot,
                        &item_ws.canon_root,
                        item_ws.effective_allow_sensitive,
                        &item_ws.effective_sensitive_patterns,
                        &target,
                        &item.path,
                    )?;
                    let content = fs::read_to_string(&target)
                        .map_err(|e| format!("Gagal membaca '{}': {e}", item.path))?;
                    let lines: Vec<&str> = content.lines().collect();
                    let total_lines = lines.len();
                    let start = item.start_line.unwrap_or(1).max(1);
                    let raw_end = item.end_line.unwrap_or(start + 119);
                    let capped_end = if is_webmcp {
                        raw_end.min(start + 159)
                    } else {
                        raw_end
                    };
                    let end = capped_end
                        .min(total_lines)
                        .max(start.saturating_sub(1));
                    let label = if has_slice_ws_override {
                        format!("@{}:{}", item_ws.alias, item.path)
                    } else {
                        item.path.clone()
                    };
                    let mut out = Vec::new();
                    out.push(format!(
                        "FILE: {} (lines {}-{} of {})",
                        label, start, end, total_lines
                    ));
                    if start <= total_lines {
                        for lidx in (start - 1)..end {
                            out.push(format!("{:>4} | {}", lidx + 1, lines[lidx]));
                        }
                    }
                    if is_webmcp && raw_end > start + 159 && start + 159 < total_lines {
                        out.push(format!(
                            "...[WebMCP slice capped at 160 lines; call start_line={} for continuation]",
                            end + 1
                        ));
                    }
                    Ok(out.join("\n"))
                })();
                match res {
                    Ok(s) => (idx, s),
                    Err(e) => (idx, format!("SLICE ERROR ({path_err_label}): {e}")),
                }
            });
        }
        let mut results = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(item) = joined {
                results.push(item);
            }
        }
        results.sort_by_key(|(idx, _)| *idx);
        results.into_iter().map(|(_, s)| s).collect::<Vec<_>>()
    };

    let symbols_fut = async {
        let mut set: JoinSet<(usize, String)> = JoinSet::new();
        for (idx, spec) in symbol_reads.iter().enumerate() {
            let ws_clone = ws.clone();
            let prot = protected_files.clone();
            let rel_owned = spec.path.clone();
            let query = spec.query.clone();
            let include_docs = spec.include_docs.unwrap_or(true);
            let max_lines = spec.max_lines.unwrap_or(160).clamp(5, 300) as usize;
            set.spawn_blocking(move || {
                let res = (|| -> Result<String, String> {
                    let target = resolve_jailed_path(&ws_clone.canon_root, &rel_owned)?;
                    check_sensitive_guard_parts(
                        &prot,
                        &ws_clone.canon_root,
                        ws_clone.effective_allow_sensitive,
                        &ws_clone.effective_sensitive_patterns,
                        &target,
                        &rel_owned,
                    )?;
                    let content = fs::read_to_string(&target)
                        .map_err(|e| format!("Gagal membaca '{}': {e}", rel_owned))?;
                    let ext = target
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    crate::semantic::read_symbol_from_content(
                        &rel_owned,
                        &ext,
                        &content,
                        &query,
                        include_docs,
                        max_lines,
                    )
                })();
                match res {
                    Ok(s) => (idx, s),
                    Err(e) => (idx, format!("SYMBOL ERROR ({rel_owned}::{query}): {e}")),
                }
            });
        }
        let mut results = Vec::with_capacity(symbol_reads.len());
        while let Some(joined) = set.join_next().await {
            if let Ok(item) = joined {
                results.push(item);
            }
        }
        results.sort_by_key(|(idx, _)| *idx);
        results.into_iter().map(|(_, s)| s).collect::<Vec<_>>()
    };

    let webmcp_max_chars = if is_webmcp {
        Some(state.config.read().await.effective_webmcp_max_output_chars())
    } else {
        None
    };

    let (overview_res, find_results, outline_results, search_results, slice_results, symbol_results) =
        tokio::join!(
            overview_fut,
            finds_fut,
            outlines_fut,
            searches_fut,
            slices_fut,
            symbols_fut
        );

    let mut sections = Vec::new();
    if include_instructions {
        let instr_txt = crate::providers::load_combined_rules_snippet(
            state.data_dir.as_deref(),
            &ws.canon_root,
        )
        .unwrap_or_else(|| {
            format!(
                "Belum ada file rules global atau workspace (`GEMINI.md`) untuk @{}.",
                ws.alias
            )
        });
        sections.push(format!("=== INSTRUCTIONS (@{}) ===\n{}", ws.alias, instr_txt));
    }
    if let Some(ov) = overview_res {
        match ov {
            Ok(txt) => sections.push(format!("=== OVERVIEW (@{}) ===\n{}", ws.alias, txt)),
            Err(e) => sections.push(format!("=== OVERVIEW ERROR ===\n{e}")),
        }
    }
    if !find_results.is_empty() {
        sections.push(format!("=== FILES ===\n{}", find_results.join("\n\n")));
    }
    if !outline_results.is_empty() {
        sections.push(format!("=== OUTLINES ===\n{}", outline_results.join("\n\n")));
    }
    if !search_results.is_empty() {
        sections.push(format!("=== SEARCHES ===\n{}", search_results.join("\n\n")));
    }
    if !slice_results.is_empty() {
        sections.push(format!("=== FILE SLICES ===\n{}", slice_results.join("\n\n")));
    }
    if !symbol_results.is_empty() {
        sections.push(format!("=== SYMBOLS ===\n{}", symbol_results.join("\n\n")));
    }

    let max_budget = webmcp_max_chars.unwrap_or(16_000);
    let final_output = format_fair_batch_sections(sections, max_budget);

    let subtask_count = (if include_instructions { 1 } else { 0 })
        + (if include_overview { 1 } else { 0 })
        + find_results.len()
        + outline_results.len()
        + search_results.len()
        + slice_results.len()
        + symbol_results.len();
    let turns_saved = subtask_count.saturating_sub(1);

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "batch_inspect".to_string(),
                target_path: None,
                summary: format!(
                    "Batch inspect on @{}: instructions={}, overview={}, {} find(s), {} outline(s), {} search(es), {} slice(s), {} symbol(s)",
                    ws.alias,
                    include_instructions,
                    include_overview,
                    find_results.len(),
                    outline_results.len(),
                    search_results.len(),
                    slice_results.len(),
                    symbol_results.len()
                ),
                diff_text: None,
                can_revert: false,
                reverted: false,
                tokens_saved: 400 + (turns_saved * 150),
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
            turns_saved,
        )
        .await;

    Ok(final_output)
}

fn format_fair_batch_sections(sections: Vec<String>, max_total_chars: usize) -> String {
    if sections.is_empty() || max_total_chars == 0 {
        return String::new();
    }
    let joined = sections.join("\n\n");
    if joined.chars().count() <= max_total_chars {
        return joined;
    }
    let n = sections.len();
    let sep_budget = n.saturating_sub(1) * 2;
    let avail = max_total_chars.saturating_sub(sep_budget);
    let mut budgets: Vec<usize> = sections.iter().map(|s| s.chars().count()).collect();
    let fair = (avail / n).max(80);

    let mut fixed_sum = 0usize;
    let mut long_indices = Vec::new();
    for (idx, &len) in budgets.iter().enumerate() {
        if len <= fair {
            fixed_sum += len;
        } else {
            long_indices.push(idx);
        }
    }
    if !long_indices.is_empty() {
        let rem = avail.saturating_sub(fixed_sum);
        let each = (rem / long_indices.len()).max(80);
        for idx in long_indices {
            budgets[idx] = budgets[idx].min(each);
        }
    }

    let clipped_sections: Vec<String> = sections
        .into_iter()
        .enumerate()
        .map(|(idx, s)| {
            let limit = budgets[idx];
            let s_chars = s.chars().count();
            if s_chars <= limit {
                s
            } else {
                let marker = format!("\n...[section clipped at {limit} chars]\n");
                let marker_len = marker.chars().count();
                if limit > marker_len + 40 {
                    let content_budget = limit - marker_len;
                    let head_len = (content_budget * 55) / 100;
                    let tail_len = content_budget - head_len;
                    let head: String = s.chars().take(head_len).collect();
                    let tail: String = s.chars().skip(s_chars.saturating_sub(tail_len)).collect();
                    format!("{head}{marker}{tail}")
                } else {
                    s.chars().take(limit).collect()
                }
            }
        })
        .collect();
    let out = clipped_sections.join("\n\n");
    if out.chars().count() > max_total_chars {
        out.chars().take(max_total_chars).collect()
    } else {
        out
    }
}

fn parse_optional_verify_commands(args: &Value) -> Result<Vec<String>, String> {
    let single = args
        .get("verify_command")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let multi = match args.get("verify_commands") {
        Some(v) if !v.is_null() => {
            let list: Vec<String> = serde_json::from_value(v.clone())
                .map_err(|e| format!("Format 'verify_commands' tidak valid: {e}"))?;
            let cleaned: Vec<String> = list
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if cleaned.is_empty() {
                return Err("Array 'verify_commands' tidak boleh kosong.".to_string());
            }
            if cleaned.len() > 5 {
                return Err(format!(
                    "Maksimal 5 verify_commands per panggilan (diterima {}).",
                    cleaned.len()
                ));
            }
            Some(cleaned)
        }
        _ => None,
    };

    match (single, multi) {
        (Some(_), Some(_)) => {
            Err("Isi salah satu: 'verify_command' ATAU 'verify_commands', bukan keduanya.".to_string())
        }
        (Some(c), None) => Ok(vec![c]),
        (None, Some(cs)) => Ok(cs),
        (None, None) => Ok(Vec::new()),
    }
}

pub(crate) async fn exec_multi_surgical_edit(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let files_val = args.get("files").ok_or("Argumen 'files' wajib diisi.")?;
    let files: Vec<MultiEditFile> = serde_json::from_value(files_val.clone())
        .map_err(|e| format!("Format 'files' tidak valid: {e}"))?;
    if files.is_empty() {
        return Err("Array 'files' tidak boleh kosong.".to_string());
    }
    if files.len() > 8 {
        return Err("multi_surgical_edit dibatasi maksimal 8 file per eksekusi.".to_string());
    }

    let verify_cmds = parse_optional_verify_commands(args)?;

    // PHASE 1: Pre-Flight path, sensitive-guard, duplicate, policy & HITL checks (ZERO locks or disk writes)
    let mut jailed_targets: Vec<(&MultiEditFile, PathBuf)> = Vec::with_capacity(files.len());
    let mut seen_targets: HashSet<PathBuf> = HashSet::new();

    for item in &files {
        let target = resolve_jailed_path(&ws.canon_root, &item.path)?;
        check_sensitive_guard_for_workspace(state, ws, &target, &item.path).await?;
        if !seen_targets.insert(target.clone()) {
            return Err(format!(
                "File duplikat '{}' dalam satu panggilan multi_surgical_edit; gabungkan edit file tersebut dalam array edits[].",
                item.path
            ));
        }
        jailed_targets.push((item, target));
    }

    if !verify_cmds.is_empty() {
        let (cmd_policy, cmd_allowlist) = state.command_policy_and_allowlist().await;
        for cmd in &verify_cmds {
            validate_command_with_policy(cmd, &cmd_policy, &cmd_allowlist)?;
        }
        let gate_args = if verify_cmds.len() == 1 {
            serde_json::json!({ "command": verify_cmds[0] })
        } else {
            serde_json::json!({ "commands": verify_cmds })
        };
        super::guardrail::enforce_guardrail_gate_for_workspace(
            state,
            source,
            "run_command_compressed",
            &gate_args,
            false,
            Some(ws),
        )
        .await?;
    }

    // Hold write lock only across prepare -> atomic writes -> snapshots -> post-checks,
    // dropping it before verify_cmds wait so concurrent sessions are not starved.
    let write_guard = acquire_ws_write_lock(state, ws).await;
    let mut prepared_list: Vec<PreparedEdit> = Vec::with_capacity(jailed_targets.len());
    for (item, target) in jailed_targets {
        let prep = prepare_surgical_edits(&item.path, target, &item.edits)?;
        prepared_list.push(prep);
    }

    // PHASE 2: Write all validated files + record under one shared group_id
    let group_id = match current_turn_group_id(state).await {
        Some(gid) => gid,
        None => format!("multi-{}", &AppState::generate_token()[..8]),
    };

    let mut diffs = Vec::with_capacity(prepared_list.len());
    let mut edited_paths = Vec::with_capacity(prepared_list.len());
    let files_count = prepared_list.len();

    for prep in prepared_list {
        apply_prepared_edit(&prep)?;
        let blob_hash = state.snapshot_store.save_blob(&prep.original);
        diffs.push(prep.diff_text.clone());
        edited_paths.push(prep.rel_path.clone());
        state
            .record_audit(
                AuditEntry {
                    id: AppState::next_audit_id(),
                    timestamp: AppState::now_timestamp(),
                    source: source.to_string(),
                    tool_name: "multi_surgical_edit".to_string(),
                    target_path: Some(prep.rel_path),
                    summary: format!("{} [group: {}]", prep.summary, group_id),
                    diff_text: Some(prep.diff_text),
                    can_revert: true,
                    reverted: false,
                    tokens_saved: prep.tokens_saved,
                    original_content: Some(prep.original),
                    original_blob_hash: blob_hash,
                    file_existed_before: true,
                    post_edit_hash: Some(prep.post_edit_hash),
                    group_id: Some(group_id.clone()),
                    move_from_path: None,
                    workspace_id: Some(ws.id.clone()),
                    workspace_alias: Some(ws.alias.clone()),
                    workspace_root: Some(ws.clean_path.clone()),
                },
                if diffs.len() == 1 {
                    files_count.saturating_sub(1)
                } else {
                    0
                },
            )
            .await;
    }

    let mut text = format!(
        "Applied atomic multi_surgical_edit across {files_count} file(s) in @{}\nGROUP_ID: {group_id}\n\n{}",
        ws.alias,
        diffs.join("\n")
    );
    append_edit_post_checks(state, source, ws, args, &edited_paths, &mut text).await;
    drop(write_guard);

    // PHASE 4: Optional verification command / pipeline with auto-promote
    if !verify_cmds.is_empty() {
        let (default_promote_secs, poll_wait_secs) = {
            let cfg = state.config.read().await;
            (
                cfg.effective_webmcp_auto_promote_secs(),
                cfg.effective_webmcp_poll_wait_secs(),
            )
        };
        let explicit_promote = args
            .get("auto_promote_secs")
            .and_then(|v| v.as_u64())
            .map(|s| s.clamp(1, 25));
        let promote_secs = explicit_promote.or_else(|| {
            if source == "WebMCP" || verify_cmds.len() > 1 {
                Some(default_promote_secs)
            } else {
                None
            }
        });

        let verify_display = verify_cmds.join(" && ");
        if let Some(p_secs) = promote_secs {
            let (summary, mut done_rx) = jobs::start_pipeline_job_for_source(
                state,
                &ws.canon_root,
                &verify_cmds,
                Some(format!("Verify multi-edit ({files_count} files)")),
                300,
                Some(ws.alias.clone()),
                Some(source.to_string()),
            )
            .await?;

            let finished_in_time =
                tokio::time::timeout(Duration::from_secs(p_secs), done_rx.wait_for(|&d| d))
                    .await
                    .is_ok();

            if finished_in_time {
                let (code, compressed) = if let Some((_, exit_opt, _, out, _)) =
                    jobs::get_job_snapshot_output(state, &summary.job_id, 60).await
                {
                    (exit_opt.unwrap_or(0), out)
                } else {
                    (0, String::new())
                };
                text.push_str(&format!(
                    "\n\nVERIFY COMMAND: {verify_display}\nEXIT CODE: {code}\nOUTPUT:\n{compressed}"
                ));
            } else {
                let (elapsed, recent, total_lines) = if let Some((_, _, el, out, tot)) =
                    jobs::get_job_snapshot_output(state, &summary.job_id, 20).await
                {
                    (el, out, tot)
                } else {
                    (p_secs, String::new(), 0)
                };
                text.push_str(&format!(
                    "\n\nAUTO_PROMOTED_VERIFY_JOB\nJOB_ID: {}\nWORKSPACE: @{}\nSTATUS: running\nELAPSED: {}s\nNEXT_FROM_LINE: {}\nHINT: Atomic multi-edit succeeded (GROUP_ID: {}). Verification exceeded {}s sync window and continues in background; call run_command(action=\"poll\", job_id=\"{}\", from_line={}) (auto long-polls up to {}s; do NOT run sleep).\nRECENT OUTPUT:\n{}",
                    summary.job_id,
                    ws.alias,
                    elapsed,
                    total_lines,
                    group_id,
                    p_secs,
                    summary.job_id,
                    total_lines,
                    poll_wait_secs,
                    if recent.is_empty() {
                        "(waiting for output...)"
                    } else {
                        recent.as_str()
                    }
                ));
            }
        } else {
            let (code, stdout, stderr) =
                run_shell_raw(&ws.canon_root, &verify_cmds[0], 30).await?;
            let (compressed, _) = compress_output(&stdout, &stderr, 60);
            text.push_str(&format!(
                "\n\nVERIFY COMMAND: {verify_display}\nEXIT CODE: {code}\nOUTPUT:\n{compressed}"
            ));
        }
    }

    Ok(text)
}

pub(crate) async fn exec_upsert_artifact(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    run_ctx: Option<&crate::agent_manager::RunContext>,
    args: &Value,
) -> Result<String, String> {
    let kind_raw = args
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'kind' wajib diisi.")?;
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'title' wajib diisi.")?;
    let content_md = args
        .get("content_md")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'content_md' wajib diisi.")?;

    let kind = crate::artifacts::ArtifactKind::parse(kind_raw)?;
    let session_id = run_ctx
        .map(|c| c.session_id.as_str())
        .unwrap_or(if source == "WebMCP" {
            "webmcp-latest"
        } else {
            "default"
        });

    let doc = state
        .artifact_store
        .upsert_artifact(&ws.clean_path, &ws.alias, session_id, kind, title, content_md)
        .await?;

    if let Some(app) = state.app_handle.read().await.clone() {
        let _ = app.emit("artifact-updated", &doc);
    }

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "upsert_artifact".to_string(),
                target_path: Some(doc.kind.filename().to_string()),
                summary: format!("Saved artifact '{}' (v{})", doc.title, doc.version),
                diff_text: None,
                can_revert: false,
                reverted: false,
                tokens_saved: 0,
                original_content: None,
                original_blob_hash: None,
                file_existed_before: false,
                post_edit_hash: None,
                group_id: current_turn_group_id(state).await,
                move_from_path: None,
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            0,
        )
        .await;

    Ok(format!(
        "ARTIFACT SAVED: id={} kind={} version={} status={:?}\nSession: {} | Workspace: @{}\nTitle: {}",
        doc.id,
        doc.kind.filename(),
        doc.version,
        doc.status,
        doc.session_id,
        ws.alias,
        doc.title
    ))
}

pub(crate) async fn exec_request_plan_approval(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    run_ctx: Option<&crate::agent_manager::RunContext>,
    args: &Value,
) -> Result<String, String> {
    let summary = args
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("Implementation Plan siap ditinjau.");
    let artifact_id = args.get("artifact_id").and_then(|v| v.as_str());
    let session_id = run_ctx
        .map(|c| c.session_id.as_str())
        .unwrap_or(if source == "WebMCP" {
            "webmcp-latest"
        } else {
            "default"
        });

    let doc = state
        .artifact_store
        .request_approval(&ws.clean_path, session_id, artifact_id)
        .await?;

    if let Some(app) = state.app_handle.read().await.clone() {
        let _ = app.emit("artifact-updated", &doc);
    }

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "request_plan_approval".to_string(),
                target_path: Some(doc.kind.filename().to_string()),
                summary: format!("Requested plan approval for '{}'", doc.title),
                diff_text: None,
                can_revert: false,
                reverted: false,
                tokens_saved: 0,
                original_content: None,
                original_blob_hash: None,
                file_existed_before: false,
                post_edit_hash: None,
                group_id: current_turn_group_id(state).await,
                move_from_path: None,
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            0,
        )
        .await;

    Ok(format!(
        "AWAITING_PLAN_APPROVAL: id={} title='{}' (v{})\nSummary: {}\nPause execution here and wait for the user to approve or request changes.",
        doc.id, doc.title, doc.version, summary.trim()
    ))
}

pub(crate) async fn exec_list_skills(
    state: &AppState,
    source: &str,
    pinned_ws: Option<&str>,
    args: &Value,
) -> Result<String, String> {
    let query_filter = args
        .get("query")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let scope_filter = args
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let compact = args
        .get("compact")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let entries = state.skills_for_context(pinned_ws).await;
    let active_all: Vec<_> = entries
        .into_iter()
        .filter(|s| s.summary.enabled && s.summary.parse_error.is_none())
        .collect();
    let total_active = active_all.len();

    let active: Vec<_> = active_all
        .into_iter()
        .filter(|s| {
            if let Some(ref sc) = scope_filter {
                let entry_scope = match s.summary.scope {
                    crate::skills::SkillScope::Workspace => "workspace",
                    crate::skills::SkillScope::Plugin => "plugin",
                    crate::skills::SkillScope::Global => "global",
                };
                if entry_scope != sc.as_str() {
                    return false;
                }
            }
            if let Some(ref q) = query_filter {
                let name_lc = s.summary.name.to_ascii_lowercase();
                let desc_lc = s.summary.description.to_ascii_lowercase();
                if !name_lc.contains(q.as_str()) && !desc_lc.contains(q.as_str()) {
                    return false;
                }
            }
            true
        })
        .collect();

    let output = if total_active == 0 {
        "Tidak ada Agent Skill (SKILL.md) aktif yang terdeteksi.".to_string()
    } else if active.is_empty() {
        format!(
            "Tidak ada Agent Skill aktif yang cocok dengan filter (query={:?}, scope={:?}) dari {} skill aktif.",
            query_filter.as_deref().unwrap_or("*"),
            scope_filter.as_deref().unwrap_or("*"),
            total_active
        )
    } else {
        let lines: Vec<String> = active
            .iter()
            .map(|s| {
                let manual_tag = if s.summary.disable_model_invocation {
                    " [explicit-only]"
                } else {
                    ""
                };
                if compact {
                    let clean_desc: String = s
                        .summary
                        .description
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let clipped_desc = if clean_desc.chars().count() > 68 {
                        let head: String = clean_desc.chars().take(65).collect();
                        format!("{head}...")
                    } else {
                        clean_desc
                    };
                    format!(
                        "- `{}` [{}]{}: {}",
                        s.summary.name, s.summary.source_label, manual_tag, clipped_desc
                    )
                } else {
                    format!(
                        "- `{}` [{}]{} (resources: {}, ~{} tok full): {}",
                        s.summary.name,
                        s.summary.source_label,
                        manual_tag,
                        s.summary.resource_files.len(),
                        s.summary.body_tokens_est,
                        s.summary.description
                    )
                }
            })
            .collect();
        let header = if active.len() == total_active {
            format!("Active Agent Skills ({}):", active.len())
        } else {
            format!("Active Agent Skills ({} of {}):", active.len(), total_active)
        };
        let filter_tip = if active.len() > 20 && !compact {
            "\nTip: Pass compact=true, query=\"...\", or scope=\"workspace|plugin|global\" to view large catalogs without truncation."
        } else {
            ""
        };
        format!(
            "{header}\n{}\n\nUse load_skill(name) to read full instructions.{filter_tip}",
            lines.join("\n")
        )
    };

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "list_skills".to_string(),
                target_path: None,
                summary: format!("Listed {} active skills", active.len()),
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
                workspace_id: None,
                workspace_alias: None,
                workspace_root: None,
            },
            0,
        )
        .await;

    Ok(output)
}

pub(crate) async fn exec_load_skill(
    state: &AppState,
    source: &str,
    pinned_ws: Option<&str>,
    args: &Value,
) -> Result<String, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("Argumen 'name' wajib diisi.")?;

    let entries = state.skills_for_context(pinned_ws).await;
    let skill = entries
        .into_iter()
        .find(|s| {
            s.summary.enabled
                && s.summary.parse_error.is_none()
                && s.summary.name.eq_ignore_ascii_case(name)
        })
        .ok_or_else(|| format!("Skill '{name}' tidak ditemukan atau sedang nonaktif."))?;

    let res_section = if skill.summary.resource_files.is_empty() {
        "(none)".to_string()
    } else {
        skill
            .summary
            .resource_files
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let output = format!(
        "SKILL: {} [{}]\nRESOURCES ({}):\n{}\n\n---\n{}",
        skill.summary.name,
        skill.summary.source_label,
        skill.summary.resource_files.len(),
        res_section,
        skill.body_md
    );

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "load_skill".to_string(),
                target_path: Some(skill.summary.name.clone()),
                summary: format!(
                    "Loaded SKILL.md '{}' [{}]",
                    skill.summary.name, skill.summary.source_label
                ),
                diff_text: None,
                can_revert: false,
                reverted: false,
                tokens_saved: 150,
                original_content: None,
                original_blob_hash: None,
                file_existed_before: false,
                post_edit_hash: None,
                group_id: None,
                move_from_path: None,
                workspace_id: None,
                workspace_alias: None,
                workspace_root: None,
            },
            1,
        )
        .await;

    Ok(output)
}

pub(crate) async fn exec_read_skill_resource(
    state: &AppState,
    source: &str,
    pinned_ws: Option<&str>,
    args: &Value,
) -> Result<String, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("Argumen 'name' wajib diisi.")?;
    let rel_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("Argumen 'path' wajib diisi.")?;

    let entries = state.skills_for_context(pinned_ws).await;
    let skill = entries
        .into_iter()
        .find(|s| {
            s.summary.enabled
                && s.summary.parse_error.is_none()
                && s.summary.name.eq_ignore_ascii_case(name)
        })
        .ok_or_else(|| format!("Skill '{name}' tidak ditemukan atau sedang nonaktif."))?;

    let text = crate::skills::read_skill_resource_file(&skill, rel_path)?;

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "read_skill_resource".to_string(),
                target_path: Some(format!("{}/{rel_path}", skill.summary.name)),
                summary: format!("Read skill resource {}/{}", skill.summary.name, rel_path),
                diff_text: None,
                can_revert: false,
                reverted: false,
                tokens_saved: 80,
                original_content: None,
                original_blob_hash: None,
                file_existed_before: false,
                post_edit_hash: None,
                group_id: None,
                move_from_path: None,
                workspace_id: None,
                workspace_alias: None,
                workspace_root: None,
            },
            0,
        )
        .await;

    Ok(format!(
        "SKILL RESOURCE: {}/{}\n\n{}",
        skill.summary.name, rel_path, text
    ))
}

#[cfg(test)]
mod list_skills_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_exec_list_skills_supports_query_scope_and_compact() {
        let base_dir = std::env::temp_dir().join("smart_mcp_list_skills_filter_test");
        let _ = std::fs::remove_dir_all(&base_dir);
        let ws_skill_dir = base_dir.join(".airlock").join("skills").join("alpha-audit");
        std::fs::create_dir_all(&ws_skill_dir).unwrap();
        std::fs::write(
            ws_skill_dir.join("SKILL.md"),
            "---\nname: alpha-audit\ndescription: Deep audit helper for workspace verification and boundary checks in long sessions.\n---\n# Body\n",
        )
        .unwrap();
        let ws_skill_dir_2 = base_dir.join(".airlock").join("skills").join("beta-helper");
        std::fs::create_dir_all(&ws_skill_dir_2).unwrap();
        std::fs::write(
            ws_skill_dir_2.join("SKILL.md"),
            "---\nname: beta-helper\ndescription: Secondary skill to ensure total active skill count exceeds filtered count.\n---\n# Body\n",
        )
        .unwrap();

        let state = AppState::new(base_dir.clone());
        let compact_ws = exec_list_skills(
            &state,
            "WebMCP",
            None,
            &json!({ "scope": "workspace", "query": "alpha", "compact": true }),
        )
        .await
        .unwrap();
        assert!(compact_ws.contains("Active Agent Skills (1 of "));
        assert!(compact_ws.contains("- `alpha-audit` [workspace:"));
        assert!(compact_ws.contains("Deep audit helper"));
        assert!(!compact_ws.contains("tok full"));

        let empty_match = exec_list_skills(
            &state,
            "WebMCP",
            None,
            &json!({ "scope": "workspace", "query": "nonexistent-xyz" }),
        )
        .await
        .unwrap();
        assert!(empty_match.contains("Tidak ada Agent Skill aktif yang cocok dengan filter"));

        let _ = std::fs::remove_dir_all(&base_dir);
    }
}
