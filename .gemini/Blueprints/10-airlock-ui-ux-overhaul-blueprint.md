# Blueprint 10: Airlock UI/UX Major Overhaul — Hybrid Precision Deck (Obsidian Airlock v2)

- **Status:** Revised Specification Ready for Handoff / Execution
- **Workspace:** `@tauri-mcp-hub` (`D:\tauri-mcp-hub`)
- **Design Direction:** **Option C — Hybrid Precision Deck** (Calm studio ergonomics for `Local Agent` & `Diff & Audit` + Live reactive signal telemetry for `MCP Bridge` & `Live Monitor` + Native Frameless Desktop Chrome)
- **Stack Constraint:** Vanilla TypeScript + Native HTML/CSS/SVG + Tauri v2 WebView2 (Zero external UI/chart runtime dependencies)

---

## 1. Executive Vision & Diagnosis of Current UI

### 1.1 Why the Current UI Feels Flat
1. **Single-Trick Cursor Effect (`src/main.ts:528-534`, `src/styles.css:827-845`):**
   - Currently, `pointermove` updates `--spot-x` and `--spot-y` on `.card::before` as a broad `360px` interior radial gradient (`var(--accent-spotlight)`). Because `.card` is repeated 30+ times with identical `#121318` fill and `#20232D` borders, the spotlight reads as a soft surface wash rather than precision craft.
2. **Standard OS Window Chrome Breaks Immersion (`src-tauri/tauri.conf.json:14-21`):**
   - The native Windows titlebar sits disconnected above the `#0D0E12` Top Command Bar, eating vertical real estate and breaking the obsidian studio feel compared to Linear, Raycast, or Zed.
3. **Uniform Box Grid Across All 5 Views:**
   - Every view (`Local Agent`, `MCP Bridge`, `Hub & Tools`, `Live Monitor`, `Diff & Audit`) relies on identical `.card` + `.card-heading` + `.card-desc` containers. Opening `MCP Bridge` or `Live Monitor` greets the user with stacked form inputs instead of a living **Airlock security boundary & live MCP signal flow**.
4. **Permanent Right-Column Crowding in `Local Agent` (`panel-agent`):**
   - Model/provider/thinking/step-budget controls occupy a permanent right sidebar, squeezing horizontal reading width for streaming reasoning (`thought`), tool-call traces, and multi-line prompts.
5. **Basic Flat Diff Viewer (`src/views/audit.ts:140-158`):**
   - Renders raw unified diff lines with sequential 1..N numbering instead of true old/new line numbers, `+add/-del` summary stats, or Side-by-Side comparison.
6. **Monolithic Stylesheet (`src/styles.css` — 3,287 lines):**
   - All shell, card, agent, bridge, monitor, audit, and modal styles live in a single 62.8 KB file, slowing down surgical visual iteration.

### 1.2 Core Design Pillars ("Hybrid Precision Deck v2")
1. **Custom Native Frameless Titlebar (Tauri v2):**
   - Merge the Windows titlebar directly into the `44px–48px` Top Command Bar (`decorations: false`, `shadow: true`) with drag regions, crisp custom window controls (`min / max / close`), and full preservation of `close_to_tray` (`src-tauri/src/main.rs:1703-1706`).
2. **Dual-Layer Obsidian Edge Illumination + 4 Signature Accents:**
   - Replace interior radial smudge with a crisp **1px border-tracing cursor sheen** (`mask-composite: exclude` + rAF proximity detection), subtle `24px` dot-matrix micro-grid on `#09090B`, `backdrop-filter: blur(12px)` glass overlays, and **4 signature accent themes** (`Electric Mint`, `Ice Cyan`, `Solar Amber`, `Carbon Monochrome`).
3. **Live Request Pulse & Streaming Ambient Edge Glow:**
   - Real-time photon pulses travel across the **Interactive Airlock Signal Pipeline** SVG conduits whenever `appendTrafficIncremental()` receives live MCP/HTTP traffic.
   - An ultra-fine `1.5px` animated luminous laser sweep runs along the bottom edge of the Top Command Bar (`[data-agent-streaming="true"]`) whenever the Local Agent is actively streaming—visible across all 5 tabs.
4. **Focus-First Agent Studio (`panel-agent`):**
   - Convert the permanent right configuration sidebar into a **Collapsible Studio Drawer** (`#agent-studio-drawer`), expanding chat & tool-trace cards into a centered `920px` studio measure with a unified floating composer dock.
5. **Diff Inspector Pro (`panel-audit`):**
   - Add a **Unified | Split (Side-by-Side)** diff toggle powered by a zero-dependency unified diff hunk parser with real old/new line numbers, `+added / -removed` badges, and a 5-block visual diffstat bar.
6. **Modular CSS Architecture (`src/styles/01..05-*.css`):**
   - Decompose `src/styles.css` into 5 clean layers without breaking any existing class names or DOM IDs.

---

## 2. Design System v2 Specification (`src/tokens.css` & `design-system/airlock/MASTER.md`)

### 2.1 Expanded 4-Accent Palette & Obsidian v2 Tokens (`src/tokens.css` & `src/dom.ts`)
Preserve all existing tokens (`--bg-canvas`, `--bg-rail`, `--bg-surface`, `--bg-elevated`, `--bg-code`, etc.), update `AccentTheme` in `src/dom.ts` to `"mint" | "cyan" | "amber" | "mono"`, and define:

| Variant | `data-accent` | `--accent` | `--accent-hover` | `--accent-fg` | `--on-accent` | Character |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Electric Mint** *(Default)* | `mint` / `:root` | `#10B981` | `#059669` | `#34D399` | `#031A12` | Bio-terminal precision |
| **Ice Cyan** | `cyan` | `#0EA5E9` | `#0284C7` | `#38BDF8` | `#041521` | Cool instrument telemetry |
| **Solar Amber** | `amber` | `#F59E0B` | `#D97706` | `#FBBF24` | `#1C1002` | Aerospace airlock console |
| **Carbon Monochrome** | `mono` | `#E4E4E7` | `#D4D4D8` | `#FAFAFA` | `#09090B` | Stealth zero-distraction studio |

Additional structural tokens in `src/tokens.css`:
```css
:root {
  /* --- Obsidian v2 Depth & Glass Surfaces --- */
  --bg-glass: rgba(18, 19, 24, 0.82);
  --bg-glass-elevated: rgba(25, 27, 34, 0.88);
  --grid-dot-color: rgba(255, 255, 255, 0.045);

  /* --- Cursor-Reactive Border Illumination --- */
  --border-sheen-size: 260px;
  --border-sheen-color: color-mix(in oklab, var(--accent-fg) 54%, transparent);
  --surface-sheen-color: color-mix(in oklab, var(--accent) 4.5%, transparent);

  /* --- Guardrail Dynamic Seal Tokens --- */
  --seal-color: var(--accent-fg);
  --seal-bg: var(--accent-tint);
  --seal-border: var(--accent-border);

  /* --- Spring & Tactile Motion Curves --- */
  --ease-spring: cubic-bezier(0.16, 1, 0.3, 1);
  --ease-snappy: cubic-bezier(0.2, 0, 0, 1);
}

:root[data-accent="amber"] {
  --accent: #F59E0B;
  --accent-hover: #D97706;
  --accent-fg: #FBBF24;
  --on-accent: #1C1002;
}

:root[data-accent="mono"] {
  --accent: #E4E4E7;
  --accent-hover: #D4D4D8;
  --accent-fg: #FAFAFA;
  --on-accent: #09090B;
}

:root[data-guardrail="safe_edit"] {
  --seal-color: var(--status-warn);
  --seal-bg: var(--status-warn-bg);
  --seal-border: var(--status-warn-border);
}

:root[data-guardrail="read_only"] {
  --seal-color: var(--status-info);
  --seal-bg: var(--status-info-bg);
  --seal-border: var(--status-info-border);
}
```

### 2.2 Dual-Layer Cursor Illumination Mechanics
- **CSS (`src/styles/02-surfaces-controls.css`):**
  - `.card::before` renders a **1px luminous border trace** using CSS mask compositing:
    ```css
    .card::before {
      content: "";
      position: absolute;
      inset: -1px;
      padding: 1px;
      border-radius: inherit;
      pointer-events: none;
      background: radial-gradient(
        var(--border-sheen-size) circle at var(--spot-x, -999px) var(--spot-y, -999px),
        var(--border-sheen-color),
        transparent 70%
      );
      -webkit-mask: linear-gradient(#000 0 0) content-box, linear-gradient(#000 0 0);
      -webkit-mask-composite: xor;
      mask-composite: exclude;
      opacity: 0;
      transition: opacity var(--dur-med) ease;
    }
    .card.is-lit::before,
    .card:hover::before {
      opacity: 1;
    }
    ```
  - `.card::after` adds an ultra-subtle `4.5%` ambient specular tint (`var(--surface-sheen-color)`) so text contrast remains strictly WCAG AA+ (`>= 7:1` for primary text).
- **TypeScript (`src/main.ts`):**
  - Upgrade the delegated `pointermove` handler with `requestAnimationFrame` throttling and `140px` proximity detection across visible `.card` elements in the active `.view-panel.active` so neighboring card edges catch light seamlessly.
  - Automatically bypassed when `window.matchMedia("(prefers-reduced-motion: reduce)").matches` is true.

---

## 3. Custom Native Frameless Titlebar (Tauri v2 Spec)

### 3.1 Tauri Configuration & Capabilities (Verified against Repo)
1. **`src-tauri/tauri.conf.json` (`app.windows[0]`):**
   - Add `"decorations": false` and `"shadow": true` to the `"main"` window definition. Keeping `"shadow": true` preserves native Windows 11 drop shadow, rounded corners, and Aero Snap layouts.
2. **`src-tauri/capabilities/default.json`:**
   - Currently has: `core:default`, `dialog:default`, `clipboard-manager:allow-write-text`, `clipboard-manager:allow-read-text`.
   - Append Tauri v2 window control permissions:
     ```json
     "core:window:allow-start-dragging",
     "core:window:allow-minimize",
     "core:window:allow-toggle-maximize",
     "core:window:allow-is-maximized",
     "core:window:allow-close"
     ```
3. **Zero Backend Changes Needed for Tray Behavior:**
   - Verified in `src-tauri/src/main.rs:1703-1706`: `.on_window_event` intercepts `tauri::WindowEvent::CloseRequested` and checks `state.close_to_tray`. Calling `getCurrentWindow().close()` from our custom Close button triggers `CloseRequested` identically to the native OS `X` button.

### 3.2 Frontend Titlebar Integration (`index.html`, `src/main.ts`, `src/styles/01-base-shell.css`)
- **Drag Region:** Place `data-tauri-drag-region` on `.topbar` and non-interactive flex spacers (`.topbar-spacer`). Interactive controls inside `.topbar` (workspace picker, guardrail buttons, palette trigger, window controls) naturally intercept clicks; add double-click on `.topbar-spacer` to call `win.toggleMaximize()`.
- **Window Controls Cluster (`.window-controls` at far right of `.topbar`):**
  - `#win-btn-min` (Minimize — `1px` crisp SVG dash), `#win-btn-max` (Maximize / Restore — switches icon dynamically on `win.onResized()` + `win.isMaximized()`), `#win-btn-close` (Close — turns `#EF4444` danger hover like Windows 11 native).
  - Keyboard accessible (`type="button"`, `aria-label="Minimize window"` / `"Maximize window"` / `"Close window"`).
  - **Browser Dev Fallback:** Guard `getCurrentWindow()` behind `Boolean((window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__)` so `npm run dev` in a standalone browser stays error-free and hides `.window-controls`.

---

## 4. CSS Modularization Plan (`src/styles.css` → `src/styles/`)

Split `src/styles.css` (3,287 lines) into 5 focused modules imported cleanly by `src/styles.css`:

| Module Path | Responsibility | Approximate Scope |
| :--- | :--- | :--- |
| `src/styles/01-base-shell.css` | Reset, typography, `.main-canvas` dot-grid, `.topbar`, frameless `.window-controls`, `#airlock-seal-badge`, streaming ambient topbar sweep, `.side-rail`, `.rail-indicator`, `.statusbar` | Shell & Navigation |
| `src/styles/02-surfaces-controls.css` | `.card` dual-layer 1px border sheen, `.btn*`, `.badge*`, `.mono-input`, `.mono-select`, `.tactile-switch`, `.toast` | Shared Design Primitives |
| `src/styles/03-agent-studio.css` | `.agent-workspace` focus layout, collapsible `#agent-studio-drawer`, `.chat-stream`, `.msg-card`, `.thought-accordion`, `.tool-step-card`, floating `.composer-dock` | View 1 (`panel-agent`) |
| `src/styles/04-telemetry-views.css` | `.airlock-pipeline` SVG/HTML conduit + live request photon pulse, `.sparkline-card`, `.traffic-table`, `.lsp-strip-list`, `.plugin-grid`, `.audit-timeline`, `.diff-viewer` + `.diff-split-grid` | Views 2, 3, 4, 5 |
| `src/styles/05-overlays-modals.css` | `.artifacts-drawer` glass slide-over, `.modal-dialog` transitions (`@starting-style`), `#command-palette-dialog`, `.slash-popover` | Overlays & Modals |

`src/styles.css` becomes a 5-line manifest:
```css
@import "./styles/01-base-shell.css";
@import "./styles/02-surfaces-controls.css";
@import "./styles/03-agent-studio.css";
@import "./styles/04-telemetry-views.css";
@import "./styles/05-overlays-modals.css";
```

---

## 5. Detailed Subsystem, Animation & View Upgrades

### 5.1 App Shell, Airlock Seal Pill & Streaming Ambient Glow (`index.html`, `src/main.ts`, `src/views/agent.ts`)
- **Airlock Seal Status Pill (`#airlock-seal-badge`):**
  - Placed in `.topbar` beside the Guardrail track. Displays a live lock/shield SVG icon + state label (`SEALED · AUTO`, `GATED · SAFE`, `LOCKED · READ`), synchronized in `renderSnapshot()` and `document.documentElement.dataset.guardrail`.
- **Streaming Ambient Edge Glow (`[data-agent-streaming="true"]`):**
  - In `src/views/agent.ts`, set `document.getElementById("app-shell")?.setAttribute("data-agent-streaming", "true")` when an agent turn starts and remove it in `finally`.
  - `.topbar::after` animates a crisp `1.5px` luminous gradient laser sweep (`@keyframes stream-sweep`, `background-size: 200% 100%`) along the bottom seam of the Top Command Bar, letting the user feel active AI execution across any tab without distracting flashiness.
  - Under `@media (prefers-reduced-motion: reduce)`, renders as a calm solid `1.5px` accent hairline.
- **Accent Switcher (`#btn-toggle-accent`):**
  - Cycles through `mint → cyan → amber → mono`, updates tooltip (`Accent: Electric Mint`, `Ice Cyan`, `Solar Amber`, `Carbon Mono`), and exposes direct theme switching inside the Command Palette (`Ctrl+K`).

### 5.2 View 1: Focus-First Agent Studio (`panel-agent` & `src/views/agent.ts`)
- **Collapsible Configuration Drawer (`#agent-studio-drawer`):**
  - Preserve every existing form control ID (`#agent-provider`, `#agent-api-key`, `#btn-save-agent-key`, `#agent-base-url`, `#agent-model-select`, `#btn-refresh-models`, `#agent-custom-model`, `#toggle-auto-rules`, `#agent-thinking-select`, `#agent-max-steps-input`, `#agent-turn-timeout-input`).
  - Wrap the right-side configuration card in `<aside id="agent-studio-drawer" class="agent-studio-drawer">` with a toggle button `#btn-toggle-studio-drawer` beside `#provider-pill-label`.
  - Persist state in `localStorage.getItem("airlock_agent_drawer_open")` (default collapsed on wide screens once an API key is configured, or open on first run when no key is saved).
  - When collapsed, `#chat-messages` and the composer expand into a centered `920px` studio column.
- **Upgraded Chat Stream, Thought Accordion & Tool Cards:**
  - **Studio Launchpad Empty State:** Clean header showing active `@workspace` alias, guardrail seal, and interactive starter prompt cards.
  - **Thought Accordion:** Glass-bordered collapsible reasoning block with a subtle shimmer indicator while streaming and duration/step metadata once complete.
  - **Tool Execution Cards:** Distinct left category stripe (`inspect/read` = Info Blue, `edit/write` = Accent, `command/job` = Amber), monospace tool signature, duration pill, and clean expandable output.
- **Unified Floating Composer Dock:**
  - Group `#attachment-chips`, `@file`/`+ Image` bar, `Direct`/`Plan First` mode switch, `<textarea id="chat-input">`, and send/stop controls inside a single elevated dock with focus-within border illumination.

### 5.3 View 2 & View 4: Interactive Airlock Signal Pipeline, Live Request Pulse & Sparkline (`panel-bridge`, `panel-monitor`, `src/views/bridge.ts`)
- **Interactive Airlock Signal Conduit (`#airlock-signal-pipeline`):**
  - Hero visual at top of `MCP Bridge` (plus a compact telemetry mirror bar in `Live Monitor`).
  - 4 clickable stage nodes joined by animated SVG conduits (`@keyframes conduit-flow`):
    1. **Client Endpoint:** Active MCP preset / distinct client activity counter.
    2. **Transport Gate:** `127.0.0.1:<port>` (Loopback) or active `Public Tunnel` badge (`ONLINE` / `LOCAL ONLY`).
    3. **Airlock Seal:** `Bearer + OAuth 2.1` + active Guardrail Mode (`--seal-color`).
    4. **Jailed Workspaces:** Mounted workspace count + active `@alias`.
  - Clicking any node jumps smoothly to its control (`#server-port-input`, tunnel section, guardrail selector, or workspace switcher).
- **Live Request Photon Pulse (`triggerPipelinePulse(entry)` in `src/views/bridge.ts`):**
  - Invoked inside `appendTrafficIncremental()` whenever a new `HttpTrafficEntry` arrives.
  - Uses Web Animations API (`Element.animate`) on 3 `<circle class="conduit-packet">` SVG elements along the connector lines (`offset-path` / `cx` interpolation, `420ms ease-out`), colored by outcome (`2xx` = `--accent-fg`, `4xx/5xx` = `--status-danger`, `AUTO-JOB/HARNESS` = `--status-warn`), and flashes the target node ring briefly.
  - Rate-limited (`min 180ms` gap) and skipped automatically when `prefers-reduced-motion: reduce` is active or when the pipeline is off-screen.
- **Live Traffic Activity Sparkline (`#traffic-sparkline` in `panel-monitor`):**
  - Zero-dependency inline `<svg viewBox="0 0 240 36">` updated inside `renderHttpTraffic()` and `appendTrafficIncremental()`, bucketing recent requests into 24 sliding windows with gradient area fill (`--accent`) and peak count / average latency readout.

### 5.4 View 3 & View 5: Hub Polish & Diff Inspector Pro (`panel-hub`, `panel-audit`, `src/views/plugins.ts`, `src/views/audit.ts`)
- **Hub & Tools (`panel-hub`):**
  - Clean status headers, monospace scope pills (`workspace` / `plugin` / `global`), and border illumination across Installed Plugins, Active Skills, and External MCP Servers.
- **Diff & Audit Pro (`panel-audit` & `src/views/audit.ts`):**
  - **Vertical Timeline Rail (`#audit-list`):** Connected vertical rail with status nodes (active vs reverted), `@alias` pills, and token-saved badges.
  - **Diffstat Summary Bar (`#diff-stats`):** Computed `+added` (green) and `-removed` (red) counts plus a crisp 5-block proportional ratio bar rendered beside `#diff-meta`.
  - **Unified | Split (Side-by-Side) Toggle (`#btn-diff-unified`, `#btn-diff-split`):**
    - Persisted in `localStorage.getItem("airlock_diff_mode")` (`"unified"` | `"split"`).
    - Implement `parseUnifiedDiffRows(diffText: string)` in `src/views/audit.ts` to parse `@@ -oldStart,oldCount +newStart,newCount @@` hunks, track exact old/new line numbers, and pair consecutive `-` (deleted) and `+` (added) lines horizontally in Split mode (`grid-template-columns: 1fr 1fr` with left/right gutters).
    - Automatically collapses to Unified view below `1040px` viewport width for clean readability.

---

## 6. DOM & Functional Preservation Contract (Zero-Regression Guardrails)

Every existing element ID, `data-*` attribute, and event hook referenced by `src/main.ts` and `src/views/*.ts` MUST remain present and functional:
- **Shell & Navigation:** `#app-shell`, `#btn-toggle-rail`, `#btn-toggle-accent`, `#rail-indicator`, `#rail-server-toggle`, `#btn-open-palette`, `#btn-hooks-trust`, `#statusbar-jobs-pill`, `#toast-region`, `#toast-stack`.
- **Agent View:** `#panel-agent`, `#chat-session-select`, `#btn-new-session`, `#btn-clear-chat`, `#btn-delete-session`, `#agent-runs-strip`, `#provider-pill-label`, `#provider-pill-dot`, `#agent-provider`, `#agent-api-key`, `#btn-save-agent-key`, `#agent-base-url`, `#agent-model-select`, `#btn-refresh-models`, `#agent-custom-model`, `#toggle-auto-rules`, `#agent-thinking-select`, `#agent-max-steps-input`, `#agent-turn-timeout-input`, `#chat-messages`, `#attachment-chips`, `#attach-file-path`, `#btn-attach-file`, `#attach-image-input`, `#btn-attach-image`, `#btn-mode-direct`, `#btn-mode-plan`, `#slash-popover`, `#chat-input`, `#btn-send-chat`, `#btn-stop-chat`.
- **Bridge, Hub, Monitor, Audit & Modals:** All existing IDs in `bridge.ts`, `plugins.ts`, `workspace.ts`, `artifacts.ts`, `audit.ts`, and `modals.ts` remain intact; new visual elements wrap or sit alongside existing IDs without renaming them.

---

## 7. Phased Implementation Plan (Self-Pacing Multi-Turn Execution)

Execute strictly **1 phase per turn** during Stage 4 (`implement`), verifying with `npm run build` (and `cargo check` for Tauri config changes) after each phase:

| Phase | Target Files | Deliverable | Verification Gate |
| :--- | :--- | :--- | :--- |
| **Phase 1** | `src/tokens.css`, `src/dom.ts`, `design-system/airlock/MASTER.md`, `src/styles.css`, `src/styles/01..05-*.css` | Upgrade design tokens v2 (4 accents: Mint/Cyan/Amber/Mono, `--border-sheen-*`, `--seal-*`, `--bg-glass*`) and modularize `src/styles.css` into 5 CSS modules with 100% parity. | `npm run build` passes cleanly; all 4 accent variants work. |
| **Phase 2** | `src-tauri/tauri.conf.json`, `src-tauri/capabilities/default.json`, `index.html`, `src/main.ts`, `src/styles/01-base-shell.css`, `src/styles/02-surfaces-controls.css` | Implement **Custom Native Frameless Titlebar** (drag region + min/max/close + browser fallback), **Dual-Layer 1px Border Illumination** (rAF proximity glow), canvas dot-grid, Top Bar **Airlock Seal Pill**, and **Streaming Ambient Edge Glow** CSS hook. | `npm run build` + `cargo check` pass cleanly; verify `close_to_tray` compatibility. |
| **Phase 3** | `index.html`, `src/views/agent.ts`, `src/styles/03-agent-studio.css` | Build **Focus-First Agent Studio**: collapsible `#agent-studio-drawer` (`localStorage`), `data-agent-streaming` topbar sweep integration, centered `920px` measure, upgraded thought/tool cards, and unified floating composer dock. | `npm run build` + verify all provider/model/thinking/chat/streaming controls. |
| **Phase 4** | `index.html`, `src/views/bridge.ts`, `src/styles/04-telemetry-views.css` | Add **Interactive Airlock Signal Pipeline** hero in `MCP Bridge` & `Live Monitor`, **Live Request Photon Pulse** on `appendTrafficIncremental()`, and zero-dependency **Live Traffic Sparkline**. | `npm run build` + verify live node updates, pulse animation, and reduced-motion guard. |
| **Phase 5** | `index.html`, `src/views/plugins.ts`, `src/views/audit.ts`, `src/styles/04-telemetry-views.css`, `src/styles/05-overlays-modals.css` | Build **Diff Inspector Pro** (`Unified | Split` Side-by-Side parser, real old/new line numbers, `+add/-del` diffstat bar), audit vertical timeline rail, `Hub & Tools` card polish, and glassmorphic Command Palette (`Ctrl+K`) / Modals / Artifacts Drawer. | `npm run build` + verify split/unified diff rendering, 1-click revert, and modal focus traps. |
| **Phase 6** | `.gemini/Blueprints/README.md`, full workspace | Final polish, accessibility & `prefers-reduced-motion` audit (`quick-reference.md` checklist), and full `npm run build` + `cargo check` verification. | Clean `tsc && vite build` and `cargo check` with zero errors. |

---

## 8. Acceptance Criteria
1. App runs with a seamless **Custom Native Frameless Titlebar** (`decorations: false`, `shadow: true`, Tauri v2 window permissions) that preserves Aero Snap and `close_to_tray`.
2. Cursor interaction produces a crisp **1px border-tracing illumination** across cards and interactive panels, complemented by **4 signature accents** (`Electric Mint`, `Ice Cyan`, `Solar Amber`, `Carbon Monochrome`).
3. `Local Agent` provides a **Collapsible Studio Drawer** for full-width reading and activates a subtle `1.5px` **Streaming Ambient Edge Glow** on the Top Command Bar during active runs.
4. `MCP Bridge` and `Live Monitor` display an **Interactive Airlock Signal Pipeline** with **Live Request Photon Pulses** and a **Live Traffic Sparkline** driven by real snapshot/traffic events.
5. `Diff & Audit` features a vertical timeline rail, `+add/-del` diffstat summary, and a **Unified | Split (Side-by-Side)** diff inspector with accurate old/new line numbers.
6. `src/styles.css` is cleanly modularized into 5 domain files under `src/styles/`, passing `npm run build` and `cargo check` with zero regressions.
