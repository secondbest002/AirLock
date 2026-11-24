use crate::chat_store::ChatStore;
use crate::config::AppConfig;
use crate::jobs::{JobManager, JobSummary};
use crate::mcp_hub::McpHubManager;
use crate::secrets::{ProviderSecrets, SecretsStore};
use crate::snapshots::SnapshotStore;
use chrono::Local;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tauri::{AppHandle, Emitter};
use tokio::process::Child;
use tokio::sync::{oneshot, Mutex, Notify, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClient {
    #[serde(default)]
    pub client_id: String,
    pub client_secret: String,
    pub auth_method: String,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub trusted: bool,
    #[serde(default)]
    pub created_at_epoch: u64,
    #[serde(default)]
    pub last_used_epoch: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct OAuthCode {
    pub client_id: String,
    pub redirect_uri: String,
    pub challenge: Option<String>,
    pub method: String,
    pub expires_at: Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokenRecord {
    pub client_id: String,
    pub expires_at_epoch: Option<u64>,
    #[serde(default)]
    pub issued_at_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthDecision {
    ApproveOnce,
    ApproveAndTrust,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalDecision {
    ApproveOnce,
    ApproveForSession,
    Deny,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingOAuthInfo {
    pub request_id: String,
    pub client_id: String,
    pub client_name: Option<String>,
    pub redirect_uri: String,
    pub scope: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingToolApproval {
    pub request_id: String,
    pub source: String,
    pub tool_name: String,
    pub summary: String,
    pub args_preview: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OAuthClientSummary {
    pub client_id: String,
    pub client_name: Option<String>,
    pub trusted: bool,
    pub redirect_uris: Vec<String>,
    pub active_access_tokens: usize,
    pub active_refresh_tokens: usize,
    pub created_at_epoch: u64,
    pub last_used_epoch: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownstreamMcpSummary {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub transport: String,
    pub url: String,
    pub has_bearer_token: bool,
    pub command: String,
    pub args: Vec<String>,
    pub enabled: bool,
    pub status: String,
    pub tools_count: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedAuth {
    auth_token: String,
    oauth_clients: HashMap<String, OAuthClient>,
    oauth_access_tokens: HashMap<String, OAuthTokenRecord>,
    oauth_refresh_tokens: HashMap<String, OAuthTokenRecord>,
    #[serde(default)]
    last_workspace_root: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpTrafficEntry {
    pub timestamp: String,
    pub method: String,
    pub path: String,
    pub rpc_method: Option<String>,
    pub status: u16,
    pub duration_ms: u64,
    pub note: String,
    #[serde(default)]
    pub request_preview: Option<String>,
    #[serde(default)]
    pub response_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: String,
    pub timestamp: String,
    pub source: String,
    pub tool_name: String,
    pub target_path: Option<String>,
    pub summary: String,
    pub diff_text: Option<String>,
    pub can_revert: bool,
    pub reverted: bool,
    pub tokens_saved: usize,
    #[serde(skip)]
    pub original_content: Option<String>,
    #[serde(default)]
    pub original_blob_hash: Option<String>,
    #[serde(default)]
    pub file_existed_before: bool,
    #[serde(default)]
    pub post_edit_hash: Option<String>,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub move_from_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenMetrics {
    pub tokens_saved_total: usize,
    pub turns_saved_total: usize,
    pub tool_calls_total: usize,
}

#[derive(Debug, Clone)]
pub struct GeminiConfig {
    pub api_key: String,
    pub model: String,
    pub history: Vec<Value>,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: "gemini-2.5-flash".to_string(),
            history: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    pub workspace_root: String,
    pub server_running: bool,
    pub server_port: u16,
    pub auth_token: String,
    pub tunnel_running: bool,
    pub tunnel_url: Option<String>,
    pub tunnel_status: String,
    pub tunnel_provider: String,
    pub idle_timeout_mins: u64,
    pub close_to_tray: bool,
    pub allow_sensitive_reads: bool,
    pub guardrail_mode: String,
    pub recent_workspaces: Vec<String>,
    pub pinned_workspaces: Vec<String>,
    pub git_branch: Option<String>,
    pub has_cloudflare_token: bool,
    pub cloudflare_public_url: String,
    pub tunnel_logs: Vec<String>,
    pub agent_running: bool,
    pub agent_provider: String,
    pub openai_base_url: String,
    pub openai_model: String,
    pub anthropic_model: String,
    pub auto_inject_project_rules: bool,
    pub gemini_model: String,
    pub has_api_key: bool,
    pub has_gemini_api_key: bool,
    pub has_openai_api_key: bool,
    pub has_anthropic_api_key: bool,
    pub metrics: TokenMetrics,
    pub audit_logs: Vec<AuditEntry>,
    pub http_traffic: Vec<HttpTrafficEntry>,
    pub oauth_clients: Vec<OAuthClientSummary>,
    pub pending_oauth_requests: Vec<PendingOAuthInfo>,
    pub pending_tool_approvals: Vec<PendingToolApproval>,
    pub background_jobs: Vec<JobSummary>,
    pub downstream_mcps: Vec<DownstreamMcpSummary>,
    pub aggregated_tools_count: usize,
}

pub struct AppState {
    pub workspace_root: RwLock<PathBuf>,
    pub auth_token: RwLock<String>,
    pub persist_auth_path: Option<PathBuf>,
    pub persist_config_path: Option<PathBuf>,
    pub config: RwLock<AppConfig>,
    pub snapshot_store: SnapshotStore,
    pub secrets_store: SecretsStore,
    pub chat_store: ChatStore,
    pub provider_secrets: RwLock<ProviderSecrets>,
    pub jobs: JobManager,
    pub mcp_hub: McpHubManager,
    pub server_port: AtomicU16,
    pub server_running: AtomicBool,
    pub server_shutdown: Mutex<Option<oneshot::Sender<()>>>,
    pub tunnel_running: AtomicBool,
    pub tunnel_url: RwLock<Option<String>>,
    pub tunnel_status: RwLock<String>,
    pub tunnel_provider: RwLock<String>,
    pub tunnel_child: Mutex<Option<Child>>,
    pub tunnel_logs: RwLock<Vec<String>>,
    pub idle_timeout_mins: AtomicU64,
    pub close_to_tray: AtomicBool,
    pub allow_sensitive_reads: AtomicBool,
    pub guardrail_mode: RwLock<String>,
    pub session_approved_tools: RwLock<HashSet<String>>,
    pub pending_tool_approvals:
        RwLock<HashMap<String, (PendingToolApproval, oneshot::Sender<ToolApprovalDecision>)>>,
    pub active_turn_group_id: RwLock<Option<String>>,
    pub require_oauth_consent: AtomicBool,
    pub agent_running: AtomicBool,
    pub agent_cancel: Arc<Notify>,
    pub last_activity: RwLock<Instant>,
    pub audit_logs: RwLock<Vec<AuditEntry>>,
    pub http_traffic: RwLock<Vec<HttpTrafficEntry>>,
    pub metrics: RwLock<TokenMetrics>,
    pub gemini_config: RwLock<GeminiConfig>,
    pub openai_history: RwLock<Vec<Value>>,
    pub anthropic_history: RwLock<Vec<Value>>,
    pub app_handle: RwLock<Option<AppHandle>>,
    pub oauth_clients: RwLock<HashMap<String, OAuthClient>>,
    pub oauth_codes: RwLock<HashMap<String, OAuthCode>>,
    pub oauth_access_tokens: RwLock<HashMap<String, OAuthTokenRecord>>,
    pub oauth_refresh_tokens: RwLock<HashMap<String, OAuthTokenRecord>>,
    pub pending_oauth: RwLock<HashMap<String, (PendingOAuthInfo, oneshot::Sender<OAuthDecision>)>>,
}

impl AppState {
    pub fn default_auth_file_path() -> PathBuf {
        crate::snapshots::default_data_dir().join("auth.json")
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(initial_root: PathBuf) -> Self {
        Self::build_state(initial_root, None, None)
    }

    pub fn new_persistent(initial_root: PathBuf, auth_file: PathBuf, config_file: PathBuf) -> Self {
        Self::build_state(initial_root, Some(auth_file), Some(config_file))
    }

    fn build_state(
        initial_root: PathBuf,
        persist_auth_path: Option<PathBuf>,
        persist_config_path: Option<PathBuf>,
    ) -> Self {
        let config = persist_config_path
            .as_ref()
            .map(|p| AppConfig::load(p))
            .unwrap_or_default();

        let data_dir_opt = persist_config_path
            .as_ref()
            .and_then(|p| p.parent().map(|dir| dir.to_path_buf()))
            .or_else(|| {
                persist_auth_path
                    .as_ref()
                    .and_then(|p| p.parent().map(|dir| dir.to_path_buf()))
            });

        let (snapshot_store, secrets_store, chat_store) = match data_dir_opt {
            Some(dir) => (
                SnapshotStore::new_persistent(dir.clone()),
                SecretsStore::new_persistent(dir.clone()),
                ChatStore::new_persistent(dir),
            ),
            None => (
                SnapshotStore::new_none(),
                SecretsStore::new_none(),
                ChatStore::new_none(),
            ),
        };

        let provider_secrets = secrets_store.load();
        let initial_audit_logs = snapshot_store.load_audit_entries();

        let loaded = persist_auth_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str::<PersistedAuth>(&raw).ok());

        let (
            auth_token,
            mut oauth_clients,
            oauth_access_tokens,
            oauth_refresh_tokens,
            resolved_root,
        ) = match loaded {
            Some(p) => {
                let saved_ws = p
                    .last_workspace_root
                    .map(PathBuf::from)
                    .filter(|dir| dir.exists() && dir.is_dir())
                    .and_then(|dir| std::fs::canonicalize(&dir).ok().or(Some(dir)))
                    .unwrap_or(initial_root);
                (
                    p.auth_token,
                    p.oauth_clients,
                    p.oauth_access_tokens,
                    p.oauth_refresh_tokens,
                    saved_ws,
                )
            }
            None => (
                Self::generate_token(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                initial_root,
            ),
        };

        for (cid, client) in oauth_clients.iter_mut() {
            if client.client_id.is_empty() {
                client.client_id = cid.clone();
            }
        }

        let gemini_cfg = GeminiConfig {
            api_key: provider_secrets.gemini_api_key.clone(),
            model: config.gemini_model.clone(),
            history: Vec::new(),
        };

        let server_port = config.server_port;
        let tunnel_provider = config.tunnel_provider.clone();
        let idle_timeout_mins = config.idle_timeout_mins;
        let close_to_tray = config.close_to_tray;
        let allow_sensitive_reads = config.allow_sensitive_reads;
        let guardrail_mode = config.guardrail_mode.clone();

        let state = Self {
            workspace_root: RwLock::new(resolved_root),
            auth_token: RwLock::new(auth_token),
            persist_auth_path: persist_auth_path.clone(),
            persist_config_path: persist_config_path.clone(),
            config: RwLock::new(config),
            snapshot_store,
            secrets_store,
            chat_store,
            provider_secrets: RwLock::new(provider_secrets),
            jobs: JobManager::default(),
            mcp_hub: McpHubManager::default(),
            server_port: AtomicU16::new(server_port),
            server_running: AtomicBool::new(false),
            server_shutdown: Mutex::new(None),
            tunnel_running: AtomicBool::new(false),
            tunnel_url: RwLock::new(None),
            tunnel_status: RwLock::new("Belum aktif".to_string()),
            tunnel_provider: RwLock::new(tunnel_provider),
            tunnel_child: Mutex::new(None),
            tunnel_logs: RwLock::new(Vec::new()),
            idle_timeout_mins: AtomicU64::new(idle_timeout_mins),
            close_to_tray: AtomicBool::new(close_to_tray),
            allow_sensitive_reads: AtomicBool::new(allow_sensitive_reads),
            guardrail_mode: RwLock::new(guardrail_mode),
            session_approved_tools: RwLock::new(HashSet::new()),
            pending_tool_approvals: RwLock::new(HashMap::new()),
            active_turn_group_id: RwLock::new(None),
            require_oauth_consent: AtomicBool::new(true),
            agent_running: AtomicBool::new(false),
            agent_cancel: Arc::new(Notify::new()),
            last_activity: RwLock::new(Instant::now()),
            audit_logs: RwLock::new(initial_audit_logs),
            http_traffic: RwLock::new(Vec::new()),
            metrics: RwLock::new(TokenMetrics::default()),
            gemini_config: RwLock::new(gemini_cfg),
            openai_history: RwLock::new(Vec::new()),
            anthropic_history: RwLock::new(Vec::new()),
            app_handle: RwLock::new(None),
            oauth_clients: RwLock::new(oauth_clients),
            oauth_codes: RwLock::new(HashMap::new()),
            oauth_access_tokens: RwLock::new(oauth_access_tokens),
            oauth_refresh_tokens: RwLock::new(oauth_refresh_tokens),
            pending_oauth: RwLock::new(HashMap::new()),
        };

        if let Some(path) = persist_auth_path {
            if !path.exists() {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Ok(token) = state.auth_token.try_read() {
                    let payload = PersistedAuth {
                        auth_token: token.clone(),
                        oauth_clients: HashMap::new(),
                        oauth_access_tokens: HashMap::new(),
                        oauth_refresh_tokens: HashMap::new(),
                        last_workspace_root: None,
                    };
                    if let Ok(json_str) = serde_json::to_string_pretty(&payload) {
                        let _ = std::fs::write(&path, json_str);
                    }
                }
            }
        }

        if let Some(cfg_path) = persist_config_path {
            if !cfg_path.exists() {
                if let Ok(cfg) = state.config.try_read() {
                    let _ = cfg.save(&cfg_path);
                }
            }
        }

        state
    }

    pub async fn save_auth_state(&self) {
        let Some(path) = &self.persist_auth_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let now = Self::now_epoch_secs();
        let raw_root = self.workspace_root.read().await.display().to_string();
        let clean_root = raw_root
            .strip_prefix(r"\\?\")
            .unwrap_or(&raw_root)
            .to_string();
        let auth_token = self.auth_token.read().await.clone();
        let oauth_clients = self.oauth_clients.read().await.clone();
        let oauth_access_tokens: HashMap<String, OAuthTokenRecord> = self
            .oauth_access_tokens
            .read()
            .await
            .iter()
            .filter(|(_, v)| v.expires_at_epoch.map(|e| e > now).unwrap_or(true))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let oauth_refresh_tokens: HashMap<String, OAuthTokenRecord> = self
            .oauth_refresh_tokens
            .read()
            .await
            .iter()
            .filter(|(_, v)| v.expires_at_epoch.map(|e| e > now).unwrap_or(true))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let payload = PersistedAuth {
            auth_token,
            oauth_clients,
            oauth_access_tokens,
            oauth_refresh_tokens,
            last_workspace_root: Some(clean_root),
        };
        if let Ok(json_str) = serde_json::to_string_pretty(&payload) {
            let tmp_path = path.with_extension("json.tmp");
            if std::fs::write(&tmp_path, &json_str).is_ok()
                && std::fs::rename(&tmp_path, path).is_err()
            {
                let _ = std::fs::write(path, &json_str);
                let _ = std::fs::remove_file(&tmp_path);
            }
        }
    }

    pub async fn save_config_state(&self) {
        let tunnel_provider = self.tunnel_provider.read().await.clone();
        let gemini_model = self.gemini_config.read().await.model.clone();
        let guardrail_mode = self.guardrail_mode.read().await.clone();
        let server_port = self.server_port.load(Ordering::SeqCst);
        let idle_timeout_mins = self.idle_timeout_mins.load(Ordering::SeqCst);
        let close_to_tray = self.close_to_tray.load(Ordering::SeqCst);
        let allow_sensitive_reads = self.allow_sensitive_reads.load(Ordering::SeqCst);

        let mut cfg = self.config.write().await;
        cfg.server_port = server_port;
        cfg.tunnel_provider = tunnel_provider;
        cfg.gemini_model = gemini_model;
        cfg.guardrail_mode = guardrail_mode;
        cfg.idle_timeout_mins = idle_timeout_mins;
        cfg.close_to_tray = close_to_tray;
        cfg.allow_sensitive_reads = allow_sensitive_reads;

        if let Some(path) = &self.persist_config_path {
            let _ = cfg.save(path);
        }
    }

    pub fn read_git_branch(root: &Path) -> Option<String> {
        let dot_git = root.join(".git");
        let head_path = if dot_git.is_dir() {
            dot_git.join("HEAD")
        } else if dot_git.is_file() {
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let gitdir_line = content
                .lines()
                .find(|l| l.trim_start().starts_with("gitdir:"))?;
            let raw_dir = gitdir_line.split_once(':')?.1.trim();
            let dir_path = Path::new(raw_dir);
            let resolved = if dir_path.is_absolute() {
                dir_path.to_path_buf()
            } else {
                root.join(dir_path)
            };
            resolved.join("HEAD")
        } else {
            return None;
        };

        let head_content = std::fs::read_to_string(head_path).ok()?;
        let trimmed = head_content.trim();
        if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
            if !branch.is_empty() {
                return Some(branch.to_string());
            }
        }
        if let Some(other_ref) = trimmed.strip_prefix("ref: ") {
            if let Some(last) = other_ref.rsplit('/').next() {
                if !last.is_empty() {
                    return Some(last.to_string());
                }
            }
        }
        if trimmed.len() >= 7 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(format!("detached:{}", &trimmed[..7]));
        }
        None
    }

    pub async fn append_tunnel_log(&self, raw_line: String) {
        let cleaned_raw = raw_line.trim_end();
        if cleaned_raw.is_empty() {
            return;
        }
        let cf_token = self.config.read().await.cloudflare_tunnel_token.clone();
        let auth_tok = self.auth_token.read().await.clone();

        let mut redacted = cleaned_raw.to_string();
        if !cf_token.is_empty() {
            redacted = redacted.replace(&cf_token, "<redacted>");
        }
        if !auth_tok.is_empty() {
            redacted = redacted.replace(&auth_tok, "<redacted>");
        }

        let formatted = format!("[{}] {}", Self::now_timestamp(), redacted);
        {
            let mut logs = self.tunnel_logs.write().await;
            logs.push(formatted.clone());
            if logs.len() > 120 {
                let excess = logs.len() - 120;
                logs.drain(0..excess);
            }
        }
        let handle = self.app_handle.read().await.clone();
        if let Some(app) = handle {
            let _ = app.emit("tunnel-log-added", &formatted);
        }
    }

    pub fn now_epoch_secs() -> u64 {
        chrono::Utc::now().timestamp().max(0) as u64
    }

    pub fn generate_token() -> String {
        let mut rng = rand::thread_rng();
        let bytes: [u8; 16] = rng.gen();
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn next_audit_id() -> String {
        let mut rng = rand::thread_rng();
        let suffix: u32 = rng.gen_range(1000..9999);
        format!("aud-{}-{}", Local::now().format("%H%M%S"), suffix)
    }

    pub fn now_timestamp() -> String {
        Local::now().format("%H:%M:%S").to_string()
    }

    pub async fn touch_activity(&self) {
        let mut last = self.last_activity.write().await;
        *last = Instant::now();
    }

    pub async fn snapshot(&self) -> DashboardSnapshot {
        let root_path = self.workspace_root.read().await.clone();
        let git_branch = Self::read_git_branch(&root_path);
        let raw_root = root_path.display().to_string();
        let workspace_root = raw_root
            .strip_prefix(r"\\?\")
            .unwrap_or(&raw_root)
            .to_string();
        let auth_token = self.auth_token.read().await.clone();
        let tunnel_url = self.tunnel_url.read().await.clone();
        let tunnel_status = self.tunnel_status.read().await.clone();
        let tunnel_provider = self.tunnel_provider.read().await.clone();
        let tunnel_logs = self.tunnel_logs.read().await.clone();
        let guardrail_mode = self.guardrail_mode.read().await.clone();

        let (
            recent_workspaces,
            pinned_workspaces,
            has_cloudflare_token,
            cloudflare_public_url,
            agent_provider,
            openai_base_url,
            openai_model,
            anthropic_model,
            auto_inject_project_rules,
            downstream_cfgs,
        ) = {
            let cfg = self.config.read().await;
            (
                cfg.recent_workspaces.clone(),
                cfg.pinned_workspaces.clone(),
                !cfg.cloudflare_tunnel_token.trim().is_empty(),
                cfg.cloudflare_public_url.clone(),
                cfg.agent_provider.clone(),
                cfg.openai_base_url.clone(),
                cfg.openai_model.clone(),
                cfg.anthropic_model.clone(),
                cfg.auto_inject_project_rules,
                cfg.downstream_mcps.clone(),
            )
        };

        let gemini_model = self.gemini_config.read().await.model.clone();
        let (has_gemini_api_key, has_openai_api_key, has_anthropic_api_key, has_api_key) = {
            let secs = self.provider_secrets.read().await;
            let gem_mem = !self.gemini_config.read().await.api_key.trim().is_empty();
            let hg = secs.has_key_for("gemini") || gem_mem;
            let ho = secs.has_key_for("openai_compat");
            let ha = secs.has_key_for("anthropic");
            let active = match agent_provider.as_str() {
                "openai_compat" => ho,
                "anthropic" => ha,
                _ => hg,
            };
            (hg, ho, ha, active)
        };

        let metrics = self.metrics.read().await.clone();
        let audit_logs = self.audit_logs.read().await.clone();
        let http_traffic = self.http_traffic.read().await.clone();

        let now = Self::now_epoch_secs();
        let access_tokens = self.oauth_access_tokens.read().await;
        let refresh_tokens = self.oauth_refresh_tokens.read().await;
        let clients_map = self.oauth_clients.read().await;

        let mut oauth_clients: Vec<OAuthClientSummary> = clients_map
            .iter()
            .map(|(cid, client)| {
                let effective_id = if client.client_id.is_empty() {
                    cid.clone()
                } else {
                    client.client_id.clone()
                };
                let active_access_tokens = access_tokens
                    .values()
                    .filter(|r| {
                        r.client_id == effective_id
                            && r.expires_at_epoch.map(|e| e > now).unwrap_or(true)
                    })
                    .count();
                let active_refresh_tokens = refresh_tokens
                    .values()
                    .filter(|r| {
                        r.client_id == effective_id
                            && r.expires_at_epoch.map(|e| e > now).unwrap_or(true)
                    })
                    .count();

                OAuthClientSummary {
                    client_id: effective_id,
                    client_name: client.client_name.clone(),
                    trusted: client.trusted,
                    redirect_uris: client.redirect_uris.clone(),
                    active_access_tokens,
                    active_refresh_tokens,
                    created_at_epoch: client.created_at_epoch,
                    last_used_epoch: client.last_used_epoch,
                }
            })
            .collect();
        oauth_clients.sort_by(|a, b| {
            b.created_at_epoch
                .cmp(&a.created_at_epoch)
                .then_with(|| a.client_id.cmp(&b.client_id))
        });

        let pending_oauth_requests: Vec<PendingOAuthInfo> = self
            .pending_oauth
            .read()
            .await
            .values()
            .map(|(info, _)| info.clone())
            .collect();

        let pending_tool_approvals: Vec<PendingToolApproval> = self
            .pending_tool_approvals
            .read()
            .await
            .values()
            .map(|(info, _)| info.clone())
            .collect();

        let background_jobs = crate::jobs::list_jobs(self).await;
        let downstream_mcps = self.mcp_hub.list_summaries(&downstream_cfgs).await;
        let aggregated_tools_count: usize = downstream_mcps
            .iter()
            .filter(|m| m.enabled)
            .map(|m| m.tools_count)
            .sum();

        DashboardSnapshot {
            workspace_root,
            server_running: self.server_running.load(Ordering::SeqCst),
            server_port: self.server_port.load(Ordering::SeqCst),
            auth_token,
            tunnel_running: self.tunnel_running.load(Ordering::SeqCst),
            tunnel_url,
            tunnel_status,
            tunnel_provider,
            idle_timeout_mins: self.idle_timeout_mins.load(Ordering::SeqCst),
            close_to_tray: self.close_to_tray.load(Ordering::SeqCst),
            allow_sensitive_reads: self.allow_sensitive_reads.load(Ordering::SeqCst),
            guardrail_mode,
            recent_workspaces,
            pinned_workspaces,
            git_branch,
            has_cloudflare_token,
            cloudflare_public_url,
            tunnel_logs,
            agent_running: self.agent_running.load(Ordering::SeqCst),
            agent_provider,
            openai_base_url,
            openai_model,
            anthropic_model,
            auto_inject_project_rules,
            gemini_model,
            has_api_key,
            has_gemini_api_key,
            has_openai_api_key,
            has_anthropic_api_key,
            metrics,
            audit_logs,
            http_traffic,
            oauth_clients,
            pending_oauth_requests,
            pending_tool_approvals,
            background_jobs,
            downstream_mcps,
            aggregated_tools_count,
        }
    }

    pub async fn emit_dashboard(&self) {
        let handle = self.app_handle.read().await.clone();
        if let Some(app) = handle {
            let snap = self.snapshot().await;
            let _ = app.emit("dashboard-updated", snap);
        }
    }

    pub async fn record_http_traffic(&self, entry: HttpTrafficEntry) {
        let emit_payload = entry.clone();
        {
            let mut traffic = self.http_traffic.write().await;
            traffic.push(entry);
            if traffic.len() > 60 {
                let excess = traffic.len() - 60;
                traffic.drain(0..excess);
            }
        }
        self.touch_activity().await;
        let handle = self.app_handle.read().await.clone();
        if let Some(app) = handle {
            let _ = app.emit("http-traffic-added", emit_payload);
        }
    }

    pub async fn record_audit(&self, entry: AuditEntry, turns_saved: usize) {
        let metrics_snapshot = {
            let mut metrics = self.metrics.write().await;
            metrics.tool_calls_total += 1;
            metrics.tokens_saved_total += entry.tokens_saved;
            metrics.turns_saved_total += turns_saved;
            metrics.clone()
        };

        let emit_payload = serde_json::json!({
            "entry": &entry,
            "metrics": &metrics_snapshot,
        });

        {
            let mut logs = self.audit_logs.write().await;
            logs.push(entry);
            if logs.len() > 200 {
                let excess = logs.len() - 200;
                logs.drain(0..excess);
            }
            self.snapshot_store.save_audit_entries(&logs);
        }

        self.touch_activity().await;
        let handle = self.app_handle.read().await.clone();
        if let Some(app) = handle {
            let _ = app.emit("audit-added", emit_payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_read_git_branch() {
        let base = std::env::temp_dir().join("smart_mcp_git_branch_test");
        let _ = fs::remove_dir_all(&base);

        let non_git = base.join("plain");
        fs::create_dir_all(&non_git).unwrap();
        assert_eq!(AppState::read_git_branch(&non_git), None);

        let normal = base.join("normal");
        fs::create_dir_all(normal.join(".git")).unwrap();
        fs::write(normal.join(".git").join("HEAD"), "ref: refs/heads/feat/phase2\n").unwrap();
        assert_eq!(
            AppState::read_git_branch(&normal).as_deref(),
            Some("feat/phase2")
        );

        let detached = base.join("detached");
        fs::create_dir_all(detached.join(".git")).unwrap();
        fs::write(
            detached.join(".git").join("HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(
            AppState::read_git_branch(&detached).as_deref(),
            Some("detached:0123456")
        );

        let worktree = base.join("worktree");
        let ext_git = base.join("ext_git");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&ext_git).unwrap();
        fs::write(ext_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(worktree.join(".git"), "gitdir: ../ext_git\n").unwrap();
        assert_eq!(AppState::read_git_branch(&worktree).as_deref(), Some("main"));

        let _ = fs::remove_dir_all(&base);
    }
}
