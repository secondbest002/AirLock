use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn default_port() -> u16 {
    9494
}

fn default_tunnel_provider() -> String {
    "cloudflare".to_string()
}

fn default_true() -> bool {
    true
}

fn default_gemini_model() -> String {
    "gemini-2.5-flash".to_string()
}

fn default_guardrail_mode() -> String {
    "autonomous".to_string()
}

fn default_command_execution_policy() -> String {
    "guarded".to_string()
}

fn default_command_allowlist() -> Vec<String> {
    crate::security::default_command_allowlist()
}

fn default_agent_provider() -> String {
    "gemini".to_string()
}

fn default_openai_base_url() -> String {
    "http://127.0.0.1:11434/v1".to_string()
}

fn default_openai_model() -> String {
    "qwen2.5-coder:7b".to_string()
}

fn default_anthropic_model() -> String {
    "claude-sonnet-4-20250514".to_string()
}

fn default_gemini_thinking_mode() -> String {
    "auto".to_string()
}

fn default_agent_max_steps() -> usize {
    16
}

fn default_agent_turn_timeout_secs() -> u64 {
    180
}

fn default_stream_idle_timeout_secs() -> u64 {
    35
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpToolProfile {
    Compact,
    Full,
}

impl McpToolProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "compact" => Some(Self::Compact),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

fn default_webmcp_default_profile() -> String {
    "compact".to_string()
}

fn default_webmcp_rpc_timeout_secs() -> u64 {
    32
}

fn default_webmcp_auto_promote_secs() -> u64 {
    18
}

fn default_webmcp_poll_wait_secs() -> u64 {
    22
}

fn default_webmcp_max_output_chars() -> usize {
    6000
}

fn default_sensitive_patterns() -> Vec<String> {
    vec![
        ".env".to_string(),
        ".env.*".to_string(),
        "*.pem".to_string(),
        "*.key".to_string(),
        "*.p12".to_string(),
        "*.pfx".to_string(),
        "id_rsa".to_string(),
        "id_ed25519".to_string(),
        "auth.json".to_string(),
        "secrets.json".to_string(),
        "credentials.json".to_string(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownstreamMcpConfig {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub transport: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub bearer_token: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub plugin_id: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LspServerOverride {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_port")]
    pub server_port: u16,
    #[serde(default = "default_tunnel_provider")]
    pub tunnel_provider: String,
    #[serde(default)]
    pub idle_timeout_mins: u64,
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    #[serde(default = "default_gemini_model")]
    pub gemini_model: String,
    #[serde(default)]
    pub recent_workspaces: Vec<String>,
    #[serde(default)]
    pub pinned_workspaces: Vec<String>,
    #[serde(default)]
    pub cloudflare_tunnel_token: String,
    #[serde(default)]
    pub cloudflare_public_url: String,
    #[serde(default = "default_sensitive_patterns")]
    pub sensitive_deny_patterns: Vec<String>,
    #[serde(default)]
    pub allow_sensitive_reads: bool,
    #[serde(default = "default_guardrail_mode")]
    pub guardrail_mode: String,
    #[serde(default = "default_command_execution_policy")]
    pub command_execution_policy: String,
    #[serde(default = "default_command_allowlist")]
    pub command_allowlist: Vec<String>,
    #[serde(default = "default_agent_provider")]
    pub agent_provider: String,
    #[serde(default = "default_openai_base_url")]
    pub openai_base_url: String,
    #[serde(default = "default_openai_model")]
    pub openai_model: String,
    #[serde(default = "default_anthropic_model")]
    pub anthropic_model: String,
    #[serde(default = "default_true")]
    pub auto_inject_project_rules: bool,
    #[serde(default)]
    pub downstream_mcps: Vec<DownstreamMcpConfig>,
    #[serde(default)]
    pub workspaces: Vec<crate::workspace::WorkspaceProfile>,
    #[serde(default)]
    pub default_workspace_id: Option<String>,
    #[serde(default = "default_gemini_thinking_mode")]
    pub gemini_thinking_mode: String,
    #[serde(default = "default_agent_max_steps")]
    pub agent_max_steps: usize,
    #[serde(default = "default_agent_turn_timeout_secs")]
    pub agent_turn_timeout_secs: u64,
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
    #[serde(default)]
    pub disabled_skills: Vec<String>,
    #[serde(default)]
    pub installed_plugins: Vec<crate::plugins::InstalledPluginRecord>,
    #[serde(default)]
    pub trusted_hook_hashes: HashMap<String, String>,
    #[serde(default)]
    pub hooks: Vec<crate::hooks::HookDefinition>,
    #[serde(default)]
    pub lsp_servers: HashMap<String, LspServerOverride>,
    #[serde(default = "default_webmcp_default_profile")]
    pub webmcp_default_profile: String,
    #[serde(default = "default_webmcp_rpc_timeout_secs")]
    pub webmcp_rpc_timeout_secs: u64,
    #[serde(default = "default_webmcp_auto_promote_secs")]
    pub webmcp_auto_promote_secs: u64,
    #[serde(default = "default_webmcp_poll_wait_secs")]
    pub webmcp_poll_wait_secs: u64,
    #[serde(default = "default_webmcp_max_output_chars")]
    pub webmcp_max_output_chars: usize,
    #[serde(default)]
    pub webmcp_sse_keepalive: bool,
    #[serde(default)]
    pub webmcp_allow_write_delegation: bool,
    #[serde(default)]
    pub webmcp_expose_skills: bool,
    #[serde(default = "default_true")]
    pub webmcp_session_harness: bool,
    #[serde(default)]
    pub webmcp_auto_verify_on_edit: bool,
    #[serde(default = "default_true")]
    pub webmcp_auto_handoff_dir: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server_port: default_port(),
            tunnel_provider: default_tunnel_provider(),
            idle_timeout_mins: 0,
            close_to_tray: default_true(),
            gemini_model: default_gemini_model(),
            recent_workspaces: Vec::new(),
            pinned_workspaces: Vec::new(),
            cloudflare_tunnel_token: String::new(),
            cloudflare_public_url: String::new(),
            sensitive_deny_patterns: default_sensitive_patterns(),
            allow_sensitive_reads: false,
            guardrail_mode: default_guardrail_mode(),
            command_execution_policy: default_command_execution_policy(),
            command_allowlist: default_command_allowlist(),
            agent_provider: default_agent_provider(),
            openai_base_url: default_openai_base_url(),
            openai_model: default_openai_model(),
            anthropic_model: default_anthropic_model(),
            auto_inject_project_rules: default_true(),
            downstream_mcps: Vec::new(),
            workspaces: Vec::new(),
            default_workspace_id: None,
            gemini_thinking_mode: default_gemini_thinking_mode(),
            agent_max_steps: default_agent_max_steps(),
            agent_turn_timeout_secs: default_agent_turn_timeout_secs(),
            stream_idle_timeout_secs: default_stream_idle_timeout_secs(),
            disabled_skills: Vec::new(),
            installed_plugins: Vec::new(),
            trusted_hook_hashes: HashMap::new(),
            hooks: Vec::new(),
            lsp_servers: HashMap::new(),
            webmcp_default_profile: default_webmcp_default_profile(),
            webmcp_rpc_timeout_secs: default_webmcp_rpc_timeout_secs(),
            webmcp_auto_promote_secs: default_webmcp_auto_promote_secs(),
            webmcp_poll_wait_secs: default_webmcp_poll_wait_secs(),
            webmcp_max_output_chars: default_webmcp_max_output_chars(),
            webmcp_sse_keepalive: false,
            webmcp_allow_write_delegation: false,
            webmcp_expose_skills: false,
            webmcp_session_harness: true,
            webmcp_auto_verify_on_edit: false,
            webmcp_auto_handoff_dir: default_true(),
        }
    }
}

impl AppConfig {
    pub fn normalized_webmcp_default_profile(&self) -> &'static str {
        McpToolProfile::parse(&self.webmcp_default_profile)
            .unwrap_or(McpToolProfile::Compact)
            .as_str()
    }

    pub fn effective_webmcp_poll_wait_secs(&self) -> u64 {
        self.webmcp_poll_wait_secs.clamp(0, 26)
    }

    pub fn effective_webmcp_auto_promote_secs(&self) -> u64 {
        self.webmcp_auto_promote_secs.clamp(5, 25)
    }

    pub fn effective_webmcp_rpc_timeout_secs(&self) -> u64 {
        let min_for_poll = self.effective_webmcp_poll_wait_secs().saturating_add(6);
        self.webmcp_rpc_timeout_secs
            .clamp(20, 60)
            .max(min_for_poll)
            .min(60)
    }

    pub fn effective_webmcp_max_output_chars(&self) -> usize {
        self.webmcp_max_output_chars.clamp(3000, 16000)
    }
    /// Returns `~/.smart-tauri-hub/config.json`.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join(".smart-tauri-hub").join("config.json")
    }

    /// Loads config from `path`, falling back to `Default` if missing or invalid.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Self>(&raw).ok())
            .unwrap_or_default()
    }

    /// Saves config to `path` using a temp file + rename (with overwrite fallback on Windows).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Gagal membuat direktori config: {e}"))?;
        }

        let json_str = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Gagal serialisasi config: {e}"))?;

        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json_str)
            .map_err(|e| format!("Gagal menulis file sementara config: {e}"))?;

        if std::fs::rename(&tmp_path, path).is_err() {
            std::fs::write(path, &json_str)
                .map_err(|e| format!("Gagal menyimpan config: {e}"))?;
            let _ = std::fs::remove_file(&tmp_path);
        }

        Ok(())
    }

    /// Returns a copy stripped of secrets so plaintext tokens/envs are never written to `config.json`.
    pub fn sanitized_for_disk(&self) -> Self {
        let mut cloned = self.clone();
        cloned.cloudflare_tunnel_token.clear();
        for mcp in &mut cloned.downstream_mcps {
            mcp.bearer_token.clear();
            mcp.env.clear();
            mcp.headers.clear();
        }
        cloned
    }

    /// Records a recently used workspace path at the front of the list (max 10).
    pub fn record_recent_workspace(&mut self, clean_path: &str) {
        let trimmed = clean_path.trim();
        if trimmed.is_empty() {
            return;
        }
        self.recent_workspaces.retain(|existing| {
            if cfg!(target_os = "windows") {
                !existing.eq_ignore_ascii_case(trimmed)
            } else {
                existing != trimmed
            }
        });
        self.recent_workspaces.insert(0, trimmed.to_string());
        if self.recent_workspaces.len() > 10 {
            self.recent_workspaces.truncate(10);
        }
    }

    /// Toggles pin status for `clean_path`, returning `true` if now pinned.
    pub fn toggle_pin_workspace(&mut self, clean_path: &str) -> bool {
        let trimmed = clean_path.trim();
        if trimmed.is_empty() {
            return false;
        }
        let existing_idx = self.pinned_workspaces.iter().position(|existing| {
            if cfg!(target_os = "windows") {
                existing.eq_ignore_ascii_case(trimmed)
            } else {
                existing == trimmed
            }
        });
        if let Some(idx) = existing_idx {
            self.pinned_workspaces.remove(idx);
            false
        } else {
            self.pinned_workspaces.insert(0, trimmed.to_string());
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_config_save_and_load_roundtrip() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_config_roundtrip_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        let cfg_path = temp_dir.join("config.json");

        let mut cfg = AppConfig::default();
        cfg.server_port = 9595;
        cfg.tunnel_provider = "cloudflare_named".to_string();
        cfg.idle_timeout_mins = 30;
        cfg.close_to_tray = false;
        cfg.gemini_model = "gemini-2.5-pro".to_string();
        cfg.allow_sensitive_reads = true;
        cfg.guardrail_mode = "safe_edit".to_string();
        cfg.command_execution_policy = "allowlist".to_string();
        cfg.command_allowlist = vec!["cargo".to_string(), "git status".to_string()];
        cfg.agent_provider = "openai_compat".to_string();
        cfg.cloudflare_tunnel_token = "eyJ_secret_cf_token".to_string();
        cfg.cloudflare_public_url = "https://mcp.example.dev".to_string();
        cfg.record_recent_workspace(r"D:\Projects\Alpha");
        cfg.record_recent_workspace(r"D:\Projects\Beta");
        cfg.record_recent_workspace(r"d:\projects\alpha");

        assert!(cfg.toggle_pin_workspace(r"D:\Projects\Alpha"));
        assert!(cfg.toggle_pin_workspace(r"D:\Projects\Beta"));
        assert!(!cfg.toggle_pin_workspace(r"d:\projects\alpha"));

        cfg.save(&cfg_path).unwrap();

        let loaded = AppConfig::load(&cfg_path);
        assert_eq!(loaded.server_port, 9595);
        assert_eq!(loaded.tunnel_provider, "cloudflare_named");
        assert_eq!(loaded.idle_timeout_mins, 30);
        assert!(!loaded.close_to_tray);
        assert_eq!(loaded.gemini_model, "gemini-2.5-pro");
        assert!(loaded.allow_sensitive_reads);
        assert_eq!(loaded.guardrail_mode, "safe_edit");
        assert_eq!(loaded.command_execution_policy, "allowlist");
        assert_eq!(loaded.command_allowlist, vec!["cargo".to_string(), "git status".to_string()]);
        assert_eq!(loaded.agent_provider, "openai_compat");
        assert_eq!(loaded.cloudflare_tunnel_token, "eyJ_secret_cf_token");
        assert_eq!(loaded.cloudflare_public_url, "https://mcp.example.dev");
        assert_eq!(loaded.recent_workspaces.len(), 2);
        assert_eq!(loaded.recent_workspaces[0], r"d:\projects\alpha");
        assert_eq!(loaded.recent_workspaces[1], r"D:\Projects\Beta");
        assert_eq!(loaded.pinned_workspaces, vec![r"D:\Projects\Beta"]);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_legacy_config_without_webmcp_fields_loads_defaults() {
        let legacy_json = r#"{
            "server_port": 9494,
            "guardrail_mode": "safe_edit"
        }"#;
        let mut cfg: AppConfig = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(cfg.normalized_webmcp_default_profile(), "compact");
        assert_eq!(cfg.effective_webmcp_rpc_timeout_secs(), 32);
        assert_eq!(cfg.effective_webmcp_auto_promote_secs(), 18);
        assert_eq!(cfg.effective_webmcp_poll_wait_secs(), 22);
        assert_eq!(cfg.effective_webmcp_max_output_chars(), 6000);
        assert!(!cfg.webmcp_sse_keepalive);
        assert!(!cfg.webmcp_allow_write_delegation);
        assert!(!cfg.webmcp_expose_skills);
        assert!(cfg.webmcp_session_harness);
        assert!(!cfg.webmcp_auto_verify_on_edit);
        assert!(cfg.webmcp_auto_handoff_dir);

        // Invariant: rpc_timeout is always >= poll_wait + 6
        cfg.webmcp_poll_wait_secs = 26;
        cfg.webmcp_rpc_timeout_secs = 20;
        assert_eq!(cfg.effective_webmcp_poll_wait_secs(), 26);
        assert_eq!(cfg.effective_webmcp_rpc_timeout_secs(), 32);
    }
}
