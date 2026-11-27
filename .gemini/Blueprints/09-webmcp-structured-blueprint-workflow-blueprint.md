# Blueprint 09 — Formalized Multi-Turn Workflow & Stage-Aware Dual Handoff Prompts (`WebMCP` & `Antigravity`)

| Atribut | Detail |
| :--- | :--- |
| **Target Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Target Executor** | Google Antigravity Agent / Developer |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Status** | Implemented & Verified (`cargo test` 120 passed, 0 warnings; `--benchmark-webmcp` PASS 4/4, Compact 10,696 B; `npm run build` OK) |
| **Prioritas** | **P1 — Formalized Multi-Turn Workflow & Stage-Aware Dual Handoff Prompts** |
| **Dependensi Blueprint** | Blueprint 04 (Skills), 06 (`session_checkpoint`), 07 (`webmcp_harness`), 08 (`.airlock/` Handoff Auto-Sync) |

---

## 1. Intent & Latar Belakang

Berdasarkan pola penggunaan intensif **Gemini Web (`WebMCP`)**, alur kerja paling efektif untuk pekerjaan coding nyata terdiri atas 4 tahap:
1. **Multi-Turn Inspect:** Gemini menginspeksi codebase sesuai tujuan dengan memuat Agent Skill yang tepat (`list_skills` -> `load_skill`), membagi inspeksi ke beberapa turn jika cakupannya besar, lalu melaporkan temuan.
2. **Multi-Turn Blueprinting:** Menggunakan skill arsitektur/desain yang tepat untuk menyusun dokumen blueprint teknis yang lengkap di `.gemini/blueprints/<NN>-<slug>.md` (atau `.gemini/Blueprints/`), dengan disiplin ketat **maksimal 1 file blueprint utuh per turn** tanpa mencampur implementasi kode.
3. **Checkpoint & Handoff:** Menyimpan `session_checkpoint(op="save")` lalu menghasilkan prompt handoff untuk berpindah ke sesi chat baru atau menyerahkan eksekusi ke **Google Antigravity**.
4. **Eksekusi Sesi Berikutnya (Dual-Path):**
   - **Via Gemini Web (`WebMCP`):** Memuat checkpoint & blueprint, memuat skill yang relevan, melakukan **pre-flight implementation checkup**, dan mengeksekusi **1 phase/batch per turn** secara *self-pacing* untuk 1 blueprint.
   - **Via Google Antigravity:** Memuat `.airlock/HANDOFF.md` + blueprint, memakai skill yang tepat, melakukan **pre-flight checkup serta audit lanjutan kondisi codebase aktual (lapangan) vs isi blueprint**, lalu mengeksekusi & memverifikasi sampai selesai.

Saat ini Airlock baru menyediakan fondasi parsial (`ANTIGRAVITY_HANDOFF_PROMPT` tunggal yang generik dan `[HARNESS_HINT]` pasca-blueprint), sehingga pengguna masih harus terus-menerus:
- Mengulang prompt manual agar Gemini WebMCP memakai skill yang tepat dan menyusun **1 blueprint per turn** secara multi-turn.
- Merevisi prompt handoff monolitik dari `session_checkpoint` agar tidak memaksa seluruh blueprint selesai dalam 1 turn.

> [!NOTE]
> **Catatan Teknis Jujur tentang Fitur Pertanyaan Lanjutan & Pilihan Jawaban Gemini Web:**
> Server MCP tidak memiliki API protokol untuk memaksa rendering widget chip UI milik klien Gemini Web secara langsung. Oleh karena itu, Blueprint 09 memformalkan **Turn-End Follow-Up Choices Protocol** secara konsisten di akhir setiap turn aktif (1 pertanyaan lanjutan singkat + 2–4 pilihan aksi pendek 1 baris yang siap dipilih/diketik), sehingga perilaku prompt follow-up + pilihan jawaban di Gemini Web terpancing dan terjaga di fase inspect, blueprint, maupun implementasi.

---

## 2. Goals, Non-Goals & Anti-Goals

### Goals
1. **Zero New Tools:** Tidak menambah tool MCP baru; Compact schema tetap hemat token (`9` core tools + skill/delegation tools yang ada, total jauh di bawah batas `12 KB`).
2. **Built-in Workflow Skill (`airlock-blueprint-workflow`):** Tersedia otomatis di semua workspace dengan `source_label: "builtin:airlock"`, dapat di-toggle aktif/nonaktif lewat UI Skills (`disabled_skills`), dan dapat di-override jika workspace/plugin/global mendefinisikan skill bernama sama.
3. **Stage-Aware Dual Handoff Prompts:** `session_checkpoint(op="save")` dan `.airlock/HANDOFF.md` secara otomatis menghasilkan **dua blok prompt terstruktur** sesuai tahap (`inspect | blueprint | implement`):
   - `WEBMCP_NEXT_SESSION_PROMPT` (*self-pacing multi-turn*, wajib skill + pre-flight + 1 phase/turn + follow-up choices).
   - `ANTIGRAVITY_HANDOFF_PROMPT` (wajib skill + pre-flight + audit lanjutan codebase aktual vs blueprint sebelum eksekusi penuh & verifikasi).
4. **Mirror Prompt di Workspace:** Menyimpan salinan redacted kedua prompt ke `.airlock/handoff/prompts.md` dan menambahkannya sebagai pointer singkat di `.airlock/HANDOFF.md` agar `HANDOFF.md` tetap `<= 6 KB`.

### Non-Goals & Anti-Goals
- Tidak mengubah aturan strict `WebMCP`-only dari Blueprint 08 (`.airlock/HANDOFF.md` & `.airlock/handoff/` tetap hanya dibuat/diperbarui oleh aktivitas `WebMCP` / `WebMCP:delegate`).
- Tidak menambah UI wizard berat; cukup memanfaatkan tab Skills + `session_checkpoint` + `initialize` + harness nudge yang sudah ada.
- Tidak mencampur implementasi kode pada turn penyusunan blueprint.

---

## 3. Arsitektur Detail & Perubahan per File

### A. Built-in Skill Bawaan Airlock (`src-tauri/src/skills.rs`, `src-tauri/src/state.rs`)

1. **`src-tauri/src/skills.rs`:**
   - Tambahkan konstanta & helper publik:
     - `pub const BUILTIN_WORKFLOW_SKILL_NAME: &str = "airlock-blueprint-workflow";`
     - `pub fn builtin_skills(disabled_norm: &HashSet<String>) -> Vec<SkillEntry>`
     - `pub fn inject_builtin_skills(entries: &mut Vec<SkillEntry>, disabled_skills: &HashSet<String>)`
   - **Precedence & Pure Filesystem Discovery:**
     - Pertahankan `discover_skills(...)` tetap murni memindai filesystem (Workspace > Plugin > Global) agar unit test filesystem skill eksisting tetap deterministik.
     - `inject_builtin_skills` dipanggil dari `AppState::refresh_skills()` (`src-tauri/src/state.rs` tepat setelah `discover_skills(...)`, sekitar baris `869-875`).
     - Jika nama skill built-in belum ada di `entries` (tidak di-override oleh workspace/plugin/global), sisipkan dengan:
       - `scope`: `SkillScope::Global`
       - `source_label`: `"builtin:airlock"`
       - `enabled`: `!disabled_norm.contains(BUILTIN_WORKFLOW_SKILL_NAME)`
       - `dir_path`: `"<builtin:airlock>"`
       - `canon_dir`: `PathBuf::new()`
       - `resource_files`: `Vec::new()`
   - **Resource Jail Guard (`read_skill_resource_file`, `skills.rs:867`):**
     - Di awal `read_skill_resource_file(skill, rel_path)`, jika `skill.summary.source_label.starts_with("builtin:")` atau `skill.canon_dir.as_os_str().is_empty()`, langsung kembalikan error informatif (`"Skill bawaan '...' tidak memiliki file resource eksternal."`) agar `PathBuf::new()` tidak pernah di-resolve terhadap current working directory.

2. **Konten Terstruktur Built-in Skill `airlock-blueprint-workflow`:**
   - **Stage 1 (`inspect`):**
     - Wajib `list_skills` -> `load_skill` yang relevan sebelum inspeksi mendalam.
     - Gunakan `batch_inspect` paralel (atau `delegate_local_agent(mode="read_only")` untuk cakupan besar).
     - Jika codebase/area besar: pecah per sub-modul/turn, laporkan temuan faktual (`file`, simbol, kontrak), daftar area yang masih perlu dicek, lalu tutup turn dengan **Follow-Up Choices**.
   - **Stage 2 (`blueprint`):**
     - Wajib `list_skills` -> `load_skill` skill arsitektur/plan yang sesuai.
     - Sepakati dulu daftar blueprint yang akan dibuat, lalu tulis **maksimal 1 file blueprint per turn** ke `.gemini/blueprints/<NN>-<slug>.md` (atau mengikuti direktori blueprint yang sudah ada di workspace).
     - Dilarang menulis kode implementasi di fase ini.
     - Setelah selesai 1 blueprint (atau batch blueprint selesai), akhiri turn dengan **Follow-Up Choices** atau lanjut ke Stage 3.
   - **Stage 3 (`handoff`):**
     - Panggil `session_checkpoint(op="save", stage="inspect"|"blueprint"|"implement", blueprint_path/blueprint_paths=..., recommended_skills=[...], decisions=[...], completed=[...], next_steps=[...])`.
     - Salin blok `WEBMCP_NEXT_SESSION_PROMPT` dan `ANTIGRAVITY_HANDOFF_PROMPT` secara **utuh verbatim** ke respons chat agar pengguna langsung punya 2 opsi untuk sesi berikutnya, ditutup dengan pertanyaan pilihan target sesi berikutnya.
   - **Stage 4 (`implement`, Self-Pacing Multi-Turn):**
     - Muat checkpoint/handoff + `.airlock/HANDOFF.md` + 1 blueprint target.
     - Wajib `list_skills` -> `load_skill` skill relevan (`recommended_skills` dll).
     - Jalankan **Pre-Flight Implementation Checkup** (bandingkan asumsi path, simbol, tipe, dan test blueprint terhadap kondisi aktual via `batch_inspect`). Jika ada mismatch signifikan, laporkan sebelum mengedit.
     - Kerjakan **1 phase/batch kecil per turn** (edit + test/lint terkait), update `session_checkpoint`, lalu **BERHENTI** dan tampilkan **Follow-Up Choices**.
   - **Turn-End Follow-Up Choices Protocol (Wajib di setiap turn workflow):**
     - Tutup turn aktif dengan **1 pertanyaan lanjutan singkat** diikuti **2–4 pilihan singkat 1 baris** yang memetakan aksi konkret berikutnya (contoh: `1. Lanjut phase berikutnya`, `2. Cek ulang / perbaiki temuan saat ini`, `3. Simpan checkpoint & tampilkan prompt handoff (WebMCP / Antigravity)`).

---

### B. Stage-Aware Dual Handoff Prompts (`src-tauri/src/checkpoints.rs`, `src-tauri/src/tools/schemas.rs`)

1. **Perluasan Struct `SessionCheckpoint` (`src-tauri/src/checkpoints.rs:16-45`):**
   Tambahkan 3 field opsional dengan `#[serde(default)]` (100% kompatibel dengan file JSON checkpoint lama):
   ```rust
   #[serde(default)]
   pub stage: Option<String>,
   #[serde(default)]
   pub blueprint_paths: Vec<String>,
   #[serde(default)]
   pub recommended_skills: Vec<String>,
   ```
   - **Normalisasi `stage`:** helper `normalize_stage(raw: Option<&str>, bp_paths: &[String]) -> String` menormalkan input ke `"inspect" | "blueprint" | "implement"`. Jika `stage` tidak dikirim: infer `"blueprint"` bila `blueprint_path` atau `blueprint_paths` terisi, selain itu `"implement"`.
   - **Kompatibilitas `blueprint_path` <-> `blueprint_paths`:**
     - Jika user mengirim `blueprint_paths` tanpa `blueprint_path`, isi `blueprint_path = blueprint_paths.first().cloned()`.
     - Jika hanya `blueprint_path` yang terisi/terdeteksi otomatis, pastikan `blueprint_paths` memuat nilai tersebut (dedup, case-aware path norm `/`).

2. **Dual Prompt Builder (`src-tauri/src/checkpoints.rs`):**
   - Tambahkan `pub(crate) fn build_webmcp_next_session_prompt(cp: &SessionCheckpoint) -> String`
   - Perbarui `pub(crate) fn build_antigravity_handoff_prompt(cp: &SessionCheckpoint) -> String` (tetap mempertahankan referensi `.airlock/HANDOFF.md`, `airlock://workspace/checkpoint`, blueprint path, `Goal:`, `Decisions:`, dan `Next steps:` agar kontrak uji eksisting tetap terjaga).
   - Keduanya memakai helper terstruktur berdasarkan `effective_stage(&cp)`:
     - **`WEBMCP_NEXT_SESSION_PROMPT`** selalu memuat 4 elemen inti:
       1. Konteks sesi (`session_checkpoint(op="load", workspace="...")` / `airlock://workspace/checkpoint`, `.airlock/HANDOFF.md`, blueprint target).
       2. **Skills:** wajib `list_skills` & `load_skill` (menyebut `airlock-blueprint-workflow` + `recommended_skills` jika ada).
       3. **Pre-Flight Checkup:** cek kesesuaian path/simbol/kondisi aktual vs rencana/blueprint via `batch_inspect` sebelum menulis blueprint/kode.
       4. **Multi-Turn & Follow-Up Choices:** instruksi eksplisit sesuai stage (maks 1 blueprint/turn atau 1 implementation phase/turn untuk 1 blueprint) + wajib tutup turn dengan pertanyaan lanjutan + 2–4 pilihan aksi.
     - **`ANTIGRAVITY_HANDOFF_PROMPT`** selalu memuat 4 elemen inti untuk Antigravity:
       1. Konteks lokasi (`.airlock/HANDOFF.md`, `airlock://workspace/checkpoint`, blueprint path, `Goal`, `Decisions`, `Next steps`).
       2. **Skills:** muat Agent Skill yang relevan (`list_skills` / `load_skill` + `recommended_skills`).
       3. **Pre-Flight & Field Audit (Lapangan vs Blueprint):** audit kesesuaian isi blueprint dengan kondisi codebase aktual (cek path/simbol berubah, API drift, dependency, atau gap yang terlewat) — laporkan atau sesuaikan dulu jika ada penyimpangan.
       4. **Eksekusi & Verifikasi Penuh:** lanjutkan/implementasi secara menyeluruh dan jalankan test/build untuk verifikasi.

3. **Output `exec_session_checkpoint` (`checkpoints.rs:593-600` & `op="load"`):**
   - `op = "save"` mengembalikan:
     ```text
     CHECKPOINT_SAVED: workspace=@<alias> stage=<stage> updated_at=... git_head=... touched=... running_jobs=...
     HANDOFF_PATH: .airlock/HANDOFF.md (jika sync aktif)
     Resume in any new chat with: session_checkpoint(op="load", workspace="<alias>") or resource airlock://workspace/checkpoint.
     WEBMCP_NEXT_SESSION_PROMPT:
     ```text
     <prompt webmcp>
     ```
     ANTIGRAVITY_HANDOFF_PROMPT:
     ```text
     <prompt antigravity>
     ```
     ```
   - `op = "load"` menambahkan baris `STAGE: ...`, `BLUEPRINT_PATHS: ...` (jika > 1 atau terisi), dan `RECOMMENDED_SKILLS: ...` (jika tidak kosong).

4. **Skema Parameter (`src-tauri/src/tools/schemas.rs:760-778`):**
   - Tambahkan 3 properti opsional pada tool `session_checkpoint`:
     - `stage`: `enum: ["inspect", "blueprint", "implement"]`
     - `blueprint_paths`: `array` of `string`
     - `recommended_skills`: `array` of `string`
   - Total tambahan ukuran JSON Compact schema sekitar `~320 bytes` (Compact schema tetap ~`10.6 KB`, jauh di bawah batas `12 KB`).

---

### C. Cermin Prompt di `.airlock/handoff/` (`src-tauri/src/handoff.rs`)

1. **`write_handoff_bundle` (`src-tauri/src/handoff.rs:1015+`):**
   - Ketika `checkpoint.is_some()`, bangun kedua prompt (`build_webmcp_next_session_prompt` & `build_antigravity_handoff_prompt`), jalankan `redact_secrets(secrets, ...)`, lalu tulis secara atomik ke `.airlock/handoff/prompts.md`.
   - Ketika `checkpoint.is_none()` (mis. setelah `op="clear"`), hapus `.airlock/handoff/prompts.md` jika ada.
   - Karena berada di bawah `.airlock/handoff/`, file ini otomatis tercakup rule `.git/info/exclude` (`/.airlock/handoff/`) dan diabaikan oleh scanner (`is_handoff_ignored_rel`).
2. **`render_handoff_md` (`src-tauri/src/handoff.rs:885-896`):**
   - Saat `checkpoint.is_some()`, tambahkan 1 baris pointer ringkas di bawah `## Supporting Files & MCP Resources`:
     `- Next-session prompts (WebMCP / Antigravity): .airlock/handoff/prompts.md`
   - Dengan menaruh isi lengkap prompt di `.airlock/handoff/prompts.md` alih-alih menjejalkannya ke `.airlock/HANDOFF.md`, batas `MAX_HANDOFF_MD_CHARS = 6144` di `HANDOFF.md` tetap terjaga lapang.

---

### D. Protokol `initialize`, Harness Nudge & Output Guard (`rpc.rs`, `webmcp_harness.rs`, `tools/mod.rs`)

1. **`src-tauri/src/mcp_server/rpc.rs` (`build_mcp_initialize_instructions`, baris `92-110`):**
   - Perbarui `handoff_clause` agar tetap padat (`<= 1200` chars keseluruhan instruksi) sambil menyebut skill `airlock-blueprint-workflow`, pengisian `stage`, penyalinan utuh `WEBMCP_NEXT_SESSION_PROMPT` & `ANTIGRAVITY_HANDOFF_PROMPT`, dan penutup turn dengan 2–4 pilihan lanjutan saat fase inspect/blueprint/implement.
2. **`src-tauri/src/webmcp_harness.rs` (`blueprint_nudge`, baris `435-444`):**
   - Perbarui teks `[HARNESS_HINT: ...]` saat file di `.gemini/blueprints/` atau plan ditulis (tetap dalam batas `MAX_TOTAL_HINT_CHARS = 320`):
     `"[HARNESS_HINT: Blueprint '{clipped_bp}' saved (max 1/turn). Call session_checkpoint(op=\"save\", stage=\"blueprint\"), output BOTH prompt blocks verbatim, & end with 2-4 follow-up choices.]"`
3. **`src-tauri/src/tools/mod.rs` (`is_webmcp_control_line`, baris `160-179`):**
   - Tambahkan `"WEBMCP_NEXT_SESSION_PROMPT:"` dan `"CHECKPOINT_STAGE:"` agar output guard tidak memangkas header prompt saat respons checkpoint dikompres.

---

## 4. Verbatim Contoh Output Prompt per Stage

### Stage 1: `inspect`
- **`WEBMCP_NEXT_SESSION_PROMPT`:**
  ```text
  At workspace '@repo', resume from checkpoint (`session_checkpoint(op="load", workspace="repo")` or `airlock://workspace/checkpoint`) and `.airlock/HANDOFF.md`.
  1. SKILLS: Call `list_skills` then load `airlock-blueprint-workflow` plus relevant skills (e.g. `architect-skill`).
  2. PRE-FLIGHT CHECKUP: Verify the inspected files/symbols against `.airlock/HANDOFF.md` via `batch_inspect` before drafting.
  3. BLUEPRINT MODE (MULTI-TURN): Propose the blueprint list if not finalized, then write at most 1 complete blueprint per turn into `.gemini/blueprints/` (NO implementation code yet).
  4. STOP & OFFER CHOICES: Stop after 1 blueprint and end with 1 short follow-up question + 2-4 concise actionable choices.
  Goal: Inspect auth & session pipeline
  Next steps: 1. Draft 01-auth-flow-blueprint.md
  ```
- **`ANTIGRAVITY_HANDOFF_PROMPT`:**
  ```text
  At workspace '@repo', read '.airlock/HANDOFF.md' and load `airlock://workspace/checkpoint` (or call `session_checkpoint(op="load", workspace="repo")`).
  1. SKILLS: Check and load relevant Agent Skills (`architect-skill`).
  2. PRE-FLIGHT & FIELD AUDIT: Audit the actual codebase state against inspection findings and verify assumptions before planning/editing.
  3. EXECUTE: Finalize the blueprint or implement step-by-step and run tests/build to verify.
  Goal: Inspect auth & session pipeline
  Next steps: 1. Draft 01-auth-flow-blueprint.md
  ```

### Stage 2: `blueprint`
- **`WEBMCP_NEXT_SESSION_PROMPT`:**
  ```text
  At workspace '@repo', implement 1 blueprint in self-pacing multi-turn mode: read `.airlock/HANDOFF.md`, blueprint '.gemini/blueprints/01-auth.md', and load `airlock://workspace/checkpoint` (or `session_checkpoint(op="load", workspace="repo")`).
  1. SKILLS: Call `list_skills` then load `airlock-blueprint-workflow` and recommended skills (`test-driven-development`).
  2. PRE-FLIGHT CHECKUP: Run `batch_inspect` to verify target files, symbols, and invariants in '.gemini/blueprints/01-auth.md' match the live codebase BEFORE editing. Flag any mismatch first.
  3. MULTI-TURN (1 BLUEPRINT): Break execution into small phases and complete ONLY ONE phase + its verification this turn. Do NOT attempt the whole blueprint in one turn.
  4. STOP & OFFER CHOICES: Update `session_checkpoint` if needed, then STOP and end with 1 short follow-up question + 2-4 concise choices (e.g. next phase / re-check / handoff to Antigravity).
  Goal: Implement JWT refresh rotation
  Decisions: Use sliding 15m TTL
  Next steps: 1. Implement Phase 1 token store
  ```
- **`ANTIGRAVITY_HANDOFF_PROMPT`:**
  ```text
  At workspace '@repo', read '.airlock/HANDOFF.md', read blueprint '.gemini/blueprints/01-auth.md', and load `airlock://workspace/checkpoint` (or call `session_checkpoint(op="load", workspace="repo")`).
  1. SKILLS: Check and load appropriate Agent Skills (`test-driven-development`).
  2. PRE-FLIGHT & FIELD AUDIT: Thoroughly inspect the live codebase against '.gemini/blueprints/01-auth.md' (check moved paths, changed signatures, missing assumptions, or conflicts) and report/adapt discrepancies before editing.
  3. IMPLEMENT & VERIFY: Execute step-by-step and run full tests/build to verify.
  Goal: Implement JWT refresh rotation
  Decisions: Use sliding 15m TTL
  Next steps: 1. Implement Phase 1 token store
  ```

### Stage 3: `implement`
- **`WEBMCP_NEXT_SESSION_PROMPT`:**
  ```text
  At workspace '@repo', continue self-pacing multi-turn implementation: read `.airlock/HANDOFF.md`, blueprint '.gemini/blueprints/01-auth.md', and load `airlock://workspace/checkpoint` (or `session_checkpoint(op="load", workspace="repo")`).
  1. SKILLS: Ensure `airlock-blueprint-workflow` and relevant skills (`verification-before-completion`) are loaded.
  2. PRE-FLIGHT CHECKUP: Check git/file drift since checkpoint via `batch_inspect` against the next unchecked phase.
  3. MULTI-TURN: Implement and verify ONLY the next single phase of this 1 blueprint, then STOP.
  4. OFFER CHOICES: End with 1 short follow-up question + 2-4 concise choices (continue next phase / debug / finalize or handoff to Antigravity).
  Goal: Complete JWT refresh rotation
  Next steps: 1. Phase 2 HTTP middleware 2. Phase 3 tests
  ```
- **`ANTIGRAVITY_HANDOFF_PROMPT`:**
  ```text
  At workspace '@repo', take over / verify implementation: read '.airlock/HANDOFF.md', '.airlock/handoff/progress.md', '.airlock/handoff/latest.diff', blueprint '.gemini/blueprints/01-auth.md', and load `airlock://workspace/checkpoint` (or `session_checkpoint(op="load", workspace="repo")`).
  1. SKILLS: Load relevant Agent Skills (`verification-before-completion`).
  2. PRE-FLIGHT & FIELD AUDIT: Audit partial WebMCP changes and live codebase state against '.gemini/blueprints/01-auth.md' to detect drift, regressions, or gaps.
  3. FINISH & VERIFY: Complete/fix remaining work and run full tests/build.
  Goal: Complete JWT refresh rotation
  Next steps: 1. Phase 2 HTTP middleware 2. Phase 3 tests
  ```

---

## 5. Security, Performance & Compatibility Guardrails

- **Secret Redaction:** `prompts.md` yang ditulis oleh `handoff.rs` wajib melewati `redact_secrets(&secrets, ...)` seperti halnya `HANDOFF.md` dan `checkpoint.json`.
- **Budget Proteksi:**
  - `build_mcp_initialize_instructions` di `rpc.rs` tetap dijaga `<= 1200` chars (sudah ada trimmer di `rpc.rs:112-120`, tetap pastikan teks penting tidak terpotong).
  - `HANDOFF.md` tetap `<= 6144` chars karena hanya menyertakan pointer ke `.airlock/handoff/prompts.md`.
  - `[HARNESS_HINT]` tetap dalam limit `MAX_TOTAL_HINT_CHARS = 320` di `webmcp_harness.rs`.
- **Virtual Skill Resource Guard:** `read_skill_resource_file` memblokir pembacaan path untuk skill ber-label `builtin:*` sehingga tidak ada akses filesystem ilegal via `PathBuf::new()`.
- **Backward Compatibility:**
  - JSON `SessionCheckpoint` lama tanpa `stage`, `blueprint_paths`, atau `recommended_skills` ter-deserialize mulus via `#[serde(default)]`.
  - Klien MCP/tes lama yang mencari `ANTIGRAVITY_HANDOFF_PROMPT:` dan `airlock://workspace/checkpoint` tetap menemukan keduanya.

---

## 6. Known Test Impacts & Verification Plan

### Test Eksisting yang Terdampak Injection Built-in Skill
Karena `AppState::refresh_skills()` akan menyisipkan `airlock-blueprint-workflow` (`disabled_skills` kosong secara default):
1. `src-tauri/src/mcp_resources.rs:779` (`test_instructions_resource_and_dynamic_skill_prompts`): `assert_eq!(arr.len(), 5)` perlu diperbarui menjadi `6` (`4` static prompt + `skill:demo-skill` + `skill:airlock-blueprint-workflow`) dengan assertion eksplisit untuk keduanya.
2. `src-tauri/src/mcp_server/tests.rs:1462-1465` (`test_mcp_skills_over_mcp_rpc_and_resource_read`): `skills/list` akan mengembalikan `rust-check` dan `airlock-blueprint-workflow` (`len == 2`); perbarui assertion untuk memverifikasi `rust-check` (`skill://rust-check`) + keberadaan `airlock-blueprint-workflow` (`builtin:airlock`).
3. Unit test filesystem murni di `src-tauri/src/skills.rs` memanggil `discover_skills(...)` secara langsung dan **tidak terpengaruh**.

### Unit & Integrasi Test Baru yang Harus Ditambahkan
1. **`skills::tests::test_builtin_workflow_skill_injection_override_and_toggle`:**
   - Memverifikasi `inject_builtin_skills` menambah `airlock-blueprint-workflow` (`builtin:airlock`), menghormati `disabled_skills`, ter-override jika workspace punya skill bernama sama, dan `read_skill_resource_file` menolak akses resource virtual dengan aman.
2. **`checkpoints::tests::test_session_checkpoint_stage_aware_dual_prompts`:**
   - Menguji `stage = "inspect" | "blueprint" | "implement"` + auto-infer saat `stage` kosong.
   - Memastikan `WEBMCP_NEXT_SESSION_PROMPT` memuat skill check + pre-flight checkup + multi-turn guardrail + follow-up choices.
   - Memastikan `ANTIGRAVITY_HANDOFF_PROMPT` memuat skill check + pre-flight & field audit kesesuaian codebase vs blueprint.
   - Memastikan backward-compatibility JSON checkpoint lama & sinkronisasi `blueprint_path` <-> `blueprint_paths`.
3. **`handoff::tests::test_handoff_writes_redacted_prompts_md_and_clears_on_reset`:**
   - Memastikan `checkpoint save` menulis `.airlock/handoff/prompts.md` (ter-redact), ter-referensi di `.airlock/HANDOFF.md`, dan terhapus saat `checkpoint clear`.
4. **`webmcp_harness::tests::test_blueprint_nudge_prompts_dual_handoff_and_choices`** (serta cek `test_initialize_returns_profile_aware_instructions` di `mcp_server/tests.rs`).

### Verification Gate Wajib
1. `Set-Location d:\tauri-mcp-hub\src-tauri; cargo test` -> **0 failures, 0 warnings**.
2. `Set-Location d:\tauri-mcp-hub\src-tauri; cargo run -- --benchmark-webmcp` -> **PASS all scenarios**.
3. `Set-Location d:\tauri-mcp-hub; npm run build` -> **`tsc && vite build` OK** (sanity check).

---

## 7. Urutan Implementasi Step-by-Step

1. **Task 1 (Built-in Skill Core):** Tambah `BUILTIN_WORKFLOW_SKILL_NAME`, `builtin_skills`, `inject_builtin_skills`, dan resource guard di `src-tauri/src/skills.rs`; panggil `inject_builtin_skills` dari `AppState::refresh_skills` di `src-tauri/src/state.rs`; tambah test built-in skill & perbarui `mcp_resources.rs:779` + `mcp_server/tests.rs:1463`.
2. **Task 2 (Stage-Aware Dual Prompts & Schema):** Perluas `SessionCheckpoint` (`stage`, `blueprint_paths`, `recommended_skills`), implementasikan `build_webmcp_next_session_prompt` & upgrade `build_antigravity_handoff_prompt`, perbarui output `exec_session_checkpoint` di `src-tauri/src/checkpoints.rs` + schema di `src-tauri/src/tools/schemas.rs`.
3. **Task 3 (Handoff Bundle Mirror):** Tulis/hapus `.airlock/handoff/prompts.md` + redaksi secret & pointer di `render_handoff_md` pada `src-tauri/src/handoff.rs`.
4. **Task 4 (Protocol Initialize, Harness & Output Guard):** Perbarui `build_mcp_initialize_instructions` (`src-tauri/src/mcp_server/rpc.rs`), `blueprint_nudge` (`src-tauri/src/webmcp_harness.rs`), dan `is_webmcp_control_line` (`src-tauri/src/tools/mod.rs`).
5. **Task 5 (Tests Baru):** Tambah test untuk `skills`, `checkpoints`, `handoff`, dan `webmcp_harness`.
6. **Task 6 (Full Verification Gate):** Jalankan `cargo test`, `cargo run -- --benchmark-webmcp`, dan `npm run build` hingga hijau penuh tanpa warning.
