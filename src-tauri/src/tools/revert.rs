use super::guardrail::sha256_hex;
use crate::security::resolve_jailed_path;
use crate::state::AppState;
use std::fs;
use std::path::PathBuf;

async fn resolve_entry_root(
    state: &AppState,
    entry_ws_root: Option<String>,
    entry_ws_id: Option<String>,
) -> Result<PathBuf, String> {
    if let Some(raw_root) = entry_ws_root {
        let trimmed = raw_root.trim();
        if !trimmed.is_empty() {
            let candidate = PathBuf::from(trimmed);
            if !candidate.exists() || !candidate.is_dir() {
                return Err(format!(
                    "Folder workspace asal '{trimmed}' tidak lagi tersedia di disk; revert dibatalkan demi keamanan."
                ));
            }
            return std::fs::canonicalize(&candidate)
                .map_err(|e| format!("Gagal memverifikasi folder workspace asal '{trimmed}': {e}"));
        }
    }

    if let Some(ws_id) = entry_ws_id {
        let trimmed_id = ws_id.trim();
        if !trimmed_id.is_empty() {
            let prof_path = {
                let cfg = state.config.read().await;
                cfg.workspaces
                    .iter()
                    .find(|w| w.id == trimmed_id)
                    .map(|w| w.path.clone())
            };
            let Some(raw_path) = prof_path else {
                return Err(format!(
                    "Workspace asal '{trimmed_id}' sudah dihapus dari registry; revert dibatalkan."
                ));
            };
            let candidate = PathBuf::from(&raw_path);
            if !candidate.exists() || !candidate.is_dir() {
                return Err(format!(
                    "Folder workspace asal '{raw_path}' tidak lagi tersedia di disk; revert dibatalkan."
                ));
            }
            return std::fs::canonicalize(&candidate)
                .map_err(|e| format!("Gagal memverifikasi folder workspace asal '{raw_path}': {e}"));
        }
    }

    Ok(state.workspace_root.read().await.clone())
}

pub async fn revert_audit_entry(
    state: &AppState,
    audit_id: &str,
    force: bool,
) -> Result<String, String> {
    let (entry_ws_root, entry_ws_id) = {
        let logs = state.audit_logs.read().await;
        let entry = logs
            .iter()
            .find(|e| e.id == audit_id)
            .ok_or_else(|| format!("Entri audit '{audit_id}' tidak ditemukan."))?;
        if !entry.can_revert {
            return Err("Entri ini tidak mendukung rollback.".to_string());
        }
        if entry.reverted {
            return Err("Perubahan ini sudah pernah di-revert.".to_string());
        }
        (entry.workspace_root.clone(), entry.workspace_id.clone())
    };

    let root = resolve_entry_root(state, entry_ws_root, entry_ws_id).await?;
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
