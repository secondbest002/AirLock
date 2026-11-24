use crate::state::AppState;
use regex::Regex;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

fn find_binary(name: &str, extra_candidates: &[PathBuf]) -> Option<PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = if cfg!(target_os = "windows") {
                dir.join(format!("{name}.exe"))
            } else {
                dir.join(name)
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for path in extra_candidates {
        if path.is_file() {
            return Some(path.clone());
        }
    }
    None
}

fn find_cloudflared_binary() -> Option<PathBuf> {
    let mut extras = Vec::new();
    if let Some(local_app) = std::env::var_os("LOCALAPPDATA") {
        extras.push(
            PathBuf::from(local_app)
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("cloudflared.exe"),
        );
    }
    extras.push(PathBuf::from(
        r"C:\Program Files (x86)\cloudflared\cloudflared.exe",
    ));
    extras.push(PathBuf::from(
        r"C:\Program Files\cloudflared\cloudflared.exe",
    ));
    find_binary("cloudflared", &extras)
}

fn find_tailscale_binary() -> Option<PathBuf> {
    let extras = vec![PathBuf::from(r"C:\Program Files\Tailscale\tailscale.exe")];
    find_binary("tailscale", &extras)
}

fn spawn_cloudflared(args: &[&str], tunnel_token: Option<&str>) -> Result<Child, String> {
    let mut cmd = if let Some(bin) = find_cloudflared_binary() {
        let mut c = Command::new(bin);
        c.args(args);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd.exe");
        let mut full_args = vec!["/C", "npx", "-y", "cloudflared"];
        full_args.extend_from_slice(args);
        c.args(&full_args);
        c
    } else {
        let mut c = Command::new("npx");
        let mut full_args = vec!["-y", "cloudflared"];
        full_args.extend_from_slice(args);
        c.args(&full_args);
        c
    };

    if let Some(tok) = tunnel_token {
        cmd.env("TUNNEL_TOKEN", tok);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    cmd.spawn()
        .map_err(|e| format!("Gagal menjalankan cloudflared / npx: {e}"))
}

fn normalize_public_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        Some(trimmed.to_string())
    } else {
        Some(format!("https://{trimmed}"))
    }
}

pub async fn start_tunnel(state: Arc<AppState>, provider: String) -> Result<(), String> {
    if state.tunnel_running.load(Ordering::SeqCst) {
        return Ok(());
    }

    let trimmed = provider.trim();
    let normalized_provider = if trimmed.eq_ignore_ascii_case("tailscale") {
        "tailscale".to_string()
    } else if trimmed.eq_ignore_ascii_case("cloudflare_named") {
        "cloudflare_named".to_string()
    } else {
        "cloudflare".to_string()
    };
    *state.tunnel_provider.write().await = normalized_provider.clone();
    state
        .append_tunnel_log(format!("Tunnel dimulai (provider: {normalized_provider})..."))
        .await;

    match normalized_provider.as_str() {
        "tailscale" => start_tailscale_funnel(state).await,
        "cloudflare_named" => start_cloudflare_named(state).await,
        _ => start_cloudflare_http2(state).await,
    }
}

async fn start_tailscale_funnel(state: Arc<AppState>) -> Result<(), String> {
    let ts_bin = find_tailscale_binary().ok_or_else(|| {
        "Tailscale tidak ditemukan. Instal lewat `winget install Tailscale.Tailscale`, login akun, dan aktifkan Funnel di tailnet Anda."
            .to_string()
    })?;

    let port = state.server_port.load(Ordering::SeqCst);
    let local_url = format!("http://127.0.0.1:{port}");
    state
        .append_tunnel_log(format!("Menjalankan `tailscale funnel --bg {local_url}`"))
        .await;

    let mut funnel_cmd = Command::new(&ts_bin);
    funnel_cmd
        .args(["funnel", "--bg", &local_url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    funnel_cmd.creation_flags(0x08000000);

    let out = tokio::time::timeout(Duration::from_secs(15), funnel_cmd.output())
        .await
        .map_err(|_| "Timeout menjalankan `tailscale funnel --bg`.".to_string())?
        .map_err(|e| format!("Gagal menjalankan tailscale funnel: {e}"))?;

    let stdout_str = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr_str = String::from_utf8_lossy(&out.stderr).to_string();
    for line in stdout_str.lines().chain(stderr_str.lines()) {
        state.append_tunnel_log(line.to_string()).await;
    }

    if !out.status.success() {
        return Err(format!(
            "Tailscale Funnel gagal ({}): {} {}",
            out.status,
            stdout_str.trim(),
            stderr_str.trim()
        ));
    }

    let mut status_cmd = Command::new(&ts_bin);
    status_cmd
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    status_cmd.creation_flags(0x08000000);

    let st_out = tokio::time::timeout(Duration::from_secs(10), status_cmd.output())
        .await
        .map_err(|_| "Timeout membaca `tailscale status --json`.".to_string())?
        .map_err(|e| format!("Gagal membaca status Tailscale: {e}"))?;

    let status_json: Value = serde_json::from_slice(&st_out.stdout)
        .map_err(|e| format!("Gagal mem-parsing output JSON Tailscale: {e}"))?;
    let dns_raw = status_json["Self"]["DNSName"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches('.');
    if dns_raw.is_empty() {
        return Err(
            "Tailscale aktif tetapi DNSName belum tersedia. Pastikan MagicDNS & HTTPS aktif di Tailscale Admin."
                .to_string(),
        );
    }

    let public_url = format!("https://{dns_raw}");
    state
        .append_tunnel_log(format!("Tailscale Funnel aktif di {public_url}"))
        .await;
    state.tunnel_running.store(true, Ordering::SeqCst);
    *state.tunnel_url.write().await = Some(public_url);
    *state.tunnel_status.write().await = "Connected (Tailscale Funnel — Permanen)".to_string();
    state.touch_activity().await;
    state.emit_dashboard().await;
    Ok(())
}

async fn start_cloudflare_named(state: Arc<AppState>) -> Result<(), String> {
    let (token, raw_public_url) = {
        let cfg = state.config.read().await;
        (
            cfg.cloudflare_tunnel_token.trim().to_string(),
            cfg.cloudflare_public_url.clone(),
        )
    };

    if token.is_empty() {
        return Err(
            "Cloudflare Tunnel Token belum dikonfigurasi. Isi token Named Tunnel terlebih dahulu."
                .to_string(),
        );
    }

    let mut child = spawn_cloudflared(
        &["tunnel", "--no-autoupdate", "--protocol", "http2", "run"],
        Some(&token),
    )?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    {
        let mut guard = state.tunnel_child.lock().await;
        *guard = Some(child);
    }

    let normalized_url = normalize_public_url(&raw_public_url);
    let status_label = if normalized_url.is_some() {
        "Connected (Cloudflare Named Tunnel)".to_string()
    } else {
        "Running (Named Tunnel — isi Public URL untuk Copy 1-Click)".to_string()
    };

    state.tunnel_running.store(true, Ordering::SeqCst);
    *state.tunnel_url.write().await = normalized_url;
    *state.tunnel_status.write().await = status_label;
    state.touch_activity().await;
    state.emit_dashboard().await;

    if let Some(out) = stdout {
        let st = state.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                st.append_tunnel_log(line).await;
            }
        });
    }

    if let Some(err) = stderr {
        let st = state.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                st.append_tunnel_log(line).await;
            }
            if st.tunnel_running.swap(false, Ordering::SeqCst) {
                *st.tunnel_url.write().await = None;
                *st.tunnel_status.write().await = "Tunnel berhenti".to_string();
                st.append_tunnel_log("Cloudflare Named Tunnel berhenti.".to_string())
                    .await;
                st.emit_dashboard().await;
            }
        });
    }

    Ok(())
}

async fn start_cloudflare_http2(state: Arc<AppState>) -> Result<(), String> {
    let port = state.server_port.load(Ordering::SeqCst);
    let local_url = format!("http://127.0.0.1:{port}");

    let mut child = spawn_cloudflared(
        &[
            "tunnel",
            "--url",
            &local_url,
            "--protocol",
            "http2",
            "--no-autoupdate",
        ],
        None,
    )?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    {
        let mut guard = state.tunnel_child.lock().await;
        *guard = Some(child);
    }

    state.tunnel_running.store(true, Ordering::SeqCst);
    {
        let mut status = state.tunnel_status.write().await;
        *status = "Menunggu URL trycloudflare.com (HTTP/2)...".to_string();
    }
    state.touch_activity().await;
    state.emit_dashboard().await;

    let url_re =
        Regex::new(r"https://[a-zA-Z0-9-]+\.trycloudflare\.com").expect("valid tunnel regex");

    if let Some(out) = stdout {
        let st = state.clone();
        let re = url_re.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(m) = re.find(&line) {
                    let found = m.as_str().to_string();
                    *st.tunnel_url.write().await = Some(found);
                    *st.tunnel_status.write().await = "Connected (Cloudflare HTTP/2)".to_string();
                    st.emit_dashboard().await;
                }
                st.append_tunnel_log(line).await;
            }
        });
    }

    if let Some(err) = stderr {
        let st = state.clone();
        let re = url_re;
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(m) = re.find(&line) {
                    let found = m.as_str().to_string();
                    *st.tunnel_url.write().await = Some(found);
                    *st.tunnel_status.write().await = "Connected (Cloudflare HTTP/2)".to_string();
                    st.emit_dashboard().await;
                }
                st.append_tunnel_log(line).await;
            }
            if st.tunnel_running.swap(false, Ordering::SeqCst) {
                *st.tunnel_url.write().await = None;
                *st.tunnel_status.write().await = "Tunnel berhenti".to_string();
                st.append_tunnel_log("Cloudflare Quick Tunnel berhenti.".to_string())
                    .await;
                st.emit_dashboard().await;
            }
        });
    }

    Ok(())
}

pub async fn stop_tunnel(state: &Arc<AppState>) {
    let provider = state.tunnel_provider.read().await.clone();
    if provider == "tailscale" {
        if let Some(ts_bin) = find_tailscale_binary() {
            let mut off_cmd = Command::new(ts_bin);
            off_cmd
                .args(["funnel", "--https=443", "off"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(target_os = "windows")]
            off_cmd.creation_flags(0x08000000);
            if let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(5), off_cmd.output()).await
            {
                let out_str = String::from_utf8_lossy(&out.stdout);
                let err_str = String::from_utf8_lossy(&out.stderr);
                for line in out_str.lines().chain(err_str.lines()) {
                    state.append_tunnel_log(line.to_string()).await;
                }
            }
        }
    }

    let child_opt = {
        let mut guard = state.tunnel_child.lock().await;
        guard.take()
    };

    if let Some(mut child) = child_opt {
        let _ = child.kill().await;
    }

    state.tunnel_running.store(false, Ordering::SeqCst);
    *state.tunnel_url.write().await = None;
    *state.tunnel_status.write().await = "Belum aktif".to_string();
    state.append_tunnel_log("Tunnel dihentikan.".to_string()).await;
    state.emit_dashboard().await;
}

pub async fn sync_existing_tailscale_funnel(state: Arc<AppState>) {
    let Some(ts_bin) = find_tailscale_binary() else {
        return;
    };

    let mut cmd = Command::new(ts_bin);
    cmd.args(["funnel", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(5), cmd.output()).await else {
        return;
    };
    if !out.status.success() {
        return;
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let port_needle = format!(":{}", state.server_port.load(Ordering::SeqCst));
    if !stdout.contains(&port_needle) {
        return;
    }

    let ts_re = Regex::new(r"https://[a-zA-Z0-9.-]+\.ts\.net").expect("valid ts regex");
    if let Some(m) = ts_re.find(&stdout) {
        *state.tunnel_provider.write().await = "tailscale".to_string();
        state.tunnel_running.store(true, Ordering::SeqCst);
        *state.tunnel_url.write().await = Some(m.as_str().to_string());
        *state.tunnel_status.write().await = "Connected (Tailscale Funnel — Permanen)".to_string();
        state
            .append_tunnel_log(format!("Terdeteksi Tailscale Funnel aktif di {}", m.as_str()))
            .await;
        state.emit_dashboard().await;
    }
}
