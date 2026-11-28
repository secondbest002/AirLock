import { invoke } from "@tauri-apps/api/core";
import { $, setToast, switchTab } from "../dom";
import { renderMarkdownInto } from "../markdown";
import type {
  AgentAttachment,
  AgentChunkV2Payload,
  AgentProvider,
  AgentRunSummary,
  AgentStepV2Payload,
  AgentThoughtV2Payload,
  AgentTurnResponse,
  ChatMessage,
  ChatSession,
  DashboardSnapshot,
  ExecutionMode,
  GeminiModelInfo,
  GeminiThinkingMode,
  StoredChatMessage,
  WorkflowSummary,
} from "../types";
import {
  isArtifactsPanelOpen,
  loadSessionArtifacts,
  openRunsTab,
  toggleArtifactsPanel,
} from "./artifacts";

const LEGACY_STORAGE_KEYS = [
  "smart_mcp_hub_gemini_api_key",
  "smart_hub_gemini_key",
  "smart_mcp_hub_gemini_model",
  "smart_mcp_hub_gemini_custom_model",
];

const DRAWER_STORAGE_KEY = "airlock_agent_drawer_open";

const STARTER_PROMPTS: ReadonlyArray<{ title: string; subtitle: string; prompt: string }> = [
  {
    title: "Overview Proyek",
    subtitle: "Peta arsitektur & entry point",
    prompt:
      "Lakukan project_overview pada workspace ini dan jelaskan arsitektur serta entry point utamanya secara ringkas.",
  },
  {
    title: "Review Git Diff",
    subtitle: "Cek potensi bug & edge case",
    prompt:
      "Jalankan git_ops status dan diff lalu review potensi bug serta edge case pada perubahan terbaru.",
  },
  {
    title: "Audit Bug",
    subtitle: "Inspeksi file, AST & search",
    prompt:
      "Audit folder kode utama di workspace ini menggunakan find_files, code_outline, dan read_file_range untuk mencari bug.",
  },
  {
    title: "Tulis Unit Test",
    subtitle: "Perkuat cakupan modul kritis",
    prompt:
      "Identifikasi modul kritis yang belum memiliki cakupan unit test memadai dan usulkan atau tambahkan unit test-nya.",
  },
];

function classifyToolStep(toolName: string): "read" | "edit" | "exec" {
  switch (toolName) {
    case "surgical_edit":
    case "multi_surgical_edit":
    case "patch_and_verify":
    case "write_file_jailed":
    case "file_ops":
    case "upsert_artifact":
      return "edit";
    case "run_command":
    case "run_command_compressed":
    case "start_command_job":
    case "poll_command_job":
    case "stop_command_job":
    case "git_ops":
    case "spawn_subagent":
    case "delegate_local_agent":
      return "exec";
    default:
      return "read";
  }
}

function setAgentDrawerOpen(open: boolean, persist = true): void {
  const shell = $("agent-shell");
  const toggleBtn = $("btn-toggle-drawer");
  const drawer = $("agent-drawer");
  if (open && isArtifactsPanelOpen()) {
    toggleArtifactsPanel(false);
  }
  shell.classList.toggle("drawer-open", open);
  toggleBtn.setAttribute("aria-expanded", open ? "true" : "false");
  drawer.toggleAttribute("inert", !open);
  if (persist) {
    try {
      localStorage.setItem(DRAWER_STORAGE_KEY, open ? "1" : "0");
    } catch {
      // ignore storage errors
    }
  }
}

const DEFAULT_MODELS: Record<AgentProvider, Array<{ id: string; label: string }>> = {
  gemini: [
    { id: "gemini-2.5-flash", label: "gemini-2.5-flash (Cepat & Hemat)" },
    { id: "gemini-2.5-pro", label: "gemini-2.5-pro (Reasoning Kompleks)" },
    { id: "gemini-2.5-flash-lite", label: "gemini-2.5-flash-lite (Ultra Ringan)" },
    { id: "gemini-3-flash-preview", label: "gemini-3-flash-preview (Preview)" },
    { id: "gemini-2.0-flash", label: "gemini-2.0-flash (Fallback Stabil)" },
  ],
  openai_compat: [
    { id: "qwen2.5-coder:7b", label: "qwen2.5-coder:7b (Ollama Local)" },
    { id: "qwen2.5-coder:14b", label: "qwen2.5-coder:14b (Ollama Local)" },
    { id: "llama3.1:8b", label: "llama3.1:8b (Ollama Local)" },
    { id: "gpt-4o-mini", label: "gpt-4o-mini (OpenAI)" },
    { id: "deepseek-chat", label: "deepseek-chat (DeepSeek)" },
  ],
  anthropic: [
    { id: "claude-sonnet-4-5", label: "claude-sonnet-4-5 (Balanced Coding)" },
    { id: "claude-opus-4-1", label: "claude-opus-4-1 (Deep Reasoning)" },
    { id: "claude-3-7-sonnet-latest", label: "claude-3-7-sonnet-latest (Extended Thinking)" },
    { id: "claude-3-5-haiku-latest", label: "claude-3-5-haiku-latest (Fast)" },
  ],
};

function generateSessionId(): string {
  return `chat-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
}

const fetchedModelsByProvider: Partial<Record<AgentProvider, GeminiModelInfo[]>> = {};
const chatHistory: ChatMessage[] = [];
const pendingAttachments: AgentAttachment[] = [];

let lastSnap: DashboardSnapshot | null = null;
let loadedWorkspaceForSessions: string | null = null;
let workspaceSessions: ChatSession[] = [];
let workspaceWorkflows: WorkflowSummary[] = [];
let currentSessionId: string = generateSessionId();
let currentSessionCreatedAt: string | null = null;
let lastRenderedProvider: AgentProvider | null = null;
let drawerAutoInitialized = false;
let activeExecutionMode: ExecutionMode = "direct";
let activeTurnSessionId: string | null = null;
let activeTurnRunId: string | null = null;
let slashMatches: WorkflowSummary[] = [];
let selectedSlashIdx = 0;
let runTurnFlowRef:
  | ((isResume: boolean, payload?: { prompt: string; attachments: AgentAttachment[] }) => Promise<void>)
  | null = null;

let activeStreamState: {
  thoughtText: string;
  bodyText: string;
  thoughtBox: HTMLDetailsElement;
  thoughtPre: HTMLElement;
  bodyEl: HTMLElement;
  transcriptEl: HTMLElement;
} | null = null;
let isTurnActive = false;
let turnStartedAt: number | null = null;
let turnTimerId: number | null = null;
let lastStepStatus = "Ready";

export function getActiveSessionId(): string {
  return currentSessionId;
}

export function setExecutionMode(mode: ExecutionMode): void {
  activeExecutionMode = mode;
  const directBtn = document.getElementById("btn-mode-direct");
  const planBtn = document.getElementById("btn-mode-plan");
  if (directBtn && planBtn) {
    const isPlan = mode === "plan_first";
    directBtn.classList.toggle("active", !isPlan);
    directBtn.setAttribute("aria-pressed", isPlan ? "false" : "true");
    planBtn.classList.toggle("active", isPlan);
    planBtn.setAttribute("aria-pressed", isPlan ? "true" : "false");
  }
}

export async function runProgrammaticTurn(
  prompt: string,
  modeOverride?: ExecutionMode
): Promise<void> {
  if (modeOverride) {
    setExecutionMode(modeOverride);
  }
  if (runTurnFlowRef) {
    await runTurnFlowRef(false, { prompt, attachments: [] });
  }
}

function renderElapsedRunStatus(): void {
  const statusEl = $("agent-run-status");
  if (isTurnActive && turnStartedAt !== null) {
    const secs = Math.max(0, Math.floor((Date.now() - turnStartedAt) / 1000));
    statusEl.textContent = `${lastStepStatus} · ${secs}s`;
  } else {
    statusEl.textContent = lastStepStatus;
  }
}

export function updateAgentStepStatus(text: string): void {
  lastStepStatus = text;
  renderElapsedRunStatus();
}

export function updateAgentStepStatusV2(payload: AgentStepV2Payload): void {
  if (activeTurnSessionId && payload.session_id !== activeTurnSessionId) return;
  if (!activeTurnRunId && payload.run_id) {
    activeTurnRunId = payload.run_id;
  }
  updateAgentStepStatus(payload.text);
}

export function appendAgentThoughtChunkV2(payload: AgentThoughtV2Payload): void {
  if (!activeStreamState || !payload.delta) return;
  if (activeTurnSessionId && payload.session_id !== activeTurnSessionId) return;
  activeStreamState.thoughtText += payload.delta;
  activeStreamState.thoughtBox.hidden = false;
  activeStreamState.thoughtPre.textContent = activeStreamState.thoughtText;
  activeStreamState.transcriptEl.scrollTop = activeStreamState.transcriptEl.scrollHeight;
}

export function appendAgentTextChunkV2(payload: AgentChunkV2Payload): void {
  if (!activeStreamState || !payload.delta) return;
  if (activeTurnSessionId && payload.session_id !== activeTurnSessionId) return;
  activeStreamState.bodyText += payload.delta;
  activeStreamState.bodyEl.textContent = activeStreamState.bodyText;
  activeStreamState.transcriptEl.scrollTop = activeStreamState.transcriptEl.scrollHeight;
}

export function renderRunsStrip(runs: AgentRunSummary[]): void {
  if (activeTurnSessionId) {
    const mine = runs.find(
      (r) => !r.parent_run_id && r.session_id === activeTurnSessionId && r.status === "running"
    );
    if (mine) {
      activeTurnRunId = mine.run_id;
    }
  }

  const strip = document.getElementById("agent-runs-strip");
  if (!strip) return;

  const visible = runs.filter(
    (r) => r.status === "running" || r.status === "awaiting_approval"
  );
  if (visible.length === 0) {
    strip.hidden = true;
    strip.replaceChildren();
    return;
  }

  strip.hidden = false;
  const nodes: HTMLElement[] = [];
  for (const run of visible) {
    const chip = document.createElement("div");
    chip.className = `run-chip ${run.parent_run_id ? "run-chip-sub" : ""}`;

    const infoBtn = document.createElement("button");
    infoBtn.type = "button";
    infoBtn.className = "run-chip-btn";
    infoBtn.title = `${run.provider}/${run.model} · ${run.last_activity_text}`;

    const dot = document.createElement("span");
    dot.className = `live-dot ${run.status === "running" ? "online" : "warn"}`;
    dot.setAttribute("aria-hidden", "true");

    const text = document.createElement("span");
    const shortLabel = run.label.length > 26 ? `${run.label.slice(0, 26)}…` : run.label;
    text.textContent = `@${run.workspace_alias} · ${shortLabel} (${run.current_step}/${run.max_steps} · ${run.elapsed_secs}s)`;

    infoBtn.append(dot, text);
    infoBtn.addEventListener("click", () => openRunsTab());
    chip.appendChild(infoBtn);

    if (run.status === "running") {
      const stop = document.createElement("button");
      stop.type = "button";
      stop.className = "run-chip-stop";
      stop.setAttribute("aria-label", `Stop run ${run.label}`);
      stop.textContent = "×";
      stop.addEventListener("click", async (e) => {
        e.stopPropagation();
        stop.disabled = true;
        await invoke("cancel_agent_run", { runId: run.run_id }).catch(() => {});
      });
      chip.appendChild(stop);
    }
    nodes.push(chip);
  }
  strip.replaceChildren(...nodes);
}

function getSelectedProvider(): AgentProvider {
  const val = ($("agent-provider") as HTMLSelectElement).value;
  if (val === "openai_compat" || val === "anthropic") return val;
  return "gemini";
}

function getSelectedModelId(): string {
  const custom = ($("agent-custom-model") as HTMLInputElement).value.trim();
  if (custom) return custom;
  return ($("agent-model") as HTMLSelectElement).value;
}

function hasSavedKeyForProvider(snap: DashboardSnapshot | null, prov: AgentProvider): boolean {
  if (!snap) return false;
  if (prov === "openai_compat") return snap.has_openai_api_key;
  if (prov === "anthropic") return snap.has_anthropic_api_key;
  return snap.has_gemini_api_key;
}

function populateModelSelect(prov: AgentProvider, preferredModel?: string): void {
  const modelSel = $("agent-model") as HTMLSelectElement;
  const customEl = $("agent-custom-model") as HTMLInputElement;
  const fetched = fetchedModelsByProvider[prov];

  const options: Array<{ id: string; label: string }> = fetched
    ? fetched.map((m) => ({
        id: m.id,
        label: m.display_name && m.display_name !== m.id ? `${m.id} (${m.display_name})` : m.id,
      }))
    : DEFAULT_MODELS[prov];

  modelSel.innerHTML = "";
  for (const item of options) {
    const opt = document.createElement("option");
    opt.value = item.id;
    opt.textContent = item.label;
    modelSel.appendChild(opt);
  }

  if (preferredModel) {
    if (options.some((o) => o.id === preferredModel)) {
      modelSel.value = preferredModel;
      if (document.activeElement !== customEl) {
        customEl.value = "";
      }
    } else if (document.activeElement !== customEl) {
      customEl.value = preferredModel;
    }
  }
}

function syncProviderFieldsUi(prov: AgentProvider, snap: DashboardSnapshot | null): void {
  $("openai-compat-fields").hidden = prov !== "openai_compat";
  $("gemini-thinking-group").hidden = prov !== "gemini";

  const keyInput = $("agent-api-key") as HTMLInputElement;
  const hasKey = hasSavedKeyForProvider(snap, prov);
  if (hasKey) {
    keyInput.placeholder = "Tersimpan aman di Windows DPAPI (isi baru untuk mengganti)";
  } else if (prov === "openai_compat") {
    keyInput.placeholder = "Opsional untuk Ollama/LM Studio lokal, atau isi API Key...";
  } else {
    keyInput.placeholder = "Isi API Key (disimpan terenkripsi DPAPI)...";
  }

  if (snap && prov === "openai_compat") {
    const baseInput = $("agent-base-url") as HTMLInputElement;
    if (document.activeElement !== baseInput && !baseInput.value.trim()) {
      baseInput.value = snap.openai_base_url;
    }
  }
}

function syncProviderPill(snap: DashboardSnapshot | null): void {
  const prov = getSelectedProvider();
  const model = getSelectedModelId() || (snap ? snap.gemini_model : "gemini-2.5-flash");
  const prefix =
    prov === "openai_compat" ? "OpenAI-Compat" : prov === "anthropic" ? "Claude" : "Gemini";
  const labelEl = document.getElementById("provider-pill-label");
  if (labelEl) {
    labelEl.textContent = `${prefix} · ${model}`;
  }
  const dotEl = document.getElementById("provider-pill-dot");
  if (dotEl) {
    const ready = hasSavedKeyForProvider(snap, prov) || prov === "openai_compat";
    dotEl.className = `live-dot ${ready ? "online" : "warn"}`;
  }
}

export function isAgentTurnActive(): boolean {
  return isTurnActive;
}

export function appendAgentThoughtChunk(delta: string): void {
  if (!activeStreamState || !delta) return;
  activeStreamState.thoughtText += delta;
  activeStreamState.thoughtBox.hidden = false;
  activeStreamState.thoughtPre.textContent = activeStreamState.thoughtText;
  activeStreamState.transcriptEl.scrollTop = activeStreamState.transcriptEl.scrollHeight;
}

export function appendAgentTextChunk(delta: string): void {
  if (!activeStreamState || !delta) return;
  activeStreamState.bodyText += delta;
  activeStreamState.bodyEl.textContent = activeStreamState.bodyText;
  activeStreamState.transcriptEl.scrollTop = activeStreamState.transcriptEl.scrollHeight;
}

function renderAttachmentChips(): void {
  const wrap = $("attachment-chips");
  wrap.innerHTML = "";
  if (pendingAttachments.length === 0) {
    wrap.hidden = true;
    return;
  }
  wrap.hidden = false;
  pendingAttachments.forEach((att, idx) => {
    const chip = document.createElement("span");
    chip.className = "attach-chip";
    chip.textContent =
      att.kind === "file"
        ? (att.path ?? att.name).startsWith("@")
          ? (att.path ?? att.name)
          : `@${att.path ?? att.name}`
        : `[img] ${att.name}`;

    const rm = document.createElement("button");
    rm.type = "button";
    rm.className = "attach-chip-remove";
    rm.setAttribute("aria-label", `Hapus lampiran ${att.name}`);
    rm.textContent = "×";
    rm.addEventListener("click", () => {
      pendingAttachments.splice(idx, 1);
      renderAttachmentChips();
    });

    chip.appendChild(rm);
    wrap.appendChild(chip);
  });
}

function readFileAsBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const raw = String(reader.result ?? "");
      const idx = raw.indexOf(",");
      resolve(idx >= 0 ? raw.slice(idx + 1) : raw);
    };
    reader.onerror = () => reject(new Error("Gagal membaca file gambar."));
    reader.readAsDataURL(file);
  });
}

async function addImageFileAttachment(file: File): Promise<void> {
  if (file.size > 4 * 1024 * 1024) {
    setToast("Ukuran gambar maksimal 4 MB.");
    return;
  }
  const b64 = await readFileAsBase64(file);
  pendingAttachments.push({
    kind: "image",
    name: file.name || "pasted-image.png",
    mime_type: file.type || "image/png",
    data_base64: b64,
  });
  renderAttachmentChips();
  setToast(`Gambar '${file.name || "clipboard"}' dilampirkan.`);
}

function createStudioLaunchpad(): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "studio-launchpad chat-empty";

  const pills = document.createElement("div");
  pills.className = "launchpad-pills";

  const defWs =
    lastSnap?.workspaces?.find((w) => w.is_default) ?? lastSnap?.workspaces?.[0] ?? null;
  const wsPill = document.createElement("span");
  wsPill.className = "ws-alias-pill";
  wsPill.textContent = defWs ? `@${defWs.alias}` : "@workspace";
  pills.appendChild(wsPill);

  const sealPill = document.createElement("span");
  sealPill.className = "airlock-seal-badge";
  const mode = lastSnap?.guardrail_mode ?? "autonomous";
  sealPill.textContent =
    mode === "read_only"
      ? "LOCKED · READ"
      : mode === "safe_edit"
        ? "GATED · SAFE"
        : "SEALED · AUTO";
  pills.appendChild(sealPill);

  const title = document.createElement("h2");
  title.className = "launchpad-title";
  title.textContent = "Airlock Agent Studio";

  const desc = document.createElement("p");
  desc.className = "launchpad-desc";
  desc.textContent =
    "Konfigurasi provider AI melalui tombol config di kanan atas (tersimpan aman di Windows DPAPI), lalu pilih workflow atau ketik tugas coding.";

  const grid = document.createElement("div");
  grid.className = "launchpad-grid";
  for (const item of STARTER_PROMPTS) {
    const cardBtn = document.createElement("button");
    cardBtn.type = "button";
    cardBtn.className = "launchpad-card";

    const cardTitle = document.createElement("span");
    cardTitle.className = "launchpad-card-title";
    cardTitle.textContent = item.title;

    const cardSub = document.createElement("span");
    cardSub.className = "launchpad-card-sub";
    cardSub.textContent = item.subtitle;

    cardBtn.append(cardTitle, cardSub);
    cardBtn.addEventListener("click", () => {
      const input = $("chat-input") as HTMLTextAreaElement;
      input.value = item.prompt;
      input.focus();
    });
    grid.appendChild(cardBtn);
  }

  wrap.append(pills, title, desc, grid);
  return wrap;
}

function renderChat(): void {
  const container = $("chat-messages");
  container.innerHTML = "";

  if (chatHistory.length === 0) {
    container.appendChild(createStudioLaunchpad());
    return;
  }

  for (const msg of chatHistory) {
    const card = document.createElement("div");
    card.className = `msg-card ${msg.role === "user" ? "msg-user" : ""}`;

    const hdr = document.createElement("div");
    hdr.className = "msg-header";
    const roleSpan = document.createElement("span");
    roleSpan.textContent = msg.role === "user" ? "YOU" : "LOCAL AGENT";

    const rightWrap = document.createElement("div");
    rightWrap.className = "msg-header-right";

    const timeSpan = document.createElement("span");
    if (msg.role === "assistant" && msg.usage && msg.usage.total_tokens > 0) {
      const cachedPart =
        msg.usage.cached_tokens > 0 ? ` · ${msg.usage.cached_tokens} cached` : "";
      timeSpan.textContent =
        `${msg.timestamp} · in ${msg.usage.prompt_tokens.toLocaleString("en-US")} / out ${msg.usage.output_tokens.toLocaleString("en-US")}${cachedPart}`;
    } else {
      timeSpan.textContent = msg.timestamp;
    }
    rightWrap.appendChild(timeSpan);

    if (msg.role === "user") {
      const resendBtn = document.createElement("button");
      resendBtn.type = "button";
      resendBtn.className = "btn btn-secondary btn-xs";
      resendBtn.textContent = "Edit";
      resendBtn.addEventListener("click", () => {
        const input = $("chat-input") as HTMLTextAreaElement;
        input.value = msg.content;
        input.focus();
      });
      rightWrap.appendChild(resendBtn);
    }

    hdr.append(roleSpan, rightWrap);
    card.appendChild(hdr);

    if (msg.role === "assistant" && msg.thought && msg.thought.trim()) {
      const details = document.createElement("details");
      details.className = "thought-box thought-accordion";
      const sum = document.createElement("summary");
      const wordCount = msg.thought.trim().split(/\s+/).filter(Boolean).length;
      sum.textContent = `Thinking Process · ${wordCount} words`;
      const pre = document.createElement("pre");
      pre.className = "thought-pre";
      pre.textContent = msg.thought;
      details.append(sum, pre);
      card.appendChild(details);
    }

    const body = document.createElement("div");
    body.className = "msg-body";
    if (msg.role === "assistant") {
      renderMarkdownInto(body, msg.content);
    } else {
      body.textContent = msg.content;
    }
    card.appendChild(body);

    if (msg.steps && msg.steps.length > 0) {
      const stepsWrap = document.createElement("div");
      stepsWrap.className = "step-badges";
      for (const s of msg.steps) {
        const cat = classifyToolStep(s.tool_name);
        const stepCard = document.createElement("details");
        stepCard.className = `tool-step-card tool-cat-${cat}`;

        const summaryEl = document.createElement("summary");
        summaryEl.className = "tool-step-summary";

        const catDot = document.createElement("span");
        catDot.className = "tool-cat-dot";
        catDot.setAttribute("aria-hidden", "true");

        const nameCode = document.createElement("code");
        nameCode.className = "tool-step-name";
        nameCode.textContent = s.tool_name;

        const tokSpan = document.createElement("span");
        tokSpan.className = "tool-step-tok";
        tokSpan.textContent = `-${s.tokens_saved} tok`;

        summaryEl.append(catDot, nameCode, tokSpan);

        const stepBody = document.createElement("div");
        stepBody.className = "tool-step-body";
        stepBody.textContent = s.summary;

        stepCard.append(summaryEl, stepBody);
        stepsWrap.appendChild(stepCard);
      }
      card.appendChild(stepsWrap);
    }

    container.appendChild(card);
  }

  container.scrollTop = container.scrollHeight;
}

async function selectChatSession(targetId: string): Promise<void> {
  if (!targetId) {
    await startNewChatSession();
    return;
  }
  const found = workspaceSessions.find((s) => s.id === targetId);
  if (!found) return;
  currentSessionId = found.id;
  currentSessionCreatedAt = found.created_at;
  chatHistory.length = 0;
  for (const m of found.messages) {
    chatHistory.push({
      role: m.role === "user" ? "user" : "assistant",
      content: m.content,
      thought: m.thought ?? undefined,
      steps: m.steps,
      usage: m.usage ?? undefined,
      cancelled: m.cancelled,
      timestamp: m.timestamp,
    });
  }
  renderChat();
  renderSessionSelector();
  await loadSessionArtifacts(currentSessionId);
}

function renderSessionSelector(): void {
  const sel = $("chat-session-select") as HTMLSelectElement;
  const delBtn = $("btn-delete-session") as HTMLButtonElement;
  sel.innerHTML = "";

  const newOpt = document.createElement("option");
  newOpt.value = "";
  newOpt.textContent = `Sesi Baru (${workspaceSessions.length} tersimpan)`;
  sel.appendChild(newOpt);

  for (const s of workspaceSessions) {
    const opt = document.createElement("option");
    opt.value = s.id;
    opt.textContent = `${s.title} (${s.messages.length} msg)`;
    sel.appendChild(opt);
  }

  const activeSession = workspaceSessions.find((s) => s.id === currentSessionId) ?? null;
  if (activeSession) {
    sel.value = currentSessionId;
    delBtn.disabled = false;
  } else {
    sel.value = "";
    delBtn.disabled = true;
  }

  const headingEl = document.getElementById("heading-agent");
  const metaEl = document.getElementById("agent-session-meta");
  if (headingEl) {
    headingEl.textContent = activeSession ? activeSession.title : "Local Agent";
  }
  if (metaEl) {
    metaEl.textContent = activeSession ? `${activeSession.messages.length} msg` : "Sesi Baru";
  }

  const railList = document.getElementById("rail-sessions-list");
  if (railList) {
    if (workspaceSessions.length === 0) {
      const empty = document.createElement("div");
      empty.className = "rail-session-empty";
      empty.textContent = "Belum ada riwayat";
      railList.replaceChildren(empty);
    } else {
      const items = workspaceSessions.slice(0, 6).map((s) => {
        const btn = document.createElement("button");
        btn.type = "button";
        btn.className = `rail-session-btn ${s.id === currentSessionId ? "active" : ""}`.trim();
        btn.title = `${s.title} (${s.messages.length} msg)`;

        const titleSpan = document.createElement("span");
        titleSpan.className = "rail-session-title";
        titleSpan.textContent = s.title;

        const countSpan = document.createElement("span");
        countSpan.className = "rail-session-count";
        countSpan.textContent = String(s.messages.length);

        btn.append(titleSpan, countSpan);
        btn.addEventListener("click", () => {
          switchTab("agent");
          void selectChatSession(s.id);
        });
        return btn;
      });
      railList.replaceChildren(...items);
    }
  }
}

async function loadWorkspaceSessions(): Promise<void> {
  try {
    workspaceSessions = await invoke<ChatSession[]>("list_chat_sessions");
    renderSessionSelector();
  } catch {
    workspaceSessions = [];
    renderSessionSelector();
  }
}

async function loadWorkspaceWorkflows(): Promise<void> {
  try {
    workspaceWorkflows = await invoke<WorkflowSummary[]>("list_workflows");
  } catch {
    workspaceWorkflows = [];
  }
}

async function persistActiveChatSession(): Promise<void> {
  if (chatHistory.length === 0) return;
  const nowIso = new Date().toISOString();
  if (!currentSessionCreatedAt) {
    currentSessionCreatedAt = nowIso;
  }
  const firstUser = chatHistory.find((m) => m.role === "user")?.content ?? "Sesi Coding";
  const cleanTitle = firstUser.replace(/\s+/g, " ").trim().slice(0, 48) || "Sesi Coding";

  const storedMsgs: StoredChatMessage[] = chatHistory.map((m) => ({
    role: m.role,
    content: m.content,
    thought: m.thought ?? null,
    steps: m.steps ?? [],
    usage: m.usage ?? null,
    cancelled: Boolean(m.cancelled),
    timestamp: m.timestamp,
  }));

  const session: ChatSession = {
    id: currentSessionId,
    title: cleanTitle,
    created_at: currentSessionCreatedAt,
    updated_at: nowIso,
    messages: storedMsgs,
  };

  try {
    workspaceSessions = await invoke<ChatSession[]>("save_chat_session", { session });
    renderSessionSelector();
  } catch {
    // non-fatal persistence failure
  }
}

export async function startNewChatSession(): Promise<void> {
  const prevSid = currentSessionId;
  currentSessionId = generateSessionId();
  currentSessionCreatedAt = null;
  chatHistory.length = 0;
  pendingAttachments.length = 0;
  renderAttachmentChips();
  renderChat();
  renderSessionSelector();
  await invoke("clear_agent_history", { sessionId: prevSid }).catch(() => {});
  await loadSessionArtifacts(currentSessionId);
}

function formatCheckpointReason(reason: string): string {
  switch (reason) {
    case "turn_budget_timeout":
      return "Turn budget timeout";
    case "stream_idle_timeout":
      return "Stream idle timeout";
    case "max_steps":
      return "Max steps tercapai";
    case "cancelled":
      return "Dibatalkan user";
    default:
      return reason;
  }
}

export function renderAgentView(snap: DashboardSnapshot): void {
  lastSnap = snap;
  const builtinCount = snap.builtin_tools_count ?? 28;
  $("active-tools-heading").textContent = `Smart Tools Aktif (${builtinCount + (snap.aggregated_tools_count ?? 0)})`;

  const providerSel = $("agent-provider") as HTMLSelectElement;
  const prov = snap.agent_provider ?? "gemini";
  if (document.activeElement !== providerSel) {
    providerSel.value = prov;
  }

  const baseUrlInput = $("agent-base-url") as HTMLInputElement;
  if (document.activeElement !== baseUrlInput) {
    baseUrlInput.value = snap.openai_base_url;
  }

  ($("toggle-auto-rules") as HTMLInputElement).checked = snap.auto_inject_project_rules;

  const thinkingSel = $("agent-thinking-select") as HTMLSelectElement;
  if (document.activeElement !== thinkingSel) {
    thinkingSel.value = snap.gemini_thinking_mode ?? "auto";
  }

  const maxStepsInput = $("agent-max-steps-input") as HTMLInputElement;
  if (document.activeElement !== maxStepsInput) {
    maxStepsInput.value = String(snap.agent_max_steps ?? 16);
  }

  const turnTimeoutInput = $("agent-turn-timeout-input") as HTMLInputElement;
  if (document.activeElement !== turnTimeoutInput) {
    turnTimeoutInput.value = String(snap.agent_turn_timeout_secs ?? 180);
  }

  const cpBanner = $("agent-checkpoint-banner");
  const cp = snap.pending_checkpoint;
  if (cp && cp.can_resume && !isTurnActive) {
    const lastToolPart = cp.last_tool ? ` · last tool: ${cp.last_tool}` : "";
    $("checkpoint-banner-text").textContent =
      `Checkpoint siap dilanjutkan (${formatCheckpointReason(cp.reason)} · ${cp.completed_steps} step${lastToolPart})`;
    cpBanner.hidden = false;
  } else {
    cpBanner.hidden = true;
  }

  const modelSel = $("agent-model") as HTMLSelectElement;
  const activeProv = getSelectedProvider();
  const targetModel =
    activeProv === "openai_compat"
      ? snap.openai_model
      : activeProv === "anthropic"
        ? snap.anthropic_model
        : snap.gemini_model;

  if (lastRenderedProvider !== activeProv) {
    lastRenderedProvider = activeProv;
    populateModelSelect(activeProv, targetModel);
  } else if (
    document.activeElement !== modelSel &&
    document.activeElement !== $("agent-custom-model")
  ) {
    if (Array.from(modelSel.options).some((o) => o.value === targetModel)) {
      modelSel.value = targetModel;
    }
  }

  syncProviderFieldsUi(activeProv, snap);
  syncProviderPill(snap);

  if (!drawerAutoInitialized) {
    drawerAutoInitialized = true;
    const needsFirstRunSetup =
      !hasSavedKeyForProvider(snap, activeProv) && activeProv !== "openai_compat";
    if (needsFirstRunSetup) {
      setAgentDrawerOpen(true, false);
    }
  }

  if (loadedWorkspaceForSessions !== snap.workspace_root) {
    const switchedFromAnother = loadedWorkspaceForSessions !== null;
    loadedWorkspaceForSessions = snap.workspace_root;
    if (switchedFromAnother) {
      currentSessionId = generateSessionId();
      currentSessionCreatedAt = null;
      chatHistory.length = 0;
      renderChat();
    }
    void loadWorkspaceSessions();
    void loadWorkspaceWorkflows();
    void loadSessionArtifacts(currentSessionId);
  }
}

function createStreamingCard(): void {
  const container = $("chat-messages");
  const emptyEl = container.querySelector(".chat-empty");
  if (emptyEl) emptyEl.remove();

  const card = document.createElement("div");
  card.className = "msg-card msg-streaming";

  const hdr = document.createElement("div");
  hdr.className = "msg-header";
  const roleSpan = document.createElement("span");
  roleSpan.textContent = "LOCAL AGENT (STREAMING)";
  const rightWrap = document.createElement("div");
  rightWrap.className = "msg-header-right";
  const shimmer = document.createElement("span");
  shimmer.className = "shimmer-bar";
  shimmer.setAttribute("aria-label", "Streaming");
  const timeSpan = document.createElement("span");
  timeSpan.textContent = new Date().toLocaleTimeString();
  rightWrap.append(shimmer, timeSpan);
  hdr.append(roleSpan, rightWrap);

  const thoughtBox = document.createElement("details");
  thoughtBox.className = "thought-box thought-accordion is-streaming";
  thoughtBox.hidden = true;
  const sum = document.createElement("summary");
  const pulseDot = document.createElement("span");
  pulseDot.className = "thought-pulse-dot";
  pulseDot.setAttribute("aria-hidden", "true");
  const sumText = document.createElement("span");
  sumText.textContent = "Thinking Process (streaming)";
  sum.append(pulseDot, sumText);
  const thoughtPre = document.createElement("pre");
  thoughtPre.className = "thought-pre";
  thoughtBox.append(sum, thoughtPre);

  const bodyEl = document.createElement("div");
  bodyEl.className = "msg-body";
  bodyEl.textContent = "Menunggu respons streaming...";

  card.append(hdr, thoughtBox, bodyEl);
  container.appendChild(card);
  container.scrollTop = container.scrollHeight;

  activeStreamState = {
    thoughtText: "",
    bodyText: "",
    thoughtBox,
    thoughtPre,
    bodyEl,
    transcriptEl: container,
  };
}

export async function initAgentView(
  onSnapshot: (snap: DashboardSnapshot) => void
): Promise<void> {
  // One-time silent migration of legacy localStorage Gemini key into encrypted DPAPI store
  const legacyKey =
    localStorage.getItem("smart_mcp_hub_gemini_api_key") ??
    localStorage.getItem("smart_hub_gemini_key") ??
    "";
  const legacyModel =
    localStorage.getItem("smart_mcp_hub_gemini_custom_model") ||
    localStorage.getItem("smart_mcp_hub_gemini_model") ||
    null;

  if (legacyKey.trim()) {
    await invoke("configure_agent_provider", {
      provider: "gemini",
      apiKey: legacyKey.trim(),
      baseUrl: null,
      model: legacyModel,
      autoInjectProjectRules: null,
      geminiThinkingMode: null,
      agentMaxSteps: null,
      agentTurnTimeoutSecs: null,
      streamIdleTimeoutSecs: null,
    }).catch(() => {});
  }
  for (const k of LEGACY_STORAGE_KEYS) {
    localStorage.removeItem(k);
  }

  const providerSel = $("agent-provider") as HTMLSelectElement;
  const modelSel = $("agent-model") as HTMLSelectElement;

  const toggleDrawerBtn = $("btn-toggle-drawer");
  try {
    const savedDrawer = localStorage.getItem(DRAWER_STORAGE_KEY);
    if (savedDrawer === "1") {
      drawerAutoInitialized = true;
      setAgentDrawerOpen(true, false);
    } else if (savedDrawer === "0") {
      drawerAutoInitialized = true;
      setAgentDrawerOpen(false, false);
    }
  } catch {
    // ignore storage errors
  }

  toggleDrawerBtn.addEventListener("click", () => {
    const nextOpen = !$("agent-shell").classList.contains("drawer-open");
    setAgentDrawerOpen(nextOpen, true);
  });

  $("btn-mode-direct").addEventListener("click", () => setExecutionMode("direct"));
  $("btn-mode-plan").addEventListener("click", () => setExecutionMode("plan_first"));

  providerSel.addEventListener("change", () => {
    const prov = getSelectedProvider();
    lastRenderedProvider = prov;
    const savedModel = lastSnap
      ? prov === "openai_compat"
        ? lastSnap.openai_model
        : prov === "anthropic"
          ? lastSnap.anthropic_model
          : lastSnap.gemini_model
      : undefined;
    populateModelSelect(prov, savedModel);
    syncProviderFieldsUi(prov, lastSnap);
    syncProviderPill(lastSnap);
  });

  modelSel.addEventListener("change", () => {
    syncProviderPill(lastSnap);
  });

  ($("agent-custom-model") as HTMLInputElement).addEventListener("input", () => {
    syncProviderPill(lastSnap);
  });

  document.querySelectorAll<HTMLButtonElement>(".preset-url-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      const targetUrl = btn.dataset.url ?? "";
      if (targetUrl) {
        ($("agent-base-url") as HTMLInputElement).value = targetUrl;
      }
    });
  });

  const saveAgentConfig = async (overrideApiKey?: string): Promise<DashboardSnapshot> => {
    const prov = getSelectedProvider();
    const keyEl = $("agent-api-key") as HTMLInputElement;
    const apiKey = overrideApiKey ?? keyEl.value.trim();
    const baseUrl =
      prov === "openai_compat"
        ? ($("agent-base-url") as HTMLInputElement).value.trim()
        : null;
    const targetModel = getSelectedModelId();
    const autoRules = ($("toggle-auto-rules") as HTMLInputElement).checked;
    const thinkingMode = ($("agent-thinking-select") as HTMLSelectElement).value as GeminiThinkingMode;
    const parsedSteps = Number.parseInt(($("agent-max-steps-input") as HTMLInputElement).value, 10);
    const maxSteps = Number.isFinite(parsedSteps) ? Math.min(30, Math.max(4, parsedSteps)) : 16;
    const parsedBudget = Number.parseInt(($("agent-turn-timeout-input") as HTMLInputElement).value, 10);
    const turnBudget = Number.isFinite(parsedBudget) ? Math.min(600, Math.max(30, parsedBudget)) : 180;

    const snap = await invoke<DashboardSnapshot>("configure_agent_provider", {
      provider: prov,
      apiKey,
      baseUrl,
      model: targetModel,
      autoInjectProjectRules: autoRules,
      geminiThinkingMode: thinkingMode,
      agentMaxSteps: maxSteps,
      agentTurnTimeoutSecs: turnBudget,
      streamIdleTimeoutSecs: null,
    });
    if (overrideApiKey === undefined && apiKey.length > 0) {
      keyEl.value = "";
    }
    onSnapshot(snap);
    return snap;
  };

  $("btn-save-agent").addEventListener("click", async () => {
    try {
      await saveAgentConfig();
      setToast(`Konfigurasi ${getSelectedProvider()} disimpan aman.`);
    } catch (err) {
      setToast(`Gagal menyimpan konfigurasi agent: ${String(err)}`);
    }
  });

  $("btn-clear-api-key").addEventListener("click", async () => {
    try {
      await saveAgentConfig("__CLEAR__");
      ($("agent-api-key") as HTMLInputElement).value = "";
      setToast(`API Key ${getSelectedProvider()} dihapus dari DPAPI store.`);
    } catch (err) {
      setToast(`Gagal menghapus API key: ${String(err)}`);
    }
  });

  $("btn-fetch-models").addEventListener("click", async () => {
    const prov = getSelectedProvider();
    const apiKey = ($("agent-api-key") as HTMLInputElement).value.trim();
    const baseUrl = ($("agent-base-url") as HTMLInputElement).value.trim() || null;
    const btn = $("btn-fetch-models") as HTMLButtonElement;
    btn.disabled = true;
    try {
      const models = await invoke<GeminiModelInfo[]>("fetch_provider_models", {
        provider: prov,
        apiKey,
        baseUrl,
      });
      fetchedModelsByProvider[prov] = models;
      const prev = modelSel.value;
      populateModelSelect(prov, models.some((m) => m.id === prev) ? prev : undefined);
      syncProviderPill(lastSnap);
      setToast(`Berhasil memuat ${models.length} model (${prov}).`);
    } catch (err) {
      setToast(`Gagal fetch models: ${String(err)}`);
    } finally {
      btn.disabled = false;
    }
  });

  $("btn-clear-chat").addEventListener("click", async () => {
    await startNewChatSession();
    setToast("Riwayat chat lokal dibersihkan.");
  });

  $("btn-new-session").addEventListener("click", async () => {
    await startNewChatSession();
    setToast("Memulai sesi chat baru.");
  });

  ($("chat-session-select") as HTMLSelectElement).addEventListener("change", async (e) => {
    const targetId = (e.target as HTMLSelectElement).value;
    await selectChatSession(targetId);
  });

  $("btn-delete-session").addEventListener("click", async () => {
    const targetToDelete = currentSessionId;
    if (!workspaceSessions.some((s) => s.id === targetToDelete)) return;
    try {
      workspaceSessions = await invoke<ChatSession[]>("delete_chat_session", {
        sessionId: targetToDelete,
      });
      await startNewChatSession();
      setToast("Sesi chat dihapus.");
    } catch (err) {
      setToast(`Gagal menghapus sesi: ${String(err)}`);
    }
  });

  const attachPopover = document.getElementById("attach-file-popover");
  const attachToggleBtn = document.getElementById("btn-toggle-attach-file");
  const closeAttachPopover = (): void => {
    if (!attachPopover || !attachToggleBtn) return;
    attachPopover.hidden = true;
    attachToggleBtn.setAttribute("aria-expanded", "false");
  };

  attachToggleBtn?.addEventListener("click", (e) => {
    e.stopPropagation();
    if (!attachPopover) return;
    const willOpen = attachPopover.hidden;
    attachPopover.hidden = !willOpen;
    attachToggleBtn.setAttribute("aria-expanded", willOpen ? "true" : "false");
    if (willOpen) {
      ($("attach-file-path") as HTMLInputElement).focus();
    }
  });

  attachPopover?.addEventListener("click", (e) => e.stopPropagation());
  document.addEventListener("click", () => closeAttachPopover());

  // File & Image Attachments
  const addPathAttachment = (): void => {
    const input = $("attach-file-path") as HTMLInputElement;
    const raw = input.value.trim();
    if (!raw) return;
    // Keep @alias:rel/path intact while stripping stray leading '@' from plain paths
    const normalized =
      raw.startsWith("@") && raw.slice(1).includes(":")
        ? raw
        : raw.replace(/^@+/, "");
    if (!normalized) return;
    pendingAttachments.push({
      kind: "file",
      name: normalized,
      path: normalized,
    });
    input.value = "";
    renderAttachmentChips();
    closeAttachPopover();
  };

  $("btn-attach-file").addEventListener("click", addPathAttachment);
  ($("attach-file-path") as HTMLInputElement).addEventListener("keydown", (e) => {
    if (e.isComposing) return;
    if (e.key === "Enter") {
      e.preventDefault();
      addPathAttachment();
    } else if (e.key === "Escape") {
      e.preventDefault();
      closeAttachPopover();
      attachToggleBtn?.focus();
    }
  });

  const slashBtn = document.getElementById("btn-insert-slash");
  slashBtn?.addEventListener("click", () => {
    const ta = $("chat-input") as HTMLTextAreaElement;
    if (!ta.value.startsWith("/")) {
      ta.value = `/${ta.value}`;
    }
    ta.focus();
    ta.dispatchEvent(new Event("input", { bubbles: true }));
  });

  const imgInput = $("attach-image-input") as HTMLInputElement;
  $("btn-attach-image").addEventListener("click", () => imgInput.click());
  imgInput.addEventListener("change", async () => {
    const file = imgInput.files?.[0];
    imgInput.value = "";
    if (!file) return;
    try {
      await addImageFileAttachment(file);
    } catch (err) {
      setToast(String(err));
    }
  });

  const chatInput = $("chat-input") as HTMLTextAreaElement;
  const slashPopover = $("slash-popover");

  const closeSlashPopover = (): void => {
    slashPopover.hidden = true;
    slashPopover.replaceChildren();
    slashMatches = [];
    selectedSlashIdx = 0;
  };

  const applySlashSelection = (wf: WorkflowSummary): void => {
    chatInput.value = `${wf.slash} `;
    if (wf.execution_mode === "plan_first") {
      setExecutionMode("plan_first");
    }
    closeSlashPopover();
    chatInput.focus();
  };

  const renderSlashPopover = (): void => {
    if (slashMatches.length === 0) {
      closeSlashPopover();
      return;
    }
    slashPopover.hidden = false;
    const nodes: HTMLElement[] = [];
    slashMatches.forEach((wf, idx) => {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.role = "option";
      btn.className = `slash-item ${idx === selectedSlashIdx ? "selected" : ""}`;
      btn.setAttribute("aria-selected", idx === selectedSlashIdx ? "true" : "false");

      const top = document.createElement("div");
      top.className = "slash-item-top";
      const code = document.createElement("code");
      code.textContent = wf.slash;
      const src = document.createElement("span");
      src.className = "badge badge-muted";
      src.textContent = wf.source;
      top.append(code, src);

      const desc = document.createElement("div");
      desc.className = "slash-item-desc";
      desc.textContent = `${wf.title} — ${wf.description}`;

      btn.append(top, desc);
      btn.addEventListener("mousedown", (ev) => {
        ev.preventDefault();
        applySlashSelection(wf);
      });
      nodes.push(btn);
    });
    slashPopover.replaceChildren(...nodes);
  };

  chatInput.addEventListener("input", () => {
    const val = chatInput.value;
    if (val.startsWith("/") && !val.includes("\n") && !val.slice(1).includes(" ")) {
      const q = val.slice(1).toLowerCase();
      slashMatches = workspaceWorkflows.filter(
        (w) =>
          w.name.toLowerCase().includes(q) ||
          w.title.toLowerCase().includes(q) ||
          w.slash.toLowerCase().startsWith(val.toLowerCase())
      );
      selectedSlashIdx = 0;
      renderSlashPopover();
    } else {
      closeSlashPopover();
    }
  });

  chatInput.addEventListener("blur", () => {
    window.setTimeout(closeSlashPopover, 120);
  });

  chatInput.addEventListener("paste", async (e) => {
    const items = e.clipboardData?.items;
    if (!items) return;
    for (const item of Array.from(items)) {
      if (item.type.startsWith("image/")) {
        const file = item.getAsFile();
        if (file) {
          e.preventDefault();
          await addImageFileAttachment(file).catch((err) => setToast(String(err)));
          return;
        }
      }
    }
  });

  document.querySelectorAll<HTMLButtonElement>(".prompt-chip[data-prompt]").forEach((chip) => {
    chip.addEventListener("click", () => {
      const tmpl = chip.dataset.prompt ?? "";
      chatInput.value = tmpl;
      closeSlashPopover();
      chatInput.focus();
    });
  });

  const stopBtn = $("btn-stop-agent") as HTMLButtonElement;
  stopBtn.addEventListener("click", async () => {
    stopBtn.disabled = true;
    await invoke("cancel_local_agent", {
      runId: activeTurnRunId,
    }).catch(() => {});
    updateAgentStepStatus("Membatalkan...");
  });

  const chatForm = $("chat-form") as HTMLFormElement;
  chatInput.addEventListener("keydown", (e) => {
    if (e.isComposing) return;
    if (!slashPopover.hidden && slashMatches.length > 0) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        selectedSlashIdx = (selectedSlashIdx + 1) % slashMatches.length;
        renderSlashPopover();
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        selectedSlashIdx = (selectedSlashIdx - 1 + slashMatches.length) % slashMatches.length;
        renderSlashPopover();
        return;
      }
      if (e.key === "Tab" || (e.key === "Enter" && !e.ctrlKey && !e.metaKey && !e.shiftKey)) {
        e.preventDefault();
        const chosen = slashMatches[selectedSlashIdx];
        if (chosen) applySlashSelection(chosen);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        closeSlashPopover();
        return;
      }
    }
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      chatForm.requestSubmit();
    }
  });

  const runTurnFlow = async (
    isResume: boolean,
    payload?: { prompt: string; displayPrompt?: string; attachments: AgentAttachment[] }
  ): Promise<void> => {
    if (isTurnActive) return;

    const targetModel = getSelectedModelId();
    try {
      await saveAgentConfig();
    } catch (err) {
      setToast(`Config error: ${String(err)}`);
      return;
    }

    const turnSessionId = currentSessionId;
    activeTurnSessionId = turnSessionId;
    activeTurnRunId = null;

    if (!isResume && payload) {
      const attSuffix =
        payload.attachments.length > 0
          ? `\n\n[Attachments: ${payload.attachments
              .map((a) => {
                if (a.kind !== "file") return a.name;
                const p = a.path ?? a.name;
                return p.startsWith("@") ? p : `@${p}`;
              })
              .join(", ")}]`
          : "";
      chatHistory.push({
        role: "user",
        content: `${payload.displayPrompt ?? payload.prompt}${attSuffix}`,
        timestamp: new Date().toLocaleTimeString(),
      });
      chatInput.value = "";
      closeSlashPopover();
      pendingAttachments.length = 0;
      renderAttachmentChips();
      renderChat();
    }

    createStreamingCard();

    const sendBtn = $("btn-send-chat") as HTMLButtonElement;
    const resumeBtn = $("btn-resume-checkpoint") as HTMLButtonElement;
    const discardBtn = $("btn-discard-checkpoint") as HTMLButtonElement;
    isTurnActive = true;
    document.getElementById("app-shell")?.setAttribute("data-agent-streaming", "true");
    sendBtn.disabled = true;
    resumeBtn.disabled = true;
    discardBtn.disabled = true;
    stopBtn.hidden = false;
    stopBtn.disabled = false;
    $("agent-checkpoint-banner").hidden = true;

    turnStartedAt = Date.now();
    lastStepStatus = isResume ? `Resuming ${targetModel}...` : `Streaming ${targetModel}...`;
    renderElapsedRunStatus();
    if (turnTimerId !== null) {
      window.clearInterval(turnTimerId);
    }
    turnTimerId = window.setInterval(renderElapsedRunStatus, 1000);

    try {
      const res = isResume
        ? await invoke<AgentTurnResponse>("resume_local_agent_turn", {
            sessionId: turnSessionId,
          })
        : await invoke<AgentTurnResponse>("run_local_agent_turn", {
            prompt: payload?.prompt ?? "",
            attachments: payload?.attachments ?? [],
            sessionId: turnSessionId,
            executionMode: activeExecutionMode,
          });
      const capturedThought = activeStreamState?.thoughtText ?? "";
      activeStreamState = null;

      chatHistory.push({
        role: "assistant",
        content: res.reply,
        thought: capturedThought,
        steps: res.steps,
        usage: res.usage,
        cancelled: res.cancelled,
        timestamp: new Date().toLocaleTimeString(),
      });
      renderChat();
      isTurnActive = false;
      onSnapshot(res.snapshot);
      await persistActiveChatSession();
      void loadSessionArtifacts(turnSessionId);
      const totalElapsed =
        turnStartedAt !== null ? Math.max(0, Math.floor((Date.now() - turnStartedAt) / 1000)) : 0;
      lastStepStatus = res.awaiting_approval
        ? `Awaiting Plan Approval (${totalElapsed}s)`
        : res.cancelled
          ? `Dibatalkan (${totalElapsed}s)`
          : res.checkpoint?.can_resume
            ? `Checkpoint (${res.steps.length} tool step · ${totalElapsed}s)`
            : `Selesai (${res.steps.length} tool step · ${totalElapsed}s)`;
    } catch (err) {
      const capturedThought = activeStreamState?.thoughtText ?? "";
      activeStreamState = null;
      chatHistory.push({
        role: "assistant",
        content: `Error: ${String(err)}`,
        thought: capturedThought,
        timestamp: new Date().toLocaleTimeString(),
      });
      renderChat();
      await persistActiveChatSession();
      lastStepStatus = "Error";
    } finally {
      document.getElementById("app-shell")?.removeAttribute("data-agent-streaming");
      if (turnTimerId !== null) {
        window.clearInterval(turnTimerId);
        turnTimerId = null;
      }
      turnStartedAt = null;
      isTurnActive = false;
      activeTurnRunId = null;
      activeTurnSessionId = null;
      sendBtn.disabled = false;
      resumeBtn.disabled = false;
      discardBtn.disabled = false;
      stopBtn.hidden = true;
      stopBtn.disabled = false;
      renderElapsedRunStatus();
      if (lastSnap) {
        renderAgentView(lastSnap);
      }
    }
  };
  runTurnFlowRef = runTurnFlow;

  $("btn-resume-checkpoint").addEventListener("click", () => {
    void runTurnFlow(true);
  });

  $("btn-discard-checkpoint").addEventListener("click", async () => {
    try {
      const next = await invoke<DashboardSnapshot>("discard_agent_checkpoint", {
        sessionId: currentSessionId,
      });
      onSnapshot(next);
      setToast("Checkpoint dibuang.");
    } catch (err) {
      setToast(`Gagal membuang checkpoint: ${String(err)}`);
    }
  });

  chatForm.addEventListener("submit", async (e) => {
    e.preventDefault();
    if (isTurnActive) return;

    const rawPrompt = chatInput.value.trim();
    if (!rawPrompt) return;

    let effectivePrompt = rawPrompt;
    const slashMatch = /^\/([a-zA-Z0-9_-]+)(?:\s+([\s\S]*))?$/.exec(rawPrompt);
    if (slashMatch) {
      const wfName = slashMatch[1].toLowerCase();
      const tailInput = (slashMatch[2] ?? "").trim();
      const wf = workspaceWorkflows.find((w) => w.name.toLowerCase() === wfName);
      if (wf) {
        if (wf.execution_mode === "plan_first") {
          setExecutionMode("plan_first");
        }
        if (wf.prompt_template.includes("{{input}}")) {
          effectivePrompt = wf.prompt_template.replaceAll("{{input}}", tailInput || "(workspace aktif)");
        } else {
          effectivePrompt = tailInput
            ? `${wf.prompt_template}\n\nKonteks Tambahan User:\n${tailInput}`
            : wf.prompt_template;
        }
      }
    }

    const attachmentsToSend = [...pendingAttachments];
    await runTurnFlow(false, {
      prompt: effectivePrompt,
      displayPrompt: rawPrompt,
      attachments: attachmentsToSend,
    });
  });
}
