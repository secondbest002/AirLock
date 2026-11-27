# Laporan Audit Codebase Menyeluruh — Airlock (`tauri-mcp-hub`)

| Atribut | Detail |
| :--- | :--- |
| **Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Tanggal Audit** | 24 November 2026 |
| **Stack Teknologi** | Tauri v2, Rust (Axum 0.7, Tokio 1, Reqwest 0.12), Vanilla TypeScript (strict), Vite 6 |
| **Cakupan** | 18 modul Rust (`src-tauri/src/`), 11 modul Frontend (`src/`), konfigurasi Tauri/Vite/Cargo |
| **Status Git** | Bukan repositori Git (`.git` tidak ditemukan) |

---

## 1. Ringkasan Eksekutif

Airlock (`tauri-mcp-hub`) adalah aplikasi desktop yang dirancang dengan arsitektur padat dan ambisius: menggabungkan **Streamable HTTP/Stdio MCP Server**, **Remote Tunnel Bridge** (Cloudflare/Tailscale), **Downstream MCP Aggregator**, dan **Multi-Provider Local Coding Agent** (Gemini, OpenAI-Compatible, Anthropic Claude) dalam satu biner Tauri v2 tanpa dependensi framework UI eksternal.

Secara fundamental, codebase memiliki fondasi rekayasa yang kuat — khususnya pada higiene XSS di frontend, proteksi directory jail berbasis kanonikalisasi path, mekanisme rollback snapshot SHA-256, pembersihan process tree di Windows, serta cakupan test integrasi/E2E backend yang komprehensif.

Namun, audit menyeluruh mengidentifikasi **11 temuan** (**4 High**, **5 Medium**, **2 Low**) yang terutama berkaitan dengan celah urutan pengecekan pada *Sensitive File Guard*, penyimpanan kredensial plaintext di disk, kepatuhan ketat OAuth 2.1, kompatibilitas sesi Streamable HTTP pada MCP Aggregator, serta efisiensi runtime (HTTP pooling, kompilasi regex, dan DPAPI FFI).

### Ringkasan Matriks Temuan

| ID | Severity | Kategori | Ringkasan Temuan | Lokasi Utama |
| :--- | :--- | :--- | :--- | :--- |
| **F-01** | **High** | Security / Bug | Bypass proteksi hardcoded `auth.json` saat `allow_sensitive_reads` aktif | `src-tauri/src/tools.rs:613-637` |
| **F-02** | **High** | Security / Secrets | Token tunnel, downstream MCP, dan OAuth disimpan plaintext di JSON | `config.rs:58-95`, `state.rs:462-509` |
| **F-03** | **High** | Security / OAuth | Validasi OAuth 2.1 longgar (`plain` PKCE, PKCE opsional, bypass param kosong) | `src-tauri/src/oauth.rs:67, 577-608` |
| **F-04** | **High** | Security / Desktop | Content Security Policy (CSP) WebView dinonaktifkan (`null`) | `src-tauri/tauri.conf.json:23-25` |
| **F-05** | **Medium** | Security / Crypto | Perbandingan token & `client_secret` tidak constant-time (`==`) | `mcp_server.rs:73-118`, `oauth.rs:512, 623` |
| **F-06** | **Medium** | Security / Sandbox | Command Guard berbasis substring blacklist tanpa isolasi OS-level | `security.rs:158-224`, `tools.rs:1395-1417` |
| **F-07** | **Medium** | Protocol / Bug | Downstream HTTP MCP membuang `mcp-session-id` setelah handshake | `src-tauri/src/mcp_hub.rs:339-388, 602-615` |
| **F-08** | **Medium** | Performance / Reliability | Windows DPAPI men-spawn proses `powershell.exe` per baca/tulis | `src-tauri/src/secrets.rs:43-115` |
| **F-09** | **Medium** | Performance | Tidak ada reuse `reqwest::Client` & rekompilasi `Regex` berulang | `providers.rs`, `local_agent.rs`, `mcp_hub.rs`, `security.rs` |
| **F-10** | **Low** | Maintainability | Modul inti monolitik & duplikasi helper `kill_pid_tree` di 3 file | `tools.rs`, `mcp_server.rs`, `providers.rs`, `jobs.rs`, `mcp_hub.rs` |
| **F-11** | **Low** | Code Hygiene / UX | `git diff` via shell di resources, legacy API, parser Markdown terbatas, tanpa Git | `mcp_resources.rs:83`, `main.rs:73`, `src/markdown.ts` |

---

## 2. Kekuatan Arsitektur Saat Ini

Sebelum masuk ke daftar perbaikan, berikut praktik baik yang sudah berjalan solid dan perlu dipertahankan:

1. **Higiene XSS Frontend yang Konsisten:** Dari 18 kemunculan `innerHTML` di seluruh direktori `src/`, semuanya hanya berupa pembersihan elemen (`""`) atau string SVG statis (`src/views/bridge.ts:78`). Renderer `src/markdown.ts` merakit seluruh node menggunakan `document.createElement` dan `textContent`.
2. **Directory Jail Berlapis (`security.rs:81-155`):** `resolve_jailed_path` menggabungkan normalisasi leksikal komponen path dengan `std::fs::canonicalize` (termasuk pencarian ancestor terdekat untuk file yang belum dibuat) sehingga mencegah *path traversal* maupun *symlink/junction escape*.
3. **Rollback Snapshot SHA-256 & Conflict Detection (`snapshots.rs`, `tools.rs:1608-1740`):** Modifikasi file menyimpan hash SHA-256 konten asli dan pasca-edit, mendeteksi perubahan eksternal sebelum revert, serta mendukung *Revert Turn Group* secara reverse-order.
4. **Manajemen Process Tree di Windows (`tools.rs:1366-1393`):** `ChildTreeGuard` mengeksekusi `taskkill /PID <pid> /T /F` di dalam `Drop`, mencegah proses anak menjadi *orphan* saat timeout atau pembatalan.
5. **Cakupan Uji Otomatis Backend:** Sebagian besar modul inti telah dilengkapi unit test dan E2E test (`mcp_server.rs`, `tools.rs`, `security.rs`, `local_agent.rs`, `providers.rs`, `jobs.rs`, `secrets.rs`, `snapshots.rs`, `chat_store.rs`, `stdio_bridge.rs`, `config.rs`).

---

## 3. Rincian Temuan & Opsi Solusi

---

### [F-01] High — Bug Bypass Proteksi Hardcoded `auth.json` pada `check_sensitive_guard`

* **Kategori:** Security / Logic Bug
* **Lokasi:** `src-tauri/src/tools.rs:613-637`, `src-tauri/src/security.rs:35-41`

#### Analisis Teknis
Fungsi `check_sensitive_guard` bertujuan memblokir akses ke file sensitif (`.env`, `*.key`, dll.) dan secara khusus memblokir `.smart-tauri-hub/auth.json` secara hardcoded. Namun, pengecekan *early return* untuk flag `allow_sensitive_reads` diletakkan di bagian paling atas:

```rust
pub(crate) async fn check_sensitive_guard(
    state: &AppState,
    root: &Path,
    target: &Path,
    rel_label: &str,
) -> Result<(), String> {
    if state.allow_sensitive_reads.load(Ordering::SeqCst) {
        return Ok(()); // <-- Langsung lolos, melewati blokir hardcoded di baris 630!
    }
    // ... pengecekan pola sensitive_deny_patterns ...
    let full_slash = target.to_string_lossy().replace('\\', "/").to_lowercase();
    if full_slash.ends_with(".smart-tauri-hub/auth.json") {
        return Err(...);
    }
    Ok(())
}
```

Selain urutan yang terbalik, proteksi hardcoded hanya mencocokkan akhiran `.smart-tauri-hub/auth.json` dan belum melindungi file sensitif internal lainnya di direktori yang sama seperti `config.json` (berisi token tunnel & downstream MCP) maupun `secrets.dpapi`.

#### Dampak
Jika pengguna mengaktifkan opsi **Allow Sensitive Files** di dashboard (misalnya untuk membaca `.env` proyek) dan workspace diarahkan ke direktori home pengguna (`%USERPROFILE%`), klien MCP atau Local Agent dapat membaca langsung `auth.json`, `config.json`, dan seluruh data internal Airlock tanpa terhalang.

#### Opsi Solusi

* **Opsi A — Pindahkan Hardcoded Block ke Atas & Blokir Seluruh Direktori `.smart-tauri-hub/` (Disarankan):**
  Pindahkan validasi direktori internal Airlock *sebelum* pengecekan `allow_sensitive_reads` dan perluas cakupannya agar menolak path apa pun yang berada di dalam `.smart-tauri-hub/`.
  ```rust
  let full_slash = target.to_string_lossy().replace('\\', "/").to_lowercase();
  if full_slash.contains("/.smart-tauri-hub/")
      || full_slash.ends_with("/.smart-tauri-hub")
      || full_slash.starts_with(".smart-tauri-hub/")
  {
      return Err(format!(
          "Akses ditolak permanen: '{rel_label}' mengarah ke direktori internal .smart-tauri-hub."
      ));
  }
  if state.allow_sensitive_reads.load(Ordering::SeqCst) {
      return Ok(());
  }
  ```
  * *Kelebihan:* Perubahan minimal (~10 baris), menutup celah secara permanen dan menyeluruh.
* **Opsi B — Bandingkan Canonical Path Terhadap `AppConfig::default_path().parent()`:**
  Selain pengecekan string, kanonikalisasi direktori data Airlock aktual (`~/.smart-tauri-hub`) dan tolak bila `target.starts_with(&canon_airlock_dir)`.
  * *Kelebihan:* Tahan terhadap perbedaan casing, junction Windows, atau custom data directory di masa depan.

#### Kriteria Verifikasi
Tambahkan unit test di `tools.rs` yang menyalakan `state.allow_sensitive_reads.store(true, ...)` lalu memastikan pembacaan `.smart-tauri-hub/auth.json` dan `.smart-tauri-hub/config.json` tetap mengembalikan `Err`.

---

### [F-02] High — Kredensial & Token Disimpan Plaintext di `config.json` dan `auth.json`

* **Kategori:** Security / Secret Management
* **Lokasi:** `src-tauri/src/config.rs:58-95`, `src-tauri/src/state.rs:118-126, 462-509`

#### Analisis Teknis
Saat ini enkripsi Windows DPAPI (`secrets.dpapi` via `SecretsStore`) hanya diterapkan pada `ProviderSecrets` (`gemini_api_key`, `openai_api_key`, `anthropic_api_key`). Sementara itu, beberapa rahasia kritikal lain masih ditulis sebagai JSON plaintext ke disk:
1. `config.json`:
   * `cloudflare_tunnel_token` (`config.rs:94`)
   * `downstream_mcps[].bearer_token` (`config.rs:66`)
   * `downstream_mcps[].env` (`config.rs:72` — sering berisi API key untuk downstream stdio MCP server seperti GitHub/Postgres/Slack)
2. `auth.json`:
   * `auth_token` (master bearer token MCP server)
   * `oauth_clients[].client_secret`
   * `oauth_access_tokens` dan `oauth_refresh_tokens` (token aktif disimpan utuh, bukan hash)

Di sisi lain, pada OS non-Windows (`secrets.rs:130-131`), `SecretsStore::save` masih melakukan fallback menyimpan JSON plaintext tanpa enkripsi maupun pembatasan file permission (`0600`).

#### Dampak
Proses lain yang berjalan dengan hak user yang sama, backup sinkronisasi cloud, atau insiden eksposur file lokal dapat langsung mengambil alih akses penuh ke MCP server, tunnel Cloudflare, serta seluruh downstream MCP server yang terhubung.

#### Opsi Solusi

* **Opsi A — Perluas `SecretsStore` (DPAPI) untuk Semua Secret & Hash OAuth Token di `auth.json` (Disarankan):**
  1. Tambahkan map `cloudflare_tunnel_token`, `downstream_bearer_tokens: HashMap<String, String>`, dan `downstream_env_secrets` ke dalam struct `ProviderSecrets` (atau `VaultSecrets` terenkripsi DPAPI), sementara `config.json` hanya menyimpan metadata non-sensitif.
  2. Untuk `oauth_access_tokens` dan `oauth_refresh_tokens`, simpan **SHA-256 hash** dari token di `auth.json` (bukan token mentah), lalu hash bearer token yang masuk sebelum lookup di `check_auth` / `handle_oauth_token`.
  3. Untuk `auth_token` dan `client_secret`, pindahkan ke vault terenkripsi (dan pastikan `stdio_bridge.rs` membaca dari `SecretsStore` yang sama).
  * *Kelebihan:* Menggunakan infrastruktur `SecretsStore` yang sudah ada, kompatibel dengan migrasi otomatis dari file lama.
* **Opsi B — Gunakan OS Keyring Native (`keyring` crate) + Permission Hardening:**
  Migrasi penyimpanan secret ke Windows Credential Manager / macOS Keychain / Linux Secret Service melalui crate `keyring`, serta terapkan file mode `0600` (Unix) / restricted ACL (Windows) pada direktori `~/.smart-tauri-hub/`.
  * *Kelebihan:* Aman lintas platform (tidak hanya Windows) dan tanpa ketergantungan file terenkripsi kustom.
  * *Trade-off:* Menambah dependensi eksternal dan perlu penanganan fallback headless/CI.

#### Kriteria Verifikasi
Inspeksi `~/.smart-tauri-hub/config.json` dan `auth.json` setelah menyimpan tunnel token, downstream MCP, dan sesi OAuth; pastikan tidak ada secret atau access/refresh token mentah yang muncul dalam plaintext.

---

### [F-03] High — Celah Validasi & Kepatuhan OAuth 2.1

* **Kategori:** Security / Authentication & Authorization
* **Lokasi:** `src-tauri/src/oauth.rs:67, 248-259, 577-608`

#### Analisis Teknis
Terdapat empat kelemahan spesifik pada implementasi OAuth 2.1 di `src-tauri/src/oauth.rs`:

1. **Bypass Validasi `client_id` dan `redirect_uri` pada `/oauth/token` (baris 577 & 585):**
   ```rust
   if !client_id.is_empty() && !rec.client_id.is_empty() && rec.client_id != client_id { ... }
   if !redirect_uri.is_empty() && !rec.redirect_uri.is_empty() && redirect_uri != rec.redirect_uri { ... }
   ```
   Karena ada kondisi `!client_id.is_empty()` dan `!redirect_uri.is_empty()`, pemanggil cukup **mengosongkan** parameter `client_id` atau `redirect_uri` di request `POST /oauth/token` untuk melewati pengecekan kecocokan dengan authorization code.
2. **PKCE Bersifat Opsional & Masih Menerima Metode `plain` (baris 67, 249-253, 593-608):**
   Metadata mengiklankan `"code_challenge_methods_supported": ["S256", "plain"]`, dan jika klien tidak mengirimkan `code_challenge` di `/oauth/authorize`, `rec.challenge` bernilai `None` sehingga validasi PKCE dilewati sepenuhnya. Spesifikasi OAuth 2.1 dan MCP Authorization Spec mewajibkan PKCE `S256`.
3. **Auto-Creation Client Tanpa Registrasi (`oauth.rs:632-650`):**
   Jika `effective_client_id` tidak ditemukan di `oauth_clients` saat token exchange, server otomatis membuat entri `OAuthClient` baru dengan `auth_method: "none"`.
4. **Validasi `redirect_uri` Longgar untuk `unregistered_client`:**
   Klien yang belum terdaftar tetap dapat memulai flow dengan sembarang `redirect_uri` HTTPS selama user menyetujui popup, tanpa verifikasi apakah `client_id` memang terdaftar jika `client_id` diberikan.

#### Dampak
Authorization code yang bocor (misalnya lewat log browser/proxy) dapat ditukarkan oleh pihak tidak berwenang tanpa harus mengetahui `redirect_uri` atau `client_id` yang sah, dan tanpa perlindungan PKCE `S256` jika klien memulai flow tanpa `code_challenge`.

#### Opsi Solusi

* **Opsi A — Pengetatan Validasi Langsung di `oauth.rs` (Disarankan):**
  1. Di `handle_oauth_token`: jika `!rec.client_id.is_empty()`, wajibkan `client_id == rec.client_id` (tidak boleh kosong). Jika `!rec.redirect_uri.is_empty()`, wajibkan `redirect_uri == rec.redirect_uri` (tidak boleh kosong).
  2. Di `handle_oauth_authorize`: tolak request jika `code_challenge` kosong atau `code_challenge_method != "S256"` (atau buat opsi kompatibilitas legacy yang default-nya `false`). Hapus `"plain"` dari `code_challenge_methods_supported`.
  3. Di `handle_oauth_authorize`: jika `client_id` terdaftar, pastikan `redirect_uri` cocok persis dengan salah satu `client.redirect_uris` *sebelum* menampilkan popup consent.
  4. Nonaktifkan auto-insert client pada `handle_oauth_token` jika `client_id` tidak terdaftar (kecuali untuk mode kompatibilitas eksplisit).
  * *Kelebihan:* Memenuhi standar OAuth 2.1 dan MCP spec terbaru dengan risiko regresi rendah pada klien modern (Claude, ChatGPT, Gemini, MCP SDK).
* **Opsi B — Mode Strict vs Compatibility Toggle:**
  Terapkan pengetatan `client_id`/`redirect_uri` secara mutlak, namun sediakan konfigurasi `oauth_require_pkce_s256` (default `true`) bagi klien lama yang belum mendukung PKCE.

#### Kriteria Verifikasi
Tambahkan E2E test negatif di `mcp_server.rs` yang menguji: (1) tukar code dengan `redirect_uri` kosong harus `401`, (2) tukar code dengan `client_id` kosong padahal code terikat ke client harus `401`, (3) `authorize` tanpa `code_challenge` atau dengan metode `plain` harus `400`.

---

### [F-04] High — Content Security Policy (CSP) Dinonaktifkan di Tauri WebView

* **Kategori:** Security / Desktop Hardening
* **Lokasi:** `src-tauri/tauri.conf.json:23-25`

#### Analisis Teknis
Konfigurasi keamanan Tauri saat ini menonaktifkan CSP sepenuhnya:
```json
"security": {
  "csp": null
}
```
Karena aplikasi desktop Tauri memiliki IPC bridge (`window.__TAURI_INTERNALS__`) yang langsung mengarah ke eksekusi shell, baca/tulis file, dan konfigurasi tunnel, CSP merupakan lapisan *defense-in-depth* krusial jika di kemudian hari muncul bug XSS (misalnya pada renderer Markdown atau fitur baru).

#### Dampak
Tidak ada pembatasan di level WebView terhadap injeksi inline script, pemuatan skrip eksternal, atau eksfiltrasi data ke domain tak dikenal apabila terjadi celah XSS di masa depan.

#### Opsi Solusi

* **Opsi A — Aktifkan CSP Ketat di `tauri.conf.json` (Disarankan):**
  Karena font sudah dibundle lokal (`@fontsource-variable/*`) dan seluruh skrip dibundle oleh Vite tanpa inline `<script>`, CSP ketat dapat langsung diterapkan:
  ```json
  "security": {
    "csp": "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self' http://127.0.0.1:* http://localhost:* ipc: http://ipc.localhost"
  }
  ```
  *(Catatan: hapus `'unsafe-inline'` dari `style-src` apabila seluruh inline `style="..."` di `index.html`/TS dipindahkan ke class CSS.)*
  * *Kelebihan:* Proteksi langsung tanpa mengubah arsitektur aplikasi.
* **Opsi B — CSP Nonce / Hash + Audit Inline Styles:**
  Pindahkan seluruh atribut `style` dinamis ke CSS custom properties / class sehingga `style-src 'self'` dapat diterapkan tanpa `'unsafe-inline'`.

#### Kriteria Verifikasi
Jalankan `npm run build` dan buka aplikasi Tauri; pastikan tidak ada error CSP di WebView DevTools pada seluruh 5 view (termasuk paste gambar/attachment dan render diff/markdown).

---

### [F-05] Medium — Perbandingan Token & Client Secret Tidak Constant-Time

* **Kategori:** Security / Cryptography
* **Lokasi:** `src-tauri/src/mcp_server.rs:73, 87, 118`, `src-tauri/src/oauth.rs:512, 623`

#### Analisis Teknis
Validasi `auth_token` di `check_auth` (`mcp_server.rs`) dan `client_secret` di `handle_oauth_token` (`oauth.rs`) menggunakan operator kesetaraan string standar Rust (`==` / `!=`), yang melakukan *short-circuit* pada byte pertama yang berbeda.

#### Dampak
Secara teoritis membuka celah *timing side-channel attack* terhadap token autentikasi MCP yang terekspos ke internet melalui Cloudflare Tunnel atau Tailscale Funnel.

#### Opsi Solusi

* **Opsi A — Gunakan Crate `subtle` (`ConstantTimeEq`) (Disarankan):**
  Tambahkan `subtle = "2"` di `Cargo.toml` dan buat helper `constant_time_eq(a: &str, b: &str) -> bool` di `security.rs` (atau bandingkan SHA-256 digest keduanya dengan `ct_eq` agar panjang string juga tidak bocor).
  ```rust
  use sha2::{Digest, Sha256};
  use subtle::ConstantTimeEq;

  pub fn secure_token_eq(a: &str, b: &str) -> bool {
      let ha = Sha256::digest(a.as_bytes());
      let hb = Sha256::digest(b.as_bytes());
      bool::from(ha.as_slice().ct_eq(hb.as_slice()))
  }
  ```
  * *Kelebihan:* Nol overhead berarti, menyembunyikan baik isi maupun panjang token.
* **Opsi B — Helper Constant-Time Tanpa Dependensi Baru:**
  Implementasikan XOR accumulator pada SHA-256 digest (panjang tetap 32 byte) menggunakan `std::hint::black_box` — meski `subtle` tetap jauh lebih disarankan karena mencegah optimasi compiler secara teruji.

#### Kriteria Verifikasi
Ganti seluruh perbandingan secret/token dengan `secure_token_eq` dan pastikan seluruh unit & E2E test di `mcp_server.rs` lulus.

---

### [F-06] Medium — Keterbatasan Command Guard Berbasis Substring Blacklist & Isolasi Shell

* **Kategori:** Security / Sandbox
* **Lokasi:** `src-tauri/src/security.rs:158-224`, `src-tauri/src/tools.rs:1395-1417`, `src-tauri/src/jobs.rs:117-160`

#### Analisis Teknis
`validate_command` menyaring perintah shell berdasarkan daftar 45 substring terlarang (`"rm -rf /"`, `"auth.json"`, `".smart-tauri-hub"`, `"invoke-expression"`, dll.). Pendekatan blacklist string ini rentan terhadap bypass trivial:
* Sisipan tanda kutip / escape shell: `type "%USERPROFILE%\.smart-tauri-"hub\auth.json` atau `type auth""json`
* Ekspansi variabel environment / perintah alternatif: `cat ~/.smart-tauri-hub/a*`, `python -c "open(...)"`, `node -e "..."`
* Variasi urutan flag: `git clean -f -d -x` atau `rm -r -f /`

Selain itu, `run_shell_raw` mengeksekusi perintah via `cmd.exe /C` dengan `current_dir(root)`, namun tanpa isolasi filesystem/process OS-level. Dalam mode **`autonomous`**, perintah shell yang lolos `validate_command` dapat membaca/menulis file di luar `workspace_root` (misalnya membaca direktori `~/.smart-tauri-hub/` atau memodifikasi file sistem lain).

#### Dampak
Di mode `autonomous`, *prompt injection* dari file repositori atau output downstream MCP dapat mengeksekusi perintah shell yang membypass `validate_command` dan keluar dari directory jail.

#### Opsi Solusi

* **Opsi A — Defense-in-Depth: Strict Policy di Mode Autonomous + Command Allowlist Opsional (Disarankan):**
  1. Tambahkan opsi konfigurasi **Command Execution Policy**: `hitl` (wajib popup, default bahkan di mode autonomous untuk perintah shell), `allowlist` (hanya prefix aman yang dikonfigurasi seperti `cargo`, `npm`, `pytest`, `git status/diff/test`, `tsc`, tanpa metakarakter chaining `&&`, `|`, `;`, `>`), atau `unrestricted_guarded`.
  2. Perketat `validate_command` dengan menolak karakter/pola obfuscation umum (misalnya deteksi tokenisasi argumen dan pemblokiran pola regex untuk `rm` + `-r`/`-f`, `git clean` + `-f`, serta blokir akses eksplisit ke `%USERPROFILE%` / `$HOME` / `~`).
  * *Kelebihan:* Praktis, langsung menurunkan risiko prompt injection tanpa kompleksitas sandbox kernel.
* **Opsi B — OS-Level Sandboxing (Windows Job Object + AppContainer / Restricted Token):**
  Jalankan proses shell anak di dalam Windows Restricted Token / AppContainer yang hanya memiliki ACL baca/tulis pada `workspace_root` dan tanpa akses ke `%USERPROFILE%\.smart-tauri-hub`.
  * *Kelebihan:* Penegakan batas keamanan sejati di level kernel OS.
  * *Trade-off:* Kompleksitas implementasi tinggi dan beberapa toolchain (seperti `cargo` yang butuh akses ke `%USERPROFILE%\.cargo` atau `npm` cache) membutuhkan whitelist direktori tambahan.

#### Kriteria Verifikasi
Tambahkan test di `security.rs` untuk variasi bypass (urutan flag berbeda, quotes, chaining) dan dokumentasikan batasan mode `autonomous` dengan jelas di UI & README.

---

### [F-07] Medium — Downstream HTTP MCP Tidak Menyimpan/Mengirim Ulang `mcp-session-id`

* **Kategori:** Protocol Compliance / Interoperability Bug
* **Lokasi:** `src-tauri/src/mcp_hub.rs:196-200, 339-388, 602-615`

#### Analisis Teknis
Saat `McpHubManager::refresh_server` melakukan handshake ke downstream HTTP MCP server (`mcp_hub.rs:339-380`), server menangkap `sid` (`mcp-session-id` header) dari respons `initialize` dan meneruskannya ke `notifications/initialized` serta `tools/list`.

Namun, `DownstreamServerRuntime` tidak memiliki field untuk menyimpan `sid`:
```rust
pub struct DownstreamServerRuntime {
    pub status: String,
    pub tools: Vec<Value>,
    pub last_error: Option<String>,
    pub stdio: Arc<Mutex<Option<StdioMcpProcess>>>,
}
```
Akibatnya, saat `call_downstream_tool` memanggil tool melalui HTTP (`mcp_hub.rs:602-615`), `session_id` selalu diisi `None`:
```rust
http_rpc_post(
    &client,
    &cfg,
    None, // <-- Session ID dari initialize tidak dikirim!
    &json!({ ... "method": "tools/call", ... }),
)
```

#### Dampak
Downstream Streamable HTTP MCP server yang bersifat stateful (sesuai spesifikasi MCP 2025-03-26 yang mewajibkan header `Mcp-Session-Id` pada semua request pasca-`initialize`) akan menolak `tools/call` dengan HTTP `400 Bad Request` atau `404 Session Not Found`.

#### Opsi Solusi

* **Opsi A — Simpan `http_session_id` di `DownstreamServerRuntime` + Auto Re-Initialize pada 404 (Disarankan):**
  1. Tambahkan `pub http_session_id: Option<String>` pada `DownstreamServerRuntime`.
  2. Simpan `sid` hasil `refresh_server` ke `entry.http_session_id`.
  3. Baca dan kirim `http_session_id.as_deref()` saat `call_downstream_tool`, serta perbarui bila server mengembalikan session ID baru.
  4. Jika `tools/call` gagal dengan HTTP `404` (session kedaluwarsa di server downstream), lakukan 1x re-handshake otomatis lalu retry `tools/call`.
  * *Kelebihan:* Sepenuhnya kompatibel dengan spesifikasi Streamable HTTP MCP 2025-03-26/2026.

#### Kriteria Verifikasi
Perluas E2E test `test_mcp_hub_http_aggregation_end_to_end` di `mcp_server.rs` dengan mock downstream HTTP MCP server yang memvalidasi keberadaan header `mcp-session-id` pada `tools/call` dan mengembalikan `400` jika tidak ada.

---

### [F-08] Medium — Overhead Spawn `powershell.exe` untuk Enkripsi/Dekripsi Windows DPAPI

* **Kategori:** Performance & Reliability
* **Lokasi:** `src-tauri/src/secrets.rs:43-115`

#### Analisis Teknis
Implementasi `dpapi_encrypt` dan `dpapi_decrypt` pada Windows memanggil `Command::new("powershell.exe")` dengan `-ExecutionPolicy Bypass` dan memuat assembly .NET `System.Security` setiap kali secrets dibaca (startup) atau disimpan.

#### Dampak
1. **Latensi Tinggi:** Setiap spawn PowerShell + inisialisasi CLR memakan waktu ~300–800 ms, memperlambat cold start aplikasi dan aksi simpan API key.
2. **Kerapuhan Lingkungan:** Dapat gagal atau lambat pada mesin korporat dengan Windows Defender/EDR, AppLocker, atau Constrained Language Mode yang membatasi `powershell.exe` / `Add-Type`.

#### Opsi Solusi

* **Opsi A — Panggil Win32 `CryptProtectData` / `CryptUnprotectData` Langsung via FFI (Disarankan):**
  Gunakan crate `windows-sys` (fitur `Win32_Security_Cryptography` & `Win32_Foundation`) — yang ringan dan kompatibel dengan toolchain `x86_64-pc-windows-gnu` — atau deklarasi `extern "system"` langsung ke `crypt32.dll` dan `kernel32.dll` (`LocalFree`).
  * *Kelebihan:* Eksekusi kurang dari 1 ms (lebih cepat ~500x), nol proses eksternal, 100% kompatibel dengan format blob DPAPI yang sama (`CurrentUser`, optional entropy `NULL`) sehingga file `secrets.dpapi` lama langsung terbaca tanpa migrasi!
* **Opsi B — Gunakan Crate `keyring` (Lihat F-02 Opsi B):**
  Menghapus kebutuhan implementasi DPAPI manual sekaligus mendukung macOS dan Linux.

#### Kriteria Verifikasi
Jalankan `test_secrets_store_roundtrip` dan uji interoperabilitas: enkripsi blob dengan implementasi lama lalu dekripsi dengan FFI `CryptUnprotectData` (dan sebaliknya) harus menghasilkan plaintext identik.

---

### [F-09] Medium — Tidak Ada Reuse `reqwest::Client` & Rekompilasi `Regex` Berulang

* **Kategori:** Performance / Resource Efficiency
* **Lokasi:**
  * `src-tauri/src/providers.rs:513, 1008` (`reqwest::Client::new()`)
  * `src-tauri/src/local_agent.rs:437` (`reqwest::Client::new()`)
  * `src-tauri/src/mcp_hub.rs:334, 590` (`reqwest::Client::builder()...build()`)
  * `src-tauri/src/security.rs:229` (`Regex::new(...)` di dalam `compress_output`)

#### Analisis Teknis
1. **HTTP Client Tidak Di-reuse:** Dokumentasi resmi `reqwest` menekankan bahwa `Client` menyimpan connection pool internal dan TLS session cache (`Arc`-backed, murah untuk di-clone). Saat ini `Client` baru dibuat pada setiap turn Local Agent dan setiap panggilan downstream HTTP MCP, sehingga setiap request melakukan ulang DNS lookup, TCP handshake, dan TLS handshake (rustls).
2. **Kompilasi Regex Berulang:** `compress_output` dipanggil pada hampir setiap eksekusi shell, git, dan downstream output, namun selalu memanggil `Regex::new(r"\x1B\[[0-9;?]*[ -/]*[@-~]")` dari awal di setiap panggilan.

#### Dampak
Penambahan latensi per request (terutama terasa pada multi-step agent loop hingga 8 step atau polling downstream MCP) serta alokasi memori dan CPU yang tidak perlu.

#### Opsi Solusi

* **Opsi A — Shared `reqwest::Client` di `AppState` & `OnceLock<Regex>` (Disarankan):**
  1. Tambahkan field `pub http_client: reqwest::Client` di `AppState` (dibuat sekali saat inisialisasi dengan konfigurasi timeout default/pool optimal) dan gunakan `.timeout(...)` per-request di `mcp_hub.rs`.
  2. Ganti `Regex::new` di `security.rs:229` dengan `static ANSI_RE: OnceLock<Regex> = OnceLock::new();`.
  * *Kelebihan:* Perubahan kecil, risiko regresi sangat rendah, langsung mengaktifkan HTTP/2 multiplexing & TLS keep-alive.

#### Kriteria Verifikasi
Pastikan tidak ada lagi `reqwest::Client::new()` di dalam hot path (`providers.rs`, `local_agent.rs`, `mcp_hub.rs`) dan jalankan `cargo test`.

---

### [F-10] Low — Modul Inti Monolitik & Duplikasi Kode Internal

* **Kategori:** Maintainability / Architecture
* **Lokasi:**
  * `src-tauri/src/tools.rs` (2.215 baris), `mcp_server.rs` (1.782 baris), `providers.rs` (1.357 baris), `index.html` (58,7 KB)
  * Duplikasi `kill_pid_tree`: `jobs.rs:82-98`, `mcp_hub.rs:31-47`, dan `ChildTreeGuard::kill_tree_now` di `tools.rs:1371-1386`
  * Duplikasi pola retry/backoff SSE & loop 8-step di `local_agent.rs:464-550` dan `providers.rs:520-610, 1015-1100`

#### Analisis Teknis
Beberapa file backend telah melampaui 1.300–2.200 baris karena mencampurkan definisi skema, guardrail engine, implementasi bisnis, dan E2E test dalam satu file. Selain itu, logika terminasi process tree di Windows terduplikasi di 3 modul berbeda, sehingga perbaikan (misalnya dukungan `SIGTERM`/process group di Linux/macOS) harus diterapkan di 3 tempat.

#### Dampak
Meningkatkan beban kognitif saat membaca/memodifikasi kode, memperbesar risiko inkonsistensi perilaku antar-modul (seperti bug yang diperbaiki di satu provider namun terlewat di provider lain), dan memperlambat waktu kompilasi inkremental.

#### Opsi Solusi

* **Opsi A — Ekstraksi Bertahap Berbasis Domain (Disarankan):**
  1. Buat modul `process_util.rs` untuk menyatukan `kill_pid_tree` dan `ChildTreeGuard`.
  2. Pecah `tools.rs` menjadi folder `tools/` (`mod.rs`, `schemas.rs`, `guardrail.rs`, `fs_tools.rs`, `shell.rs`, `revert.rs`).
  3. Pecah `providers.rs` menjadi folder `providers/` (`mod.rs`, `openai.rs`, `anthropic.rs`, `gemini.rs`, `common.rs`).
  * *Kelebihan:* Tidak mengubah kontrak publik/IPC apa pun; dapat dilakukan bertahap tanpa menghentikan pengembangan fitur.

#### Kriteria Verifikasi
`cargo test` dan `cargo clippy` lulus bersih pasca-ekstraksi tanpa perubahan perilaku eksternal.

---

### [F-11] Low — Inkonsistensi Minor, Legacy API, Keterbatasan Markdown Frontend, & Tanpa Git

* **Kategori:** Code Hygiene & UX
* **Lokasi:**
  * `src-tauri/src/mcp_resources.rs:83`
  * `src-tauri/src/main.rs:73-98`, `src-tauri/src/state.rs:176-191`
  * `src/markdown.ts:1-184`
  * Root workspace `D:\tauri-mcp-hub` (belum ada `.git`)

#### Analisis Teknis
1. **`workspace://git/diff` Menggunakan Shell (`mcp_resources.rs:83`):** Resource ini memanggil `run_shell_raw(&root, "git diff", 8)` yang membungkusnya ke `cmd.exe /C`, padahal `tools_extra.rs:384` sudah memanggil `Command::new("git").args(["diff"])` secara langsung tanpa overhead/risiko shell.
2. **Legacy `configure_gemini` & `gemini_config`:** State dan IPC command masih mempertahankan `gemini_config.api_key` sekaligus `provider_secrets.gemini_api_key`, sehingga ada dua sumber kebenaran (*dual source of truth*) yang harus disinkronisasi manual di `main.rs:82-88` dan `local_agent.rs:394-403`.
3. **Parser Markdown Kustom Terbatas (`src/markdown.ts`):** Saat ini hanya mendukung heading `h1-h3`, blockquote, `ul`/`ol`, code fence, dan inline `` `code` ``, `**bold**`, `*italic*`. Output LLM coding agent sering memuat **tabel Markdown**, **hyperlink `[text](url)`**, dan **nested inline formatting** (misalnya `` **`code`** ``), yang saat ini dirender sebagai teks mentah.
4. **Workspace Belum Diinisialisasi Git:** Tidak adanya `.git` membuat histori perubahan, branching, dan rollback di level proyek tidak tersedia.

#### Opsi Solusi

* **Opsi A — Perbaikan Cepat Terfokus (Disarankan):**
  1. Ganti pemanggilan di `mcp_resources.rs:83` menjadi `Command::new("git").arg("diff")` langsung dengan `current_dir(&root)` dan timeout 8 detik.
  2. Hapus penyimpanan `api_key` duplikat dari `GeminiConfig` (gunakan `provider_secrets` sebagai satu-satunya sumber kebenaran) dan jadikan `configure_gemini` sekadar wrapper tipis ke logika `configure_agent_provider` (atau deprecate jika frontend sudah tidak memanggilnya).
  3. Tambahkan dukungan parser tabel Markdown (`| col |`) dan tautan aman (`https://` / `http://` saja, menolak `javascript:`) pada `src/markdown.ts` tetap berbasis DOM API murni.
  4. Jalankan `git init` dengan `.gitignore` yang mengecualikan `node_modules/`, `dist/`, `src-tauri/target/`, dan `.cargo/config.toml` lokal.

---

## 4. Roadmap Perbaikan yang Disarankan

### Fase 1 — Quick Wins Keamanan & Bug Kritis (Estimasi: 0,5 – 1 hari)
- [ ] **[F-01]** Pindahkan blokir permanen `.smart-tauri-hub/` ke atas `allow_sensitive_reads` di `tools.rs:613`.
- [ ] **[F-03]** Perbaiki bypass `client_id`/`redirect_uri` kosong & wajibkan PKCE `S256` di `oauth.rs`.
- [ ] **[F-04]** Aktifkan CSP ketat di `src-tauri/tauri.conf.json`.
- [ ] **[F-05]** Terapkan perbandingan constant-time (`subtle` + SHA-256) untuk seluruh token/secret.
- [ ] **[F-07]** Simpan & teruskan `mcp-session-id` pada downstream HTTP MCP di `mcp_hub.rs`.
- [ ] **[F-11]** Ganti `workspace://git/diff` di `mcp_resources.rs:83` agar memanggil `git` tanpa shell, dan inisialisasi Git repo.

### Fase 2 — Hardening Secret & Optimasi Runtime (Estimasi: 1 – 2 hari)
- [ ] **[F-02]** Migrasikan `cloudflare_tunnel_token`, `downstream_mcps[].bearer_token`, dan `env` sensitif ke `SecretsStore` (DPAPI), serta simpan hash SHA-256 untuk OAuth access/refresh tokens di `auth.json`.
- [ ] **[F-08]** Ganti spawn `powershell.exe` di `secrets.rs` dengan Win32 FFI `CryptProtectData` / `CryptUnprotectData`.
- [ ] **[F-09]** Simpan shared `reqwest::Client` di `AppState` dan cache ANSI `Regex` dengan `OnceLock`.
- [ ] **[F-06]** Tambahkan opsi *Command Execution Policy* (HITL default / allowlist) dan perketat deteksi bypass di `security.rs`.

### Fase 3 — Refactoring Maintainability & Peningkatan UI (Estimasi: 1 – 2 hari)
- [ ] **[F-10]** Satukan `kill_pid_tree` & `ChildTreeGuard` ke modul utilitas bersama, lalu pecah `tools.rs` dan `providers.rs` menjadi sub-modul terstruktur.
- [ ] **[F-11]** Bersihkan duplikasi state `gemini_config.api_key` dan tambahkan dukungan tabel + safe links pada `src/markdown.ts`.
