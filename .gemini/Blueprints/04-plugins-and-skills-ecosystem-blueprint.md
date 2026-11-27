# Blueprint 04 — Agent Plugins 1.0.0, Progressive `SKILL.md` Engine & Skills over MCP (`SEP-2640`)

| Atribut | Detail |
| :--- | :--- |
| **Target Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Target Executor** | Google Antigravity Agent (Autonomous / Co-pilot Handoff) |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Status** | Ready for Implementation |
| **Prioritas** | P2 — Ekstensibilitas Ekosistem Terbuka & Anti-Tool-Bloat |
| **Dependensi Blueprint** | Independen; selaras penuh dengan Blueprint 01 (per-workspace skills) dan Blueprint 02 (deterministic tool sorting untuk caching) |

---

## 1. Handoff Brief untuk Agent Antigravity

Dokumen ini merancang sistem **Skills & Plugins** standar industri untuk Airlock dengan tiga lapisan terintegrasi:
1. **Progressive Disclosure Agent Skills (`SKILL.md`):** Mendukung standar terbuka folder `SKILL.md` (+ `references/`, `scripts/`, `assets/`) dari 3 sumber (Workspace, Plugin, Global). Local Agent hanya menyuntikkan katalog ringkas (`name` + `description`, ~25–40 token/skill) ke system prompt, dan memuat instruksi penuh hanya saat dibutuhkan melalui tool `load_skill` & `read_skill_resource`.
2. **Agent Plugins 1.0.0 Package Manager:** Mendukung paket plugin standar (`plugin.json` + `mcp.json` + `skills/`) yang dapat diinstal dari folder lokal atau Git URL dengan **1 klik Enable/Disable** yang serentak mengelola downstream MCP server (`mcp_hub.rs`) + skills + secrets terenkripsi DPAPI (`secrets.rs`).
3. **Skills over MCP (`SEP-2640`):** Menjadikan Airlock bukan hanya *konsumen* skill di Local Agent, melainkan juga **MCP Skills Provider** (`skills/list`, `skills/get`, `skill://{name}` resources, plus built-in fallback tools) sehingga klien eksternal (VS Code, Gemini Web, Claude, Antigravity, Cursor) dapat menarik skill lokal/tim secara hemat token lewat Airlock.

### Invarian Arsitektur Airlock yang WAJIB Dipatuhi
1. **Zero Secret Leakage ke `config.json`:** Placeholder `${SECRET:NAME}` di `mcp.json` plugin **wajib** disimpan di `SecretsStore` (Windows DPAPI `secrets.dpapi`), bukan plaintext di `config.json`.
2. **Directory Jail untuk Skill Resources:** Pembacaan `references/` atau `assets/` di dalam folder skill **wajib** divalidasi dengan `security::resolve_jailed_path(&skill_dir, rel_path)` sehingga `read_skill_resource("my-skill", "../../secrets.dpapi")` atau symlink keluar folder skill selalu ditolak.
3. **Never Auto-Execute Skill Scripts:** File di `skills/<name>/scripts/` bersifat pasif. Jika LLM ingin menjalankan skrip, ia harus memakai `run_command_compressed` yang tetap tunduk pada Guardrail Mode, Command Guard, dan HITL approval.
4. **Security Review Gate Sebelum Aktivasi Plugin:** Plugin baru dari Git URL atau folder eksternal **wajib** dipasang dalam status `enabled: false` (atau dikonfirmasi melalui modal review) sampai pengguna meninjau command stdio, endpoint HTTP, dan environment variables yang diminta `mcp.json`.
5. **Zero External UI Framework & XSS-Safe DOM:** Seluruh tampilan Plugins & Skills di View 3 (`#panel-hub`) dibangun dengan Vanilla TypeScript + Obsidian Studio CSS tanpa `innerHTML` dinamis.

---

## 2. Analisis Kondisi Saat Ini (As-Is)

| Komponen | Lokasi Eksisting | Kondisi & Gap Saat Ini |
| :--- | :--- | :--- |
| **Project Rules Injection** | `src-tauri/src/providers.rs:57-94` | `load_project_rules_snippet` menyuntikkan seluruh isi `AGENTS.md`/`CLAUDE.md` hingga 6.000 karakter secara eager di setiap turn — belum mendukung *progressive disclosure* berbasis `SKILL.md`. |
| **Downstream MCP Hub** | `src-tauri/src/mcp_hub.rs`, `config.rs:58-75` | `DownstreamMcpConfig` didaftarkan manual satu per satu di UI; tidak memiliki konsep paket (`plugin_id`), tidak mendukung resolve placeholder `${SECRET:...}`, dan tidak bundling skills. |
| **DPAPI Secrets Store** | `src-tauri/src/secrets.rs:7-14` | `ProviderSecrets` hanya punya 3 field (`gemini_api_key`, `openai_api_key`, `anthropic_api_key`) — belum punya map untuk plugin/MCP secrets. |
| **MCP Capabilities** | `src-tauri/src/mcp_server.rs:273-277` | Hanya mengiklankan `tools`, `resources`, `prompts`; belum mengiklankan ekstensi `io.modelcontextprotocol/skills` atau metode `skills/list` & `skills/get`. |
| **Hub & Tools UI** | `index.html:682-760`, `src/views/bridge.ts:388-490` | Hanya berisi tabel `Downstream MCP Aggregator` dan `Built-in Smart Tool Simulator`. |

---

## 3. Arsitektur Target (To-Be)

### 3.1 Struktur Direktori Standar (Disk Layout)

```text
~/.smart-tauri-hub/
├── config.json                  # AppConfig (+ disabled_skills, installed_plugins metadata)
├── secrets.dpapi                # DPAPI-encrypted ProviderSecrets (+ plugin_secrets map)
├── skills/                      # User Global Skills
│   └── rust-audit/
│       ├── SKILL.md
│       └── references/
│           └── checklist.md
└── plugins/                     # Installed Agent Plugins 1.0.0
    └── plg_playwright_1a2b/
        ├── plugin.json          # Metadata standar Agent Plugins 1.0.0
        ├── mcp.json             # Definisi downstream MCP (stdio / streamable-http)
        ├── .airlock-meta.json   # Source URL, installed_at, sha256 manifest, enabled
        └── skills/
            └── browser-qa/
                ├── SKILL.md
                └── references/

<workspace>/                     # Per-Project Skills (auto-discovered, tertinggi prioritas)
├── .airlock/skills/<name>/SKILL.md
├── .agents/skills/<name>/SKILL.md
└── .claude/skills/<name>/SKILL.md
```

### 3.2 Diagram Arsitektur & Alur Progressive Disclosure

```text
  [Workspace Skills]   [Plugin Skills]   [Global Skills]
          \                   |                 /
           v                  v                v
     +---------------------------------------------------+
     |        SkillRegistry (src-tauri/src/skills.rs)    |
     |  - Precedence: Workspace > Plugin > Global        |
     |  - Parse YAML frontmatter (name, description)     |
     |  - Jailed resource reader (references/, assets/)  |
     +-------------------+-------------------------------+
                         |
           +-------------+-------------+
           |                           |
           v                           v
  +-----------------------+   +-------------------------------+
  | Local Agent Prompt    |   | MCP Server (SEP-2640 + Tools) |
  | Tier 1: Catalog only  |   | - RPC: skills/list, skills/get|
  | (~30 tok/skill)       |   | - URI: skill://{name}/{path}  |
  | Tier 2: load_skill()  |   | - Tools: list_skills,         |
  | Tier 3: read_skill_   |   |   load_skill,                 |
  |         resource()    |   |   read_skill_resource         |
  +-----------------------+   +-------------------------------+
```

---

## 4. Kontrak Data & Desain Implementasi Detail

### 4.1 Modul Baru: `src-tauri/src/skills.rs`

#### 4.1.1 Format Standar `SKILL.md` yang Didukung
```markdown
---
name: tauri-security-audit
description: Audit Tauri v2 IPC commands, CSP configuration, and filesystem jail boundaries for security regressions.
license: MIT
compatibility: airlock >= 0.1
allowed-tools: project_overview smart_search read_file_range code_outline
---

# Tauri v2 Security Audit Instructions
...
```

**Parser Frontmatter Tanpa Bloat Dependensi:**
Tulis parser frontmatter yang bersih dan deterministik di `skills.rs` (`parse_skill_markdown(raw: &str) -> Result<ParsedSkill, String>`):
* Wajib dimulai dengan baris `---` dan ditutup dengan baris `---` (dukung `\r\n` dan `\n`!).
* Parse pasangan `key: value` top-level (mendukung single/double quoted strings, block scalar sederhana `|`/`>` untuk `description` multi-baris, serta sub-block `metadata:` key-value terindentasi).
* **Validasi Nama Skill (Standar Agent Skills):**
  * Panjang 1–64 karakter, hanya `[a-z0-9-]`, tidak diawali/diakhiri `-`, tidak mengandung `--`.
  * Jika frontmatter tidak ada atau `name` kosong, fallback ke nama folder yang disanitasi hanya jika `description` ada; bila file korup, catat warning di `SkillSummary.parse_error` (jangan crash seluruh registry!).
* Hitung estimasi token: `catalog_tokens = (name.len() + description.len()) / 4` dan `body_tokens = body.len() / 4` agar UI dapat menampilkan efisiensi penghematan token Progressive Disclosure!

#### 4.1.2 Struct Data `skills.rs`
```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Workspace,
    Plugin,
    Global,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub scope: SkillScope,
    pub source_label: String,       // e.g. "workspace:api", "plugin:playwright", "global"
    pub plugin_id: Option<String>,
    pub enabled: bool,
    pub dir_path: String,           // Clean path tanpa \\?\
    pub resource_files: Vec<String>,// Relative paths di references/, scripts/, assets/
    pub allowed_tools: Vec<String>,
    pub catalog_tokens_est: usize,
    pub body_tokens_est: usize,
    pub parse_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub summary: SkillSummary,
    pub canon_dir: PathBuf,
    pub body_md: String,
    pub raw_md: String,
}
```

#### 4.1.3 Discovery & Precedence Rules
`pub fn discover_skills(workspace_roots: &[(String, PathBuf)], plugins: &[InstalledPluginRecord], global_skills_dir: &Path, disabled_skills: &HashSet<String>) -> Vec<SkillEntry>`:
1. Pindai dengan urutan prioritas **tertinggi → terendah**:
   * **Workspace:** untuk tiap workspace aktif, pindai `.airlock/skills/*`, `.agents/skills/*`, `.claude/skills/*` (hanya kedalaman 1 folder yang berisi `SKILL.md`).
   * **Plugin:** untuk tiap plugin yang `enabled == true`, pindai `<plugin_dir>/skills/*/SKILL.md`.
   * **Global:** pindai `~/.smart-tauri-hub/skills/*/SKILL.md`.
2. **Deduplikasi Deterministik:** Gunakan `BTreeMap<String, SkillEntry>` berdasarkan `name` (lowercase). Entri prioritas lebih tinggi menimpa yang lebih rendah (misalnya proyek dapat meng-override skill global bernama sama). Karena memakai `BTreeMap`, urutan katalog selalu alfabetis dan **100% deterministik** (menjaga implicit prompt caching Gemini di Blueprint 02!).
3. **Batas Keamanan Scan:** Maksimal 100 skill total; ukuran `SKILL.md` maks 256 KB; resource file maks 512 KB; abaikan symlink yang keluar dari root direktori skill.

#### 4.1.4 Built-in Tools untuk Skills (3 Tool Read-Only)
Tambahkan ke `mcp_tools_list()` (`tools.rs`), `is_known_builtin_tool`, dan `is_tool_read_only`:
1. **`list_skills`**: mengembalikan daftar skill aktif (`name`, `description`, `scope`, jumlah `resources`).
2. **`load_skill`** (`name: string`): mengembalikan isi lengkap `SKILL.md` + daftar file di `references/` dan `scripts/` (sehingga LLM tahu file referensi apa yang tersedia untuk dibaca dengan `read_skill_resource`).
3. **`read_skill_resource`** (`name: string`, `path: string`): memvalidasi `resolve_jailed_path(&skill.canon_dir, path)` + cek bukan biner/terlalu besar (>128 KB), lalu mengembalikan teks file referensi tersebut.

#### 4.1.5 Integrasi ke Local Agent System Prompt (`local_agent.rs:334`)
Jika ada >= 1 skill aktif, tambahkan blok ringkas ke `system_prompt`:
```text
### Available Skills (Progressive Disclosure)
Before solving specialized tasks, check if a skill below applies and call `load_skill(name)` to load its full instructions:
- `tauri-security-audit`: Audit Tauri v2 IPC commands, CSP configuration, and filesystem jail boundaries...
- `browser-qa`: Run visual and DOM verification using Playwright MCP tools...
```
*(Batas keamanan token: jika user memasang >40 skill, batasi deskripsi tiap skill maks 140 karakter sehingga blok katalog tidak pernah melebihi ~1.500 token.)*

---

### 4.2 Modul Baru: `src-tauri/src/plugins.rs` (Agent Plugins 1.0.0)

#### 4.2.1 Skema Manifest Standar `plugin.json` & `mcp.json`
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default = "default_version")]
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
    pub transport: Option<String>,       // "stdio" | "http" (inferred jika tidak diisi)
    #[serde(default)]
    pub url: Option<String>,             // untuk HTTP / Streamable-HTTP
    #[serde(default)]
    pub headers: HashMap<String, String>,// dukung Authorization: Bearer ${SECRET:...}
    #[serde(default)]
    pub command: Option<String>,         // untuk stdio
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,    // dukung "${SECRET:GITHUB_TOKEN}"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMcpFile {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, PluginMcpServerDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPluginRecord {
    pub id: String,                      // "plg_<sanitized_name>_<6hex>"
    pub name: String,
    pub version: String,
    pub description: String,
    pub source: String,                  // "local:<path>" | "git:<url>"
    pub install_dir: String,             // Clean path di ~/.smart-tauri-hub/plugins/<id>
    pub enabled: bool,
    pub trusted: bool,                   // Wajib true setelah Security Review Gate
    pub manifest_sha256: String,         // Hash gabungan plugin.json + mcp.json
    pub mcp_server_ids: Vec<String>,     // ID downstream MCP yang dibuat plugin ini
    pub required_secrets: Vec<String>,   // Daftar key ${SECRET:XYZ} yang terdeteksi
    pub installed_at: String,
}
```

#### 4.2.2 Secret Interpolation & Ekstensi `SecretsStore` (`src-tauri/src/secrets.rs`)
1. Perluas `ProviderSecrets` di `secrets.rs:7` dengan field backward-compatible:
   ```rust
   #[serde(default)]
   pub plugin_secrets: HashMap<String, HashMap<String, String>>, // plugin_id -> (key -> secret_value)
   ```
2. Helper `extract_secret_placeholders(mcp: &PluginMcpFile) -> Vec<String>` mendeteksi pola `${SECRET:KEY_NAME}` di `env` dan `headers`.
3. Helper `materialize_plugin_mcp_configs(plugin, secrets) -> Vec<DownstreamMcpConfig>`:
   * Mengonversi setiap entri di `mcp.json` menjadi `DownstreamMcpConfig` dengan:
     * `id: format!("{}::{}", plugin.id, server_key)`
     * `namespace: sanitize_namespace(&format!("{}_{}", plugin.name, server_key))` (atau `server_key` jika plugin hanya punya 1 server dan tidak bentrok)
     * `plugin_id: Some(plugin.id.clone())` (tambahkan `#[serde(default)] pub plugin_id: Option<String>` pada `DownstreamMcpConfig` dan `DownstreamMcpSummary`).
     * Interpolasi `${SECRET:KEY_NAME}` dilakukan **hanya di runtime RAM** tepat sebelum diteruskan ke `McpHubManager` — nilai secret **tidak pernah** diserialisasi ke `config.json`!

#### 4.2.3 Alur Instalasi Plugin Aman (2 Tahap: Preview → Install & Trust)
1. **Tahap 1 — `inspect_plugin_source(source_type: "local"|"git", location: String, subdir: Option<String>) -> PluginInstallPreview`:**
   * Jika `"local"`: validasi folder berisi minimal `plugin.json` atau `mcp.json` atau `skills/*/SKILL.md`.
   * Jika `"git"`: clone ke direktori staging sementara (`~/.smart-tauri-hub/plugins/.staging/<rand>`) menggunakan `Command::new("git").args(["clone", "--depth", "1", "--", &url, &staging_dir])` **tanpa shell** (validasi URL hanya izinkan `https://` atau `ssh://`/`git@`, tolak argumen berawalan `-` untuk mencegah git option injection!).
   * Parse `plugin.json`, `mcp.json`, dan scan `skills/*/SKILL.md`.
   * Kembalikan `PluginInstallPreview` ke UI: daftar command stdio yang akan dijalankan, daftar URL HTTP eksternal, daftar `required_secrets`, dan daftar skills.
2. **Tahap 2 — `confirm_install_plugin(staging_token, enable_immediately: bool, secrets: HashMap<String, String>) -> DashboardSnapshot`:**
   * Pindahkan/salin staging ke `~/.smart-tauri-hub/plugins/<id>/` (tanpa menyalin `.git/` agar hemat disk dan aman).
   * Simpan `secrets` ke `SecretsStore` (DPAPI).
   * Simpan `InstalledPluginRecord` ke `AppConfig.installed_plugins`.
   * Jika `enable_immediately == true`, panggil `mcp_hub.refresh_server(...)` untuk semua MCP server milik plugin, refresh `SkillRegistry`, dan emit dashboard snapshot!
3. **Toggle & Uninstall:**
   * `toggle_plugin(plugin_id, enabled)`: mengaktifkan/mematikan semua MCP server plugin (`mcp_hub.remove_server` saat disable untuk langsung membunuh proses stdio!) + memperbarui `SkillRegistry`.
   * `uninstall_plugin(plugin_id)`: stop & remove seluruh MCP server plugin, hapus secret `plugin_secrets.remove(&plugin_id)`, hapus folder `~/.smart-tauri-hub/plugins/<id>/`, dan simpan config.

---

### 4.3 Skills over MCP (`SEP-2640`) pada `mcp_server.rs` & `mcp_resources.rs`

1. **Capabilities Negotiation (`mcp_server.rs:273` & `306`):**
   ```json
   "capabilities": {
     "tools": { "listChanged": true },
     "resources": { "subscribe": false, "listChanged": true },
     "prompts": { "listChanged": true },
     "experimental": {
       "io.modelcontextprotocol/skills": { "listChanged": true }
     }
   }
   ```
2. **Metode JSON-RPC Baru di `handle_single_rpc` (`mcp_server.rs:262`):**
   * `"skills/list"`:
     ```json
     {
       "skills": [
         {
           "name": "tauri-security-audit",
           "description": "...",
           "uri": "skill://tauri-security-audit",
           "resources": ["skill://tauri-security-audit/references/checklist.md"]
         }
       ],
       "_meta": { "cacheControl": { "type": "ephemeral" } }
     }
     ```
   * `"skills/get"` (param wajib: `name: string`):
     ```json
     {
       "name": "tauri-security-audit",
       "description": "...",
       "uri": "skill://tauri-security-audit",
       "content": "<isi SKILL.md>",
       "resources": [
         { "path": "references/checklist.md", "uri": "skill://tauri-security-audit/references/checklist.md" }
       ]
     }
     ```
   * Sertakan `"skills": ...` pula di respons `"server/discover"` (`mcp_server.rs:290`)!
3. **Resource Templates Baru di `mcp_resources.rs`:**
   * `skill://{name}` → membaca `SKILL.md` dari `SkillRegistry` (hanya skill yang `enabled == true`).
   * `skill://{name}/{rel_path}` → membaca resource file di dalam folder skill dengan `resolve_jailed_path`.

---

## 5. Spesifikasi UI / UX (`index.html`, `src/views/plugins.ts`, `src/views/bridge.ts`)

Tambahkan 2 kartu baru pada **View 3 (`#panel-hub` — `Hub & Smart Tools`)** di atas atau berdampingan dengan tabel Downstream MCP Aggregator, serta modul TS baru `src/views/plugins.ts`:

1. **Card: Installed Plugins (`Agent Plugins 1.0.0`):**
   * Header dengan tombol primer **`+ Install Plugin`**.
   * Tabel/daftar plugin: `Name & Version`, `Source`, `MCP Servers (N)`, `Skills (N)`, `Secrets Status` (hijau jika lengkap, kuning `"Missing 1 secret"` jika ada placeholder belum diisi), toggle `Enabled`, tombol `Secrets`, tombol `Uninstall`.
   * **Modal `+ Install Plugin` (2-Step Security Review):**
     * Step 1: Pilih tab `Local Folder` (pakai `@tauri-apps/plugin-dialog`) atau `Git URL` (+ opsional `Subfolder`), klik **`Inspect Plugin`**.
     * Step 2 (Security Review): Tampilkan kotak peringatan jelas yang mencantumkan setiap command stdio (`npx ...`, `python ...`), URL HTTP eksternal, form input untuk setiap `${SECRET:...}` yang dibutuhkan, dan daftar skill yang dibundel. Tombol **`Trust & Install`**.
2. **Card: Agent Skills Catalog (`SKILL.md`):**
   * Filter bar: `All` | `Workspace` | `Plugin` | `Global` + tombol **`+ New Global Skill`** (membuat scaffold `~/.smart-tauri-hub/skills/<name>/SKILL.md` lalu membuka di VS Code/preview) + tombol **`Rescan Skills`**.
   * Daftar skill menampilkan: nama monospace, badge scope (`Workspace` / `Plugin` / `Global`), deskripsi 1 baris, badge token (`~32 tok catalog / ~840 tok full`), toggle `Enabled`, dan tombol **`Inspect SKILL.md`** (modal preview menggunakan `renderMarkdownInto`).
3. **Indikator di `Downstream MCP Aggregator` (`src/views/bridge.ts:388`):**
   * Baris downstream MCP yang dikelola plugin menampilkan badge kecil `Plugin` dan tombol `Remove` dinonaktifkan dengan tooltip `"Dikelola oleh plugin — hapus lewat panel Plugins"`.

---

## 6. Tauri IPC Commands Baru (`src-tauri/src/main.rs`)

| Command | Parameter | Return |
| :--- | :--- | :--- |
| `list_skills` | — | `Result<Vec<SkillSummary>, String>` |
| `get_skill_detail` | `name: String` | `Result<SkillDetail, String>` (summary + full `body_md`) |
| `toggle_skill_enabled` | `name: String`, `enabled: bool` | `Result<DashboardSnapshot, String>` |
| `create_global_skill` | `name: String`, `description: String`, `body_md: String` | `Result<DashboardSnapshot, String>` |
| `rescan_skills` | — | `Result<DashboardSnapshot, String>` |
| `inspect_plugin_source` | `source_type: String`, `location: String`, `subdir: Option<String>` | `Result<PluginInstallPreview, String>` |
| `confirm_install_plugin` | `staging_id: String`, `enable_immediately: bool`, `secrets: HashMap<String, String>` | `Result<DashboardSnapshot, String>` |
| `toggle_plugin` | `plugin_id: String`, `enabled: bool` | `Result<DashboardSnapshot, String>` |
| `update_plugin_secrets` | `plugin_id: String`, `secrets: HashMap<String, String>` | `Result<DashboardSnapshot, String>` |
| `uninstall_plugin` | `plugin_id: String` | `Result<DashboardSnapshot, String>` |

---

## 7. Pitfalls & Gotchas Kritis untuk Antigravity

1. **Git Option Injection pada `inspect_plugin_source`:**
   User bisa menempel string berbahaya seperti `--upload-pack=...` sebagai URL Git. Wajib validasi bahwa `location.trim()` **dimulai** dengan `https://` (atau `http://127.0.0.1`/`localhost` untuk testing) dan **selalu** sisipkan argumen `"--"` sebelum URL saat memanggil `Command::new("git")`.
2. **Jangan Tulis Secret Hasil Interpolasi ke `config.json`:**
   `AppConfig.downstream_mcps` diserialisasi ke `config.json`. Oleh karena itu, **jangan** masukkan MCP server milik plugin (yang sudah berisi secret hasil interpolasi!) langsung ke `AppConfig.downstream_mcps`. Simpan plugin di `AppConfig.installed_plugins`, lalu bangun daftar gabungan `effective_downstream_mcps(state)` secara dinamis di memori (`manual downstream_mcps` + `materialized plugin MCPs` dengan secret dari `provider_secrets.plugin_secrets`). Ini menjamin `config.json` tetap 100% bersih dari secret plugin!
3. **Konflik Namespace Antar Plugin & Manual MCP:**
   Jika 2 plugin sama-sama mendefinisikan server bernama `"github"`, gunakan fallback namespace `<plugin_slug>_<server_key>` (disanitasi dengan `mcp_hub::sanitize_namespace`) agar tidak menimpa tool milik server lain.
4. **Cleanup Folder Staging Sementara:**
   Jika user mengklik `Inspect Plugin` lalu menutup modal tanpa menginstal, hapus folder `.staging/<staging_id>` saat modal ditutup atau bersihkan staging yang berumur >1 jam pada saat startup aplikasi.
5. **Validasi Path `read_skill_resource` pada Windows:**
   Tolak path absolut (`C:\...`, `\\server\...`), tolak komponen `..`, dan panggil `security::resolve_jailed_path(&skill.canon_dir, rel_path)` agar tidak mungkin keluar dari direktori skill.
6. **Backward Compatibility `secrets.dpapi`:**
   Pastikan `#[serde(default)]` terpasang pada `ProviderSecrets.plugin_secrets` di `secrets.rs` sehingga file `secrets.dpapi` eksisting milik user tetap bisa didekripsi tanpa kehilangan API key Gemini/OpenAI/Anthropic!

---

## 8. Roadmap Implementasi Bertahap

### Fase 1 — Progressive `SKILL.md` Engine & Built-in Skill Tools
- [ ] Buat `src-tauri/src/skills.rs` (frontmatter parser, validasi nama, 3-tier discovery, jailed resource reader, unit tests).
- [ ] Tambahkan `disabled_skills: Vec<String>` ke `AppConfig` dan `skills: Vec<SkillSummary>` ke `DashboardSnapshot`.
- [ ] Tambahkan 3 built-in tools (`list_skills`, `load_skill`, `read_skill_resource`) di `tools.rs` dan injeksi katalog Tier 1 di `local_agent.rs`.
- [ ] Ekspos `skills/list`, `skills/get`, capability `io.modelcontextprotocol/skills`, dan resource `skill://{name}/{rel_path}` di `mcp_server.rs` & `mcp_resources.rs`.

### Fase 2 — Agent Plugins 1.0.0 Manager & DPAPI Plugin Secrets
- [ ] Perluas `ProviderSecrets` di `secrets.rs` dengan `plugin_secrets` (tetap backward-compatible).
- [ ] Buat `src-tauri/src/plugins.rs` (manifest parser `plugin.json`/`mcp.json`, `${SECRET:...}` extractor & runtime interpolator, staging inspector untuk Local & Git, install/toggle/uninstall).
- [ ] Hubungkan `effective_downstream_mcps(state)` antara `plugins.rs` dan `mcp_hub.rs` tanpa menulis secret ke `config.json`.
- [ ] Daftarkan seluruh IPC commands Skills & Plugins di `src-tauri/src/main.rs`.

### Fase 3 — Frontend UI (`Hub & Smart Tools`) & End-to-End Verification
- [ ] Perbarui `src/types.ts` dan buat `src/views/plugins.ts` (Plugins table, 2-step Security Review Install modal, Secrets modal, Skills Catalog + Preview & Create modal).
- [ ] Integrasikan ke `index.html`, `src/styles.css`, `src/main.ts`, dan `src/views/bridge.ts`.
- [ ] Jalankan seluruh automated tests & verifikasi manual.

---

## 9. Rencana Pengujian & Definition of Done

### Automated Tests Wajib (`cargo test`)
1. `skills::tests::test_frontmatter_parsing_crlf_and_validation` (uji frontmatter valid, multi-baris, CRLF Windows, dan fallback file korup).
2. `skills::tests::test_discovery_precedence_workspace_overrides_plugin_and_global` (skill bernama sama di workspace mengalahkan plugin & global, urutan output deterministik).
3. `skills::tests::test_read_skill_resource_blocks_traversal_and_absolute_paths` (`../`, `C:\Windows\...`, symlink luar ditolak).
4. `plugins::tests::test_plugin_mcp_secret_interpolation_never_leaks_to_config_json` (verifikasi `${SECRET:API_KEY}` ter-resolve di runtime `DownstreamMcpConfig`, tetapi `config.json` tidak mengandung nilai secret).
5. `plugins::tests::test_git_url_validation_blocks_option_injection` (tolak `--upload-pack=...`, `file://...`, dsb).
6. `plugins::tests::test_plugin_install_toggle_and_uninstall_lifecycle` (install dari folder temp → tools & skill muncul → disable → tools & skill hilang → uninstall → folder & secret bersih).
7. `mcp_server::tests::test_mcp_skills_over_mcp_rpc_and_resource_read` (E2E POST `skills/list`, `skills/get`, dan `resources/read` untuk `skill://<name>`).

### Verifikasi Build & Runtime
```powershell
cd src-tauri
cargo test
cd ..
npm run build
```
**Definition of Done:** Airlock mampu menginstal paket **Agent Plugins 1.0.0** (folder lokal atau Git URL) dengan review keamanan 2 tahap, memuat skill `SKILL.md` secara *progressive disclosure* tanpa membebani token system prompt, menyimpan secret plugin terenkripsi DPAPI, serta melayani `skills/list` & `skill://` lewat MCP server.
