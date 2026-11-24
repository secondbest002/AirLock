use crate::chat_store::ChatStore;
use crate::config::AppConfig;
use crate::jobs::{JobManager, JobSummary};
use crate::mcp_hub::McpHubManager;
use crate::secrets::{ProviderSecrets, SecretsStore};
use crate::snapshots::SnapshotStore;
use crate::workspace::{
    build_workspace_summaries, clean_path_string, default_alias_for_path, ensure_unique_alias,
    normalize_clean_path, WorkspaceProfile, WorkspaceSummary,
};
use chrono::Local;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::time::Instant;
use tauri::{AppHandle, Emitter};
use tokio::process::Child;
use tokio::sync::{oneshot, watch, Mutex, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClient {
    #[serde(default)]
    pub client_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingElicitation {
    pub request_id: String,
    pub server_id: String,
    pub server_name: String,
    pub message: String,
    pub requested_schema: Option<Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
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
    pub plugin_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedAuth {
    #[serde(default, skip_serializing_if = "String::is_empty")]
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
pub struct TurnCheckpointInfo {
    pub can_resume: bool,
    pub reason: String,
    pub completed_steps: usize,
    #[serde(default)]
    pub last_tool: Option<String>,
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
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub workspace_alias: Option<String>,
    #[serde(default)]
    pub workspace_root: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenMetrics {
    pub tokens_saved_total: usize,
    pub turns_saved_total: usize,
    pub tool_calls_total: usize,
}

#[derive(Debug, Clone)]
pub struct GeminiConfig {
    pub model: String,
    pub history: Vec<Value>,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            model: "gemini-2.5-flash".to_string(),
            history: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    pub workspace_root: String,
    pub default_workspace_id: String,
    pub workspaces: Vec<WorkspaceSummary>,
    pub builtin_tools_count: usize,
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
    pub command_execution_policy: String,
    pub command_allowlist: Vec<String>,
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
    pub gemini_thinking_mode: String,
    pub agent_max_steps: usize,
    pub agent_turn_timeout_secs: u64,
    pub stream_idle_timeout_secs: u64,
    pub pending_checkpoint: Option<TurnCheckpointInfo>,
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
    pub pending_elicitations: Vec<PendingElicitation>,
    pub background_jobs: Vec<JobSummary>,
    pub downstream_mcps: Vec<DownstreamMcpSummary>,
    pub aggregated_tools_count: usize,
    pub skills: Vec<crate::skills::SkillSummary>,
    pub plugins: Vec<crate::plugins::PluginSummary>,
    pub active_runs: Vec<crate::agent_manager::AgentRunSummary>,
    pub hooks_status: crate::hooks::HooksStatusSummary,
    pub active_lsp_servers: Vec<crate::lsp_bridge::LspServerSummary>,
}

pub struct AppState {
    pub workspace_root: RwLock<PathBuf>,
    pub default_workspace_id: RwLock<String>,
    pub auth_token: RwLock<String>,
    pub persist_auth_path: Option<PathBuf>,
    pub persist_config_path: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub config: RwLock<AppConfig>,
    pub snapshot_store: SnapshotStore,
    pub secrets_store: SecretsStore,
    pub chat_store: ChatStore,
    pub artifact_store: crate::artifacts::ArtifactStore,
    pub agent_manager: crate::agent_manager::AgentRunManager,
    pub provider_secrets: RwLock<ProviderSecrets>,
    pub skills_cache: RwLock<Vec<crate::skills::SkillEntry>>,
    pub plugin_staging: RwLock<HashMap<String, crate::plugins::PluginStagingEntry>>,
    pub jobs: JobManager,
    pub mcp_tasks: crate::mcp_tasks::McpTaskManager,
    pub mcp_hub: McpHubManager,
    pub lsp_manager: crate::lsp_bridge::LspManager,
    pub acp_bridge: crate::acp_server::AcpBridgeHandle,
    pub http_client: reqwest::Client,
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
    pub pending_elicitations:
        RwLock<HashMap<String, (PendingElicitation, oneshot::Sender<ElicitationResponse>)>>,
    pub active_turn_group_id: RwLock<Option<String>>,
    pub require_oauth_consent: AtomicBool,
    pub agent_running: AtomicBool,
    pub agent_cancel_tx: watch::Sender<bool>,
    pub pending_checkpoint: RwLock<Option<TurnCheckpointInfo>>,
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
        let mut config = persist_config_path
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

        let (snapshot_store, secrets_store, chat_store, artifact_store) = match data_dir_opt.clone()
        {
            Some(dir) => (
                SnapshotStore::new_persistent(dir.clone()),
                SecretsStore::new_persistent(dir.clone()),
                ChatStore::new_persistent(dir.clone()),
                crate::artifacts::ArtifactStore::new_persistent(dir),
            ),
            None => (
                SnapshotStore::new_none(),
                SecretsStore::new_none(),
                ChatStore::new_none(),
                crate::artifacts::ArtifactStore::new_none(),
            ),
        };

        let mut provider_secrets = secrets_store.load();
        let initial_audit_logs = snapshot_store.load_audit_entries();

        let mut secrets_dirty = false;
        let mut config_dirty = false;
        let mut auth_dirty = false;

        // Migrate legacy plaintext secrets from config.json into DPAPI SecretsStore
        if !config.cloudflare_tunnel_token.trim().is_empty() {
            if provider_secrets.cloudflare_tunnel_token.trim().is_empty() {
                provider_secrets.cloudflare_tunnel_token =
                    config.cloudflare_tunnel_token.trim().to_string();
                secrets_dirty = true;
            }
            config.cloudflare_tunnel_token.clear();
            config_dirty = true;
        }
        for mcp in &mut config.downstream_mcps {
            if !mcp.bearer_token.trim().is_empty() {
                let entry = provider_secrets
                    .downstream_bearer_tokens
                    .entry(mcp.id.clone())
                    .or_default();
                if entry.trim().is_empty() {
                    *entry = mcp.bearer_token.trim().to_string();
                    secrets_dirty = true;
                }
                mcp.bearer_token.clear();
                config_dirty = true;
            }
            if !mcp.env.is_empty() {
                let entry = provider_secrets
                    .downstream_envs
                    .entry(mcp.id.clone())
                    .or_default();
                if entry.is_empty() {
                    *entry = mcp.env.clone();
                    secrets_dirty = true;
                }
                mcp.env.clear();
                config_dirty = true;
            }
        }

        let loaded = persist_auth_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str::<PersistedAuth>(&raw).ok());

        let (
            loaded_auth_token,
            mut oauth_clients,
            raw_access_tokens,
            raw_refresh_tokens,
            loaded_last_ws_root,
            resolved_root,
        ) = match loaded {
            Some(p) => {
                let saved_ws = p
                    .last_workspace_root
                    .as_deref()
                    .map(PathBuf::from)
                    .filter(|dir| dir.exists() && dir.is_dir())
                    .and_then(|dir| std::fs::canonicalize(&dir).ok().or(Some(dir)))
                    .unwrap_or(initial_root);
                (
                    p.auth_token,
                    p.oauth_clients,
                    p.oauth_access_tokens,
                    p.oauth_refresh_tokens,
                    p.last_workspace_root,
                    saved_ws,
                )
            }
            None => (
                String::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                None,
                initial_root,
            ),
        };

        let auth_token = if !provider_secrets.auth_token.trim().is_empty() {
            if !loaded_auth_token.trim().is_empty() {
                auth_dirty = true;
            }
            provider_secrets.auth_token.clone()
        } else if !loaded_auth_token.trim().is_empty() {
            provider_secrets.auth_token = loaded_auth_token;
            secrets_dirty = true;
            auth_dirty = true;
            provider_secrets.auth_token.clone()
        } else {
            let generated = Self::generate_token();
            provider_secrets.auth_token = generated.clone();
            secrets_dirty = true;
            generated
        };

        for (cid, client) in oauth_clients.iter_mut() {
            if client.client_id.is_empty() {
                client.client_id = cid.clone();
            }
            if !client.client_secret.is_empty() {
                let entry = provider_secrets
                    .oauth_client_secrets
                    .entry(client.client_id.clone())
                    .or_default();
                if entry.is_empty() {
                    *entry = client.client_secret.clone();
                    secrets_dirty = true;
                }
                auth_dirty = true;
            } else if let Some(sec) = provider_secrets.oauth_client_secrets.get(&client.client_id) {
                if !sec.is_empty() {
                    client.client_secret = sec.clone();
                }
            }
        }

        let mut oauth_access_tokens = HashMap::new();
        for (k, v) in raw_access_tokens {
            if k.starts_with("sha256:") {
                oauth_access_tokens.insert(k, v);
            } else {
                oauth_access_tokens.insert(crate::security::hash_oauth_token(&k), v);
                auth_dirty = true;
            }
        }

        let mut oauth_refresh_tokens = HashMap::new();
        for (k, v) in raw_refresh_tokens {
            if k.starts_with("sha256:") {
                oauth_refresh_tokens.insert(k, v);
            } else {
                oauth_refresh_tokens.insert(crate::security::hash_oauth_token(&k), v);
                auth_dirty = true;
            }
        }

        if secrets_dirty && secrets_store.path.is_some() {
            let _ = secrets_store.save(&provider_secrets);
        }

        if let Some(path) = persist_auth_path.as_ref() {
            if !path.exists() || auth_dirty {
                let sanitized_clients: HashMap<String, OAuthClient> = oauth_clients
                    .iter()
                    .map(|(cid, c)| {
                        let mut c_clean = c.clone();
                        c_clean.client_secret.clear();
                        (cid.clone(), c_clean)
                    })
                    .collect();
                let payload = PersistedAuth {
                    auth_token: String::new(),
                    oauth_clients: sanitized_clients,
                    oauth_access_tokens: oauth_access_tokens.clone(),
                    oauth_refresh_tokens: oauth_refresh_tokens.clone(),
                    last_workspace_root: loaded_last_ws_root,
                };
                Self::write_persisted_auth_file(path, &payload);
            }
        }

        // Initialize / migrate multi-workspace registry cleanly (do NOT auto-enable legacy pinned repos)
        if config.workspaces.is_empty() {
            let (canon_init, clean_init) = normalize_clean_path(&resolved_root)
                .unwrap_or_else(|_| (resolved_root.clone(), clean_path_string(&resolved_root)));
            let alias = ensure_unique_alias(&default_alias_for_path(&canon_init), &[], None);
            let id = format!("ws-{}", &Self::generate_token()[..8]);
            config.workspaces.push(WorkspaceProfile {
                id: id.clone(),
                alias,
                path: clean_init,
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: Vec::new(),
                pinned: true,
            });
            config.default_workspace_id = Some(id);
            config_dirty = true;
        } else {
            // Clean any invalid empty ids/aliases from manual edits
            let snapshot_profiles = config.workspaces.clone();
            let mut seen_ids = Vec::new();
            for prof in &mut config.workspaces {
                if prof.id.trim().is_empty() || seen_ids.contains(&prof.id) {
                    prof.id = format!("ws-{}", &Self::generate_token()[..8]);
                    config_dirty = true;
                }
                seen_ids.push(prof.id.clone());
                if prof.alias.trim().is_empty() {
                    prof.alias = ensure_unique_alias(
                        &default_alias_for_path(Path::new(&prof.path)),
                        &snapshot_profiles,
                        Some(&prof.id),
                    );
                    config_dirty = true;
                }
            }
            if !config.workspaces.iter().any(|w| w.enabled) {
                if let Some(first) = config.workspaces.first_mut() {
                    first.enabled = true;
                    config_dirty = true;
                }
            }
            let current_def_valid = config
                .default_workspace_id
                .as_deref()
                .map(|did| config.workspaces.iter().any(|w| w.id == did && w.enabled))
                .unwrap_or(false);
            if !current_def_valid {
                config.default_workspace_id = config
                    .workspaces
                    .iter()
                    .find(|w| w.enabled)
                    .map(|w| w.id.clone());
                config_dirty = true;
            }
        }

        let default_workspace_id = config
            .default_workspace_id
            .clone()
            .or_else(|| config.workspaces.first().map(|w| w.id.clone()))
            .unwrap_or_default();

        let effective_root = config
            .workspaces
            .iter()
            .find(|w| w.id == default_workspace_id)
            .and_then(|w| normalize_clean_path(Path::new(&w.path)).ok().map(|(c, _)| c))
            .unwrap_or(resolved_root);

        if let Some(cfg_path) = persist_config_path.as_ref() {
            if !cfg_path.exists() || config_dirty {
                let _ = config.sanitized_for_disk().save(cfg_path);
            }
        }

        let gemini_cfg = GeminiConfig {
            model: config.gemini_model.clone(),
            history: Vec::new(),
        };

        let server_port = config.server_port;
        let tunnel_provider = config.tunnel_provider.clone();
        let idle_timeout_mins = config.idle_timeout_mins;
        let close_to_tray = config.close_to_tray;
        let allow_sensitive_reads = config.allow_sensitive_reads;
        let guardrail_mode = config.guardrail_mode.clone();

        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let (agent_cancel_tx, _agent_cancel_rx) = watch::channel(false);

        Self {
            workspace_root: RwLock::new(effective_root),
            default_workspace_id: RwLock::new(default_workspace_id),
            auth_token: RwLock::new(auth_token),
            persist_auth_path,
            persist_config_path,
            data_dir: data_dir_opt,
            config: RwLock::new(config),
            snapshot_store,
            secrets_store,
            chat_store,
            artifact_store,
            agent_manager: crate::agent_manager::AgentRunManager::default(),
            provider_secrets: RwLock::new(provider_secrets),
            skills_cache: RwLock::new(Vec::new()),
            plugin_staging: RwLock::new(HashMap::new()),
            jobs: JobManager::default(),
            mcp_tasks: crate::mcp_tasks::McpTaskManager::default(),
            mcp_hub: McpHubManager::default(),
            lsp_manager: crate::lsp_bridge::LspManager::default(),
            acp_bridge: crate::acp_server::AcpBridgeHandle::default(),
            http_client,
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
            pending_elicitations: RwLock::new(HashMap::new()),
            active_turn_group_id: RwLock::new(None),
            require_oauth_consent: AtomicBool::new(true),
            agent_running: AtomicBool::new(false),
            agent_cancel_tx,
            pending_checkpoint: RwLock::new(None),
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
        }
    }

    fn write_persisted_auth_file(path: &Path, payload: &PersistedAuth) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json_str) = serde_json::to_string_pretty(payload) {
            let tmp_path = path.with_extension("json.tmp");
            if std::fs::write(&tmp_path, &json_str).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
                }
                if std::fs::rename(&tmp_path, path).is_err() {
                    let _ = std::fs::write(path, &json_str);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                    }
                    let _ = std::fs::remove_file(&tmp_path);
                }
            }
        }
    }

    pub fn cancel_agent(&self) {
        let _ = self.agent_cancel_tx.send(true);
    }

    pub fn reset_agent_cancel(&self) -> watch::Receiver<bool> {
        let _ = self.agent_cancel_tx.send(false);
        self.agent_cancel_tx.subscribe()
    }

    pub async fn sync_default_workspace_root(&self) {
        let (target_id, target_path_opt) = {
            let mut cfg = self.config.write().await;
            let state_def = self.default_workspace_id.read().await.clone();
            let chosen_id = cfg
                .workspaces
                .iter()
                .find(|w| w.id == state_def && w.enabled)
                .or_else(|| {
                    cfg.default_workspace_id
                        .as_deref()
                        .and_then(|did| cfg.workspaces.iter().find(|w| w.id == did && w.enabled))
                })
                .or_else(|| cfg.workspaces.iter().find(|w| w.enabled))
                .or_else(|| cfg.workspaces.first())
                .map(|w| w.id.clone())
                .unwrap_or_default();
            cfg.default_workspace_id = if chosen_id.is_empty() {
                None
            } else {
                Some(chosen_id.clone())
            };
            let path = cfg
                .workspaces
                .iter()
                .find(|w| w.id == chosen_id)
                .map(|w| w.path.clone());
            (chosen_id, path)
        };
        *self.default_workspace_id.write().await = target_id;
        if let Some(p) = target_path_opt {
            let path_buf = PathBuf::from(&p);
            let canon = std::fs::canonicalize(&path_buf).unwrap_or(path_buf);
            *self.workspace_root.write().await = canon;
        }
    }

    pub fn plugins_dir(&self) -> PathBuf {
        self.data_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("airlock_ephemeral_plugins"))
            .join("plugins")
    }

    pub fn global_skills_dir(&self) -> Option<PathBuf> {
        self.data_dir.as_ref().map(|d| d.join("skills"))
    }

    pub async fn refresh_skills(&self) -> Vec<crate::skills::SkillEntry> {
        let (workspaces, cfg_def, plugins, disabled_vec) = {
            let cfg = self.config.read().await;
            (
                cfg.workspaces.clone(),
                cfg.default_workspace_id.clone(),
                cfg.installed_plugins.clone(),
                cfg.disabled_skills.clone(),
            )
        };
        let state_def = self.default_workspace_id.read().await.clone();
        let def_id = if !state_def.is_empty() {
            state_def
        } else {
            cfg_def.unwrap_or_default()
        };

        let mut ws_roots: Vec<(String, PathBuf)> = Vec::new();
        let mut rest: Vec<(String, PathBuf)> = Vec::new();
        for w in &workspaces {
            if !w.enabled {
                continue;
            }
            let p = PathBuf::from(&w.path);
            let canon = std::fs::canonicalize(&p).unwrap_or(p);
            if !canon.is_dir() {
                continue;
            }
            if w.id == def_id {
                ws_roots.push((w.alias.clone(), canon));
            } else {
                rest.push((w.alias.clone(), canon));
            }
        }
        rest.sort_by(|a, b| a.0.cmp(&b.0));
        ws_roots.extend(rest);

        if ws_roots.is_empty() {
            let root = self.workspace_root.read().await.clone();
            if root.is_dir() {
                ws_roots.push(("default".to_string(), root));
            }
        }

        let disabled_set: HashSet<String> = disabled_vec.into_iter().collect();
        let entries = crate::skills::discover_skills(
            &ws_roots,
            &plugins,
            self.global_skills_dir().as_deref(),
            &disabled_set,
        );
        *self.skills_cache.write().await = entries.clone();
        entries
    }

    pub async fn skills_for_context(
        &self,
        pinned_ws: Option<&str>,
    ) -> Vec<crate::skills::SkillEntry> {
        let mut entries = self.refresh_skills().await;
        if let Some(pin) = pinned_ws.map(str::trim).filter(|s| !s.is_empty()) {
            match crate::workspace::resolve_workspace(self, None, Some(pin)).await {
                Ok(ws) => {
                    let allowed_src = format!("workspace:{}", ws.alias);
                    entries.retain(|e| {
                        e.summary.scope != crate::skills::SkillScope::Workspace
                            || e.summary.source_label == allowed_src
                    });
                }
                Err(_) => {
                    entries.retain(|e| e.summary.scope != crate::skills::SkillScope::Workspace);
                }
            }
        }
        entries
    }

    pub async fn resolved_downstream_mcps(&self) -> Vec<crate::config::DownstreamMcpConfig> {
        let (mut list, plugins) = {
            let cfg = self.config.read().await;
            (cfg.downstream_mcps.clone(), cfg.installed_plugins.clone())
        };
        let secs = self.provider_secrets.read().await;
        let mut used_namespaces: HashSet<String> = HashSet::new();
        for item in &mut list {
            item.plugin_id = None;
            used_namespaces.insert(crate::mcp_hub::sanitize_namespace(&item.namespace));
            if item.bearer_token.trim().is_empty() {
                if let Some(tok) = secs.downstream_bearer_tokens.get(&item.id) {
                    item.bearer_token = tok.clone();
                }
            }
            if item.env.is_empty() {
                if let Some(env_map) = secs.downstream_envs.get(&item.id) {
                    item.env = env_map.clone();
                }
            }
        }
        for plugin in &plugins {
            let plg_mcps = crate::plugins::materialize_plugin_mcp_configs(
                plugin,
                secs.plugin_secrets.get(&plugin.id),
                &mut used_namespaces,
            );
            list.extend(plg_mcps);
        }
        list
    }

    pub async fn save_auth_state(&self) {
        let auth_token = self.auth_token.read().await.clone();
        let oauth_clients = self.oauth_clients.read().await.clone();
        {
            let mut secs = self.provider_secrets.write().await;
            secs.auth_token = auth_token;
            secs.oauth_client_secrets
                .retain(|cid, _| oauth_clients.contains_key(cid));
            for (cid, client) in &oauth_clients {
                if !client.client_secret.is_empty() {
                    secs.oauth_client_secrets
                        .insert(cid.clone(), client.client_secret.clone());
                }
            }
            let _ = self.secrets_store.save(&secs);
        }

        let Some(path) = &self.persist_auth_path else {
            return;
        };
        let now = Self::now_epoch_secs();
        let raw_root = self.workspace_root.read().await.display().to_string();
        let clean_root = raw_root
            .strip_prefix(r"\\?\")
            .unwrap_or(&raw_root)
            .to_string();
        let sanitized_clients: HashMap<String, OAuthClient> = oauth_clients
            .into_iter()
            .map(|(cid, mut c)| {
                c.client_secret.clear();
                (cid, c)
            })
            .collect();
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
            auth_token: String::new(),
            oauth_clients: sanitized_clients,
            oauth_access_tokens,
            oauth_refresh_tokens,
            last_workspace_root: Some(clean_root),
        };
        Self::write_persisted_auth_file(path, &payload);
    }

    pub async fn command_policy_and_allowlist(&self) -> (String, Vec<String>) {
        let cfg = self.config.read().await;
        (
            cfg.command_execution_policy.clone(),
            cfg.command_allowlist.clone(),
        )
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
            let _ = cfg.sanitized_for_disk().save(path);
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
        let cf_token = self
            .provider_secrets
            .read()
            .await
            .cloudflare_tunnel_token
            .clone();
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
            cloudflare_public_url,
            agent_provider,
            openai_base_url,
            openai_model,
            anthropic_model,
            auto_inject_project_rules,
            workspaces_raw,
            cfg_default_id,
            gemini_thinking_mode,
            agent_max_steps,
            agent_turn_timeout_secs,
            stream_idle_timeout_secs,
            command_execution_policy,
            command_allowlist,
        ) = {
            let cfg = self.config.read().await;
            (
                cfg.recent_workspaces.clone(),
                cfg.pinned_workspaces.clone(),
                cfg.cloudflare_public_url.clone(),
                cfg.agent_provider.clone(),
                cfg.openai_base_url.clone(),
                cfg.openai_model.clone(),
                cfg.anthropic_model.clone(),
                cfg.auto_inject_project_rules,
                cfg.workspaces.clone(),
                cfg.default_workspace_id.clone(),
                cfg.gemini_thinking_mode.clone(),
                cfg.agent_max_steps,
                cfg.agent_turn_timeout_secs,
                cfg.stream_idle_timeout_secs,
                cfg.command_execution_policy.clone(),
                cfg.command_allowlist.clone(),
            )
        };
        let state_default_id = self.default_workspace_id.read().await.clone();
        let default_workspace_id = if !state_default_id.is_empty() {
            state_default_id
        } else {
            cfg_default_id.unwrap_or_default()
        };
        let workspaces = build_workspace_summaries(
            &workspaces_raw,
            &default_workspace_id,
            &guardrail_mode,
        );
        let pending_checkpoint = self.pending_checkpoint.read().await.clone();
        let builtin_tools_count = crate::tools::mcp_tools_list()
            .as_array()
            .map(|a| a.len())
            .unwrap_or(28);

        let gemini_model = self.gemini_config.read().await.model.clone();
        let (
            has_cloudflare_token,
            has_gemini_api_key,
            has_openai_api_key,
            has_anthropic_api_key,
            has_api_key,
        ) = {
            let secs = self.provider_secrets.read().await;
            let hcf = !secs.cloudflare_tunnel_token.trim().is_empty();
            let hg = secs.has_key_for("gemini");
            let ho = secs.has_key_for("openai_compat");
            let ha = secs.has_key_for("anthropic");
            let active = match agent_provider.as_str() {
                "openai_compat" => ho,
                "anthropic" => ha,
                _ => hg,
            };
            (hcf, hg, ho, ha, active)
        };
        let downstream_cfgs = self.resolved_downstream_mcps().await;

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

        let pending_elicitations: Vec<PendingElicitation> = self
            .pending_elicitations
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
        let skills_entries = self.refresh_skills().await;
        let skills: Vec<crate::skills::SkillSummary> =
            skills_entries.iter().map(|s| s.summary.clone()).collect();
        let plugins = {
            let cfg = self.config.read().await;
            let secs = self.provider_secrets.read().await;
            crate::plugins::build_plugin_summaries(&cfg.installed_plugins, &secs, &skills_entries)
        };
        let active_runs = self.agent_manager.list_runs().await;
        let active_lsp_servers = self.lsp_manager.list_summaries().await;
        let has_active_runs = active_runs
            .iter()
            .any(|r| r.status == crate::agent_manager::AgentRunStatus::Running);
        let hooks_status = match crate::workspace::resolve_workspace(self, None, None).await {
            Ok(ws) => crate::hooks::get_hooks_status(self, &ws).await,
            Err(_) => crate::hooks::HooksStatusSummary {
                workspace_alias: "default".to_string(),
                workspace_path: workspace_root.clone(),
                hooks_file_exists: false,
                current_sha256: None,
                trusted: false,
                hash_changed: false,
                hooks: Vec::new(),
                parse_error: None,
            },
        };

        DashboardSnapshot {
            workspace_root,
            default_workspace_id,
            workspaces,
            builtin_tools_count,
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
            command_execution_policy,
            command_allowlist,
            recent_workspaces,
            pinned_workspaces,
            git_branch,
            has_cloudflare_token,
            cloudflare_public_url,
            tunnel_logs,
            agent_running: self.agent_running.load(Ordering::SeqCst) || has_active_runs,
            agent_provider,
            openai_base_url,
            openai_model,
            anthropic_model,
            auto_inject_project_rules,
            gemini_model,
            gemini_thinking_mode,
            agent_max_steps,
            agent_turn_timeout_secs,
            stream_idle_timeout_secs,
            pending_checkpoint,
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
            pending_elicitations,
            background_jobs,
            downstream_mcps,
            aggregated_tools_count,
            skills,
            plugins,
            active_runs,
            hooks_status,
            active_lsp_servers,
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

    #[tokio::test]
    async fn test_legacy_plaintext_secrets_migrate_to_dpapi_and_hashed_tokens() {
        let base = std::env::temp_dir().join("smart_mcp_legacy_migration_test");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        let auth_file = base.join("auth.json");
        let config_file = base.join("config.json");

        let mut legacy_cfg = AppConfig::default();
        legacy_cfg.cloudflare_tunnel_token = "eyJ_legacy_cf_secret".to_string();
        let mut legacy_env = HashMap::new();
        legacy_env.insert("PG_PASS".to_string(), "super_pg_secret".to_string());
        legacy_cfg.downstream_mcps.push(crate::config::DownstreamMcpConfig {
            id: "mcp-legacy-1".to_string(),
            name: "LegacyPg".to_string(),
            namespace: "legacypg".to_string(),
            transport: "stdio".to_string(),
            url: "https://example.com/mcp".to_string(),
            bearer_token: "legacy_downstream_bearer".to_string(),
            command: "npx".to_string(),
            args: vec![],
            env: legacy_env,
            headers: HashMap::new(),
            plugin_id: None,
            enabled: true,
        });
        legacy_cfg.save(&config_file).unwrap();

        let mut legacy_clients = HashMap::new();
        legacy_clients.insert(
            "cid_legacy".to_string(),
            OAuthClient {
                client_id: "cid_legacy".to_string(),
                client_secret: "legacy_client_secret_val".to_string(),
                auth_method: "client_secret_post".to_string(),
                client_name: Some("Legacy Client".to_string()),
                redirect_uris: vec!["https://gemini.google.com/oauth/callback".to_string()],
                trusted: true,
                created_at_epoch: 1000,
                last_used_epoch: None,
            },
        );
        let mut legacy_access = HashMap::new();
        legacy_access.insert(
            "smcp_at_legacy_raw".to_string(),
            OAuthTokenRecord {
                client_id: "cid_legacy".to_string(),
                expires_at_epoch: None,
                issued_at_epoch: 1000,
            },
        );
        let mut legacy_refresh = HashMap::new();
        legacy_refresh.insert(
            "smcp_rt_legacy_raw".to_string(),
            OAuthTokenRecord {
                client_id: "cid_legacy".to_string(),
                expires_at_epoch: None,
                issued_at_epoch: 1000,
            },
        );
        let legacy_auth = PersistedAuth {
            auth_token: "legacy_master_auth_tok".to_string(),
            oauth_clients: legacy_clients,
            oauth_access_tokens: legacy_access,
            oauth_refresh_tokens: legacy_refresh,
            last_workspace_root: None,
        };
        fs::write(&auth_file, serde_json::to_string_pretty(&legacy_auth).unwrap()).unwrap();

        // Startup triggers automatic migration
        let state = AppState::new_persistent(base.clone(), auth_file.clone(), config_file.clone());

        // Runtime state retains all migrated values
        assert_eq!(*state.auth_token.read().await, "legacy_master_auth_tok");
        assert_eq!(
            state.provider_secrets.read().await.cloudflare_tunnel_token,
            "eyJ_legacy_cf_secret"
        );
        let resolved = state.resolved_downstream_mcps().await;
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].bearer_token, "legacy_downstream_bearer");
        assert_eq!(
            resolved[0].env.get("PG_PASS").map(|s| s.as_str()),
            Some("super_pg_secret")
        );
        assert_eq!(
            state
                .oauth_clients
                .read()
                .await
                .get("cid_legacy")
                .unwrap()
                .client_secret,
            "legacy_client_secret_val"
        );
        assert!(state
            .oauth_access_tokens
            .read()
            .await
            .contains_key(&crate::security::hash_oauth_token("smcp_at_legacy_raw")));
        assert!(state
            .oauth_refresh_tokens
            .read()
            .await
            .contains_key(&crate::security::hash_oauth_token("smcp_rt_legacy_raw")));

        // Rewritten config.json and auth.json contain zero plaintext secrets/tokens
        let cfg_disk = fs::read_to_string(&config_file).unwrap();
        assert!(!cfg_disk.contains("eyJ_legacy_cf_secret"));
        assert!(!cfg_disk.contains("legacy_downstream_bearer"));
        assert!(!cfg_disk.contains("super_pg_secret"));

        let auth_disk = fs::read_to_string(&auth_file).unwrap();
        assert!(!auth_disk.contains("legacy_master_auth_tok"));
        assert!(!auth_disk.contains("legacy_client_secret_val"));
        assert!(!auth_disk.contains("smcp_at_legacy_raw"));
        assert!(!auth_disk.contains("smcp_rt_legacy_raw"));
        assert!(auth_disk.contains("sha256:"));

        // DPAPI SecretsStore decrypts all migrated secrets on reload
        let reloaded_vault = SecretsStore::new_persistent(base.clone()).load();
        assert_eq!(reloaded_vault.auth_token, "legacy_master_auth_tok");
        assert_eq!(reloaded_vault.cloudflare_tunnel_token, "eyJ_legacy_cf_secret");
        assert_eq!(
            reloaded_vault
                .downstream_bearer_tokens
                .get("mcp-legacy-1")
                .map(|s| s.as_str()),
            Some("legacy_downstream_bearer")
        );

        let _ = fs::remove_dir_all(&base);
    }
}
