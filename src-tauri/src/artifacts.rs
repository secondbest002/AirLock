use crate::security::resolve_jailed_path;
use crate::state::AppState;
use crate::tools::sha256_hex;
use crate::workspace::clean_path_string;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    ImplementationPlan,
    TaskList,
    Walkthrough,
    ReviewNote,
}

impl ArtifactKind {
    pub fn filename(&self) -> &'static str {
        match self {
            Self::ImplementationPlan => "implementation_plan.md",
            Self::TaskList => "task.md",
            Self::Walkthrough => "walkthrough.md",
            Self::ReviewNote => "review_note.md",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "implementation_plan" | "plan" => Ok(Self::ImplementationPlan),
            "task_list" | "tasks" | "task" => Ok(Self::TaskList),
            "walkthrough" => Ok(Self::Walkthrough),
            "review_note" | "review" => Ok(Self::ReviewNote),
            other => Err(format!(
                "Jenis artifact tidak valid: '{other}'. Gunakan implementation_plan, task_list, walkthrough, atau review_note."
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Draft,
    AwaitingApproval,
    Approved,
    ChangesRequested,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactComment {
    pub id: String,
    pub line_anchor: Option<u32>,
    pub quote: Option<String>,
    pub author: String,
    pub body: String,
    pub resolved: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactDocument {
    pub id: String,
    pub session_id: String,
    pub workspace_alias: String,
    pub kind: ArtifactKind,
    pub title: String,
    pub content_md: String,
    pub status: ArtifactStatus,
    pub version: u32,
    pub updated_at: String,
    #[serde(default)]
    pub comments: Vec<ArtifactComment>,
}

pub struct ArtifactStore {
    root_dir: Option<PathBuf>,
    mem: RwLock<HashMap<String, Vec<ArtifactDocument>>>,
}

impl ArtifactStore {
    pub fn new_none() -> Self {
        Self {
            root_dir: None,
            mem: RwLock::new(HashMap::new()),
        }
    }

    pub fn new_persistent(data_dir: PathBuf) -> Self {
        Self {
            root_dir: Some(data_dir.join("artifacts")),
            mem: RwLock::new(HashMap::new()),
        }
    }

    fn ws_hash(clean_ws: &str) -> String {
        let norm = if cfg!(target_os = "windows") {
            clean_ws.trim().to_ascii_lowercase()
        } else {
            clean_ws.trim().to_string()
        };
        let hex = sha256_hex(norm.as_bytes());
        hex.chars().take(12).collect()
    }

    pub fn sanitize_session_id(raw: &str) -> String {
        let cleaned: String = raw
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(64)
            .collect();
        if cleaned.is_empty() {
            "default".to_string()
        } else {
            cleaned
        }
    }

    fn cache_key(clean_ws: &str, session_id: &str) -> String {
        format!("{}::{}", Self::ws_hash(clean_ws), Self::sanitize_session_id(session_id))
    }

    fn session_dir(&self, clean_ws: &str, session_id: &str) -> Option<PathBuf> {
        let root = self.root_dir.as_ref()?;
        Some(
            root.join(Self::ws_hash(clean_ws))
                .join(Self::sanitize_session_id(session_id)),
        )
    }

    fn load_session_disk(&self, clean_ws: &str, session_id: &str) -> Vec<ArtifactDocument> {
        let Some(dir) = self.session_dir(clean_ws, session_id) else {
            return Vec::new();
        };
        let manifest = dir.join("manifest.json");
        let Ok(raw) = fs::read_to_string(&manifest) else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<ArtifactDocument>>(&raw).unwrap_or_default()
    }

    fn save_session_disk(
        &self,
        clean_ws: &str,
        session_id: &str,
        docs: &[ArtifactDocument],
    ) -> Result<(), String> {
        let Some(dir) = self.session_dir(clean_ws, session_id) else {
            return Ok(());
        };
        fs::create_dir_all(&dir)
            .map_err(|e| format!("Gagal membuat direktori artifacts: {e}"))?;

        for doc in docs {
            let md_path = dir.join(doc.kind.filename());
            let _ = fs::write(md_path, &doc.content_md);
        }

        let json_str = serde_json::to_string_pretty(docs)
            .map_err(|e| format!("Gagal serialisasi artifact manifest: {e}"))?;
        let manifest_path = dir.join("manifest.json");
        let tmp_path = dir.join("manifest.json.tmp");
        fs::write(&tmp_path, &json_str)
            .map_err(|e| format!("Gagal menulis tmp artifact manifest: {e}"))?;
        if fs::rename(&tmp_path, &manifest_path).is_err() {
            fs::write(&manifest_path, &json_str)
                .map_err(|e| format!("Gagal menyimpan artifact manifest: {e}"))?;
            let _ = fs::remove_file(&tmp_path);
        }
        Ok(())
    }

    async fn load_or_get_session(
        &self,
        clean_ws: &str,
        session_id: &str,
    ) -> (String, Vec<ArtifactDocument>) {
        let key = Self::cache_key(clean_ws, session_id);
        {
            let guard = self.mem.read().await;
            if let Some(existing) = guard.get(&key) {
                return (key, existing.clone());
            }
        }
        let disk_docs = self.load_session_disk(clean_ws, session_id);
        let mut guard = self.mem.write().await;
        let entry = guard.entry(key.clone()).or_insert_with(|| disk_docs);
        (key, entry.clone())
    }

    async fn commit_session(
        &self,
        clean_ws: &str,
        session_id: &str,
        key: String,
        docs: Vec<ArtifactDocument>,
    ) -> Result<(), String> {
        self.save_session_disk(clean_ws, session_id, &docs)?;
        self.mem.write().await.insert(key, docs);
        Ok(())
    }

    pub async fn list_session_artifacts(
        &self,
        clean_ws: &str,
        session_id: &str,
    ) -> Vec<ArtifactDocument> {
        let (_, docs) = self.load_or_get_session(clean_ws, session_id).await;
        docs
    }

    pub async fn list_workspace_latest_by_kind(
        &self,
        clean_ws: &str,
        session_prefixes: &[&str],
    ) -> Vec<ArtifactDocument> {
        fn matches_prefix(sid: &str, prefixes: &[&str]) -> bool {
            prefixes.iter().any(|p| {
                sid == *p || (p.ends_with('_') && sid.starts_with(p))
            })
        }

        fn kind_rank(kind: &ArtifactKind) -> usize {
            match kind {
                ArtifactKind::ImplementationPlan => 0,
                ArtifactKind::TaskList => 1,
                ArtifactKind::Walkthrough => 2,
                ArtifactKind::ReviewNote => 3,
            }
        }

        let ws_hash = Self::ws_hash(clean_ws);
        let key_prefix = format!("{ws_hash}::");
        let mut by_session: HashMap<String, Vec<ArtifactDocument>> = HashMap::new();

        {
            let guard = self.mem.read().await;
            for (k, docs) in guard.iter() {
                if let Some(sid) = k.strip_prefix(&key_prefix) {
                    if matches_prefix(sid, session_prefixes) && !docs.is_empty() {
                        by_session.insert(sid.to_string(), docs.clone());
                    }
                }
            }
        }

        if let Some(root) = self.root_dir.as_ref() {
            let ws_dir = root.join(&ws_hash);
            if let Ok(entries) = fs::read_dir(&ws_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let Some(sid) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if by_session.contains_key(sid) || !matches_prefix(sid, session_prefixes) {
                        continue;
                    }
                    let manifest = path.join("manifest.json");
                    if let Ok(raw) = fs::read_to_string(&manifest) {
                        if let Ok(docs) = serde_json::from_str::<Vec<ArtifactDocument>>(&raw) {
                            if !docs.is_empty() {
                                by_session.insert(sid.to_string(), docs);
                            }
                        }
                    }
                }
            }
        }

        let mut best: [Option<ArtifactDocument>; 4] = [None, None, None, None];
        for docs in by_session.into_values() {
            for doc in docs {
                let idx = kind_rank(&doc.kind);
                let replace = match &best[idx] {
                    None => true,
                    Some(cur) => {
                        let new_key = (
                            doc.updated_at.as_str(),
                            doc.session_id == "webmcp-latest",
                            doc.version,
                        );
                        let cur_key = (
                            cur.updated_at.as_str(),
                            cur.session_id == "webmcp-latest",
                            cur.version,
                        );
                        new_key > cur_key
                    }
                };
                if replace {
                    best[idx] = Some(doc);
                }
            }
        }

        best.into_iter().flatten().collect()
    }

    pub async fn upsert_artifact(
        &self,
        clean_ws: &str,
        ws_alias: &str,
        session_id: &str,
        kind: ArtifactKind,
        title: &str,
        content_md: &str,
    ) -> Result<ArtifactDocument, String> {
        let safe_session = Self::sanitize_session_id(session_id);
        let clean_title = title.trim();
        let effective_title = if clean_title.is_empty() {
            kind.filename().to_string()
        } else {
            clean_title.to_string()
        };
        let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

        let (key, mut docs) = self.load_or_get_session(clean_ws, &safe_session).await;
        let updated_doc = if let Some(existing) = docs.iter_mut().find(|d| d.kind == kind) {
            existing.workspace_alias = ws_alias.to_string();
            existing.title = effective_title;
            existing.content_md = content_md.to_string();
            existing.version += 1;
            existing.updated_at = now;
            if kind == ArtifactKind::ImplementationPlan
                && matches!(existing.status, ArtifactStatus::ChangesRequested)
            {
                existing.status = ArtifactStatus::Draft;
            }
            existing.clone()
        } else {
            let initial_status = match kind {
                ArtifactKind::Walkthrough => ArtifactStatus::Completed,
                _ => ArtifactStatus::Draft,
            };
            let created = ArtifactDocument {
                id: format!("art_{}", &AppState::generate_token()[..8]),
                session_id: safe_session.clone(),
                workspace_alias: ws_alias.to_string(),
                kind,
                title: effective_title,
                content_md: content_md.to_string(),
                status: initial_status,
                version: 1,
                updated_at: now,
                comments: Vec::new(),
            };
            docs.push(created.clone());
            created
        };

        self.commit_session(clean_ws, &safe_session, key, docs)
            .await?;
        Ok(updated_doc)
    }

    pub async fn request_approval(
        &self,
        clean_ws: &str,
        session_id: &str,
        artifact_id: Option<&str>,
    ) -> Result<ArtifactDocument, String> {
        let safe_session = Self::sanitize_session_id(session_id);
        let (key, mut docs) = self.load_or_get_session(clean_ws, &safe_session).await;

        let target_idx = if let Some(id) = artifact_id.map(str::trim).filter(|s| !s.is_empty()) {
            docs.iter().position(|d| d.id == id)
        } else {
            docs.iter()
                .position(|d| d.kind == ArtifactKind::ImplementationPlan)
                .or_else(|| if docs.is_empty() { None } else { Some(0) })
        }
        .ok_or_else(|| {
            "Belum ada Implementation Plan di sesi ini. Panggil upsert_artifact(kind='implementation_plan', ...) terlebih dahulu."
                .to_string()
        })?;

        let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        docs[target_idx].status = ArtifactStatus::AwaitingApproval;
        docs[target_idx].updated_at = now;
        let doc = docs[target_idx].clone();

        self.commit_session(clean_ws, &safe_session, key, docs)
            .await?;
        Ok(doc)
    }

    pub async fn is_plan_approved(&self, clean_ws: &str, session_id: &str) -> bool {
        let docs = self.list_session_artifacts(clean_ws, session_id).await;
        docs.iter().any(|d| {
            d.kind == ArtifactKind::ImplementationPlan
                && matches!(d.status, ArtifactStatus::Approved | ArtifactStatus::Completed)
        })
    }

    pub async fn add_comment(
        &self,
        clean_ws: &str,
        session_id: &str,
        artifact_id: &str,
        line_anchor: Option<u32>,
        quote: Option<String>,
        body: &str,
    ) -> Result<ArtifactDocument, String> {
        let clean_body = body.trim();
        if clean_body.is_empty() {
            return Err("Isi komentar artifact tidak boleh kosong.".to_string());
        }
        let safe_session = Self::sanitize_session_id(session_id);
        let (key, mut docs) = self.load_or_get_session(clean_ws, &safe_session).await;
        let doc = docs
            .iter_mut()
            .find(|d| d.id == artifact_id.trim())
            .ok_or_else(|| format!("Artifact '{artifact_id}' tidak ditemukan pada sesi ini."))?;

        let clipped_quote = quote
            .map(|q| q.trim().chars().take(160).collect::<String>())
            .filter(|q| !q.is_empty());

        doc.comments.push(ArtifactComment {
            id: format!("cmt_{}", &AppState::generate_token()[..8]),
            line_anchor,
            quote: clipped_quote,
            author: "user".to_string(),
            body: clean_body.to_string(),
            resolved: false,
            created_at: Local::now().format("%H:%M:%S").to_string(),
        });
        doc.updated_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let updated = doc.clone();

        self.commit_session(clean_ws, &safe_session, key, docs)
            .await?;
        Ok(updated)
    }

    pub async fn resolve_comment(
        &self,
        clean_ws: &str,
        session_id: &str,
        artifact_id: &str,
        comment_id: &str,
        resolved: bool,
    ) -> Result<ArtifactDocument, String> {
        let safe_session = Self::sanitize_session_id(session_id);
        let (key, mut docs) = self.load_or_get_session(clean_ws, &safe_session).await;
        let doc = docs
            .iter_mut()
            .find(|d| d.id == artifact_id.trim())
            .ok_or_else(|| format!("Artifact '{artifact_id}' tidak ditemukan."))?;
        let cmt = doc
            .comments
            .iter_mut()
            .find(|c| c.id == comment_id.trim())
            .ok_or_else(|| format!("Komentar '{comment_id}' tidak ditemukan."))?;
        cmt.resolved = resolved;
        doc.updated_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let updated = doc.clone();

        self.commit_session(clean_ws, &safe_session, key, docs)
            .await?;
        Ok(updated)
    }

    pub async fn approve_artifact(
        &self,
        clean_ws: &str,
        session_id: &str,
        artifact_id: &str,
    ) -> Result<ArtifactDocument, String> {
        let safe_session = Self::sanitize_session_id(session_id);
        let (key, mut docs) = self.load_or_get_session(clean_ws, &safe_session).await;
        let doc = docs
            .iter_mut()
            .find(|d| d.id == artifact_id.trim())
            .ok_or_else(|| format!("Artifact '{artifact_id}' tidak ditemukan."))?;
        doc.status = ArtifactStatus::Approved;
        doc.updated_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let updated = doc.clone();

        self.commit_session(clean_ws, &safe_session, key, docs)
            .await?;
        Ok(updated)
    }

    pub async fn request_changes_and_format_feedback(
        &self,
        clean_ws: &str,
        session_id: &str,
        artifact_id: &str,
    ) -> Result<(ArtifactDocument, String), String> {
        let safe_session = Self::sanitize_session_id(session_id);
        let (key, mut docs) = self.load_or_get_session(clean_ws, &safe_session).await;
        let doc = docs
            .iter_mut()
            .find(|d| d.id == artifact_id.trim())
            .ok_or_else(|| format!("Artifact '{artifact_id}' tidak ditemukan."))?;
        doc.status = ArtifactStatus::ChangesRequested;
        doc.updated_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

        let unresolved: Vec<&ArtifactComment> =
            doc.comments.iter().filter(|c| !c.resolved).collect();
        if unresolved.is_empty() {
            return Err("Tambahkan minimal 1 komentar review yang belum resolved sebelum Request Changes.".to_string());
        }

        let mut feedback_lines = Vec::with_capacity(unresolved.len() + 3);
        feedback_lines.push(format!(
            "Perubahan diminta pada artifact `{}` (v{} — {}). Perbaiki melalui `upsert_artifact` lalu panggil `request_plan_approval` kembali:",
            doc.title,
            doc.version,
            doc.kind.filename()
        ));
        for (idx, cmt) in unresolved.iter().enumerate() {
            let anchor = match cmt.line_anchor {
                Some(line) => format!("Line {line}"),
                None => "General".to_string(),
            };
            let quote_str = match &cmt.quote {
                Some(q) => format!(" (`\"{}\"`)", q.replace('`', "'")),
                None => String::new(),
            };
            feedback_lines.push(format!("{}. [{}]{}: {}", idx + 1, anchor, quote_str, cmt.body));
        }
        let prompt = feedback_lines.join("\n");
        let updated = doc.clone();

        self.commit_session(clean_ws, &safe_session, key, docs)
            .await?;
        Ok((updated, prompt))
    }

    pub async fn export_artifact_to_workspace(
        &self,
        clean_ws: &str,
        ws_canon_root: &Path,
        session_id: &str,
        artifact_id: &str,
    ) -> Result<String, String> {
        let docs = self.list_session_artifacts(clean_ws, session_id).await;
        let doc = docs
            .into_iter()
            .find(|d| d.id == artifact_id.trim())
            .ok_or_else(|| format!("Artifact '{artifact_id}' tidak ditemukan."))?;

        let rel = format!(".airlock/artifacts/{}", doc.kind.filename());
        let dest = resolve_jailed_path(ws_canon_root, &rel)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Gagal membuat folder .airlock/artifacts: {e}"))?;
        }
        fs::write(&dest, &doc.content_md)
            .map_err(|e| format!("Gagal mengekspor artifact ke workspace: {e}"))?;
        Ok(clean_path_string(&dest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_artifact_crud_comments_and_feedback_formatting() {
        let base = std::env::temp_dir().join("smart_mcp_artifacts_crud_test");
        let _ = fs::remove_dir_all(&base);
        let data_dir = base.join("data");
        let ws_dir = base.join("repo");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&ws_dir).unwrap();

        let clean_ws = clean_path_string(&ws_dir);
        let store = ArtifactStore::new_persistent(data_dir.clone());

        // 1. Upsert v1 then v2
        let doc_v1 = store
            .upsert_artifact(
                &clean_ws,
                "repo",
                "sess-abc",
                ArtifactKind::ImplementationPlan,
                "Auth Refactor Plan",
                "# Plan\n1. Add JWT\n2. Add tests\n",
            )
            .await
            .unwrap();
        assert_eq!(doc_v1.version, 1);
        assert_eq!(doc_v1.status, ArtifactStatus::Draft);

        let doc_v2 = store
            .upsert_artifact(
                &clean_ws,
                "repo",
                "sess-abc",
                ArtifactKind::ImplementationPlan,
                "Auth Refactor Plan v2",
                "# Plan\n1. Add OAuth PKCE\n2. Verify tokens\n",
            )
            .await
            .unwrap();
        assert_eq!(doc_v2.id, doc_v1.id);
        assert_eq!(doc_v2.version, 2);

        // 2. Request approval
        let awaiting = store
            .request_approval(&clean_ws, "sess-abc", None)
            .await
            .unwrap();
        assert_eq!(awaiting.status, ArtifactStatus::AwaitingApproval);
        assert!(!store.is_plan_approved(&clean_ws, "sess-abc").await);

        // 3. Add comments (one resolved, one unresolved) & format feedback
        let with_c1 = store
            .add_comment(
                &clean_ws,
                "sess-abc",
                &doc_v2.id,
                Some(2),
                Some("1. Add OAuth PKCE".to_string()),
                "Gunakan PKCE S256 saja, tolak plain.",
            )
            .await
            .unwrap();
        let c1_id = with_c1.comments[0].id.clone();

        let with_c2 = store
            .add_comment(
                &clean_ws,
                "sess-abc",
                &doc_v2.id,
                Some(3),
                None,
                "Tambahkan unit test.",
            )
            .await
            .unwrap();
        let c2_id = with_c2.comments[1].id.clone();

        store
            .resolve_comment(&clean_ws, "sess-abc", &doc_v2.id, &c2_id, true)
            .await
            .unwrap();

        let (rev_doc, feedback) = store
            .request_changes_and_format_feedback(&clean_ws, "sess-abc", &doc_v2.id)
            .await
            .unwrap();
        assert_eq!(rev_doc.status, ArtifactStatus::ChangesRequested);
        assert!(feedback.contains("[Line 2]"));
        assert!(feedback.contains("1. Add OAuth PKCE"));
        assert!(feedback.contains("Gunakan PKCE S256 saja"));
        assert!(!feedback.contains(&c2_id));
        assert!(!feedback.contains("Tambahkan unit test."));
        assert_eq!(rev_doc.comments[0].id, c1_id);

        // 4. Approve marks plan approved & reloads cleanly from disk
        let approved = store
            .approve_artifact(&clean_ws, "sess-abc", &doc_v2.id)
            .await
            .unwrap();
        assert_eq!(approved.status, ArtifactStatus::Approved);
        assert!(store.is_plan_approved(&clean_ws, "sess-abc").await);

        let fresh_store = ArtifactStore::new_persistent(data_dir.clone());
        let reloaded = fresh_store.list_session_artifacts(&clean_ws, "sess-abc").await;
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].version, 2);
        assert_eq!(reloaded[0].status, ArtifactStatus::Approved);

        // 5. Export writes markdown into jailed .airlock/artifacts/
        let canon_ws = fs::canonicalize(&ws_dir).unwrap();
        let exported_path = fresh_store
            .export_artifact_to_workspace(&clean_ws, &canon_ws, "sess-abc", &doc_v2.id)
            .await
            .unwrap();
        assert!(exported_path.ends_with("implementation_plan.md"));
        assert!(ws_dir.join(".airlock/artifacts/implementation_plan.md").is_file());

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_list_workspace_latest_by_kind_merges_webmcp_and_delegated() {
        let base = std::env::temp_dir().join("smart_mcp_artifacts_merge_test");
        let _ = fs::remove_dir_all(&base);
        let data_dir = base.join("data");
        let ws_dir = base.join("repo");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&ws_dir).unwrap();

        let clean_ws = clean_path_string(&ws_dir);
        let store = ArtifactStore::new_persistent(data_dir.clone());

        store
            .upsert_artifact(
                &clean_ws,
                "repo",
                "webmcp-latest",
                ArtifactKind::ImplementationPlan,
                "Main WebMCP Plan",
                "# Plan\nStep 1\n",
            )
            .await
            .unwrap();
        store
            .upsert_artifact(
                &clean_ws,
                "repo",
                "webmcp-latest",
                ArtifactKind::TaskList,
                "Old Tasks",
                "- [ ] Task A\n",
            )
            .await
            .unwrap();
        store
            .upsert_artifact(
                &clean_ws,
                "repo",
                "delegated_abc123",
                ArtifactKind::ReviewNote,
                "Delegated Review",
                "# Review\nLooks good\n",
            )
            .await
            .unwrap();
        store
            .upsert_artifact(
                &clean_ws,
                "repo",
                "delegated_abc123",
                ArtifactKind::TaskList,
                "Updated Tasks",
                "- [x] Task A\n",
            )
            .await
            .unwrap();
        // Also ensure an unrelated desktop chat session is excluded
        store
            .upsert_artifact(
                &clean_ws,
                "repo",
                "chat-desktop-1",
                ArtifactKind::Walkthrough,
                "Desktop Walkthrough",
                "# Desktop\n",
            )
            .await
            .unwrap();

        // Update updated_at deterministically on disk/reloaded store
        let fresh_store = ArtifactStore::new_persistent(data_dir);
        let merged = fresh_store
            .list_workspace_latest_by_kind(&clean_ws, &["webmcp-latest", "delegated_"])
            .await;

        assert_eq!(merged.len(), 3);
        let plan = merged
            .iter()
            .find(|d| d.kind == ArtifactKind::ImplementationPlan)
            .expect("implementation_plan should be preserved");
        assert_eq!(plan.title, "Main WebMCP Plan");
        let review = merged
            .iter()
            .find(|d| d.kind == ArtifactKind::ReviewNote)
            .expect("review_note from delegated run should be merged");
        assert_eq!(review.title, "Delegated Review");
        let tasks = merged
            .iter()
            .find(|d| d.kind == ArtifactKind::TaskList)
            .expect("task_list should exist");
        assert!(
            tasks.title == "Old Tasks" || tasks.title == "Updated Tasks",
            "expected merged task list"
        );
        assert!(!merged.iter().any(|d| d.kind == ArtifactKind::Walkthrough));

        let _ = fs::remove_dir_all(&base);
    }
}
