# Blueprint 05 — MCP 2026 (`Tasks` & `Elicitation`), Agent Client Protocol (`--acp`) & Semantic Code Intelligence (`Tree-sitter` + `LSP`)

| Atribut | Detail |
| :--- | :--- |
| **Target Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Target Executor** | Google Antigravity Agent (Autonomous / Co-pilot Handoff) |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Status** | Ready for Implementation |
| **Prioritas** | P3 — Standar Protokol 2026, Integrasi Editor Native & Kecerdasan Kode Semantik |
| **Dependensi Blueprint** | Memanfaatkan `JobManager` + `watch` notifier (Blueprint 02), `WorkspaceResolver` (Blueprint 01), dan `AgentRunManager` (Blueprint 03) |

---

## 1. Handoff Brief untuk Agent Antigravity

Dokumen ini membawa Airlock ke garis depan ekosistem AI developer tools 2026 melalui tiga kapabilitas strategis:
1. **MCP 2026 Primitives (`Tasks` & `Elicitation`):**
   * Membungkus `JobManager` (`jobs.rs`) ke dalam primitif resmi **`MCP Tasks`** (`tasks/get`, `tasks/result`, `tasks/list`, `tasks/cancel`, dan `tools/call` dengan parameter `task`) sehingga eksekusi berdurasi panjang kompatibel penuh dengan spesifikasi MCP terbaru.
   * Mengimplementasikan **`MCP Elicitation`** secara dua arah: (a) *Downstream Bridge* — saat downstream MCP server di `mcp_hub.rs` mengirim `elicitation/create`, Airlock merender form modal terstruktur native di desktop UI; (b) *Server-Side Elicitation* — pada klien `--stdio` atau Streamable HTTP SSE yang mendukung `capabilities.elicitation`.
2. **Agent Client Protocol (`tauri-mcp-hub.exe --acp`):**
   * Menambahkan mode CLI `--acp` (sejajar dengan `--stdio` di `main.rs:770`) agar **Airlock Local Coding Agent** (dengan Multi-Provider Gemini/Claude/Ollama, Guardrails, Skills, dan Artifacts) dapat dijalankan **native di dalam Zed, JetBrains IDE, Neovim, dan editor kompatibel ACP lainnya** tanpa membangun editor sendiri.
3. **Semantic Code Intelligence (`Tree-sitter` AST + On-Demand `LSP` Bridge):**
   * Mengganti `code_outline` berbasis regex (`tools_extra.rs:187-201`) dengan parser AST **`tree-sitter`** (dengan fallback regex untuk ekstensi lain) + menambahkan tool **`read_symbol`** agar LLM dapat membaca tepat satu fungsi/struct/class beserta rentang barisnya tanpa menebak `start_line`/`end_line`.
   * Menambahkan **`LspBridge`** lazy-spawn dengan auto-shutdown idle (5 menit) untuk `rust-analyzer`, `typescript-language-server`, dan `pyright-langserver`, mengekspos `lsp_diagnostics`, `lsp_definition`, `lsp_references`, dan `lsp_hover`.

### Invarian Arsitektur Airlock yang WAJIB Dipatuhi
1. **Kompatibilitas Toolchain Windows GNU (`x86_64-pc-windows-gnu` + LLVM-MinGW):**
   Periksa `src-tauri/.cargo/config.toml.example`: proyek ini dikompilasi memakai LLVM-MinGW (`gcc.exe` / `clang++.exe`). Semua crate `tree-sitter` dan grammar **wajib** memakai versi ABI yang sinkron dan diuji kompilasinya di `cargo test` Windows GNU; gunakan Cargo feature flag `semantic-ast` (default enabled) agar bisa di-isolate jika terjadi konflik linker.
2. **Stdout 100% Murni JSON-RPC pada `--acp` dan `--stdio`:**
   Saat berjalan dalam mode `--acp`, **dilarang keras** memanggil `println!` atau menulis log debug ke `stdout`. Semua log diagnostik wajib dikirim ke `stderr` (`eprintln!`). Satu byte non-JSON di `stdout` akan memutus koneksi editor Zed/JetBrains!
3. **Zero Idle Bloat untuk LSP:**
   Language server seperti `rust-analyzer` dapat memakan 500 MB–1,5 GB RAM. `LspBridge` **tidak boleh** di-spawn saat startup aplikasi. Hanya spawn *on-demand* saat tool `lsp_*` pertama kali dipanggil, batasi maks 2 LSP aktif bersamaan, dan matikan otomatis (`kill_pid_tree`) setelah idle 5 menit serta saat aplikasi quit!
4. **Backward Compatibility MCP Stateless POST:**
   Klien lama (termasuk Gemini Web stateless POST) tidak boleh rusak oleh penambahan `Tasks` atau `Elicitation`. Semua perilaku baru bersifat *capability-negotiated* saat `initialize`.

---

## 2. Analisis Kondisi Saat Ini (As-Is)

| Komponen | Lokasi Eksisting | Keterbatasan Saat Ini |
| :--- | :--- | :--- |
| **Long-Running Tools** | `src-tauri/src/jobs.rs`, `tools_extra.rs:587-688` | Menggunakan 3 custom tools (`start_command_job`, `poll_command_job`, `stop_command_job`); belum mengekspos metode standar MCP `tasks/*`. |
| **MCP HTTP GET/SSE** | `src-tauri/src/mcp_server.rs:183-205` | `GET /mcp` dengan `Accept: text/event-stream` mengembalikan `405 Method Not Allowed`, dan `mcp_hub.rs:151` mengabaikan server-to-client JSON-RPC request dari downstream stdio MCP (hanya menunggu `val["id"] == id`). |
| **CLI Modes** | `src-tauri/src/main.rs:770-772`, `stdio_bridge.rs` | Hanya mendukung `--stdio` (untuk MCP server). Belum ada mode `--acp` untuk menjalankan Local Agent dari dalam IDE. |
| **Code Outline** | `src-tauri/src/tools_extra.rs:160-255` | Menggunakan regex baris-tunggal (`Regex::new(pattern_str)`). Tidak mengetahui rentang baris akhir (`end_line`), tidak bisa membedakan fungsi di dalam komentar/string literal, dan tidak mendukung `read_symbol`. |
| **Diagnostics & Navigation** | — | Tidak ada integrasi LSP; agent harus menjalankan `cargo check` / `tsc` penuh untuk melihat error dan memakai `smart_search` teks mentah untuk mencari definisi/referensi simbol. |

---

## 3. Arsitektur Target (To-Be)

### 3.1 Diagram Arsitektur Terpadu

```text
  +--------------------+       +-------------------------+       +--------------------+
  | Zed / JetBrains    |       | Gemini / Claude / VSCode|       | Airlock Desktop UI |
  | (ACP Client)       |       | (MCP 2026 Client)       |       | (Obsidian Studio)  |
  +---------+----------+       +------------+------------+       +---------+----------+
            | stdin/stdout                  | HTTP / --stdio               | Tauri IPC
            v                               v                              v
  +--------------------+       +-------------------------+       +--------------------+
  | acp_server.rs      |       | mcp_server.rs           |       | Elicitation Modal  |
  | --acp JSON-RPC     |       | - tasks/* (MCP 2026)    |       | & LSP Status UI    |
  | - session/prompt   |       | - elicitation/create    |       +--------------------+
  | - session/update   |       +------------+------------+
  | - request_perm     |                    |
  +---------+----------+                    |
            |                               v
            +------------>  +-------------------------------+
                            |      Airlock Core Engine      |
                            |  (AppState + Guardrail Gate)  |
                            +-------+---------------+-------+
                                    |               |
               +--------------------+               +--------------------+
               v                                                         v
  +-----------------------------+                         +-----------------------------+
  | semantic.rs (Tree-sitter)   |                         | lsp_bridge.rs (Lazy LSP)    |
  | - AST code_outline (L1-L45) |                         | - rust-analyzer / ts / py   |
  | - read_symbol(path, name)   |                         | - lsp_diagnostics/def/refs  |
  | - Regex fallback            |                         | - 5m idle reaper + tree kill|
  +-----------------------------+                         +-----------------------------+
```

---

## 4. Spesifikasi Detail Setiap Pilar

---

### Pilar 1 — MCP 2026 Primitives: `Tasks` & `Elicitation`

#### 4.1.1 MCP Tasks (`src-tauri/src/mcp_tasks.rs`)
MCP Tasks mendefinisikan state machine resmi untuk operasi asinkron yang melengkapi (bukan menghapus) `start_command_job` / `poll_command_job`:

* **Capabilities Advertisement (`mcp_server.rs:273` & `306`):**
  ```json
  "capabilities": {
    "tools": { "listChanged": true },
    "resources": { "subscribe": false, "listChanged": true },
    "prompts": { "listChanged": true },
    "tasks": {
      "list": {},
      "cancel": {},
      "requests": {
        "tools": { "call": {} }
      }
    }
  }
  ```
* **Mapping Status `JobStatus` → `McpTaskStatus`:**
  * `JobStatus::Running` → `"working"`
  * `JobStatus::Completed` → `"completed"`
  * `JobStatus::Failed` | `JobStatus::TimedOut` → `"failed"`
  * `JobStatus::Killed` → `"cancelled"`
* **Task-Augmented `tools/call` (`mcp_server.rs:405`):**
  * Jika `params` pada `tools/call` menyertakan objek `"task": { "ttl"?: number }`:
    1. Enqueue eksekusi tool ke `TaskManager` (atau langsung delegasikan ke `JobManager` jika `run_command_compressed`/`start_command_job`).
    2. Kembalikan respons **instan (<50ms)**:
       ```json
       {
         "task": {
           "taskId": "task-a1b2c3d4",
           "status": "working",
           "statusMessage": "Executing tool...",
           "createdAt": "2026-11-24T05:27:00Z",
           "lastUpdatedAt": "2026-11-24T05:27:00Z",
           "ttl": 300000,
           "pollInterval": 2000
         }
       }
       ```
* **4 Metode JSON-RPC Baru di `handle_single_rpc` (`mcp_server.rs`):**
  * `tasks/get` (`{ taskId }`) → mengembalikan metadata `task` terbaru (status, `statusMessage`, cuplikan jumlah baris log).
  * `tasks/result` (`{ taskId }`) → jika masih `working`, tunggu hingga batas 15s (atau kembalikan status `working` / error sesuai spec) lalu kembalikan hasil `CallToolResult` final (`{ content: [...], isError }`).
  * `tasks/list` (`{ cursor? }`) → daftar semua active & recent tasks (dari `TaskManager` + `JobManager`).
  * `tasks/cancel` (`{ taskId }`) → menghentikan task/job dan memanggil `kill_pid_tree`.
* **Garbage Collection TTL:** Task yang sudah selesai lebih dari `ttl` (default 15 menit) dibersihkan berkala pada idle watcher (`main.rs:810`).

#### 4.1.2 MCP Elicitation (2 Arah: Downstream Bridge & Server-Side)

1. **Arah A — Downstream MCP Elicitation Bridge (`mcp_hub.rs` + Tauri UI Modal):**
   Saat ini `StdioMcpProcess::request` (`mcp_hub.rs:146-156`) mengabaikan pesan dari downstream server yang `id`-nya tidak cocok dengan request aktif. Ubah loop pembacaannya:
   * Jika downstream server mengirim JSON-RPC request dengan `"method": "elicitation/create"` dan memiliki `"id"`:
     1. Parse `params.message` dan `params.requestedSchema` (JSON Schema flat object: `string`, `number`, `integer`, `boolean`, `enum`).
     2. Buat `PendingElicitation { request_id, server_name, message, schema, created_at }` + `oneshot::channel::<ElicitationResponse>()`.
     3. Emit event Tauri `mcp-elicitation-request` (angkat & fokuskan window utama, sama seperti `PendingToolApproval` di `tools.rs:457-464`).
     4. Tunggu jawaban user (timeout 120s).
     5. Kirim respons JSON-RPC kembali ke stdin downstream MCP server:
        ```json
        {
          "jsonrpc": "2.0",
          "id": "<downstream_req_id>",
          "result": {
            "action": "accept",
            "content": { "confirm": true, "branch_name": "feat/auth" }
          }
        }
        ```
        *(Jika user klik Decline/Cancel atau timeout, kirim `"action": "decline"` atau `"action": "cancel"`.)*
   * Iklankan `"capabilities": { "elicitation": {} }` saat Airlock melakukan `initialize` ke downstream MCP server (`mcp_hub.rs:169` & `349`)!
   * **UI Frontend (`src/views/modals.ts`):** Buat dynamic form builder aman (tanpa `innerHTML`, hanya `createElement("input"|"select"|"label")`) berdasarkan `requestedSchema.properties` (dukung tipe `string`, `number`/`integer`, `boolean`, dan `enum`), dengan tombol **Submit**, **Decline**, dan **Cancel**.

2. **Arah B — Server-Side Elicitation untuk Klien yang Mendukung (Opsional / Fase 2):**
   * Catat `client_supports_elicitation: bool` pada session map saat klien mengirim `initialize` dengan `capabilities.elicitation`.
   * Pada transport `--stdio` (bidirectional), saat tool butuh konfirmasi terstruktur dan GUI tidak terbuka (embedded mode), kirim `elicitation/create` ke klien alih-alih gagal timeout HITL!

---

### Pilar 2 — Agent Client Protocol (`src-tauri/src/acp_server.rs`, mode `--acp`)

#### 4.2.1 Entry Point CLI (`src-tauri/src/main.rs:770`)
```rust
if std::env::args().any(|a| a == "--stdio") {
    std::process::exit(stdio_bridge::run_stdio_bridge());
}
if std::env::args().any(|a| a == "--acp") {
    std::process::exit(acp_server::run_acp_server());
}
```

#### 4.2.2 Protokol & Metode ACP yang Diimplementasikan
ACP menggunakan JSON-RPC 2.0 berbasis newline-delimited JSON di `stdin`/`stdout` (identik dengan pola thread reader di `stdio_bridge.rs:137-154`):

| Metode ACP | Arah | Implementasi di Airlock (`acp_server.rs`) |
| :--- | :--- | :--- |
| `initialize` | Client → Agent | Negosiasi `protocolVersion`, simpan `clientCapabilities` (apakah editor mendukung `fs.readTextFile`, `fs.writeTextFile`, `terminal`), balas `agentCapabilities: { loadSession: true, promptCapabilities: { image: true, embeddedContext: true } }` + `agentInfo: { name: "airlock", version: "0.1.0" }`. |
| `session/new` | Client → Agent | Terima `cwd` (dan opsional `mcpServers` tambahan dari editor!). Validasi & mount `cwd` sebagai workspace jailed sesi tersebut, buat `sessionId: "acp_<8hex>"`, balas `{ sessionId, modes: ["direct", "plan_first"] }`. |
| `session/load` | Client → Agent | Muat sesi tersimpan dari `ChatStore` (`chat_store.rs`) dan replay pesan lama sebagai `session/update` notifications. |
| `session/set_mode` | Client → Agent | Ubah `execution_mode` sesi antara `"direct"` dan `"plan_first"` (terhubung ke Blueprint 03!). |
| `session/prompt` | Client → Agent | Konversi blok konten ACP (`text`, `resource`, `image`) → prompt + `AgentAttachment`, jalankan `local_agent` / `AgentRunManager` dengan sink streaming ACP, balas `{ stopReason: "end_turn" | "cancelled" | "max_turn_requests" }`. |
| `session/cancel` | Client → Agent (Notif) | Kirim sinyal cancel ke `watch::Sender<bool>` milik sesi ACP terkait. |
| `session/update` | Agent → Client (Notif) | Streaming real-time ke editor: `agent_thought_chunk`, `agent_message_chunk`, `tool_call` (pending/in_progress), `tool_call_update` (completed/failed + diff), dan `plan` (entries dari `ArtifactKind::TaskList`). |
| `session/request_permission` | Agent → Client (Req) | **Jembatan HITL Native Editor!** Saat `guardrail_mode == "safe_edit"` dan tool butuh HITL di mode `--acp`, kirim `session/request_permission` dengan opsi (`allow_once`, `allow_always`, `reject_once`) ke editor Zed/JetBrains alih-alih mencoba membuka window GUI Tauri! |

#### 4.2.3 Contoh Konfigurasi Klien ACP (Dokumentasi & Preset di `src/presets.ts`)
Tambahkan preset **"Zed / JetBrains (ACP)"** di `src/presets.ts`:
```json
{
  "agent_servers": {
    "Airlock": {
      "command": "D:\\tauri-mcp-hub\\src-tauri\\target\\release\\tauri-mcp-hub.exe",
      "args": ["--acp"]
    }
  }
}
```
*(Keamanan: Di mode `--acp`, Airlock tetap membaca API key dari Windows DPAPI `secrets.dpapi`, tetap menegakkan Directory Jail, Sensitive File Guard, Command Guard, dan menyimpan snapshot SHA-256 ke `~/.smart-tauri-hub/snapshots/` sehingga edit yang dilakukan dari Zed tetap bisa di-audit dan di-revert dari aplikasi desktop Airlock!)*

---

### Pilar 3 — Semantic Code Intelligence (`src-tauri/src/semantic.rs` & `src-tauri/src/lsp_bridge.rs`)

#### 4.3.1 Tree-sitter AST Outline & `read_symbol` (`src-tauri/src/semantic.rs`)

1. **Dependensi Cargo (di balik feature `semantic-ast`, default aktif):**
   Tambahkan di `src-tauri/Cargo.toml`:
   ```toml
   [features]
   default = ["semantic-ast"]
   semantic-ast = [
     "dep:tree-sitter",
     "dep:tree-sitter-rust",
     "dep:tree-sitter-typescript",
     "dep:tree-sitter-javascript",
     "dep:tree-sitter-python",
     "dep:tree-sitter-go"
   ]
   ```
   *(Pastikan versi seluruh crate `tree-sitter-*` cocok dengan major/minor ABI `tree-sitter` yang dipilih — cek `cargo check` segera setelah menambahkan dependensi di Windows GNU!)*

2. **Struktur Simbol AST:**
   ```rust
   #[derive(Debug, Clone, Serialize)]
   pub struct CodeSymbol {
       pub name: String,
       pub kind: String,          // "fn" | "method" | "struct" | "enum" | "trait" | "impl" | "class" | "interface" | "type"
       pub parent: Option<String>,// Misal "impl AppState" atau "class AuthService"
       pub start_line: usize,     // 1-based
       pub end_line: usize,       // 1-based
       pub signature: String,     // 1 baris deklarasi bersih (maks 140 char)
   }
   ```

3. **Upgrade `code_outline` (`tools_extra.rs:160`):**
   * Untuk `.rs`, `.ts`, `.tsx`, `.js`, `.jsx`, `.py`, `.go`: gunakan `semantic::extract_ast_symbols(&ext, &content)` → format output kini menyertakan **rentang baris lengkap & hirarki**:
     ```text
     OUTLINE [AST]: src-tauri/src/security.rs (390 baris, 10 simbol)

     L   4- 32 | [fn] pub fn wildcard_match(pattern: &str, text: &str) -> bool
     L  35- 78 | [fn] pub fn is_sensitive_path(path: &Path, patterns: &[String]) -> Option<String>
     L  81-155 | [fn] pub fn resolve_jailed_path(root: &Path, target: &str) -> Result<PathBuf, String>
     ```
   * Untuk ekstensi lain (`.md`, `.java`, `.cs`, `.cpp`, dll.) atau jika parse gagal: fallback mulus ke regex eksisting (pindahkan `Regex` ke `OnceLock` per bahasa agar tidak dikompilasi ulang!).

4. **Built-in Tool Baru: `read_symbol` (Read-Only):**
   ```json
   {
     "name": "read_symbol",
     "description": "Reads the exact source code of a specific function, method, struct, class, or interface by symbol name using Tree-sitter AST (includes optional doc comments above it). Saves massive tokens vs guessing line numbers.",
     "inputSchema": {
       "type": "object",
       "properties": {
         "workspace": { "type": "string" },
         "path": { "type": "string", "description": "Relative file path" },
         "symbol": { "type": "string", "description": "Symbol name (e.g. 'resolve_jailed_path' or 'AppState::snapshot')" },
         "include_docs": { "type": "boolean", "description": "Include leading doc comments/attributes (default true)" }
       },
       "required": ["path", "symbol"]
     }
   }
   ```
   * Mendukung pencarian nama sederhana (`snapshot`) maupun qualified (`AppState::snapshot`). Jika ada lebih dari 1 kecocokan (misalnya beberapa method bernama `new`), kembalikan daftar kandidat beserta `parent` dan `L<start>-<end>` (atau semua jika total baris < 120).

#### 4.3.2 On-Demand Lightweight LSP Bridge (`src-tauri/src/lsp_bridge.rs`)

1. **Deteksi Bahasa & Binary Lokal (Zero Bundled Bloat):**
   Tidak perlu membundle binary LSP raksasa. Deteksi otomatis binary yang sudah terinstal di `PATH` pengguna:
   * Rust (`*.rs`, deteksi `Cargo.toml`): `rust-analyzer`
   * TypeScript / JavaScript (`*.ts`, `*.tsx`, `*.js`, deteksi `tsconfig.json`/`package.json`): `typescript-language-server --stdio` (via `cmd.exe /C` di Windows jika `.cmd`)
   * Python (`*.py`): `pyright-langserver --stdio` atau `basedpyright-langserver --stdio`
   * Go (`*.go`, deteksi `go.mod`): `gopls`
   *(User juga bisa menimpa/menambah mapping command di `AppConfig.lsp_servers`.)*

2. **Lifecycle & Resource Guard (`LspManager`):**
   * Key pool: `(canon_workspace_root, lang_id)`.
   * **Lazy Spawn:** Hanya dijalankan saat tool `lsp_*` pertama kali dipanggil.
   * **Max Active LSPs = 2:** Jika user membuka bahasa ke-3, evict (matikan bersih dengan `kill_pid_tree`) server yang paling lama idle (LRU).
   * **Idle Auto-Reaper (5 menit):** Background task memeriksa `last_used` setiap 60 detik; jika >300s tidak dipakai, kirim `shutdown` + `exit` lalu `kill_pid_tree`.
   * **Protokol Framing LSP:** Gunakan framing standar `Content-Length: <bytes>\r\n\r\n<json>` (berbeda dari newline-delimited JSON pada MCP stdio!).
   * **Document Sync (`didOpen`/`didChange`):** Sebelum menjalankan `definition`/`references`/`hover`/`diagnostics` pada sebuah file, kirim `textDocument/didOpen` (atau `didChange` jika mtime/hash berubah) dengan isi file terbaru dari disk (setelah lolos `resolve_jailed_path` & `check_sensitive_guard`!).

3. **4 Built-in Tools LSP (Semua Read-Only):**
   * **`lsp_diagnostics`** (`path?: string`, `workspace?: string`, `severity?: "error"|"warning"|"all"`):
     Mengambil diagnostik dari cache `textDocument/publishDiagnostics` (atau `textDocument/diagnostic` pull LSP 3.17), mengembalikan format super ringkas:
     ```text
     LSP DIAGNOSTICS (rust-analyzer · src-tauri/src/tools.rs · 2 errors):
     L 619:8 [error] mismatched types: expected `Result<(), String>`, found `()`
     ```
     *Jika binary LSP tidak terinstal di sistem, kembalikan pesan ramah & actionable (bukan crash): `"LSP 'rust-analyzer' tidak ditemukan di PATH. Instal atau gunakan run_command_compressed('cargo check')."`*
   * **`lsp_definition`** (`path`, `line` 1-based, `character` 1-based): mengembalikan lokasi file (dikonversi ke relative path di dalam workspace jail!) + 5 baris preview konteks definisi.
   * **`lsp_references`** (`path`, `line`, `character`, `max_results` default 30): mengembalikan seluruh referensi lintas-file yang ter-jail di dalam workspace + 1 baris cuplikan kode per lokasi.
   * **`lsp_hover`** (`path`, `line`, `character`): mengembalikan type signature & dokumentasi Markdown singkat.

---

## 5. Spesifikasi UI / UX & IPC Tambahan

1. **Live Monitor (`#panel-monitor`) — Section LSP & MCP Tasks:**
   * Tambahkan strip kecil status **Active LSP Servers** (menampilkan bahasa, workspace, status `ready`/`indexing`, idle timer, tombol `Stop LSP`).
   * Tabel Background Jobs menampilkan badge `MCP Task` untuk task yang dibuat via `tools/call(task: {})`.
2. **Modal Elicitation Dinamis (`src/views/modals.ts`):**
   * Queue modal identik dengan `oauthConsentQueue` & `toolApprovalQueue` untuk event `mcp-elicitation-request`.
   * IPC baru: `resolve_mcp_elicitation(request_id: String, action: String, content: Option<Value>)`.
3. **Presets (`src/presets.ts`):**
   * Tambahkan preset `ACP (Zed / JetBrains)` yang men-generate snippet JSON dengan path aktual executable `--acp`.

---

## 6. Pitfalls & Gotchas Kritis untuk Antigravity

1. **LSP Framing `Content-Length` BUKAN Newline JSON:**
   LSP memakai header HTTP-style `Content-Length: N\r\n\r\n` di mana `N` adalah **jumlah byte UTF-8**, bukan jumlah `char` Rust! Gunakan `body.len()` saat menulis dan `read_exact(&mut buf)` saat membaca agar tidak korup ketika file mengandung karakter non-ASCII / emoji / komentar bahasa Indonesia.
2. **URI File Windows (`file:///D:/...`) pada LSP:**
   LSP mewajibkan RFC 8089 file URI. Di Windows, `D:\tauri-mcp-hub\src\main.rs` harus dikonversi dengan `url::Url::from_file_path(&clean_path)` (strip `\\?\` dulu!) dan saat menerima URI balik dari LSP (`file:///d%3A/...`), konversi balik via `url.to_file_path()` lalu **tetap validasi** melalui `resolve_jailed_path` (jangan sampai `lsp_definition` membocorkan isi file di luar workspace jika tidak diminta, atau tandai `[external crate/stdlib]` tanpa membaca isinya bila di luar jail!).
3. **`rust-analyzer` Cold Indexing Delay:**
   Saat pertama kali di-spawn pada repo Rust besar, `rust-analyzer` butuh beberapa detik untuk memuat `Cargo.toml`. Implementasikan timeout terukur (maks 12s pada WebMCP, 25s pada LocalAgent) dan jika masih indexing (`$/progress`), kembalikan status informatif `"LSP sedang indexing workspace; coba panggil kembali dalam beberapa detik"` alih-alih membunuh proses LSP!
4. **Windows GNU C/C++ Compilation untuk `tree-sitter`:**
   Jika salah satu grammar crate (seperti `tree-sitter-typescript` yang memakai `scanner.c`) menimbulkan error kompilasi di LLVM-MinGW, cek versi crate yang memakai pure-C scanner (bukan C++), dan jalankan `cargo test` segera setelah menambahkan tiap grammar.
5. **Mode `--acp` Tidak Punya Window GUI:**
   Di `acp_server.rs`, `state.app_handle` bernilai `None`. Pastikan `enforce_guardrail_gate` mengecek apakah ada *ACP Permission Responder* aktif sebelum mencoba emit ke `app_handle`! Jika keduanya `None` (headless tanpa ACP), barulah kembalikan error HITL unavailable.
6. **Sinkronisasi Shutdown:**
   Pastikan `state.lsp_manager.shutdown_all().await` dipanggil di `main.rs:881` (`"quit"` tray menu), di akhir `stdio_bridge.rs`, dan di akhir `acp_server.rs` agar tidak ada proses `rust-analyzer` atau `tsserver` tertinggal di Task Manager Windows!

---

## 7. Roadmap Implementasi Bertahap

### Fase 1 — Tree-sitter AST Outline & `read_symbol` (Nilai Instan & Risiko Rendah)
- [ ] Tambahkan `tree-sitter` + 5 grammar (`rust`, `typescript`, `javascript`, `python`, `go`) di balik feature `semantic-ast` pada `Cargo.toml` dan verifikasi kompilasi pada toolchain Windows GNU.
- [ ] Buat `src-tauri/src/semantic.rs` (AST symbol extractor + `read_symbol` + doc comment capture + regex fallback).
- [ ] Upgrade `exec_code_outline` di `tools_extra.rs` dan tambahkan built-in tool `read_symbol` di `tools.rs`.

### Fase 2 — MCP 2026 `Tasks` & Downstream `Elicitation` Bridge
- [ ] Buat `src-tauri/src/mcp_tasks.rs` yang membungkus `JobManager` + async tool tasks dengan state machine MCP Tasks (`tasks/get`, `tasks/result`, `tasks/list`, `tasks/cancel`).
- [ ] Integrasikan `tasks/*` & task-augmented `tools/call` ke `mcp_server.rs`.
- [ ] Implementasikan interception `elicitation/create` pada `mcp_hub.rs` + queue & modal form dinamis di `src/views/modals.ts` + IPC `resolve_mcp_elicitation`.

### Fase 3 — Agent Client Protocol (`--acp`)
- [ ] Buat `src-tauri/src/acp_server.rs` (`initialize`, `session/new`, `session/load`, `session/set_mode`, `session/prompt`, `session/cancel`, streaming `session/update`, dan `session/request_permission` HITL bridge).
- [ ] Daftarkan flag `--acp` di `src-tauri/src/main.rs` dan tambahkan preset konfigurasi ACP di `src/presets.ts`.

### Fase 4 — On-Demand Lightweight `LspBridge`
- [ ] Buat `src-tauri/src/lsp_bridge.rs` (LSP `Content-Length` transport, Windows URI helper, lazy spawn, LRU max 2 servers, 5-minute idle reaper, `didOpen`/`didChange` sync).
- [ ] Tambahkan 4 built-in tools read-only (`lsp_diagnostics`, `lsp_definition`, `lsp_references`, `lsp_hover`) dan integrasikan shutdown ke quit handler.
- [ ] Tambahkan strip status Active LSP & tombol Stop di View 4 (`#panel-monitor`).

---

## 8. Rencana Pengujian & Definition of Done

### Automated Tests Wajib (`cargo test`)
1. `semantic::tests::test_ast_outline_and_read_symbol_rust_ts_python` (verifikasi `start_line`–`end_line` akurat, tidak tertipu fungsi di dalam komentar/string, dan `read_symbol` mengembalikan fungsi + doc comment persis).
2. `mcp_tasks::tests::test_mcp_tasks_lifecycle_create_get_result_cancel` (E2E POST `tools/call` dengan `task: {}` → `tasks/get` → `tasks/result` dan uji `tasks/cancel`).
3. `mcp_hub::tests::test_downstream_elicitation_request_roundtrip` (mock stdio server mengirim `elicitation/create`, test resolver membalas `accept`, server menerima payload dan menyelesaikan `tools/call`).
4. `acp_server::tests::test_acp_initialize_session_new_and_cancel_protocol` (uji framing JSON-RPC newline-delimited dan verifikasi tidak ada output korup).
5. `lsp_bridge::tests::test_lsp_content_length_framing_with_utf8_multibyte` (verifikasi header `Content-Length` menghitung byte UTF-8 secara tepat untuk teks multibyte).
6. `lsp_bridge::tests::test_windows_file_uri_roundtrip_and_jail_boundary` (verifikasi konversi `D:\...` ↔ `file:///D:/...` dan penolakan baca file di luar jail).

### Verifikasi Build & Runtime
```powershell
cd src-tauri
cargo test
cd ..
npm run build
```
**Definition of Done:**
* `code_outline` menghasilkan rentang baris AST akurat dan `read_symbol` mengambil fungsi spesifik secara presisi.
* Airlock melayani protokol `tasks/*` dan merender modal Elicitation untuk downstream MCP server.
* `tauri-mcp-hub.exe --acp` dapat dihubungkan langsung ke klien ACP (Zed/JetBrains/test harness) dengan streaming thought/message/tool update dan permission bridge.
* `LspBridge` dapat mengambil diagnostik/definisi/referensi secara on-demand dan otomatis mematikan proses LSP saat idle atau aplikasi ditutup.
