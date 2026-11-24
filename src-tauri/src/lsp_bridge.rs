use crate::process_util::kill_pid_tree;
use crate::security::{compress_output, resolve_jailed_path};
use crate::state::{AppState, AuditEntry};
use crate::tools::{check_sensitive_guard_for_workspace, sha256_hex};
use crate::workspace::{clean_path_string, normalize_clean_path, ResolvedWorkspace};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter,
};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio::task::AbortHandle;

const MAX_ACTIVE_LSP_SERVERS: usize = 2;
const MAX_LSP_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct LspServerSummary {
    pub key: String,
    pub lang_id: String,
    pub workspace_alias: String,
    pub workspace_path: String,
    pub command: String,
    pub status: String,
    pub idle_secs: u64,
    pub open_docs_count: usize,
    pub diagnostics_count: usize,
}

struct LspClient {
    key: String,
    lang_id: String,
    workspace_alias: String,
    workspace_path: String,
    command: String,
    pid: Option<u32>,
    child: Mutex<Child>,
    stdin: Arc<Mutex<BufWriter<ChildStdin>>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    diagnostics: Arc<RwLock<HashMap<String, Vec<Value>>>>,
    indexing: Arc<AtomicBool>,
    open_docs: Arc<Mutex<HashMap<String, (i32, String)>>>,
    last_used: RwLock<Instant>,
    reader_abort: AbortHandle,
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.reader_abort.abort();
        if let Some(pid) = self.pid {
            kill_pid_tree(pid);
        }
        if let Ok(mut ch) = self.child.try_lock() {
            let _ = ch.start_kill();
        }
    }
}

pub struct LspManager {
    servers: RwLock<HashMap<String, Arc<LspClient>>>,
    spawn_lock: Mutex<()>,
}

impl Default for LspManager {
    fn default() -> Self {
        Self {
            servers: RwLock::new(HashMap::new()),
            spawn_lock: Mutex::new(()),
        }
    }
}

pub async fn write_lsp_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Value,
) -> Result<(), String> {
    let body = serde_json::to_string(msg)
        .map_err(|e| format!("Gagal serialisasi pesan LSP: {e}"))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(|e| format!("Gagal menulis header LSP: {e}"))?;
    writer
        .write_all(body.as_bytes())
        .await
        .map_err(|e| format!("Gagal menulis body LSP: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("Gagal flush stdin LSP: {e}"))?;
    Ok(())
}

pub async fn read_lsp_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Value>, String> {
    let mut content_length: Option<usize> = None;
    let mut saw_header = false;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("Gagal membaca header LSP: {e}"))?;
        if n == 0 {
            if !saw_header {
                return Ok(None);
            }
            return Err("Proses LSP menutup stream di tengah header.".to_string());
        }

        let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
        if trimmed.is_empty() {
            if !saw_header {
                continue;
            }
            break;
        }
        saw_header = true;
        if let Some((name, val)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                let len = val
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| format!("Header Content-Length LSP tidak valid: '{}'", val.trim()))?;
                if len == 0 || len > MAX_LSP_MESSAGE_BYTES {
                    return Err(format!("Content-Length LSP di luar batas aman: {len}"));
                }
                content_length = Some(len);
            }
        }
    }

    let len = content_length
        .ok_or_else(|| "Header LSP tidak menyertakan Content-Length.".to_string())?;
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .map_err(|e| format!("Gagal membaca body LSP ({len} bytes): {e}"))?;

    let parsed: Value = serde_json::from_slice(&buf)
        .map_err(|e| format!("Payload JSON LSP tidak valid: {e}"))?;
    Ok(Some(parsed))
}

pub fn path_to_lsp_uri(path: &Path) -> Result<String, String> {
    let clean = clean_path_string(path);
    let url = url::Url::from_file_path(Path::new(&clean))
        .map_err(|_| format!("Gagal mengonversi path ke file URI: {clean}"))?;
    Ok(url.to_string())
}

fn normalize_uri_key(uri: &str) -> String {
    if let Ok(parsed) = url::Url::parse(uri) {
        if parsed.scheme() == "file" {
            if let Ok(p) = parsed.to_file_path() {
                let clean = clean_path_string(&p).replace('\\', "/");
                return if cfg!(target_os = "windows") {
                    clean.to_ascii_lowercase()
                } else {
                    clean
                };
            }
        }
    }
    uri.trim().to_string()
}

pub fn lsp_uri_to_jailed_path(
    canon_root: &Path,
    uri: &str,
) -> Result<Option<(PathBuf, String)>, String> {
    let parsed = url::Url::parse(uri.trim())
        .map_err(|e| format!("URI LSP tidak valid '{}': {e}", uri))?;
    if parsed.scheme() != "file" {
        return Ok(None);
    }
    let Ok(raw_path) = parsed.to_file_path() else {
        return Ok(None);
    };

    let canon_ws = std::fs::canonicalize(canon_root).unwrap_or_else(|_| canon_root.to_path_buf());
    let candidate = match std::fs::canonicalize(&raw_path) {
        Ok(c) => c,
        Err(_) => raw_path,
    };

    let clean_ws = clean_path_string(&canon_ws).replace('\\', "/");
    let clean_cand = clean_path_string(&candidate).replace('\\', "/");

    let rel_str_opt = if let Ok(rel) = candidate.strip_prefix(&canon_ws) {
        Some(rel.to_string_lossy().replace('\\', "/"))
    } else if cfg!(target_os = "windows") {
        let ws_lower = clean_ws.trim_end_matches('/').to_ascii_lowercase();
        let cand_lower = clean_cand.to_ascii_lowercase();
        if cand_lower == ws_lower {
            Some(".".to_string())
        } else if let Some(rest) = cand_lower.strip_prefix(&(ws_lower + "/")) {
            let prefix_len = clean_ws.trim_end_matches('/').len() + 1;
            let _ = rest;
            Some(clean_cand[prefix_len..].to_string())
        } else {
            None
        }
    } else {
        None
    };

    let Some(rel_str) = rel_str_opt else {
        return Ok(None);
    };

    if rel_str.is_empty() || rel_str == "." {
        return Ok(None);
    }

    let jailed = resolve_jailed_path(&canon_ws, &rel_str)?;
    Ok(Some((jailed, rel_str)))
}

fn detect_lsp_for_ext(ext: &str) -> Option<(&'static str, &'static str, Vec<&'static str>)> {
    match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "rs" => Some(("rust", "rust-analyzer", vec![])),
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => {
            Some(("typescript", "typescript-language-server", vec!["--stdio"]))
        }
        "py" | "pyi" => Some(("python", "pyright-langserver", vec!["--stdio"])),
        "go" => Some(("go", "gopls", vec![])),
        _ => None,
    }
}

fn detect_workspace_default_lang(root: &Path) -> &'static str {
    if root.join("Cargo.toml").exists() {
        "rust"
    } else if root.join("tsconfig.json").exists() || root.join("package.json").exists() {
        "typescript"
    } else if root.join("go.mod").exists() {
        "go"
    } else {
        "python"
    }
}

fn find_executable_in_path(cmd_name: &str) -> Option<PathBuf> {
    let trimmed = cmd_name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let direct = Path::new(trimmed);
    if direct.components().count() > 1 || direct.is_absolute() {
        return direct.is_file().then(|| direct.to_path_buf());
    }

    let path_var = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(target_os = "windows") {
        let pathext = std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let mut out = vec![String::new()];
        for ext in pathext.split(';') {
            let clean = ext.trim();
            if !clean.is_empty() {
                out.push(clean.to_string());
            }
        }
        out
    } else {
        vec![String::new()]
    };

    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = if ext.is_empty() || trimmed.to_ascii_lowercase().ends_with(&ext.to_ascii_lowercase()) {
                dir.join(trimmed)
            } else {
                dir.join(format!("{trimmed}{ext}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

impl LspClient {
    async fn send_notification(&self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        let mut stdin = self.stdin.lock().await;
        write_lsp_message(&mut *stdin, &msg).await
    }

    async fn send_request(
        &self,
        method: &str,
        params: Value,
        timeout_secs: u64,
    ) -> Result<Value, String> {
        *self.last_used.write().await = Instant::now();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<Value>();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        {
            let mut stdin = self.stdin.lock().await;
            if let Err(err) = write_lsp_message(&mut *stdin, &payload).await {
                self.pending.lock().await.remove(&id);
                return Err(err);
            }
        }

        match tokio::time::timeout(Duration::from_secs(timeout_secs.max(1)), rx).await {
            Ok(Ok(val)) => {
                *self.last_used.write().await = Instant::now();
                Ok(val)
            }
            Ok(Err(_)) => Err(format!("LSP '{}' menutup koneksi sebelum merespons '{method}'.", self.command)),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                if self.indexing.load(Ordering::Relaxed) {
                    Err(format!(
                        "LSP '{}' sedang indexing workspace; coba panggil kembali dalam beberapa detik.",
                        self.command
                    ))
                } else {
                    Err(format!(
                        "Timeout ({timeout_secs}s) menunggu respons LSP '{}' untuk '{method}'.",
                        self.command
                    ))
                }
            }
        }
    }

    async fn sync_document(
        &self,
        state: &AppState,
        ws: &ResolvedWorkspace,
        rel_path: &str,
    ) -> Result<(PathBuf, String), String> {
        let target = resolve_jailed_path(&ws.canon_root, rel_path)?;
        check_sensitive_guard_for_workspace(state, ws, &target, rel_path).await?;
        let text = fs::read_to_string(&target)
            .map_err(|e| format!("Gagal membaca file '{rel_path}': {e}"))?;
        let uri = path_to_lsp_uri(&target)?;
        let hash = sha256_hex(text.as_bytes());
        let ext = target
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let doc_lang = match ext.as_str() {
            "rs" => "rust",
            "ts" | "mts" | "cts" => "typescript",
            "tsx" => "typescriptreact",
            "js" | "mjs" | "cjs" => "javascript",
            "jsx" => "javascriptreact",
            "py" | "pyi" => "python",
            "go" => "go",
            _ => self.lang_id.as_str(),
        };

        let mut docs = self.open_docs.lock().await;
        match docs.get_mut(&uri) {
            None => {
                docs.insert(uri.clone(), (1, hash));
                drop(docs);
                self.send_notification(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": doc_lang,
                            "version": 1,
                            "text": text
                        }
                    }),
                )
                .await?;
            }
            Some((version, old_hash)) if *old_hash != hash => {
                *version += 1;
                *old_hash = hash;
                let next_ver = *version;
                drop(docs);
                self.send_notification(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "version": next_ver
                        },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await?;
            }
            _ => {}
        }

        *self.last_used.write().await = Instant::now();
        Ok((target, uri))
    }

    async fn shutdown(&self) {
        let _ = self.send_request("shutdown", Value::Null, 1).await;
        let _ = self.send_notification("exit", Value::Null).await;
        self.reader_abort.abort();
        if let Some(pid) = self.pid {
            kill_pid_tree(pid);
        }
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
    }

    async fn to_summary(&self) -> LspServerSummary {
        let idle_secs = self.last_used.read().await.elapsed().as_secs();
        let open_docs_count = self.open_docs.lock().await.len();
        let diagnostics_count: usize = self
            .diagnostics
            .read()
            .await
            .values()
            .map(|v| v.len())
            .sum();
        let status = if self.indexing.load(Ordering::Relaxed) {
            "indexing".to_string()
        } else {
            "ready".to_string()
        };
        LspServerSummary {
            key: self.key.clone(),
            lang_id: self.lang_id.clone(),
            workspace_alias: self.workspace_alias.clone(),
            workspace_path: self.workspace_path.clone(),
            command: self.command.clone(),
            status,
            idle_secs,
            open_docs_count,
            diagnostics_count,
        }
    }
}

impl LspManager {
    async fn resolve_command_and_args(
        &self,
        state: &AppState,
        lang_id: &str,
        default_cmd: &str,
        default_args: &[&str],
    ) -> Result<(String, Vec<String>), String> {
        let override_opt = {
            let cfg = state.config.read().await;
            cfg.lsp_servers.get(lang_id).cloned()
        };
        if let Some(ov) = override_opt {
            let cmd = ov.command.trim().to_string();
            if !cmd.is_empty() {
                if find_executable_in_path(&cmd).is_some() {
                    return Ok((cmd, ov.args));
                }
                return Err(format!(
                    "LSP '{}' (override untuk '{lang_id}') tidak ditemukan di PATH. Periksa AppConfig.lsp_servers atau gunakan run_command_compressed.",
                    cmd
                ));
            }
        }

        if find_executable_in_path(default_cmd).is_some() {
            return Ok((
                default_cmd.to_string(),
                default_args.iter().map(|s| (*s).to_string()).collect(),
            ));
        }

        if lang_id == "python" && find_executable_in_path("basedpyright-langserver").is_some() {
            return Ok((
                "basedpyright-langserver".to_string(),
                vec!["--stdio".to_string()],
            ));
        }

        Err(format!(
            "LSP '{}' tidak ditemukan di PATH. Instal server LSP untuk '{}' atau gunakan run_command_compressed untuk verifikasi.",
            default_cmd, lang_id
        ))
    }

    async fn get_or_spawn(
        &self,
        state: &AppState,
        ws: &ResolvedWorkspace,
        lang_id: &str,
        default_cmd: &str,
        default_args: &[&str],
    ) -> Result<Arc<LspClient>, String> {
        let ws_key = if cfg!(target_os = "windows") {
            ws.clean_path.to_ascii_lowercase()
        } else {
            ws.clean_path.clone()
        };
        let key = format!("{ws_key}::{lang_id}");

        {
            let servers = self.servers.read().await;
            if let Some(existing) = servers.get(&key) {
                let mut child = existing.child.lock().await;
                let exited = matches!(child.try_wait(), Ok(Some(_)));
                drop(child);
                if !exited {
                    *existing.last_used.write().await = Instant::now();
                    return Ok(Arc::clone(existing));
                }
            }
        }

        let _spawn_guard = self.spawn_lock.lock().await;
        // Double-check after acquiring spawn_lock
        {
            let servers = self.servers.read().await;
            if let Some(existing) = servers.get(&key) {
                let mut child = existing.child.lock().await;
                let exited = matches!(child.try_wait(), Ok(Some(_)));
                drop(child);
                if !exited {
                    *existing.last_used.write().await = Instant::now();
                    return Ok(Arc::clone(existing));
                }
            }
        }

        let (cmd_name, args) = self
            .resolve_command_and_args(state, lang_id, default_cmd, default_args)
            .await?;

        // Evict LRU servers to enforce MAX_ACTIVE_LSP_SERVERS = 2
        loop {
            let oldest_to_evict = {
                let servers = self.servers.read().await;
                let active_count = servers.len() - usize::from(servers.contains_key(&key));
                if active_count < MAX_ACTIVE_LSP_SERVERS {
                    None
                } else {
                    let mut oldest: Option<(String, Instant)> = None;
                    for (k, cli) in servers.iter() {
                        if k == &key {
                            continue;
                        }
                        let ts = *cli.last_used.read().await;
                        if oldest.as_ref().map(|(_, t)| ts < *t).unwrap_or(true) {
                            oldest = Some((k.clone(), ts));
                        }
                    }
                    oldest.map(|(k, _)| k)
                }
            };
            let Some(evict_key) = oldest_to_evict else {
                break;
            };
            let removed = {
                let mut servers = self.servers.write().await;
                servers.remove(&evict_key)
            };
            if let Some(srv) = removed {
                srv.shutdown().await;
            }
        }

        let resolved_exe = find_executable_in_path(&cmd_name);
        let is_windows_script = cfg!(target_os = "windows")
            && resolved_exe
                .as_ref()
                .and_then(|p| p.extension())
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
                .unwrap_or(false);

        let mut cmd = if is_windows_script {
            let mut c = Command::new("cmd.exe");
            c.arg("/C").arg(&cmd_name).args(&args);
            c
        } else {
            let mut c = Command::new(&cmd_name);
            c.args(&args);
            c
        };

        cmd.current_dir(&ws.clean_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000);

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "LSP '{}' tidak ditemukan di PATH. Instal server LSP untuk '{}' atau gunakan run_command_compressed.",
                    cmd_name, lang_id
                )
            } else {
                format!("Gagal menjalankan LSP '{}': {e}", cmd_name)
            }
        })?;

        let pid = child.id();
        let child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("Gagal membuka stdin LSP '{cmd_name}'."))?;
        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("Gagal membuka stdout LSP '{cmd_name}'."))?;

        let stdin_arc = Arc::new(Mutex::new(BufWriter::new(child_stdin)));
        let pending_arc: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diag_arc: Arc<RwLock<HashMap<String, Vec<Value>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let indexing_arc = Arc::new(AtomicBool::new(false));

        let reader_stdin = Arc::clone(&stdin_arc);
        let reader_pending = Arc::clone(&pending_arc);
        let reader_diag = Arc::clone(&diag_arc);
        let reader_indexing = Arc::clone(&indexing_arc);

        let reader_handle = tokio::spawn(async move {
            let mut buf_reader = BufReader::new(child_stdout);
            while let Ok(Some(msg)) = read_lsp_message(&mut buf_reader).await {
                let method_opt = msg.get("method").and_then(|v| v.as_str()).map(str::to_string);
                let id_opt = msg.get("id").cloned();

                match (method_opt, id_opt) {
                    (None, Some(id_val)) => {
                        if let Some(id_num) = id_val.as_u64() {
                            if let Some(tx) = reader_pending.lock().await.remove(&id_num) {
                                let _ = tx.send(msg);
                            }
                        }
                    }
                    (Some(method), Some(req_id)) => {
                        let reply = match method.as_str() {
                            "workspace/configuration" => {
                                let count = msg["params"]["items"]
                                    .as_array()
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": req_id,
                                    "result": vec![Value::Null; count]
                                })
                            }
                            "window/workDoneProgress/create" | "client/registerCapability" => {
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": req_id,
                                    "result": Value::Null
                                })
                            }
                            "workspace/applyEdit" => {
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": req_id,
                                    "result": { "applied": false, "failureReason": "Airlock LSP bridge is read-only." }
                                })
                            }
                            _ => json!({
                                "jsonrpc": "2.0",
                                "id": req_id,
                                "error": { "code": -32601, "message": format!("Method not handled: {method}") }
                            }),
                        };
                        let mut st = reader_stdin.lock().await;
                        let _ = write_lsp_message(&mut *st, &reply).await;
                    }
                    (Some(method), None) => {
                        if method == "textDocument/publishDiagnostics" {
                            if let Some(uri) = msg["params"]["uri"].as_str() {
                                let norm = normalize_uri_key(uri);
                                let diags = msg["params"]["diagnostics"]
                                    .as_array()
                                    .cloned()
                                    .unwrap_or_default();
                                let mut map = reader_diag.write().await;
                                if diags.is_empty() {
                                    map.remove(&norm);
                                } else {
                                    map.insert(norm, diags);
                                }
                            }
                        } else if method == "$/progress" {
                            if let Some(kind) = msg["params"]["value"]["kind"].as_str() {
                                if kind == "begin" {
                                    reader_indexing.store(true, Ordering::Relaxed);
                                } else if kind == "end" {
                                    reader_indexing.store(false, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    (None, None) => {}
                }
            }
        });

        let client = Arc::new(LspClient {
            key: key.clone(),
            lang_id: lang_id.to_string(),
            workspace_alias: ws.alias.clone(),
            workspace_path: ws.clean_path.clone(),
            command: cmd_name.clone(),
            pid,
            child: Mutex::new(child),
            stdin: stdin_arc,
            next_id: AtomicU64::new(1),
            pending: pending_arc,
            diagnostics: diag_arc,
            indexing: indexing_arc,
            open_docs: Arc::new(Mutex::new(HashMap::new())),
            last_used: RwLock::new(Instant::now()),
            reader_abort: reader_handle.abort_handle(),
        });

        let root_uri = path_to_lsp_uri(&ws.canon_root)?;
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": ws.alias }],
            "capabilities": {
                "window": { "workDoneProgress": true },
                "textDocument": {
                    "synchronization": { "didSave": false, "willSave": false },
                    "publishDiagnostics": { "relatedInformation": true },
                    "definition": { "linkSupport": true },
                    "references": {},
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "diagnostic": {}
                }
            },
            "clientInfo": { "name": "airlock", "version": "0.1.0" }
        });

        if let Err(err) = client.send_request("initialize", init_params, 12).await {
            client.shutdown().await;
            return Err(err);
        }
        client.send_notification("initialized", json!({})).await?;

        let mut servers = self.servers.write().await;
        if let Some(old) = servers.insert(key, Arc::clone(&client)) {
            drop(servers);
            old.shutdown().await;
        }
        Ok(client)
    }

    pub async fn reap_idle_servers(&self, max_idle: Duration) {
        let stale_keys = {
            let servers = self.servers.read().await;
            let mut out = Vec::new();
            for (k, srv) in servers.iter() {
                if srv.last_used.read().await.elapsed() >= max_idle {
                    out.push(k.clone());
                }
            }
            out
        };
        for key in stale_keys {
            let removed = {
                let mut servers = self.servers.write().await;
                servers.remove(&key)
            };
            if let Some(srv) = removed {
                srv.shutdown().await;
            }
        }
    }

    pub async fn stop_server(&self, key: Option<&str>) -> usize {
        let to_stop: Vec<Arc<LspClient>> = {
            let mut servers = self.servers.write().await;
            match key.map(str::trim).filter(|s| !s.is_empty()) {
                Some(k) => servers.remove(k).into_iter().collect(),
                None => servers.drain().map(|(_, v)| v).collect(),
            }
        };
        let count = to_stop.len();
        for srv in to_stop {
            srv.shutdown().await;
        }
        count
    }

    pub async fn shutdown_all(&self) {
        let _ = self.stop_server(None).await;
    }

    pub async fn list_summaries(&self) -> Vec<LspServerSummary> {
        let list: Vec<Arc<LspClient>> = {
            let servers = self.servers.read().await;
            servers.values().cloned().collect()
        };
        let mut out = Vec::with_capacity(list.len());
        for srv in list {
            out.push(srv.to_summary().await);
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }
}

fn severity_label(code: u64) -> &'static str {
    match code {
        1 => "error",
        2 => "warning",
        3 => "info",
        4 => "hint",
        _ => "diag",
    }
}

fn severity_matches(code: u64, filter: &str) -> bool {
    match filter {
        "error" => code == 1,
        "warning" => code == 1 || code == 2,
        _ => true,
    }
}

fn format_lsp_diagnostic_line(diag: &Value) -> String {
    let line = diag["range"]["start"]["line"].as_u64().unwrap_or(0) + 1;
    let col = diag["range"]["start"]["character"].as_u64().unwrap_or(0) + 1;
    let sev = severity_label(diag["severity"].as_u64().unwrap_or(1));
    let msg = diag["message"]
        .as_str()
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .trim();
    format!("L{line:>4}:{col} [{sev}] {msg}")
}

fn lsp_timeout_for_source(source: &str) -> u64 {
    if source == "WebMCP" {
        12
    } else {
        25
    }
}

async fn record_lsp_audit(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    tool_name: &str,
    target_path: Option<String>,
    summary: String,
    output_len: usize,
) {
    let tokens_saved = if output_len > 0 { 120 } else { 20 };
    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: tool_name.to_string(),
                target_path,
                summary,
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
}

pub async fn exec_lsp_diagnostics(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let path_opt = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let severity = args
        .get("severity")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "all".to_string());
    let timeout_secs = lsp_timeout_for_source(source);

    let (client, target_rel, target_uri_opt) = if let Some(rel) = path_opt {
        let target = resolve_jailed_path(&ws.canon_root, rel)?;
        check_sensitive_guard_for_workspace(state, ws, &target, rel).await?;
        let ext = target
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let (lang_id, default_cmd, default_args) = detect_lsp_for_ext(ext).ok_or_else(|| {
            format!("Bahasa untuk file '{rel}' belum didukung LspBridge (didukung: rs, ts, tsx, js, jsx, py, go).")
        })?;
        let cli = state
            .lsp_manager
            .get_or_spawn(state, ws, lang_id, default_cmd, &default_args)
            .await?;
        let (_, uri) = cli.sync_document(state, ws, rel).await?;
        (cli, Some(rel.to_string()), Some(uri))
    } else {
        let lang_id = detect_workspace_default_lang(&ws.canon_root);
        let (_, default_cmd, default_args) = detect_lsp_for_ext(match lang_id {
            "rust" => "rs",
            "typescript" => "ts",
            "go" => "go",
            _ => "py",
        })
        .expect("default language supported");
        let cli = state
            .lsp_manager
            .get_or_spawn(state, ws, lang_id, default_cmd, &default_args)
            .await?;
        (cli, None, None)
    };

    let mut lines_out = Vec::new();
    if let (Some(rel), Some(uri)) = (&target_rel, &target_uri_opt) {
        let mut items: Vec<Value> = Vec::new();
        if let Ok(pull_resp) = client
            .send_request(
                "textDocument/diagnostic",
                json!({ "textDocument": { "uri": uri } }),
                timeout_secs.min(8),
            )
            .await
        {
            if let Some(arr) = pull_resp["result"]["items"].as_array() {
                items = arr.clone();
            }
        }
        if items.is_empty() {
            let key_uri = normalize_uri_key(uri);
            let mut cached = client.diagnostics.read().await.get(&key_uri).cloned().unwrap_or_default();
            if cached.is_empty() {
                tokio::time::sleep(Duration::from_millis(350)).await;
                cached = client.diagnostics.read().await.get(&key_uri).cloned().unwrap_or_default();
            }
            items = cached;
        }
        for d in items {
            let sev_code = d["severity"].as_u64().unwrap_or(1);
            if severity_matches(sev_code, &severity) {
                lines_out.push(format_lsp_diagnostic_line(&d));
            }
        }
        let header = if lines_out.is_empty() {
            format!(
                "LSP DIAGNOSTICS ({} · {} · severity={}): tidak ada error/warning terdeteksi.",
                client.command, rel, severity
            )
        } else {
            format!(
                "LSP DIAGNOSTICS ({} · {} · {} item):\n{}",
                client.command,
                rel,
                lines_out.len(),
                lines_out.join("\n")
            )
        };
        let (compressed, _) = compress_output(&header, "", 120);
        record_lsp_audit(
            state,
            source,
            ws,
            "lsp_diagnostics",
            Some(rel.clone()),
            format!("LSP diagnostics on {rel} ({} items)", lines_out.len()),
            compressed.len(),
        )
        .await;
        return Ok(compressed);
    }

    tokio::time::sleep(Duration::from_millis(250)).await;
    let diag_snapshot: HashMap<String, Vec<Value>> = client.diagnostics.read().await.clone();
    let ws_prefix = ws.clean_path.replace('\\', "/").trim_end_matches('/').to_string();
    for (uri_key, diags) in diag_snapshot {
        let rel_display = if cfg!(target_os = "windows") {
            let lower_p = ws_prefix.to_ascii_lowercase();
            uri_key
                .strip_prefix(&(lower_p + "/"))
                .unwrap_or(&uri_key)
                .to_string()
        } else {
            uri_key
                .strip_prefix(&(ws_prefix.clone() + "/"))
                .unwrap_or(&uri_key)
                .to_string()
        };
        for d in diags {
            let sev_code = d["severity"].as_u64().unwrap_or(1);
            if severity_matches(sev_code, &severity) {
                lines_out.push(format!("{rel_display}: {}", format_lsp_diagnostic_line(&d)));
            }
        }
    }

    let summary_text = if lines_out.is_empty() {
        format!(
            "LSP DIAGNOSTICS ({} · workspace @{} · status={}): belum ada diagnostik terpublikasi (pass parameter 'path' untuk file spesifik).",
            client.command,
            ws.alias,
            if client.indexing.load(Ordering::Relaxed) { "indexing" } else { "ready" }
        )
    } else {
        format!(
            "LSP DIAGNOSTICS ({} · workspace @{} · {} item):\n{}",
            client.command,
            ws.alias,
            lines_out.len(),
            lines_out.join("\n")
        )
    };
    let (compressed, _) = compress_output(&summary_text, "", 120);
    record_lsp_audit(
        state,
        source,
        ws,
        "lsp_diagnostics",
        None,
        format!("Workspace LSP diagnostics (@{})", ws.alias),
        compressed.len(),
    )
    .await;
    Ok(compressed)
}

fn parse_pos_args(args: &Value) -> Result<(&str, u64, u64), String> {
    let rel_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("Argumen 'path' wajib diisi.")?;
    let line_1 = args
        .get("line")
        .and_then(|v| v.as_u64())
        .filter(|&n| n >= 1)
        .ok_or("Argumen 'line' (1-based >= 1) wajib diisi.")?;
    let char_1 = args
        .get("character")
        .and_then(|v| v.as_u64())
        .filter(|&n| n >= 1)
        .ok_or("Argumen 'character' (1-based >= 1) wajib diisi.")?;
    Ok((rel_path, line_1, char_1))
}

fn extract_locations(val: &Value) -> Vec<(String, usize, usize)> {
    let mut raw_list = Vec::new();
    match val {
        Value::Array(arr) => raw_list.extend(arr.iter()),
        Value::Object(_) => raw_list.push(val),
        _ => {}
    }

    let mut out = Vec::new();
    for item in raw_list {
        if let Some(uri) = item.get("targetUri").and_then(|v| v.as_str()) {
            let line = item["targetSelectionRange"]["start"]["line"]
                .as_u64()
                .or_else(|| item["targetRange"]["start"]["line"].as_u64())
                .unwrap_or(0) as usize
                + 1;
            let col = item["targetSelectionRange"]["start"]["character"]
                .as_u64()
                .or_else(|| item["targetRange"]["start"]["character"].as_u64())
                .unwrap_or(0) as usize
                + 1;
            out.push((uri.to_string(), line, col));
        } else if let Some(uri) = item.get("uri").and_then(|v| v.as_str()) {
            let line = item["range"]["start"]["line"].as_u64().unwrap_or(0) as usize + 1;
            let col = item["range"]["start"]["character"].as_u64().unwrap_or(0) as usize + 1;
            out.push((uri.to_string(), line, col));
        }
    }
    out
}

fn preview_lines_around(path: &Path, center_line: usize, radius: usize) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = raw.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let idx = center_line.saturating_sub(1).min(lines.len().saturating_sub(1));
    let start = idx.saturating_sub(radius);
    let end = (idx + radius).min(lines.len().saturating_sub(1));
    let mut formatted = Vec::new();
    for i in start..=end {
        formatted.push(format!("{:>4} | {}", i + 1, lines[i]));
    }
    Some(formatted.join("\n"))
}

pub async fn exec_lsp_definition(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let (rel_path, line_1, char_1) = parse_pos_args(args)?;
    let target = resolve_jailed_path(&ws.canon_root, rel_path)?;
    check_sensitive_guard_for_workspace(state, ws, &target, rel_path).await?;
    let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
    let (lang_id, default_cmd, default_args) = detect_lsp_for_ext(ext).ok_or_else(|| {
        format!("Ekstensi file '{rel_path}' belum didukung LspBridge.")
    })?;

    let client = state
        .lsp_manager
        .get_or_spawn(state, ws, lang_id, default_cmd, &default_args)
        .await?;
    let (_, uri) = client.sync_document(state, ws, rel_path).await?;
    let resp = client
        .send_request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line_1 - 1, "character": char_1 - 1 }
            }),
            lsp_timeout_for_source(source),
        )
        .await?;

    if let Some(err) = resp.get("error") {
        return Err(format!("LSP definition error: {err}"));
    }

    let locs = extract_locations(&resp["result"]);
    if locs.is_empty() {
        return Ok(format!(
            "LSP DEFINITION: tidak ada definisi ditemukan pada {rel_path}:L{line_1}:{char_1}."
        ));
    }

    let mut sections = Vec::new();
    for (loc_uri, loc_line, loc_col) in locs.into_iter().take(6) {
        match lsp_uri_to_jailed_path(&ws.canon_root, &loc_uri)? {
            Some((jailed, rel_loc)) => {
                if check_sensitive_guard_for_workspace(state, ws, &jailed, &rel_loc)
                    .await
                    .is_err()
                {
                    sections.push(format!("[sensitive-blocked] {rel_loc}:L{loc_line}:{loc_col}"));
                    continue;
                }
                let snippet = preview_lines_around(&jailed, loc_line, 2)
                    .unwrap_or_else(|| "(tidak bisa membaca cuplikan)".to_string());
                sections.push(format!("@{}/{rel_loc}:L{loc_line}:{loc_col}\n{snippet}", ws.alias));
            }
            None => {
                let short_name = loc_uri.rsplit('/').next().unwrap_or(&loc_uri);
                sections.push(format!("[external] {short_name}:L{loc_line}:{loc_col}"));
            }
        }
    }

    let out = format!(
        "LSP DEFINITION ({} · {rel_path}:L{line_1}:{char_1}):\n{}",
        client.command,
        sections.join("\n---\n")
    );
    record_lsp_audit(
        state,
        source,
        ws,
        "lsp_definition",
        Some(rel_path.to_string()),
        format!("LSP definition at {rel_path}:L{line_1}:{char_1}"),
        out.len(),
    )
    .await;
    Ok(out)
}

pub async fn exec_lsp_references(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let (rel_path, line_1, char_1) = parse_pos_args(args)?;
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .clamp(1, 100) as usize;
    let target = resolve_jailed_path(&ws.canon_root, rel_path)?;
    check_sensitive_guard_for_workspace(state, ws, &target, rel_path).await?;
    let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
    let (lang_id, default_cmd, default_args) = detect_lsp_for_ext(ext).ok_or_else(|| {
        format!("Ekstensi file '{rel_path}' belum didukung LspBridge.")
    })?;

    let client = state
        .lsp_manager
        .get_or_spawn(state, ws, lang_id, default_cmd, &default_args)
        .await?;
    let (_, uri) = client.sync_document(state, ws, rel_path).await?;
    let resp = client
        .send_request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line_1 - 1, "character": char_1 - 1 },
                "context": { "includeDeclaration": true }
            }),
            lsp_timeout_for_source(source),
        )
        .await?;

    if let Some(err) = resp.get("error") {
        return Err(format!("LSP references error: {err}"));
    }

    let locs = extract_locations(&resp["result"]);
    if locs.is_empty() {
        return Ok(format!(
            "LSP REFERENCES: tidak ada referensi ditemukan untuk {rel_path}:L{line_1}:{char_1}."
        ));
    }

    let total = locs.len();
    let mut rows = Vec::new();
    for (loc_uri, loc_line, loc_col) in locs.into_iter().take(max_results) {
        match lsp_uri_to_jailed_path(&ws.canon_root, &loc_uri)? {
            Some((jailed, rel_loc)) => {
                if check_sensitive_guard_for_workspace(state, ws, &jailed, &rel_loc)
                    .await
                    .is_err()
                {
                    rows.push(format!("[sensitive-blocked] {rel_loc}:L{loc_line}:{loc_col}"));
                    continue;
                }
                let line_txt = fs::read_to_string(&jailed)
                    .ok()
                    .and_then(|c| c.lines().nth(loc_line.saturating_sub(1)).map(|l| l.trim().to_string()))
                    .unwrap_or_default();
                let clean_line: String = line_txt.chars().take(120).collect();
                rows.push(format!("{rel_loc}:L{loc_line}:{loc_col} | {clean_line}"));
            }
            None => {
                let short_name = loc_uri.rsplit('/').next().unwrap_or(&loc_uri);
                rows.push(format!("[external] {short_name}:L{loc_line}:{loc_col}"));
            }
        }
    }

    let suffix = if total > max_results {
        format!("\n... (+{} referensi lain, naikkan max_results hingga 100)", total - max_results)
    } else {
        String::new()
    };
    let out = format!(
        "LSP REFERENCES ({} · {rel_path}:L{line_1}:{char_1} · {} ditampilkan dari {total}):\n{}{}",
        client.command,
        rows.len(),
        rows.join("\n"),
        suffix
    );
    record_lsp_audit(
        state,
        source,
        ws,
        "lsp_references",
        Some(rel_path.to_string()),
        format!("LSP references ({total} hits) for {rel_path}:L{line_1}"),
        out.len(),
    )
    .await;
    Ok(out)
}

fn extract_hover_text(contents: &Value) -> String {
    if let Some(s) = contents.as_str() {
        return s.to_string();
    }
    if let Some(val) = contents.get("value").and_then(|v| v.as_str()) {
        return val.to_string();
    }
    if let Some(arr) = contents.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .or_else(|| item.get("value").and_then(|v| v.as_str()).map(str::to_string))
            })
            .collect();
        return parts.join("\n---\n");
    }
    String::new()
}

pub async fn exec_lsp_hover(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let (rel_path, line_1, char_1) = parse_pos_args(args)?;
    let target = resolve_jailed_path(&ws.canon_root, rel_path)?;
    check_sensitive_guard_for_workspace(state, ws, &target, rel_path).await?;
    let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
    let (lang_id, default_cmd, default_args) = detect_lsp_for_ext(ext).ok_or_else(|| {
        format!("Ekstensi file '{rel_path}' belum didukung LspBridge.")
    })?;

    let client = state
        .lsp_manager
        .get_or_spawn(state, ws, lang_id, default_cmd, &default_args)
        .await?;
    let (_, uri) = client.sync_document(state, ws, rel_path).await?;
    let resp = client
        .send_request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line_1 - 1, "character": char_1 - 1 }
            }),
            lsp_timeout_for_source(source),
        )
        .await?;

    if let Some(err) = resp.get("error") {
        return Err(format!("LSP hover error: {err}"));
    }

    let hover_txt = extract_hover_text(&resp["result"]["contents"]).trim().to_string();
    if hover_txt.is_empty() {
        return Ok(format!(
            "LSP HOVER: tidak ada info tipe/dokumentasi pada {rel_path}:L{line_1}:{char_1}."
        ));
    }
    let out = format!(
        "LSP HOVER ({} · {rel_path}:L{line_1}:{char_1}):\n{hover_txt}",
        client.command
    );
    let (compressed, _) = compress_output(&out, "", 80);
    record_lsp_audit(
        state,
        source,
        ws,
        "lsp_hover",
        Some(rel_path.to_string()),
        format!("LSP hover at {rel_path}:L{line_1}:{char_1}"),
        compressed.len(),
    )
    .await;
    Ok(compressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_lsp_content_length_framing_with_utf8_multibyte() {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "textDocument/hover",
            "params": {
                "note": "Halo 🚀 Airlock — 日本語テスト ✓",
                "count": 42
            }
        });
        let raw_body = serde_json::to_string(&payload).unwrap();
        assert!(
            raw_body.len() > raw_body.chars().count(),
            "test string must contain multibyte UTF-8 characters"
        );

        let mut raw_buf = Vec::new();
        write_lsp_message(&mut raw_buf, &payload)
            .await
            .expect("write_lsp_message must succeed");
        let expected_header = format!("Content-Length: {}\r\n\r\n", raw_body.len());
        assert!(
            raw_buf.starts_with(expected_header.as_bytes()),
            "header Content-Length must count exact UTF-8 bytes"
        );

        let mut cursor_reader = BufReader::new(std::io::Cursor::new(raw_buf));
        let decoded = read_lsp_message(&mut cursor_reader)
            .await
            .expect("read_lsp_message must succeed")
            .expect("should return payload");
        assert_eq!(decoded, payload);
        assert!(
            read_lsp_message(&mut cursor_reader)
                .await
                .expect("EOF should return Ok(None)")
                .is_none()
        );

        let (mut writer_end, reader_end) = tokio::io::duplex(4096);
        let reply = json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": "αβγ🚀" } });
        write_lsp_message(&mut writer_end, &reply).await.unwrap();
        writer_end.shutdown().await.unwrap();
        drop(writer_end);

        let mut duplex_reader = BufReader::new(reader_end);
        let decoded_reply = read_lsp_message(&mut duplex_reader)
            .await
            .expect("duplex read_lsp_message must succeed")
            .expect("should return reply payload");
        assert_eq!(decoded_reply, reply);
        assert!(
            read_lsp_message(&mut duplex_reader)
                .await
                .expect("duplex EOF should return Ok(None)")
                .is_none()
        );
    }

    #[test]
    fn test_windows_file_uri_roundtrip_and_jail_boundary() {
        let base = std::env::temp_dir().join("smart_mcp_lsp_uri_jail_test");
        let _ = fs::remove_dir_all(&base);
        let ws_dir = base.join("workspace");
        let outside_dir = base.join("outside_secret");
        fs::create_dir_all(ws_dir.join("src")).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();

        let inside_file = ws_dir.join("src").join("main.rs");
        let outside_file = outside_dir.join("secret.rs");
        fs::write(&inside_file, "fn main() {}\n").unwrap();
        fs::write(&outside_file, "const SECRET: &str = \"x\";\n").unwrap();

        let (canon_root, _clean_root) = normalize_clean_path(&ws_dir).unwrap();
        let canon_inside = std::fs::canonicalize(&inside_file).unwrap();

        let inside_uri = path_to_lsp_uri(&canon_inside).expect("canonical path should convert to URI");
        assert!(
            !inside_uri.contains("?"),
            "LSP URI must strip \\\\?\\ prefix: {inside_uri}"
        );
        assert!(inside_uri.starts_with("file:///"));

        let resolved = lsp_uri_to_jailed_path(&canon_root, &inside_uri)
            .expect("in-workspace URI must validate cleanly")
            .expect("in-workspace URI must resolve to jailed path");
        assert_eq!(resolved.1, "src/main.rs");

        let outside_uri = path_to_lsp_uri(&outside_file).unwrap();
        let blocked = lsp_uri_to_jailed_path(&canon_root, &outside_uri)
            .expect("outside URI should cleanly return Ok(None) instead of reading external path");
        assert!(blocked.is_none());

        let _ = fs::remove_dir_all(&base);
    }
}
