# Blueprint 01 — Multi-Workspace Architecture & Implementation Guide

| Atribut | Detail |
| :--- | :--- |
| **Target Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Target Executor** | Google Antigravity Agent (Autonomous / Co-pilot Handoff) |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Status** | Ready for Implementation |
| **Prioritas** | P1 — Fondasi Lintas-Repo untuk WebMCP, Local Agent, dan Audit |
| **Dependensi Blueprint** | Independen (disarankan dikerjakan sebelum Blueprint 03 & 04) |

---

## 1. Handoff Brief untuk Agent Antigravity

Dokumen ini adalah spesifikasi arsitektur dan implementasi lengkap untuk meningkatkan Airlock dari arsitektur **Single Global Workspace** (`AppState.workspace_root: RwLock<PathBuf>`) menjadi **Multi-Workspace Registry** dengan alias terisolasi, guardrail per-workspace, routing HTTP/MCP per-koneksi, serta audit & rollback lintas-workspace yang aman.

### Invarian Arsitektur Airlock yang WAJIB Dipatuhi
Sebelum menulis satu baris kode pun, agent **wajib** menjaga seluruh invarian berikut:
1. **Zero UI Framework:** Frontend adalah **Vanilla TypeScript (strict)** + **Vite 6** + **Obsidian Studio CSS**. Jangan menambahkan React, Vue, Svelte, atau Tailwind.
2. **Higiene XSS Ketat:** Jangan pernah menyisipkan data user/path/alias ke `innerHTML`. Gunakan `document.createElement`, `textContent`, `setAttribute`, dan `append`.
3. **Backward Compatibility 100%:**
   * File persistensi tetap di `~/.smart-tauri-hub/` (`config.json`, `auth.json`, `secrets.dpapi`, `snapshots/`, `chats/`).
   * Klien MCP lama yang memanggil tool **tanpa** parameter `workspace` tetap harus berjalan mulus dengan menargetkan `default_workspace`.
   * IPC command lama (`set_workspace_root`, `toggle_pin_workspace`) harus tetap berfungsi agar tidak merusak skrip/UI eksisting.
4. **Windows Path Normalization:** Selalu strip prefix extended-length Windows (`r"\\?\"`) saat menyimpan ke `config.json`, `auth.json`, `DashboardSnapshot`, dan `AuditEntry`, namun selalu gunakan path hasil `std::fs::canonicalize` saat menegakkan `resolve_jailed_path`.
5. **Toolchain & Verifikasi:** Build & test menggunakan Rust GNU toolchain (`cd src-tauri && cargo test`) dan frontend (`npm run build`). Jangan biarkan warning `tsc` (`noUnusedLocals`, `noUnusedParameters`) lolos.

---

## 2. Analisis Kondisi Saat Ini (As-Is)

Saat ini seluruh sistem mengasumsikan hanya ada **satu** `workspace_root` global:

| Komponen | Lokasi Eksisting | Keterbatasan Saat Ini |
| :--- | :--- | :--- |
| **Global State** | `src-tauri/src/state.rs:235` | `pub workspace_root: RwLock<PathBuf>` — mengganti folder langsung menimpa konteks semua klien MCP dan sesi Local Agent aktif. |
| **Config** | `src-tauri/src/config.rs:78-115` | Hanya menyimpan `recent_workspaces: Vec<String>`, `pinned_workspaces: Vec<String>`, dan satu `guardrail_mode` / `sensitive_deny_patterns` global. |
| **Tool Execution** | `src-tauri/src/tools.rs:539` | `execute_tool` selalu membaca `state.workspace_root.read().await.clone()`. Tidak ada parameter `workspace` pada 14 built-in tools. |
| **Audit & Revert** | `src-tauri/src/state.rs:143-166`, `tools.rs:1613` | `AuditEntry` **tidak** menyimpan asal workspace. `revert_audit_entry` selalu merevert terhadap `state.workspace_root` saat ini — **berbahaya jika workspace sudah berganti!** |
| **HITL Session Cache** | `src-tauri/src/state.rs:259`, `tools.rs:428` | `session_approved_tools: RwLock<HashSet<String>>` hanya menyimpan nama tool (global), belum dibedakan per-workspace. |
| **MCP HTTP Server** | `src-tauri/src/mcp_server.rs:18-20, 251` | `TokenQuery` hanya punya `token`. `handle_single_rpc` tidak menerima konteks workspace dari request HTTP/header. |
| **MCP Resources** | `src-tauri/src/mcp_resources.rs:8-70` | URI hanya `workspace://overview`, `workspace://git/diff`, `workspace://file/{rel_path}` — semuanya membaca `state.workspace_root`. |
| **Local Agent & Attachments** | `src-tauri/src/local_agent.rs:321`, `providers.rs:162` | System prompt hanya menyuntikkan 1 root & 1 project rules. Attachment `@file` hanya meresolusi terhadap 1 root. |
| **Background Jobs** | `src-tauri/src/jobs.rs:26-49` | `JobSummary` & `ManagedJob` tidak mencatat workspace asal job. |
| **Frontend Workspace UI** | `src/views/workspace.ts:1-186` | Hanya 1 input bar + dropdown Recent/Pinned untuk mengganti satu root tunggal. |

---

## 3. Arsitektur Target (To-Be)

### 3.1 Diagram Arsitektur & Resolusi Workspace

```text
                    +-------------------------------------------------------+
                    |                  Incoming Requests                    |
                    +-------------------------------------------------------+
                       |                        |                        |
         WebMCP HTTP Request           Local Agent / Simulator        Tauri UI Actions
   (?workspace=api / X-Airlock-Ws)     (args.workspace / @api:...)   (Manage Profiles)
                       |                        |                        |
                       v                        v                        v
        +---------------------------------------------------------------------------+
        |                 WorkspaceResolver (src-tauri/src/workspace.rs)            |
        |                                                                           |
        |  Priority Order:                                                          |
        |   1. Explicit tool arg: args["workspace"] (alias or ID)                   |
        |   2. Connection/Request pinned workspace (?workspace=... / header)        |
        |   3. Active Default Workspace (AppState.active_workspace_id)              |
        |                                                                           |
        |  Returns: ResolvedWorkspace { id, alias, canon_root, clean_root,          |
        |            effective_guardrail, effective_sensitive_patterns,             |
        |            allow_sensitive_reads }                                        |
        +---------------------------------------------------------------------------+
                       |                        |                        |
                       v                        v                        v
            +--------------------+   +--------------------+   +--------------------+
            | Directory Jail     |   | Per-Workspace      |   | Scoped Audit &     |
            | resolve_jailed_path|   | Guardrail & HITL   |   | Snapshot Revert    |
            | (per canon_root)   |   | (key: ws_id::tool) |   | (pins ws_root)     |
            +--------------------+   +--------------------+   +--------------------+
```

### 3.2 Keputusan Desain Kunci

1. **Workspace Profile & Alias Unik:**
   * Setiap workspace yang terpasang memiliki `id` stabil (`ws_<8hex>`), `alias` URL-safe (`[a-z0-9_-]`, contoh: `api`, `web`, `mobile`, `docs`), `root_path` (clean string tanpa `\\?\`), `enabled: bool`, serta override opsional untuk `guardrail_mode`, `allow_sensitive_reads`, dan `extra_sensitive_patterns`.
   * `alias` adalah nama yang dilihat dan dipakai oleh LLM (jauh lebih hemat token dan lebih aman dibanding mengirim absolute Windows path seperti `D:\Users\...\backend`).
2. **Effective Guardrail Policy (Most-Restrictive / Explicit Override):**
   * Jika `profile.guardrail_mode` bernilai `Some("read_only" | "safe_edit" | "autonomous")`, gunakan mode tersebut untuk semua tool yang menarget workspace itu.
   * Jika `None`, warisi `AppState.guardrail_mode` global.
   * **Security Rule:** Jika **global** mode sedang diset ke `read_only` (panic switch / emergency lock), global `read_only` **selalu menang** atas override workspace mana pun!
3. **Cross-Workspace Isolation:**
   * Setiap operasi tool hanya bisa mengakses file di dalam `canon_root` workspace yang diresolusi.
   * Operasi `file_ops(action="move")` **dilarang** memindahkan file lintas-workspace (hanya dalam satu jail).
   * HITL `ApproveForSession` menyimpan key `format!("{ws_id}::{tool_name}")` pada `session_approved_tools`, sehingga menyetujui `run_command_compressed` di workspace `sandbox` tidak otomatis meloloskan shell command di workspace `production`.
4. **Scoped HTTP/MCP Routing:**
   * Klien MCP dapat mengakses:
     * Default multi-workspace hub: `POST /mcp` atau `/mcp/:token` (agent bisa mengakses semua workspace aktif via `workspace: "alias"`).
     * Locked single-workspace endpoint: `POST /mcp?workspace=api` atau header `X-Airlock-Workspace: api` (semua tool call otomatis diarahkan ke `api`; jika tool call mencoba menyebut workspace lain yang berbeda dari header/query pin, **tolak** dengan error `403`/tool error agar satu klien WebMCP bisa diisolasi ketat ke 1 repo saja).
5. **Multi-Workspace Tool Discovery (`list_workspaces`):**
   * Tambahkan 1 built-in tool read-only baru: `list_workspaces` (total built-in tool menjadi 15, atau lebih jika digabung Blueprint 02). Tool ini mengembalikan daftar workspace aktif beserta `alias`, `guardrail_mode`, `git_branch`, dan flag `is_default` dalam format ringkas hemat token.
   * Semua tool eksisting yang menyentuh filesystem/git/shell menerima parameter opsional `workspace: string` di `inputSchema`.

---

## 4. Kontrak Data & Skema Lengkap

### 4.1 Modul Baru: `src-tauri/src/workspace.rs`

Buat modul khusus `workspace.rs` agar logika multi-workspace terisolasi rapi dan mudah diuji.

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceProfile {
    pub id: String,                          // Stabil, mis. "ws_a1b2c3d4"
    pub alias: String,                       // Unik lowercase [a-z0-9_-], mis. "backend"
    pub path: String,                        // Clean path tanpa prefix \\?\
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub guardrail_mode: Option<String>,      // None = inherit global; Some("read_only"|"safe_edit"|"autonomous")
    #[serde(default)]
    pub allow_sensitive_reads: Option<bool>, // None = inherit global
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
    pub exists: bool,
    pub git_branch: Option<String>,
    pub guardrail_mode: Option<String>,
    pub effective_guardrail_mode: String,
    pub allow_sensitive_reads: Option<bool>,
    pub pinned: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedWorkspace {
    pub id: String,
    pub alias: String,
    pub canon_root: PathBuf,
    pub clean_root: String,
    pub effective_guardrail: String,
    pub effective_allow_sensitive: bool,
    pub effective_sensitive_patterns: Vec<String>,
}
```

**Helper Wajib di `workspace.rs`:**
* `pub fn sanitize_alias(raw: &str) -> String`: ubah ke lowercase, ganti spasi/simbol jadi `-`, pangkas non-alphanumeric selain `_`/`-`, panjang 1..32, fallback `"ws"`.
* `pub fn ensure_unique_alias(desired: &str, existing: &[WorkspaceProfile], ignore_id: Option<&str>) -> String`: tambahkan sufiks `-2`, `-3`, dst jika bentrok.
* `pub fn normalize_clean_path(path: &Path) -> Result<(PathBuf, String), String>`: validasi `exists() && is_dir()`, panggil `canonicalize`, lalu strip `r"\\?\"`.
* `pub async fn resolve_workspace(state: &AppState, requested: Option<&str>, pinned_connection_ws: Option<&str>) -> Result<ResolvedWorkspace, String>`:
  * Jika `pinned_connection_ws` bernilai `Some(pin)`:
    * Jika `requested` juga `Some(req)` dan tidak cocok dengan `pin` (baik `id` maupun `alias`, case-insensitive), kembalikan `Err("Akses ditolak: koneksi MCP ini dikunci ke workspace '{pin}', tidak dapat mengakses '{req}'.")`.
    * Resolusi terhadap `pin`.
  * Jika `pinned_connection_ws` adalah `None`:
    * Jika `requested` adalah `Some(req)` (non-empty), cari workspace dengan `enabled == true` yang `alias.eq_ignore_ascii_case(req)` atau `id == req`. Jika tidak ditemukan atau disabled, kembalikan `Err` informatif yang menyertakan daftar alias aktif!
    * Jika `requested` adalah `None` atau empty, gunakan `active_workspace_id` (atau fallback `state.workspace_root`).
  * Hitung `effective_guardrail`: jika global == `"read_only"` → `"read_only"`, else `profile.guardrail_mode.unwrap_or(global)`.
  * Gabungkan `effective_sensitive_patterns = global_patterns + profile.extra_sensitive_patterns`.

### 4.2 Perubahan pada `src-tauri/src/config.rs`

Tambahkan field berikut pada `AppConfig` dengan `#[serde(default)]` agar file `config.json` lama ter-deserialize tanpa error:

```rust
#[serde(default)]
pub workspaces: Vec<WorkspaceProfile>,
#[serde(default)]
pub default_workspace_id: Option<String>,
```

**Migrasi Otomatis pada Startup (`state.rs::build_state`):**
1. Jika `cfg.workspaces` kosong saat startup:
   * Ambil `initial_root` (dari `PersistedAuth.last_workspace_root` atau `default_workspace_dir()`).
   * Buat 1 `WorkspaceProfile` awal dengan `alias` diambil dari nama folder terakhir (`file_name()`, disanitasi via `sanitize_alias`), `enabled: true`, `pinned: true`.
   * Set `cfg.default_workspace_id = Some(profile.id.clone())`.
   * Migrasikan `pinned_workspaces` lama yang masih valid di disk sebagai profil tambahan (dengan `enabled: false` atau `true` — rekomendasikan `enabled: true` hanya untuk root aktif, dan `pinned_workspaces` tetap terbaca sebagai quick-add agar tidak mendadak membuka akses repo lama tanpa izin user!).
2. Sinkronkan `state.workspace_root` agar selalu mencerminkan `canon_root` dari `default_workspace_id` (menjaga kompatibilitas penuh dengan seluruh kode lama).

### 4.3 Perubahan pada `src-tauri/src/state.rs`

1. **`AuditEntry` (KRITIS — Cegah Revert Salah Repo):**
   ```rust
   #[serde(default)]
   pub workspace_id: Option<String>,
   #[serde(default)]
   pub workspace_alias: Option<String>,
   #[serde(default)]
   pub workspace_root: Option<String>,
   ```
2. **`DashboardSnapshot`:**
   ```rust
   pub default_workspace_id: String,
   pub workspaces: Vec<WorkspaceSummary>,
   pub builtin_tools_count: usize, // Hapus hardcode angka 14 di frontend!
   ```
3. **`AppState`:**
   * Pertahankan `pub workspace_root: RwLock<PathBuf>` (sinkron dengan default workspace).
   * Tambahkan `pub default_workspace_id: RwLock<String>`.
   * Catatan: Profil lengkap tetap berada di `state.config` (`AppConfig.workspaces`) sehingga tidak terjadi duplikasi/desinkronisasi state.

### 4.4 Perubahan pada `src-tauri/src/jobs.rs`

Tambahkan pada `ManagedJob` dan `JobSummary`:
```rust
#[serde(default)]
pub workspace_alias: Option<String>,
```
Sehingga `workspace://jobs/active` dan tabel Background Jobs di Live Monitor langsung menampilkan badge `[api]` / `[web]`.

### 4.5 Perubahan pada MCP Tools (`tools.rs` & `tools_extra.rs`)

1. **Tool Baru `list_workspaces` (Read-Only):**
   * Schema: `properties: {}` (tanpa argumen wajib).
   * Output ringkas:
     ```text
     ACTIVE WORKSPACES (2):
     * [default] api  | D:\projects\backend  | mode: safe_edit  | branch: main
       [mounted] web  | D:\projects\frontend | mode: autonomous | branch: feat/ui
     ```
2. **Tambahkan `workspace` Opsional pada 12 Built-in Tools:**
   * `project_overview`, `smart_search`, `read_file_range`, `surgical_edit`, `patch_and_verify`, `run_command_compressed`, `write_file_jailed`, `find_files`, `code_outline`, `git_ops`, `file_ops`, `start_command_job`.
   * Properti JSON Schema:
     ```json
     "workspace": {
       "type": "string",
       "description": "Optional workspace alias or ID (call list_workspaces to view mounted workspaces; defaults to active workspace)"
     }
     ```
   * Khusus `read_file_range`: izinkan juga field opsional `workspace` di dalam setiap item `batch_paths` (fallback ke `args.workspace` lalu default workspace) sehingga 1 panggilan `read_file_range` dapat membaca file dari repo `api` dan `web` sekaligus!
3. **Ubah Signature `execute_tool` & `enforce_guardrail_gate`:**
   * Buat `execute_tool_with_context(state, source, tool_name, args, pinned_ws: Option<&str>) -> ToolOutput` dan pertahankan wrapper `execute_tool(...)` yang memanggilnya dengan `pinned_ws = None` agar semua test lama tetap compile tanpa perubahan massal.
   * Di dalam `execute_tool_with_context`:
     1. Resolusi `ResolvedWorkspace` terlebih dahulu (kecuali untuk `list_workspaces`, `poll_command_job`, `stop_command_job`).
     2. Panggil `enforce_guardrail_gate` menggunakan `resolved.effective_guardrail` dan HITL session key `format!("{}::{tool_name}", resolved.id)` serta tampilkan `[workspace: {alias}]` pada `PendingToolApproval.summary`.
     3. Panggil `check_sensitive_guard` menggunakan `resolved.effective_allow_sensitive` dan `resolved.effective_sensitive_patterns` (jangan lupa sekalian perbaiki bug urutan `.smart-tauri-hub/` dari Temuan Audit F-01!).
     4. Simpan `workspace_id`, `workspace_alias`, dan `workspace_root` pada setiap `AuditEntry` yang dicatat.
4. **Perbaiki `revert_audit_entry` (`tools.rs:1608`):**
   * Gunakan root dari `entry.workspace_root` (jika ada) atau cari profil via `entry.workspace_id`, baru fallback ke `state.workspace_root` untuk entri audit legacy.
   * Pastikan `resolve_jailed_path(&entry_root, rel_path)` tetap dijalankan terhadap root asal entri tersebut!

### 4.6 Perubahan pada MCP Server & Resources (`mcp_server.rs` & `mcp_resources.rs`)

1. **`TokenQuery` di `mcp_server.rs:18`:**
   ```rust
   #[derive(Debug, Deserialize)]
   pub struct TokenQuery {
       token: Option<String>,
       workspace: Option<String>,
   }
   ```
2. **Header Pinning (`mcp_server.rs`):**
   * Tambahkan `HeaderName::from_static("x-airlock-workspace")` ke daftar `allow_headers` pada `CorsLayer` (`mcp_server.rs:727`).
   * Di `process_mcp_post`: ekstrak `pinned_ws = query.workspace.clone().or_else(|| headers.get("x-airlock-workspace")...)`.
   * Teruskan `pinned_ws.as_deref()` ke `handle_single_rpc_with_context(state, msg, pinned_ws)` (pertahankan `handle_single_rpc` memanggil dengan `None` untuk kompatibilitas `stdio_bridge.rs` dan test).
   * Catat informasi workspace pada `HttpTrafficEntry.note` jika request memakai workspace pin.
3. **MCP Resources (`mcp_resources.rs`):**
   * Tambahkan resource statis `workspace://list` (`application/json` daftar workspace aktif).
   * Tambahkan resource templates baru:
     * `workspace://{alias}/overview`
     * `workspace://{alias}/git/diff`
     * `workspace://{alias}/file/{rel_path}`
   * Pertahankan seluruh URI lama (`workspace://overview`, `workspace://git/diff`, `workspace://file/{rel_path}`) agar mengarah ke default workspace (atau `pinned_ws` jika ada).

### 4.7 Perubahan pada Local Agent & Attachments (`local_agent.rs` & `providers.rs`)

1. **System Prompt Multi-Workspace (`local_agent.rs:334`):**
   * Jika hanya 1 workspace aktif: tampilkan format ringkas seperti sekarang.
   * Jika >1 workspace aktif: tampilkan tabel ringkas daftar mounted workspaces (`alias`, `path`, `mode`, `default`) di system prompt dan instruksikan agent bahwa ia dapat mengoper parameter `workspace: "<alias>"` ke semua tool.
   * Jika `auto_inject_project_rules` aktif: muat rules dari default workspace (maks 3.000 char) dan tambahkan header singkat bila workspace lain juga memiliki `AGENTS.md`/`CLAUDE.md` (beri tahu agent untuk membacanya dengan `read_file_range` bila perlu, agar tidak memboroskan token!).
2. **Cross-Workspace `@file` Attachments (`providers.rs:162`):**
   * Tambahkan `#[serde(default)] pub workspace: Option<String>` pada `AgentAttachment`.
   * Dukung sintaks `alias:path/to/file.ts` atau field `att.workspace` di `build_attachment_text_context`, dengan tetap menegakkan `resolve_jailed_path` dan `check_sensitive_guard` milik workspace target.

### 4.8 Tauri IPC Commands Baru (`src-tauri/src/main.rs`)

Daftarkan pada `tauri::generate_handler![...]`:

| Command | Parameter | Return | Fungsi |
| :--- | :--- | :--- | :--- |
| `add_workspace` | `path: String`, `alias: Option<String>`, `guardrail_mode: Option<String>`, `set_as_default: Option<bool>` | `Result<DashboardSnapshot, String>` | Menambah & mount workspace baru ke registry. |
| `update_workspace` | `id: String`, `alias: Option<String>`, `enabled: Option<bool>`, `guardrail_mode: Option<String>`, `allow_sensitive_reads: Option<bool>`, `extra_sensitive_patterns: Option<Vec<String>>`, `pinned: Option<bool>` | `Result<DashboardSnapshot, String>` | Mengubah alias, enable/disable, atau policy per-workspace. |
| `remove_workspace` | `id: String` | `Result<DashboardSnapshot, String>` | Menghapus workspace dari registry (tolak jika hanya tersisa 1 workspace). |
| `set_default_workspace` | `id_or_alias: String` | `Result<DashboardSnapshot, String>` | Mengubah active default workspace tanpa menghapus workspace lain yang ter-mount. |
| `open_in_editor` *(update)* | Tambah `workspace_alias: Option<String>` / `workspace_root: Option<String>` | `Result<String, String>` | Memastikan tombol "Open in VS Code / Explorer" di Diff Inspector membuka file pada repo asal audit entry. |

*Catatan Kompatibilitas `set_workspace_root(path)`*: Jika `path` sudah ada di `cfg.workspaces`, set sebagai `default_workspace_id` dan aktifkan (`enabled = true`); jika belum ada, otomatis panggil logika `add_workspace` dan jadikan default.

### 4.9 Kontrak TypeScript (`src/types.ts`)

```ts
export interface WorkspaceSummary {
  id: string;
  alias: string;
  path: string;
  enabled: boolean;
  is_default: boolean;
  exists: boolean;
  git_branch: string | null;
  guardrail_mode: GuardrailMode | null;
  effective_guardrail_mode: GuardrailMode;
  allow_sensitive_reads: boolean | null;
  pinned: boolean;
}
```
Tambahkan `default_workspace_id: string`, `workspaces: WorkspaceSummary[]`, dan `builtin_tools_count: number` ke `DashboardSnapshot`, serta `workspace_alias?: string | null` pada `AuditEntry`, `JobSummary`, dan `AgentAttachment`.

---

## 5. Spesifikasi UI / UX (Obsidian Studio — Vanilla TS)

1. **Topbar Workspace Pill (`index.html` + `src/views/workspace.ts`):**
   * Tampilkan alias default workspace + badge jumlah workspace ter-mount bila > 1 (contoh: `api` + pill `+2 mounted`).
   * Klik membuka popover **Workspace Manager** yang terbagi 2 bagian:
     * **Mounted Workspaces:** daftar workspace dengan badge alias (`@api`), status `default`, dropdown ringkas guardrail override (`Inherit` / `Read-Only` / `Safe Edit` / `Autonomous`), tombol toggle Active/Disabled, tombol "Set Default", tombol Copy Scoped MCP URL (`/mcp/<token>?workspace=<alias>`), dan tombol Remove.
     * **Add Workspace:** tombol folder picker (`@tauri-apps/plugin-dialog`) + input alias opsional.
2. **Audit List & Diff Inspector (`src/views/audit.ts`):**
   * Tampilkan badge kecil alias workspace (misal `[api]`) di samping nama tool pada daftar audit dan header Diff Inspector.
   * Kirim `entry.workspace_alias` saat klik `Open in VS Code` / `Open in Explorer`.
3. **Statusbar (`src/main.ts:63`):**
   * Ubah `const toolsTotal = 14 + (snap.aggregated_tools_count ?? 0);` menjadi `const toolsTotal = (snap.builtin_tools_count ?? 15) + (snap.aggregated_tools_count ?? 0);`.
4. **Command Palette (`src/main.ts`):**
   * Tambahkan aksi dinamis `Switch Default Workspace -> @<alias>` untuk setiap workspace ter-mount dan `Add Workspace Folder...`.

---

## 6. Pitfalls & Gotchas Kritis untuk Antigravity

Perhatikan 6 jebakan berikut agar implementasi tidak menghasilkan bug halus:

1. **Jangan Pegang `RwLock` Lintas `.await`:** Saat membaca `state.config.read().await` untuk meresolusi workspace, clone `WorkspaceProfile` dan `drop` guard sebelum melakukan `await` lain (seperti HITL approval atau disk I/O).
2. **Audit Revert Lintas-Workspace:** Jika `entry.workspace_root` sudah dihapus/unmounted dari registry, `revert_audit_entry` harus tetap memverifikasi bahwa path masih valid dan tidak boleh jatuh ke `state.workspace_root` yang salah. Jika root asli tidak ada lagi, kembalikan `Err` jelas.
3. **Konflik Alias Case-Insensitive di Windows:** Alias `"API"` dan `"api"` harus dianggap sama karena URL query & path Windows case-insensitive. Selalu normalisasi ke lowercase saat penyimpanan dan pencarian.
4. **Koneksi MCP Terkunci (`?workspace=api`) + `list_workspaces`:** Jika koneksi MCP dikunci ke `api`, `list_workspaces` hanya boleh menampilkan workspace `api` saja (jangan membocorkan path workspace lain yang ada di mesin user!).
5. **Global `read_only` Override Precedence:** Pastikan unit test memverifikasi: jika global `guardrail_mode == "read_only"` dan workspace `web` punya override `Some("autonomous")`, tool mutatif ke `web` **tetap ditolak**.
6. **Persistensi Chat Multi-Workspace (`chat_store.rs`):** Sesi chat di `chat_store.rs` saat ini di-hash berdasarkan `clean_workspace_string(state)`. Pertahankan perilaku ini untuk default workspace agar riwayat chat lama user tidak hilang; tambahkan catatan metadata workspace alias di tampilan UI bila perlu.

---

## 7. Roadmap Implementasi Bertahap

### Fase 1 — Core Registry & Resolver (Backend)
- [ ] Buat `src-tauri/src/workspace.rs` (`WorkspaceProfile`, `WorkspaceSummary`, `ResolvedWorkspace`, `sanitize_alias`, `ensure_unique_alias`, `resolve_workspace`).
- [ ] Daftarkan `mod workspace;` di `src-tauri/src/main.rs`.
- [ ] Tambahkan `workspaces` & `default_workspace_id` di `src-tauri/src/config.rs` + unit test backward-compat deserialization `config.json` lama.
- [ ] Implementasikan migrasi startup & sinkronisasi `default_workspace_id` di `src-tauri/src/state.rs`, plus field `workspace_*` di `AuditEntry`, `JobSummary`, dan `DashboardSnapshot`.

### Fase 2 — Tool Engine, Guardrail Per-Workspace & Safe Revert
- [ ] Tambahkan tool `list_workspaces` dan parameter `workspace` opsional pada 12 built-in tools di `src-tauri/src/tools.rs` (`mcp_tools_list`, `is_known_builtin_tool`, `is_tool_read_only`).
- [ ] Implementasikan `execute_tool_with_context`, HITL scoping `ws_id::tool`, per-workspace guardrail (dengan global `read_only` precedence), dan perbaikan bug urutan `.smart-tauri-hub/` di `check_sensitive_guard`.
- [ ] Perbarui `tools.rs` dan `tools_extra.rs` agar mengisi `workspace_id/alias/root` pada semua `AuditEntry` dan `ManagedJob`.
- [ ] Perbarui `revert_audit_entry` agar selalu menggunakan root asal entri audit.

### Fase 3 — MCP HTTP Pinning, Resources & Local Agent
- [ ] Perbarui `TokenQuery`, CORS `x-airlock-workspace`, dan `handle_single_rpc_with_context` di `src-tauri/src/mcp_server.rs`.
- [ ] Tambahkan `workspace://list` dan URI ber-alias `workspace://{alias}/...` di `src-tauri/src/mcp_resources.rs`.
- [ ] Perbarui system prompt multi-workspace di `local_agent.rs` dan attachment `@alias:path` di `providers.rs`.
- [ ] Tambahkan IPC commands (`add_workspace`, `update_workspace`, `remove_workspace`, `set_default_workspace`) & update `open_in_editor` di `src-tauri/src/main.rs`.

### Fase 4 — Frontend UI & Verifikasi End-to-End
- [ ] Perbarui `src/types.ts` dan `src/main.ts` (`builtin_tools_count`, Command Palette workspace switcher).
- [ ] Upgrade `index.html`, `src/styles.css`, `src/views/workspace.ts`, dan `src/views/audit.ts` dengan Workspace Manager popover + badge alias.
- [ ] Jalankan seluruh test suite & verifikasi manual.

---

## 8. Rencana Pengujian & Definition of Done

### Automated Tests Wajib (`cargo test`)
1. `workspace::tests::test_alias_sanitization_and_uniqueness`
2. `workspace::tests::test_resolve_workspace_precedence_and_pin_isolation` (verifikasi koneksi yang di-pin ke `api` menolak `workspace: "web"`).
3. `tools::tests::test_multi_workspace_cross_repo_read_and_scoped_revert` (edit file di workspace `B` saat default adalah `A`, ganti default ke `A`, jalankan `revert_audit_entry` → file di `B` kembali asli dan `A` tidak tersentuh).
4. `tools::tests::test_per_workspace_guardrail_and_global_read_only_override` (workspace `web` set `autonomous` + global `read_only` → tetap diblokir; global `safe_edit` + workspace `api` set `read_only` → `api` diblokir, `web` lolos).
5. `tools::tests::test_hitl_session_approval_scoped_per_workspace` (approve session tool di `ws_a` tetap memicu HITL di `ws_b`).
6. `mcp_server::tests::test_mcp_http_pinned_workspace_query_and_header` (E2E POST `/mcp?workspace=api` dan header `X-Airlock-Workspace`).
7. `mcp_resources::tests::test_aliased_resource_uris` (`workspace://list`, `workspace://{alias}/file/...`).

### Verifikasi Build & Runtime
```powershell
cd src-tauri
cargo test
cd ..
npm run build
```
**Definition of Done:** Seluruh test Rust lama & baru lulus 100%, `npm run build` (termasuk `tsc` strict) lulus tanpa error/warning, dan file `config.json` lama dari versi sebelumnya berhasil bermigrasi otomatis tanpa kehilangan pengaturan.
