use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredChatStep {
    pub tool_name: String,
    pub summary: String,
    pub tokens_saved: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StoredChatUsage {
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub thought: Option<String>,
    #[serde(default)]
    pub steps: Vec<StoredChatStep>,
    #[serde(default)]
    pub usage: Option<StoredChatUsage>,
    #[serde(default)]
    pub cancelled: bool,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatSession {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub messages: Vec<StoredChatMessage>,
}

#[derive(Debug, Clone)]
pub struct ChatStore {
    pub chats_dir: Option<PathBuf>,
}

impl ChatStore {
    pub fn new_none() -> Self {
        Self { chats_dir: None }
    }

    pub fn new_persistent(data_dir: PathBuf) -> Self {
        Self {
            chats_dir: Some(data_dir.join("chats")),
        }
    }

    fn workspace_file(&self, workspace_root: &str) -> Option<PathBuf> {
        let dir = self.chats_dir.as_ref()?;
        let norm = if cfg!(target_os = "windows") {
            workspace_root.trim().to_lowercase()
        } else {
            workspace_root.trim().to_string()
        };
        let digest = Sha256::digest(norm.as_bytes());
        let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        Some(dir.join(format!("{hex}.json")))
    }

    pub fn list_sessions(&self, workspace_root: &str) -> Vec<ChatSession> {
        let Some(path) = self.workspace_file(workspace_root) else {
            return Vec::new();
        };
        let mut sessions = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<ChatSession>>(&raw).ok())
            .unwrap_or_default();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        sessions
    }

    fn write_sessions(&self, workspace_root: &str, sessions: &[ChatSession]) -> Result<(), String> {
        let Some(path) = self.workspace_file(workspace_root) else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Gagal membuat direktori chat: {e}"))?;
        }
        let json_str = serde_json::to_string_pretty(sessions)
            .map_err(|e| format!("Gagal serialisasi chat sessions: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json_str)
            .map_err(|e| format!("Gagal menulis file sementara chat: {e}"))?;
        if std::fs::rename(&tmp, &path).is_err() {
            std::fs::write(&path, &json_str)
                .map_err(|e| format!("Gagal menyimpan chat sessions: {e}"))?;
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(())
    }

    pub fn save_session(
        &self,
        workspace_root: &str,
        session: ChatSession,
    ) -> Result<Vec<ChatSession>, String> {
        let mut sessions = self.list_sessions(workspace_root);
        sessions.retain(|s| s.id != session.id);
        sessions.insert(0, session);
        if sessions.len() > 30 {
            sessions.truncate(30);
        }
        self.write_sessions(workspace_root, &sessions)?;
        Ok(sessions)
    }

    pub fn delete_session(
        &self,
        workspace_root: &str,
        session_id: &str,
    ) -> Result<Vec<ChatSession>, String> {
        let mut sessions = self.list_sessions(workspace_root);
        sessions.retain(|s| s.id != session_id);
        self.write_sessions(workspace_root, &sessions)?;
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_chat_store_sessions_roundtrip() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_chat_store_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let store = ChatStore::new_persistent(temp_dir.clone());
        let ws = r"D:\Projects\Demo";

        let s1 = ChatSession {
            id: "sess-1".to_string(),
            title: "Periksa auth".to_string(),
            created_at: "2026-11-23T10:00:00Z".to_string(),
            updated_at: "2026-11-23T10:01:00Z".to_string(),
            messages: vec![StoredChatMessage {
                role: "user".to_string(),
                content: "cek auth.rs".to_string(),
                thought: None,
                steps: Vec::new(),
                usage: None,
                cancelled: false,
                timestamp: "10:00:00".to_string(),
            }],
        };

        let list1 = store.save_session(ws, s1).unwrap();
        assert_eq!(list1.len(), 1);
        assert_eq!(store.list_sessions(ws).len(), 1);

        let list2 = store.delete_session(ws, "sess-1").unwrap();
        assert!(list2.is_empty());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
