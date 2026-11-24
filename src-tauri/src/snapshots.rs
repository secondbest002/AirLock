use crate::state::AuditEntry;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub fn default_data_dir() -> PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".smart-tauri-hub")
}

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    pub snapshots_dir: Option<PathBuf>,
    pub audit_file: Option<PathBuf>,
}

impl SnapshotStore {
    pub fn new_none() -> Self {
        Self {
            snapshots_dir: None,
            audit_file: None,
        }
    }

    pub fn new_persistent(data_dir: PathBuf) -> Self {
        Self {
            snapshots_dir: Some(data_dir.join("snapshots")),
            audit_file: Some(data_dir.join("audit_history.json")),
        }
    }

    pub fn save_blob(&self, content: &str) -> Option<String> {
        let dir = self.snapshots_dir.as_ref()?;
        if std::fs::create_dir_all(dir).is_err() {
            return None;
        }
        let digest = Sha256::digest(content.as_bytes());
        let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let blob_path = dir.join(format!("{hash}.blob"));
        if !blob_path.exists() {
            let tmp_path = dir.join(format!("{hash}.blob.tmp"));
            if std::fs::write(&tmp_path, content).is_err() {
                return None;
            }
            if std::fs::rename(&tmp_path, &blob_path).is_err() {
                if std::fs::write(&blob_path, content).is_err() {
                    let _ = std::fs::remove_file(&tmp_path);
                    return None;
                }
                let _ = std::fs::remove_file(&tmp_path);
            }
        }
        Some(hash)
    }

    pub fn load_blob(&self, hash: &str) -> Option<String> {
        let dir = self.snapshots_dir.as_ref()?;
        let clean_hash = hash.trim();
        if clean_hash.is_empty() || !clean_hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        std::fs::read_to_string(dir.join(format!("{clean_hash}.blob"))).ok()
    }

    pub fn save_audit_entries(&self, entries: &[AuditEntry]) {
        let Some(path) = &self.audit_file else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let start = entries.len().saturating_sub(200);
        let slice = &entries[start..];
        let Ok(json_str) = serde_json::to_string_pretty(slice) else {
            return;
        };
        let tmp_path = path.with_extension("json.tmp");
        if std::fs::write(&tmp_path, &json_str).is_ok()
            && std::fs::rename(&tmp_path, path).is_err()
        {
            let _ = std::fs::write(path, &json_str);
            let _ = std::fs::remove_file(&tmp_path);
        }
    }

    pub fn load_audit_entries(&self) -> Vec<AuditEntry> {
        let Some(path) = &self.audit_file else {
            return Vec::new();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<AuditEntry>>(&raw).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_snapshot_store_blob_and_audit_roundtrip() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_snapshot_store_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let store = SnapshotStore::new_persistent(temp_dir.clone());
        let original = "fn main() {\n    println!(\"hello\");\n}\n";
        let hash = store.save_blob(original).expect("blob hash should be saved");
        assert_eq!(store.load_blob(&hash).as_deref(), Some(original));

        let entry = AuditEntry {
            id: "aud-test-1".to_string(),
            timestamp: "10:00:00".to_string(),
            source: "WebMCP".to_string(),
            tool_name: "surgical_edit".to_string(),
            target_path: Some("src/main.rs".to_string()),
            summary: "Updated main".to_string(),
            diff_text: Some("-a\n+b\n".to_string()),
            can_revert: true,
            reverted: false,
            tokens_saved: 150,
            original_content: None,
            original_blob_hash: Some(hash.clone()),
            file_existed_before: true,
            post_edit_hash: Some("deadbeef".to_string()),
            group_id: Some("turn-1".to_string()),
            move_from_path: None,
        };

        store.save_audit_entries(&[entry]);
        let loaded = store.load_audit_entries();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "aud-test-1");
        assert_eq!(loaded[0].original_blob_hash.as_deref(), Some(hash.as_str()));
        assert_eq!(loaded[0].post_edit_hash.as_deref(), Some("deadbeef"));
        assert_eq!(loaded[0].group_id.as_deref(), Some("turn-1"));
        assert!(loaded[0].file_existed_before);

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
