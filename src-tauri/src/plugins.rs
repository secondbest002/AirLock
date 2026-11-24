use crate::config::DownstreamMcpConfig;
use crate::mcp_hub::sanitize_namespace;
use crate::secrets::ProviderSecrets;
use crate::security::resolve_jailed_path;
use crate::skills::{scan_plugin_skills_preview, SkillEntry, SkillScope, SkillSummary};
use crate::state::AppState;
use crate::tools::sha256_hex;
use crate::workspace::clean_path_string;
use chrono::Local;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};
use tokio::process::Command;

const MAX_PLUGIN_COPY_FILES: usize = 500;
const MAX_PLUGIN_COPY_BYTES: u64 = 25 * 1024 * 1024;
const STAGING_TTL_SECS: u64 = 3600;

fn default_plugin_version() -> String {
    "0.1.0".to_string()
}

fn secret_placeholder_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\$\{SECRET:([A-Za-z0-9_.-]+)\}").expect("valid secret placeholder regex")
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default = "default_plugin_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMcpServerDef {
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginMcpFile {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, PluginMcpServerDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPluginRecord {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub source: String,
    pub install_dir: String,
    pub enabled: bool,
    pub trusted: bool,
    pub manifest_sha256: String,
    #[serde(default)]
    pub mcp_server_ids: Vec<String>,
    #[serde(default)]
    pub required_secrets: Vec<String>,
    #[serde(default)]
    pub installed_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub source: String,
    pub install_dir: String,
    pub enabled: bool,
    pub trusted: bool,
    pub manifest_sha256: String,
    pub mcp_servers_count: usize,
    pub skills_count: usize,
    pub required_secrets: Vec<String>,
    pub missing_secrets: Vec<String>,
    pub configured_secrets: Vec<String>,
    pub installed_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginMcpPreviewItem {
    pub server_key: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub url: String,
    pub env_keys: Vec<String>,
    pub header_keys: Vec<String>,
    pub secret_placeholders: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginInstallPreview {
    pub staging_id: String,
    pub source_type: String,
    pub location: String,
    pub subdir: Option<String>,
    pub manifest: PluginManifest,
    pub manifest_sha256: String,
    pub mcp_servers: Vec<PluginMcpPreviewItem>,
    pub skills: Vec<SkillSummary>,
    pub required_secrets: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PluginStagingEntry {
    #[allow(dead_code)]
    pub staging_id: String,
    pub cleanup_dir: Option<PathBuf>,
    pub source_dir: PathBuf,
    pub source_label: String,
    pub manifest: PluginManifest,
    pub manifest_sha256: String,
    pub mcp_server_keys: Vec<String>,
    pub required_secrets: Vec<String>,
    pub created_at: Instant,
}

pub fn sanitize_plugin_slug(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_underscore = false;
    for ch in raw.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_underscore = false;
        } else if (ch == '-' || ch == '_' || ch == ' ') && !prev_underscore && !out.is_empty() {
            out.push('_');
            prev_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "plugin".to_string()
    } else if trimmed.len() > 32 {
        trimmed[..32].trim_matches('_').to_string()
    } else {
        trimmed
    }
}

pub fn validate_git_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Git URL tidak boleh kosong.".to_string());
    }
    if trimmed.starts_with('-') {
        return Err("Git URL tidak valid: tidak boleh diawali '-' (option injection blocked).".to_string());
    }
    if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("Git URL tidak boleh mengandung spasi atau karakter kontrol.".to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("file:") || lower.starts_with("ext::") || lower.starts_with("fd::") {
        return Err(format!("Skema Git URL '{}' ditolak demi keamanan.", trimmed));
    }

    if let Ok(parsed) = url::Url::parse(trimmed) {
        let scheme = parsed.scheme();
        if scheme == "https" || scheme == "ssh" {
            if parsed.host_str().map(str::is_empty).unwrap_or(true) {
                return Err("Git URL wajib memiliki host yang valid.".to_string());
            }
            return Ok(trimmed.to_string());
        }
        if scheme == "http" {
            let is_loopback = matches!(
                parsed.host_str(),
                Some("127.0.0.1") | Some("localhost") | Some("::1") | Some("[::1]")
            );
            if is_loopback {
                return Ok(trimmed.to_string());
            }
            return Err("Git URL HTTP non-lokal ditolak; gunakan HTTPS atau SSH.".to_string());
        }
        return Err(format!("Skema Git URL '{scheme}://' tidak diizinkan."));
    }

    // SCP-style git@host:owner/repo.git
    if let Some((user_host, path_part)) = trimmed.split_once(':') {
        if let Some((user, host)) = user_host.split_once('@') {
            let valid_user = !user.is_empty()
                && user
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
            let valid_host = !host.is_empty()
                && !host.starts_with('-')
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
            let valid_path = !path_part.is_empty()
                && !path_part.starts_with('-')
                && !path_part.contains("..");
            if valid_user && valid_host && valid_path {
                return Ok(trimmed.to_string());
            }
        }
    }

    Err("Format Git URL tidak valid. Gunakan HTTPS, SSH, atau git@host:org/repo.git.".to_string())
}

fn collect_placeholders_from_str(s: &str, out: &mut BTreeSet<String>) {
    let re = secret_placeholder_re();
    for cap in re.captures_iter(s) {
        if let Some(m) = cap.get(1) {
            out.insert(m.as_str().to_string());
        }
    }
}

pub fn extract_secret_placeholders(mcp: &PluginMcpFile) -> Vec<String> {
    let mut keys = BTreeSet::new();
    for def in mcp.mcp_servers.values() {
        if let Some(u) = &def.url {
            collect_placeholders_from_str(u, &mut keys);
        }
        for v in def.env.values() {
            collect_placeholders_from_str(v, &mut keys);
        }
        for v in def.headers.values() {
            collect_placeholders_from_str(v, &mut keys);
        }
        for a in &def.args {
            collect_placeholders_from_str(a, &mut keys);
        }
    }
    keys.into_iter().collect()
}

pub fn interpolate_secret_str(raw: &str, secret_map: &HashMap<String, String>) -> String {
    let re = secret_placeholder_re();
    re.replace_all(raw, |caps: &regex::Captures| {
        let key = &caps[1];
        secret_map
            .get(key)
            .map(String::as_str)
            .unwrap_or("")
            .to_string()
    })
    .to_string()
}

fn load_plugin_mcp_file(dir: &Path) -> Result<Option<PluginMcpFile>, String> {
    let mcp_path = dir.join("mcp.json");
    if !mcp_path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&mcp_path)
        .map_err(|e| format!("Gagal membaca mcp.json plugin: {e}"))?;
    let parsed = serde_json::from_str::<PluginMcpFile>(&raw)
        .map_err(|e| format!("Format mcp.json plugin tidak valid: {e}"))?;
    Ok(Some(parsed))
}

fn load_or_synthesize_manifest(dir: &Path) -> Result<(PluginManifest, Vec<String>), String> {
    let manifest_path = dir.join("plugin.json");
    if manifest_path.is_file() {
        let raw = fs::read_to_string(&manifest_path)
            .map_err(|e| format!("Gagal membaca plugin.json: {e}"))?;
        let mut parsed = serde_json::from_str::<PluginManifest>(&raw)
            .map_err(|e| format!("Format plugin.json tidak valid: {e}"))?;
        parsed.name = parsed.name.trim().to_string();
        if parsed.name.is_empty() {
            parsed.name = dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed-plugin")
                .to_string();
        }
        return Ok((parsed, Vec::new()));
    }

    let fallback_name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("local-plugin")
        .to_string();
    Ok((
        PluginManifest {
            name: fallback_name,
            version: default_plugin_version(),
            description: "Agent Plugin tanpa plugin.json eksplisit".to_string(),
            author: None,
            homepage: None,
            tags: Vec::new(),
        },
        vec!["plugin.json tidak ditemukan — metadata nama/versi dibuat otomatis dari nama folder.".to_string()],
    ))
}

fn compute_plugin_manifest_hash(dir: &Path) -> String {
    let plugin_bytes = fs::read(dir.join("plugin.json")).unwrap_or_default();
    let mcp_bytes = fs::read(dir.join("mcp.json")).unwrap_or_default();
    let mut combined = Vec::with_capacity(plugin_bytes.len() + mcp_bytes.len() + 2);
    combined.extend_from_slice(&plugin_bytes);
    combined.push(b'\n');
    combined.extend_from_slice(&mcp_bytes);
    sha256_hex(&combined)
}

fn allocate_unique_namespace(base: &str, used: &mut HashSet<String>) -> String {
    let clean = sanitize_namespace(base);
    if used.insert(clean.clone()) {
        return clean;
    }
    for idx in 2..1000 {
        let candidate = format!("{clean}_{idx}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    let fallback = format!("{clean}_{}", used.len() + 1);
    used.insert(fallback.clone());
    fallback
}

pub fn materialize_plugin_mcp_configs(
    plugin: &InstalledPluginRecord,
    secret_map: Option<&HashMap<String, String>>,
    used_namespaces: &mut HashSet<String>,
) -> Vec<DownstreamMcpConfig> {
    if !plugin.enabled || !plugin.trusted {
        return Vec::new();
    }
    let dir = PathBuf::from(&plugin.install_dir);
    let Ok(Some(mcp_file)) = load_plugin_mcp_file(&dir) else {
        return Vec::new();
    };
    let empty_secrets = HashMap::new();
    let secrets = secret_map.unwrap_or(&empty_secrets);

    let sorted_servers: BTreeMap<String, PluginMcpServerDef> =
        mcp_file.mcp_servers.into_iter().collect();
    let multi_server = sorted_servers.len() > 1;
    let plugin_slug = sanitize_plugin_slug(&plugin.name);
    let mut out = Vec::with_capacity(sorted_servers.len());

    for (server_key, def) in sorted_servers {
        let url_interp = def
            .url
            .as_deref()
            .map(|u| interpolate_secret_str(u.trim(), secrets))
            .unwrap_or_default();
        let command_str = def.command.as_deref().unwrap_or("").trim().to_string();
        let transport = match def.transport.as_deref().map(|s| s.trim().to_ascii_lowercase()) {
            Some(t) if t == "http" || t == "streamable-http" || t == "sse" => "http".to_string(),
            Some(t) if t == "stdio" => "stdio".to_string(),
            _ => {
                if !url_interp.is_empty() {
                    "http".to_string()
                } else {
                    "stdio".to_string()
                }
            }
        };

        let preferred_ns = if multi_server {
            format!("{plugin_slug}_{server_key}")
        } else {
            let candidate = sanitize_namespace(&server_key);
            if used_namespaces.contains(&candidate) {
                format!("{plugin_slug}_{server_key}")
            } else {
                candidate
            }
        };
        let namespace = allocate_unique_namespace(&preferred_ns, used_namespaces);

        let mut env = HashMap::with_capacity(def.env.len());
        for (k, v) in def.env {
            env.insert(k, interpolate_secret_str(&v, secrets));
        }

        let mut headers = HashMap::new();
        let mut bearer_token = String::new();
        for (k, v) in def.headers {
            let interp = interpolate_secret_str(&v, secrets);
            if k.trim().eq_ignore_ascii_case("authorization") {
                if let Some(tok) = interp
                    .trim()
                    .strip_prefix("Bearer ")
                    .or_else(|| interp.trim().strip_prefix("bearer "))
                {
                    bearer_token = tok.trim().to_string();
                    continue;
                }
            }
            headers.insert(k, interp);
        }

        let args = def
            .args
            .into_iter()
            .map(|a| interpolate_secret_str(&a, secrets))
            .collect();

        out.push(DownstreamMcpConfig {
            id: format!("{}::{}", plugin.id, server_key),
            name: format!("{} ({})", plugin.name, server_key),
            namespace,
            transport,
            url: url_interp,
            bearer_token,
            command: command_str,
            args,
            env,
            headers,
            plugin_id: Some(plugin.id.clone()),
            enabled: true,
        });
    }

    out
}

fn resolve_optional_subdir(base: &Path, subdir: Option<&str>) -> Result<PathBuf, String> {
    let Some(sub_trim) = subdir.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(base.to_path_buf());
    };
    if sub_trim.starts_with('/') || sub_trim.starts_with('\\') {
        return Err("Parameter subdir harus berupa path relatif di dalam plugin.".to_string());
    }
    for comp in Path::new(sub_trim).components() {
        if matches!(
            comp,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err("Path traversal pada subdir plugin ditolak.".to_string());
        }
    }
    let resolved = resolve_jailed_path(base, sub_trim)?;
    if !resolved.is_dir() {
        return Err(format!("Subfolder plugin '{sub_trim}' bukan direktori."));
    }
    Ok(resolved)
}

fn build_mcp_preview_items(
    mcp: &PluginMcpFile,
) -> (Vec<PluginMcpPreviewItem>, Vec<String>, Vec<String>) {
    let sorted: BTreeMap<&String, &PluginMcpServerDef> = mcp.mcp_servers.iter().collect();
    let mut items = Vec::with_capacity(sorted.len());
    let mut warnings = Vec::new();
    let all_secrets = extract_secret_placeholders(mcp);

    for (key, def) in sorted {
        let url = def.url.as_deref().unwrap_or("").trim().to_string();
        let command = def.command.as_deref().unwrap_or("").trim().to_string();
        let transport = match def.transport.as_deref().map(|s| s.trim().to_ascii_lowercase()) {
            Some(t) if t == "http" || t == "streamable-http" || t == "sse" => "http".to_string(),
            Some(t) if t == "stdio" => "stdio".to_string(),
            _ => {
                if !url.is_empty() {
                    "http".to_string()
                } else {
                    "stdio".to_string()
                }
            }
        };

        let mut env_keys: Vec<String> = def.env.keys().cloned().collect();
        env_keys.sort();
        let mut header_keys: Vec<String> = def.headers.keys().cloned().collect();
        header_keys.sort();

        let mut per_server_secrets = BTreeSet::new();
        collect_placeholders_from_str(&url, &mut per_server_secrets);
        for v in def.env.values() {
            collect_placeholders_from_str(v, &mut per_server_secrets);
        }
        for v in def.headers.values() {
            collect_placeholders_from_str(v, &mut per_server_secrets);
        }
        for a in &def.args {
            collect_placeholders_from_str(a, &mut per_server_secrets);
        }

        if transport == "stdio" && !command.is_empty() {
            let cmd_preview = if def.args.is_empty() {
                command.clone()
            } else {
                format!("{command} {}", def.args.join(" "))
            };
            warnings.push(format!(
                "MCP '{key}' akan men-spawn proses stdio lokal: `{cmd_preview}`"
            ));
        } else if !url.is_empty() {
            warnings.push(format!(
                "MCP '{key}' akan mengirim tool calls ke endpoint HTTP eksternal: `{url}`"
            ));
        }

        items.push(PluginMcpPreviewItem {
            server_key: key.clone(),
            transport,
            command,
            args: def.args.clone(),
            url,
            env_keys,
            header_keys,
            secret_placeholders: per_server_secrets.into_iter().collect(),
        });
    }

    (items, all_secrets, warnings)
}

pub async fn inspect_plugin_source(
    state: &AppState,
    source_type: &str,
    location: &str,
    subdir: Option<&str>,
) -> Result<PluginInstallPreview, String> {
    let stype = source_type.trim().to_ascii_lowercase();
    let loc_trim = location.trim();
    if loc_trim.is_empty() {
        return Err("Lokasi sumber plugin tidak boleh kosong.".to_string());
    }

    let staging_id = format!("stg_{}", &AppState::generate_token()[..8]);
    {
        let mut staging_map = state.plugin_staging.write().await;
        staging_map.retain(|_, entry| {
            let alive = entry.created_at.elapsed().as_secs() <= STAGING_TTL_SECS;
            if !alive {
                if let Some(ref dir) = entry.cleanup_dir {
                    let _ = fs::remove_dir_all(dir);
                }
            }
            alive
        });
    }
    let (cleanup_dir, base_dir, source_label) = match stype.as_str() {
        "local" => {
            let path = PathBuf::from(loc_trim);
            let canon = fs::canonicalize(&path).map_err(|e| {
                format!("Folder plugin lokal '{loc_trim}' tidak ditemukan: {e}")
            })?;
            if !canon.is_dir() {
                return Err(format!("'{loc_trim}' bukan direktori."));
            }
            (None, canon.clone(), format!("local:{}", clean_path_string(&canon)))
        }
        "git" => {
            let validated_url = validate_git_url(loc_trim)?;
            let staging_root = state.plugins_dir().join(".staging").join(&staging_id);
            if staging_root.exists() {
                let _ = fs::remove_dir_all(&staging_root);
            }
            fs::create_dir_all(
                staging_root
                    .parent()
                    .expect("staging dir has parent"),
            )
            .map_err(|e| format!("Gagal membuat direktori staging plugin: {e}"))?;

            let mut cmd = Command::new("git");
            cmd.args(["clone", "--depth", "1", "--", &validated_url])
                .arg(&staging_root)
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(target_os = "windows")]
            cmd.creation_flags(0x08000000);

            let out = tokio::time::timeout(Duration::from_secs(60), cmd.output())
                .await
                .map_err(|_| {
                    let _ = fs::remove_dir_all(&staging_root);
                    "Git clone plugin timeout setelah 60 detik.".to_string()
                })?
                .map_err(|e| {
                    let _ = fs::remove_dir_all(&staging_root);
                    format!("Gagal menjalankan git clone: {e}")
                })?;

            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let _ = fs::remove_dir_all(&staging_root);
                return Err(format!("Git clone plugin gagal: {}", stderr.trim()));
            }
            let canon = fs::canonicalize(&staging_root).unwrap_or_else(|_| staging_root.clone());
            (
                Some(canon.clone()),
                canon,
                format!("git:{validated_url}"),
            )
        }
        other => return Err(format!("source_type plugin tidak dikenal: '{other}' (gunakan 'local' atau 'git').")),
    };

    let inspected_res = (|| -> Result<(PathBuf, PluginManifest, String, Vec<PluginMcpPreviewItem>, Vec<SkillSummary>, Vec<String>, Vec<String>), String> {
        let source_dir = resolve_optional_subdir(&base_dir, subdir)?;
        let has_manifest = source_dir.join("plugin.json").is_file();
        let has_mcp = source_dir.join("mcp.json").is_file();
        let bundled_skills = scan_plugin_skills_preview(&source_dir);
        if !has_manifest && !has_mcp && bundled_skills.is_empty() {
            return Err("Folder tidak berisi plugin.json, mcp.json, maupun skills/*/SKILL.md yang valid.".to_string());
        }

        let (manifest, mut warnings) = load_or_synthesize_manifest(&source_dir)?;
        let mcp_opt = load_plugin_mcp_file(&source_dir)?;
        let (mcp_items, required_secrets, mcp_warnings) = match mcp_opt {
            Some(ref m) => build_mcp_preview_items(m),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        warnings.extend(mcp_warnings);
        if !required_secrets.is_empty() {
            warnings.push(format!(
                "Plugin membutuhkan {} secret terenkripsi DPAPI: {}",
                required_secrets.len(),
                required_secrets.join(", ")
            ));
        }

        let manifest_sha256 = compute_plugin_manifest_hash(&source_dir);
        Ok((
            source_dir,
            manifest,
            manifest_sha256,
            mcp_items,
            bundled_skills,
            required_secrets,
            warnings,
        ))
    })();

    let (
        source_dir,
        manifest,
        manifest_sha256,
        mcp_servers,
        skills,
        required_secrets,
        warnings,
    ) = match inspected_res {
        Ok(v) => v,
        Err(e) => {
            if let Some(dir) = cleanup_dir {
                let _ = fs::remove_dir_all(dir);
            }
            return Err(e);
        }
    };

    let mcp_server_keys: Vec<String> = mcp_servers.iter().map(|m| m.server_key.clone()).collect();
    let entry = PluginStagingEntry {
        staging_id: staging_id.clone(),
        cleanup_dir,
        source_dir,
        source_label,
        manifest: manifest.clone(),
        manifest_sha256: manifest_sha256.clone(),
        mcp_server_keys,
        required_secrets: required_secrets.clone(),
        created_at: Instant::now(),
    };
    state
        .plugin_staging
        .write()
        .await
        .insert(staging_id.clone(), entry);

    Ok(PluginInstallPreview {
        staging_id,
        source_type: stype,
        location: loc_trim.to_string(),
        subdir: subdir.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        manifest,
        manifest_sha256,
        mcp_servers,
        skills,
        required_secrets,
        warnings,
    })
}

pub async fn cancel_staging(state: &AppState, staging_id: &str) {
    let removed = state.plugin_staging.write().await.remove(staging_id.trim());
    if let Some(entry) = removed {
        if let Some(dir) = entry.cleanup_dir {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

fn copy_plugin_tree_recursive(
    src_root: &Path,
    src_dir: &Path,
    dst_dir: &Path,
    file_count: &mut usize,
    total_bytes: &mut u64,
) -> Result<(), String> {
    fs::create_dir_all(dst_dir)
        .map_err(|e| format!("Gagal membuat folder tujuan plugin: {e}"))?;
    let entries = fs::read_dir(src_dir)
        .map_err(|e| format!("Gagal membaca direktori sumber plugin: {e}"))?;
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();

    for path in children {
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name == ".git" || name == "node_modules" || name == ".airlock-meta.json" {
            continue;
        }
        let Ok(sym_meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if sym_meta.file_type().is_symlink() {
            continue;
        }
        let Ok(canon_src) = fs::canonicalize(&path) else {
            continue;
        };
        if !canon_src.starts_with(src_root) {
            continue;
        }
        let dest_path = dst_dir.join(name);
        if sym_meta.is_dir() {
            copy_plugin_tree_recursive(src_root, &canon_src, &dest_path, file_count, total_bytes)?;
        } else if sym_meta.is_file() {
            *file_count += 1;
            if *file_count > MAX_PLUGIN_COPY_FILES {
                return Err(format!(
                    "Paket plugin melebihi batas maksimum {MAX_PLUGIN_COPY_FILES} file."
                ));
            }
            *total_bytes += sym_meta.len();
            if *total_bytes > MAX_PLUGIN_COPY_BYTES {
                return Err("Ukuran total paket plugin melebihi batas 25 MB.".to_string());
            }
            fs::copy(&canon_src, &dest_path)
                .map_err(|e| format!("Gagal menyalin file plugin '{name}': {e}"))?;
        }
    }
    Ok(())
}

pub async fn confirm_install_plugin(
    state: &AppState,
    staging_id: &str,
    enable_immediately: bool,
    secrets: HashMap<String, String>,
) -> Result<InstalledPluginRecord, String> {
    let staged = state
        .plugin_staging
        .write()
        .await
        .remove(staging_id.trim())
        .ok_or_else(|| "Sesi preview/staging plugin tidak ditemukan atau sudah kedaluwarsa.".to_string())?;

    let plugins_root = state.plugins_dir();
    fs::create_dir_all(&plugins_root)
        .map_err(|e| format!("Gagal membuat direktori plugins: {e}"))?;

    let slug = sanitize_plugin_slug(&staged.manifest.name);
    let plugin_id = format!("plg_{slug}_{}", &AppState::generate_token()[..6]);
    let target_dir = plugins_root.join(&plugin_id);

    let canon_src_root =
        fs::canonicalize(&staged.source_dir).unwrap_or_else(|_| staged.source_dir.clone());
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let copy_res = copy_plugin_tree_recursive(
        &canon_src_root,
        &canon_src_root,
        &target_dir,
        &mut file_count,
        &mut total_bytes,
    );

    if let Some(dir) = staged.cleanup_dir {
        let _ = fs::remove_dir_all(dir);
    }

    if let Err(err) = copy_res {
        let _ = fs::remove_dir_all(&target_dir);
        return Err(err);
    }

    let canon_target = fs::canonicalize(&target_dir).unwrap_or_else(|_| target_dir.clone());
    let mcp_server_ids: Vec<String> = staged
        .mcp_server_keys
        .iter()
        .map(|k| format!("{plugin_id}::{k}"))
        .collect();

    let record = InstalledPluginRecord {
        id: plugin_id.clone(),
        name: staged.manifest.name,
        version: staged.manifest.version,
        description: staged.manifest.description,
        source: staged.source_label,
        install_dir: clean_path_string(&canon_target),
        enabled: enable_immediately,
        trusted: true,
        manifest_sha256: staged.manifest_sha256,
        mcp_server_ids,
        required_secrets: staged.required_secrets,
        installed_at: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    };

    if let Ok(meta_json) = serde_json::to_string_pretty(&record) {
        let _ = fs::write(canon_target.join(".airlock-meta.json"), meta_json);
    }

    // Store non-empty secrets strictly in DPAPI SecretsStore (never in config.json)
    {
        let mut secs = state.provider_secrets.write().await;
        let mut clean_map = HashMap::new();
        for (k, v) in secrets {
            let kt = k.trim();
            let vt = v.trim();
            if !kt.is_empty() && !vt.is_empty() {
                clean_map.insert(kt.to_string(), vt.to_string());
            }
        }
        if !clean_map.is_empty() {
            secs.plugin_secrets.insert(plugin_id.clone(), clean_map);
            state.secrets_store.save(&secs)?;
        }
    }

    {
        let mut cfg = state.config.write().await;
        cfg.installed_plugins.push(record.clone());
    }
    state.save_config_state().await;

    if enable_immediately {
        let plugin_cfgs: Vec<DownstreamMcpConfig> = state
            .resolved_downstream_mcps()
            .await
            .into_iter()
            .filter(|c| c.plugin_id.as_deref() == Some(&plugin_id))
            .collect();
        for cfg in plugin_cfgs {
            let _ = state.mcp_hub.refresh_server(&state.http_client, &cfg).await;
        }
    }

    let _ = state.refresh_skills().await;
    Ok(record)
}

pub async fn toggle_plugin(state: &AppState, plugin_id: &str, enabled: bool) -> Result<(), String> {
    let server_ids = {
        let mut cfg = state.config.write().await;
        let Some(rec) = cfg
            .installed_plugins
            .iter_mut()
            .find(|p| p.id == plugin_id)
        else {
            return Err(format!("Plugin '{plugin_id}' tidak ditemukan."));
        };
        rec.enabled = enabled;
        if enabled {
            rec.trusted = true;
        }
        rec.mcp_server_ids.clone()
    };

    state.save_config_state().await;

    if enabled {
        let plugin_cfgs: Vec<DownstreamMcpConfig> = state
            .resolved_downstream_mcps()
            .await
            .into_iter()
            .filter(|c| c.plugin_id.as_deref() == Some(plugin_id))
            .collect();
        for cfg in plugin_cfgs {
            let _ = state.mcp_hub.refresh_server(&state.http_client, &cfg).await;
        }
    } else {
        for sid in server_ids {
            state.mcp_hub.remove_server(&sid).await;
        }
    }

    let _ = state.refresh_skills().await;
    Ok(())
}

pub async fn update_plugin_secrets(
    state: &AppState,
    plugin_id: &str,
    secrets: HashMap<String, String>,
) -> Result<(), String> {
    let is_enabled = {
        let cfg = state.config.read().await;
        let Some(rec) = cfg.installed_plugins.iter().find(|p| p.id == plugin_id) else {
            return Err(format!("Plugin '{plugin_id}' tidak ditemukan."));
        };
        rec.enabled
    };

    {
        let mut secs = state.provider_secrets.write().await;
        let entry = secs
            .plugin_secrets
            .entry(plugin_id.to_string())
            .or_default();
        for (k, v) in secrets {
            let kt = k.trim();
            if kt.is_empty() {
                continue;
            }
            let vt = v.trim();
            if vt == "__CLEAR__" {
                entry.remove(kt);
            } else if !vt.is_empty() {
                entry.insert(kt.to_string(), vt.to_string());
            }
        }
        if entry.is_empty() {
            secs.plugin_secrets.remove(plugin_id);
        }
        state.secrets_store.save(&secs)?;
    }

    if is_enabled {
        let plugin_cfgs: Vec<DownstreamMcpConfig> = state
            .resolved_downstream_mcps()
            .await
            .into_iter()
            .filter(|c| c.plugin_id.as_deref() == Some(plugin_id))
            .collect();
        for cfg in plugin_cfgs {
            let _ = state.mcp_hub.refresh_server(&state.http_client, &cfg).await;
        }
    }

    Ok(())
}

pub async fn uninstall_plugin(state: &AppState, plugin_id: &str) -> Result<(), String> {
    let removed = {
        let mut cfg = state.config.write().await;
        let Some(idx) = cfg.installed_plugins.iter().position(|p| p.id == plugin_id) else {
            return Err(format!("Plugin '{plugin_id}' tidak ditemukan."));
        };
        cfg.installed_plugins.remove(idx)
    };

    for sid in &removed.mcp_server_ids {
        state.mcp_hub.remove_server(sid).await;
    }

    {
        let mut secs = state.provider_secrets.write().await;
        if secs.plugin_secrets.remove(plugin_id).is_some() {
            let _ = state.secrets_store.save(&secs);
        }
    }

    let plugins_root = fs::canonicalize(state.plugins_dir()).unwrap_or_else(|_| state.plugins_dir());
    let install_path = PathBuf::from(&removed.install_dir);
    if let Ok(canon_install) = fs::canonicalize(&install_path) {
        if canon_install.starts_with(&plugins_root) && canon_install != plugins_root {
            let _ = fs::remove_dir_all(canon_install);
        }
    }

    state.save_config_state().await;
    let _ = state.refresh_skills().await;
    Ok(())
}

pub fn cleanup_stale_staging(plugins_dir: &Path) {
    let staging_dir = plugins_dir.join(".staging");
    let Ok(entries) = fs::read_dir(&staging_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if let Ok(age) = now.duration_since(modified) {
                    if age.as_secs() > STAGING_TTL_SECS {
                        let _ = fs::remove_dir_all(path);
                    }
                }
            }
        }
    }
}

pub fn build_plugin_summaries(
    plugins: &[InstalledPluginRecord],
    secrets: &ProviderSecrets,
    skills: &[SkillEntry],
) -> Vec<PluginSummary> {
    plugins
        .iter()
        .map(|p| {
            let stored = secrets.plugin_secrets.get(&p.id);
            let mut missing = Vec::new();
            let mut configured = Vec::new();
            for req in &p.required_secrets {
                let has_val = stored
                    .and_then(|m| m.get(req))
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false);
                if has_val {
                    configured.push(req.clone());
                } else {
                    missing.push(req.clone());
                }
            }
            let skills_count = skills
                .iter()
                .filter(|s| {
                    s.summary.scope == SkillScope::Plugin
                        && s.summary.plugin_id.as_deref() == Some(p.id.as_str())
                })
                .count();
            PluginSummary {
                id: p.id.clone(),
                name: p.name.clone(),
                version: p.version.clone(),
                description: p.description.clone(),
                source: p.source.clone(),
                install_dir: p.install_dir.clone(),
                enabled: p.enabled,
                trusted: p.trusted,
                manifest_sha256: p.manifest_sha256.clone(),
                mcp_servers_count: p.mcp_server_ids.len(),
                skills_count,
                required_secrets: p.required_secrets.clone(),
                missing_secrets: missing,
                configured_secrets: configured,
                installed_at: p.installed_at.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretsStore;

    #[test]
    fn test_git_url_validation_blocks_option_injection() {
        assert!(validate_git_url("--upload-pack=calc.exe").is_err());
        assert!(validate_git_url("-c core.sshCommand=evil https://github.com/a/b").is_err());
        assert!(validate_git_url("file:///C:/Windows/System32").is_err());
        assert!(validate_git_url("ext::sh -c evil").is_err());
        assert!(validate_git_url("http://example.com/repo.git").is_err());
        assert!(validate_git_url("").is_err());

        assert!(validate_git_url("https://github.com/modelcontextprotocol/servers.git").is_ok());
        assert!(validate_git_url("git@github.com:modelcontextprotocol/servers.git").is_ok());
        assert!(validate_git_url("http://127.0.0.1:8080/test-plugin.git").is_ok());
    }

    #[tokio::test]
    async fn test_plugin_mcp_secret_interpolation_never_leaks_to_config_json() {
        let base = std::env::temp_dir().join("smart_mcp_plugin_secret_interp_test");
        let _ = fs::remove_dir_all(&base);
        let data_dir = base.join("data");
        let pkg_dir = base.join("sample_plugin");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&pkg_dir).unwrap();

        fs::write(
            pkg_dir.join("plugin.json"),
            r#"{"name":"VaultProbe","version":"1.2.0","description":"Tests secret interpolation"}"#,
        )
        .unwrap();
        fs::write(
            pkg_dir.join("mcp.json"),
            r#"{
              "mcpServers": {
                "probe": {
                  "transport": "http",
                  "url": "http://127.0.0.1:59999/mcp",
                  "headers": {
                    "Authorization": "Bearer ${SECRET:API_KEY}",
                    "X-Custom-Key": "${SECRET:EXTRA_KEY}"
                  },
                  "env": {
                    "INTERNAL_TOKEN": "${SECRET:API_KEY}"
                  }
                }
              }
            }"#,
        )
        .unwrap();

        let state = AppState::new_persistent(
            base.clone(),
            data_dir.join("auth.json"),
            data_dir.join("config.json"),
        );

        let preview = inspect_plugin_source(
            &state,
            "local",
            pkg_dir.to_str().unwrap(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(preview.required_secrets, vec!["API_KEY", "EXTRA_KEY"]);

        let mut sec_vals = HashMap::new();
        sec_vals.insert("API_KEY".to_string(), "super-secret-api-key-value".to_string());
        sec_vals.insert("EXTRA_KEY".to_string(), "extra-secret-header-value".to_string());

        let installed = confirm_install_plugin(&state, &preview.staging_id, false, sec_vals)
            .await
            .unwrap();
        toggle_plugin(&state, &installed.id, true).await.unwrap();

        let resolved = state.resolved_downstream_mcps().await;
        let probe = resolved
            .iter()
            .find(|c| c.plugin_id.as_deref() == Some(installed.id.as_str()))
            .expect("materialized plugin MCP should be present");
        assert_eq!(probe.bearer_token, "super-secret-api-key-value");
        assert_eq!(
            probe.headers.get("X-Custom-Key").map(String::as_str),
            Some("extra-secret-header-value")
        );
        assert_eq!(
            probe.env.get("INTERNAL_TOKEN").map(String::as_str),
            Some("super-secret-api-key-value")
        );

        // config.json must stay completely clean of secret values and plugin materialized configs
        let cfg_on_disk = fs::read_to_string(data_dir.join("config.json")).unwrap();
        assert!(!cfg_on_disk.contains("super-secret-api-key-value"));
        assert!(!cfg_on_disk.contains("extra-secret-header-value"));

        // DPAPI SecretsStore persists the plugin secrets safely
        let vault = SecretsStore::new_persistent(data_dir.clone()).load();
        assert_eq!(
            vault
                .plugin_secrets
                .get(&installed.id)
                .and_then(|m| m.get("API_KEY"))
                .map(String::as_str),
            Some("super-secret-api-key-value")
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn test_plugin_install_toggle_and_uninstall_lifecycle() {
        let base = std::env::temp_dir().join("smart_mcp_plugin_lifecycle_test");
        let _ = fs::remove_dir_all(&base);
        let data_dir = base.join("data");
        let pkg_dir = base.join("bundle_plugin");
        fs::create_dir_all(pkg_dir.join("skills/bundle-helper/references")).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        fs::write(
            pkg_dir.join("plugin.json"),
            r#"{"name":"BundleSuite","version":"2.0.0","description":"Bundled skill + MCP"}"#,
        )
        .unwrap();
        fs::write(
            pkg_dir.join("mcp.json"),
            r#"{"mcpServers":{"docs":{"transport":"http","url":"http://127.0.0.1:59998/mcp"}}}"#,
        )
        .unwrap();
        fs::write(
            pkg_dir.join("skills/bundle-helper/SKILL.md"),
            "---\nname: bundle-helper\ndescription: Skill from BundleSuite\n---\nHelper steps",
        )
        .unwrap();
        fs::write(
            pkg_dir.join("skills/bundle-helper/references/ref.txt"),
            "ref data",
        )
        .unwrap();

        let state = AppState::new_persistent(
            base.clone(),
            data_dir.join("auth.json"),
            data_dir.join("config.json"),
        );

        let preview = inspect_plugin_source(&state, "local", pkg_dir.to_str().unwrap(), None)
            .await
            .unwrap();
        assert_eq!(preview.skills.len(), 1);
        assert_eq!(preview.mcp_servers.len(), 1);

        let record = confirm_install_plugin(&state, &preview.staging_id, false, HashMap::new())
            .await
            .unwrap();
        assert!(!record.enabled);
        assert!(state.skills_cache.read().await.is_empty());

        // Enable -> skill and downstream MCP appear
        toggle_plugin(&state, &record.id, true).await.unwrap();
        assert_eq!(state.skills_cache.read().await.len(), 1);
        assert_eq!(state.resolved_downstream_mcps().await.len(), 1);

        // Disable -> skill and downstream MCP disappear immediately
        toggle_plugin(&state, &record.id, false).await.unwrap();
        assert!(state.skills_cache.read().await.is_empty());
        assert!(state.resolved_downstream_mcps().await.is_empty());

        // Re-enable then uninstall -> everything cleaned from config, disk, skills, secrets
        toggle_plugin(&state, &record.id, true).await.unwrap();
        let mut sec = HashMap::new();
        sec.insert("TOKEN".to_string(), "temp_tok".to_string());
        update_plugin_secrets(&state, &record.id, sec)
            .await
            .unwrap();

        let install_dir = PathBuf::from(&record.install_dir);
        assert!(install_dir.exists());

        uninstall_plugin(&state, &record.id).await.unwrap();
        assert!(!install_dir.exists());
        assert!(state.config.read().await.installed_plugins.is_empty());
        assert!(state.skills_cache.read().await.is_empty());
        assert!(state.resolved_downstream_mcps().await.is_empty());
        assert!(!state
            .provider_secrets
            .read()
            .await
            .plugin_secrets
            .contains_key(&record.id));

        let _ = fs::remove_dir_all(&base);
    }
}
