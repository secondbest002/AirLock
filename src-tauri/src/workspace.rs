use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceProfile {
    pub id: String,
    pub alias: String,
    pub path: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub guardrail_override: Option<String>,
    #[serde(default)]
    pub allow_sensitive_override: Option<bool>,
    #[serde(default)]
    pub extra_sensitive_patterns: Vec<String>,
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceSummary {
    pub id: String,
    pub alias: String,
    pub path: String,
    pub enabled: bool,
    pub is_default: bool,
    pub guardrail_override: Option<String>,
    pub effective_guardrail: String,
    pub allow_sensitive_override: Option<bool>,
    pub pinned: bool,
    pub exists: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedWorkspace {
    pub id: String,
    pub alias: String,
    pub canon_root: PathBuf,
    pub clean_path: String,
    pub effective_guardrail: String,
    pub effective_allow_sensitive: bool,
    pub effective_sensitive_patterns: Vec<String>,
}

pub fn clean_path_string(path: &Path) -> String {
    let raw = path.display().to_string();
    raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string()
}

pub fn normalize_clean_path(path: &Path) -> Result<(PathBuf, String), String> {
    if !path.exists() || !path.is_dir() {
        return Err(format!("Folder tidak ditemukan: {}", path.display()));
    }
    let canon =
        std::fs::canonicalize(path).map_err(|e| format!("Gagal memverifikasi folder: {e}"))?;
    let clean = clean_path_string(&canon);
    Ok((canon, clean))
}

pub fn paths_equal(a: &str, b: &str) -> bool {
    if cfg!(target_os = "windows") {
        a.trim().eq_ignore_ascii_case(b.trim())
    } else {
        a.trim() == b.trim()
    }
}

pub fn sanitize_alias(raw: &str) -> String {
    let mut out = String::with_capacity(32);
    let mut prev_sep = false;

    for ch in raw.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' {
            out.push(lower);
            prev_sep = false;
        } else if lower == '-' {
            if !out.is_empty() && !prev_sep {
                out.push('-');
                prev_sep = true;
            }
        } else if !out.is_empty() && !prev_sep {
            out.push('-');
            prev_sep = true;
        }
    }

    let trimmed = out.trim_matches(|c| c == '-' || c == '_').to_string();
    let mut clipped: String = trimmed.chars().take(32).collect();
    clipped = clipped
        .trim_matches(|c| c == '-' || c == '_')
        .to_string();
    if clipped.is_empty() {
        "workspace".to_string()
    } else {
        clipped
    }
}

pub fn ensure_unique_alias(
    candidate: &str,
    existing: &[WorkspaceProfile],
    ignore_id: Option<&str>,
) -> String {
    let base = sanitize_alias(candidate);
    let taken = |cand: &str| -> bool {
        existing.iter().any(|w| {
            if ignore_id == Some(w.id.as_str()) {
                return false;
            }
            w.alias.eq_ignore_ascii_case(cand)
        })
    };

    if !taken(&base) {
        return base;
    }

    for n in 2..=999 {
        let suffix = format!("-{n}");
        let max_prefix = 32usize.saturating_sub(suffix.len());
        let prefix: String = base.chars().take(max_prefix).collect();
        let prefix_clean = prefix.trim_end_matches(|c| c == '-' || c == '_');
        let cand = format!(
            "{}{}",
            if prefix_clean.is_empty() {
                "ws"
            } else {
                prefix_clean
            },
            suffix
        );
        if !taken(&cand) {
            return cand;
        }
    }

    format!("ws-{}", &AppState::generate_token()[..6])
}

pub fn valid_guardrail_value(mode: &str) -> bool {
    matches!(mode, "read_only" | "safe_edit" | "autonomous")
}

/// Global `read_only` lock ALWAYS overrides any per-workspace override.
pub fn effective_guardrail_mode(global_mode: &str, override_mode: Option<&str>) -> String {
    if global_mode.trim() == "read_only" {
        return "read_only".to_string();
    }
    if let Some(ov) = override_mode {
        let t = ov.trim();
        if valid_guardrail_value(t) {
            return t.to_string();
        }
    }
    if valid_guardrail_value(global_mode.trim()) {
        global_mode.trim().to_string()
    } else {
        "autonomous".to_string()
    }
}

pub fn find_workspace_profile<'a>(
    workspaces: &'a [WorkspaceProfile],
    key: &str,
) -> Option<&'a WorkspaceProfile> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return None;
    }
    let clean_alias = trimmed.strip_prefix('@').unwrap_or(trimmed).trim();
    if clean_alias.is_empty() {
        return None;
    }
    workspaces
        .iter()
        .find(|w| w.alias.eq_ignore_ascii_case(clean_alias) || w.id == trimmed)
}

pub fn default_alias_for_path(path: &Path) -> String {
    let clean = clean_path_string(path);
    let clean_path = Path::new(&clean);
    let base = clean_path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("workspace");
    sanitize_alias(base)
}

pub fn build_workspace_summaries(
    workspaces: &[WorkspaceProfile],
    default_id: &str,
    global_guardrail: &str,
) -> Vec<WorkspaceSummary> {
    workspaces
        .iter()
        .map(|w| {
            let exists = Path::new(&w.path).is_dir();
            WorkspaceSummary {
                id: w.id.clone(),
                alias: w.alias.clone(),
                path: w.path.clone(),
                enabled: w.enabled,
                is_default: w.id == default_id,
                guardrail_override: w.guardrail_override.clone(),
                effective_guardrail: effective_guardrail_mode(
                    global_guardrail,
                    w.guardrail_override.as_deref(),
                ),
                allow_sensitive_override: w.allow_sensitive_override,
                pinned: w.pinned,
                exists,
            }
        })
        .collect()
}

pub async fn resolve_workspace(
    state: &AppState,
    requested: Option<&str>,
    pinned_connection_ws: Option<&str>,
) -> Result<ResolvedWorkspace, String> {
    let (workspaces, global_patterns, cfg_default_id) = {
        let cfg = state.config.read().await;
        (
            cfg.workspaces.clone(),
            cfg.sensitive_deny_patterns.clone(),
            cfg.default_workspace_id.clone(),
        )
    };
    let state_default_id = state.default_workspace_id.read().await.clone();
    let global_guardrail = state.guardrail_mode.read().await.clone();
    let global_allow_sensitive = state
        .allow_sensitive_reads
        .load(std::sync::atomic::Ordering::SeqCst);
    let fallback_root = state.workspace_root.read().await.clone();

    let effective_default_id = if !state_default_id.trim().is_empty() {
        state_default_id
    } else {
        cfg_default_id.unwrap_or_default()
    };

    let pin_key = pinned_connection_ws
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let req_key = requested.map(str::trim).filter(|s| !s.is_empty());

    // Handle edge case where workspaces vec is empty (e.g. manually cleared in a test)
    if workspaces.is_empty() {
        if let Some(pin) = pin_key {
            return Err(format!(
                "Workspace ter-pin '@{pin}' tidak ditemukan (daftar workspace kosong)."
            ));
        }
        if let Some(req) = req_key {
            let clean = req.strip_prefix('@').unwrap_or(req);
            let fallback_alias = default_alias_for_path(&fallback_root);
            if !clean.eq_ignore_ascii_case(&fallback_alias) && clean != "default" {
                return Err(format!("Workspace '{req}' tidak ditemukan."));
            }
        }
        let canon = std::fs::canonicalize(&fallback_root).unwrap_or(fallback_root);
        let clean = clean_path_string(&canon);
        let alias = default_alias_for_path(&canon);
        return Ok(ResolvedWorkspace {
            id: "default".to_string(),
            alias,
            canon_root: canon,
            clean_path: clean,
            effective_guardrail: effective_guardrail_mode(&global_guardrail, None),
            effective_allow_sensitive: global_allow_sensitive,
            effective_sensitive_patterns: global_patterns,
        });
    }

    let prof: &WorkspaceProfile = if let Some(pin) = pin_key {
        let pin_prof = find_workspace_profile(&workspaces, pin).ok_or_else(|| {
            format!("Koneksi di-pin ke workspace '{pin}', tetapi workspace tersebut tidak terdaftar.")
        })?;
        if !pin_prof.enabled {
            return Err(format!(
                "Workspace ter-pin '@{}' sedang nonaktif.",
                pin_prof.alias
            ));
        }
        if let Some(req) = req_key {
            match find_workspace_profile(&workspaces, req) {
                Some(req_prof) if req_prof.id == pin_prof.id => {}
                _ => {
                    return Err(format!(
                        "Koneksi ini di-pin ke workspace '@{}'; akses lintas-workspace ke '{}' ditolak.",
                        pin_prof.alias, req
                    ));
                }
            }
        }
        pin_prof
    } else if let Some(req) = req_key {
        let req_prof = find_workspace_profile(&workspaces, req).ok_or_else(|| {
            let avail: Vec<String> = workspaces
                .iter()
                .filter(|w| w.enabled)
                .map(|w| format!("@{}", w.alias))
                .collect();
            format!(
                "Workspace '{}' tidak ditemukan. Workspace aktif tersedia: {}.",
                req,
                if avail.is_empty() {
                    "(tidak ada)".to_string()
                } else {
                    avail.join(", ")
                }
            )
        })?;
        if !req_prof.enabled {
            return Err(format!(
                "Workspace '@{}' sedang nonaktif. Aktifkan di Workspace Manager terlebih dahulu.",
                req_prof.alias
            ));
        }
        req_prof
    } else {
        workspaces
            .iter()
            .find(|w| w.id == effective_default_id && w.enabled)
            .or_else(|| workspaces.iter().find(|w| w.enabled))
            .ok_or_else(|| "Tidak ada workspace aktif yang tersedia.".to_string())?
    };

    let (canon_root, clean_path) = normalize_clean_path(Path::new(&prof.path)).map_err(|_| {
        format!(
            "Folder workspace '@{}' tidak ditemukan di disk: {}",
            prof.alias, prof.path
        )
    })?;

    let eff_guardrail = effective_guardrail_mode(&global_guardrail, prof.guardrail_override.as_deref());
    let eff_allow_sensitive = prof
        .allow_sensitive_override
        .unwrap_or(global_allow_sensitive);

    let mut eff_patterns = global_patterns;
    for pat in &prof.extra_sensitive_patterns {
        let t = pat.trim();
        if !t.is_empty() && !eff_patterns.iter().any(|existing| existing == t) {
            eff_patterns.push(t.to_string());
        }
    }

    Ok(ResolvedWorkspace {
        id: prof.id.clone(),
        alias: prof.alias.clone(),
        canon_root,
        clean_path,
        effective_guardrail: eff_guardrail,
        effective_allow_sensitive: eff_allow_sensitive,
        effective_sensitive_patterns: eff_patterns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_alias_sanitization_and_uniqueness() {
        assert_eq!(sanitize_alias("My API_v2! "), "my-api_v2");
        assert_eq!(sanitize_alias("---"), "workspace");
        assert_eq!(sanitize_alias("  Tauri MCP Hub  "), "tauri-mcp-hub");
        let long = "a_very_long_repository_name_that_exceeds_thirty_two_characters";
        assert!(sanitize_alias(long).len() <= 32);

        let existing = vec![
            WorkspaceProfile {
                id: "ws-1".to_string(),
                alias: "api".to_string(),
                path: "/tmp/api".to_string(),
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
            WorkspaceProfile {
                id: "ws-2".to_string(),
                alias: "api-2".to_string(),
                path: "/tmp/api2".to_string(),
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
        ];

        assert_eq!(ensure_unique_alias("API", &existing, None), "api-3");
        assert_eq!(
            ensure_unique_alias("api", &existing, Some("ws-1")),
            "api"
        );
        assert_eq!(ensure_unique_alias("web", &existing, None), "web");

        // Global read_only ALWAYS overrides autonomous override
        assert_eq!(
            effective_guardrail_mode("read_only", Some("autonomous")),
            "read_only"
        );
        assert_eq!(
            effective_guardrail_mode("autonomous", Some("safe_edit")),
            "safe_edit"
        );
        assert_eq!(
            effective_guardrail_mode("safe_edit", None),
            "safe_edit"
        );
    }

    #[tokio::test]
    async fn test_resolve_workspace_precedence_and_pin_isolation() {
        let base = std::env::temp_dir().join("smart_mcp_ws_resolve_test");
        let _ = fs::remove_dir_all(&base);
        let dir_api = base.join("api");
        let dir_web = base.join("web");
        fs::create_dir_all(&dir_api).unwrap();
        fs::create_dir_all(&dir_web).unwrap();

        let (_, clean_api) = normalize_clean_path(&dir_api).unwrap();
        let (_, clean_web) = normalize_clean_path(&dir_web).unwrap();

        let state = AppState::new(dir_api.clone());
        {
            let mut cfg = state.config.write().await;
            cfg.workspaces = vec![
                WorkspaceProfile {
                    id: "ws-api".to_string(),
                    alias: "api".to_string(),
                    path: clean_api,
                    enabled: true,
                    guardrail_override: None,
                    allow_sensitive_override: None,
                    extra_sensitive_patterns: vec![],
                    pinned: true,
                },
                WorkspaceProfile {
                    id: "ws-web".to_string(),
                    alias: "web".to_string(),
                    path: clean_web,
                    enabled: true,
                    guardrail_override: Some("read_only".to_string()),
                    allow_sensitive_override: None,
                    extra_sensitive_patterns: vec!["*.secret".to_string()],
                    pinned: false,
                },
            ];
            cfg.default_workspace_id = Some("ws-api".to_string());
        }
        *state.default_workspace_id.write().await = "ws-api".to_string();

        // 1. No arg, no pin -> default (@api)
        let r_def = resolve_workspace(&state, None, None).await.unwrap();
        assert_eq!(r_def.alias, "api");

        // 2. Explicit arg overrides default (@web)
        let r_web = resolve_workspace(&state, Some("@web"), None).await.unwrap();
        assert_eq!(r_web.alias, "web");
        assert_eq!(r_web.effective_guardrail, "read_only");
        assert!(r_web
            .effective_sensitive_patterns
            .iter()
            .any(|p| p == "*.secret"));

        // 3. Connection pin locks to @web even when no explicit arg given
        let r_pin = resolve_workspace(&state, None, Some("web")).await.unwrap();
        assert_eq!(r_pin.alias, "web");

        // 4. Explicit matching arg on pinned connection succeeds
        let r_pin_same = resolve_workspace(&state, Some("web"), Some("web"))
            .await
            .unwrap();
        assert_eq!(r_pin_same.alias, "web");

        // 5. Cross-workspace arg on pinned connection is REJECTED
        let err = resolve_workspace(&state, Some("api"), Some("web"))
            .await
            .unwrap_err();
        assert!(err.contains("di-pin ke workspace '@web'"));
        assert!(err.contains("ditolak"));

        let _ = fs::remove_dir_all(&base);
    }
}
