import { invoke } from "@tauri-apps/api/core";
import { $, setToast } from "../dom";
import { renderMarkdownInto } from "../markdown";
import type {
  AgentAttachment,
  AgentProvider,
  AgentTurnResponse,
  ChatMessage,
  ChatSession,
  DashboardSnapshot,
  GeminiModelInfo,
  StoredChatMessage,
} from "../types";

const LEGACY_STORAGE_KEYS = [
  "smart_mcp_hub_gemini_api_key",
  "smart_hub_gemini_key",
  "smart_mcp_hub_gemini_model",
  "smart_mcp_hub_gemini_custom_model",
];

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

const fetchedModelsByProvider: Partial<Record<AgentProvider, GeminiModelInfo[]>> = {};
const chatHistory: ChatMessage[] = [];
const pendingAttachments: AgentAttachment[] = [];

let lastSnap: DashboardSnapshot | null = null;
let loadedWorkspaceForSessions: string | null = null;
let workspaceSessions: ChatSession[] = [];
let currentSessionId: string | null = null;
let currentSessionCreatedAt: string | null = null;
let lastRenderedProvider: AgentProvider | null = null;

let activeStreamState: {
  thoughtText: string;
  bodyText: string;
  thoughtBox: HTMLDetailsElement;
  thoughtPre: HTMLElement;
  bodyEl: HTMLElement;
  transcriptEl: HTMLElement;
} | null = null;
let isTurnActive = false;

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
    chip.textContent = att.kind === "file" ? `@${att.path ?? att.name}` : `[img] ${att.name}`;

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

function renderChat(): void {
  const container = $("chat-messages");
  container.innerHTML = "";

  if (chatHistory.length === 0) {
    const empty = document.createElement("div");
    empty.className = "chat-empty";
    empty.textContent =
      "Konfigurasi provider AI melalui tombol config di kanan atas (disimpan aman dengan Windows DPAPI), lalu ketik tugas coding untuk workspace aktif.";
    container.appendChild(empty);
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
      details.className = "thought-box";
      const sum = document.createElement("summary");
      sum.textContent = "Thinking Process";
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
        const badge = document.createElement("span");
        badge.className = "step-badge";
        badge.textContent = `${s.tool_name} (-${s.tokens_saved} tok)`;
        badge.title = s.summary;
        stepsWrap.appendChild(badge);
      }
      card.appendChild(stepsWrap);
    }

    container.appendChild(card);
  }

  container.scrollTop = container.scrollHeight;
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

  if (currentSessionId && workspaceSessions.some((s) => s.id === currentSessionId)) {
    sel.value = currentSessionId;
    delBtn.disabled = false;
  } else {
    sel.value = "";
    delBtn.disabled = true;
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

async function persistActiveChatSession(): Promise<void> {
  if (chatHistory.length === 0) return;
  const nowIso = new Date().toISOString();
  if (!currentSessionId) {
    currentSessionId = `chat-${Date.now().toString(36)}`;
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
    created_at: currentSessionCreatedAt ?? nowIso,
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
  currentSessionId = null;
  currentSessionCreatedAt = null;
  chatHistory.length = 0;
  pendingAttachments.length = 0;
  renderAttachmentChips();
  renderChat();
  renderSessionSelector();
  await invoke("clear_agent_history").catch(() => {});
}

export function renderAgentView(snap: DashboardSnapshot): void {
  lastSnap = snap;
  $("active-tools-heading").textContent = `Smart Tools Aktif (${14 + (snap.aggregated_tools_count ?? 0)})`;

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

  if (loadedWorkspaceForSessions !== snap.workspace_root) {
    const switchedFromAnother = loadedWorkspaceForSessions !== null;
    loadedWorkspaceForSessions = snap.workspace_root;
    if (switchedFromAnother) {
      currentSessionId = null;
      currentSessionCreatedAt = null;
      chatHistory.length = 0;
      renderChat();
      void invoke("clear_agent_history").catch(() => {});
    }
    void loadWorkspaceSessions();
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
  thoughtBox.className = "thought-box";
  thoughtBox.hidden = true;
  const sum = document.createElement("summary");
  sum.textContent = "Thinking Process";
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
    }).catch(() => {});
  }
  for (const k of LEGACY_STORAGE_KEYS) {
    localStorage.removeItem(k);
  }

  const providerSel = $("agent-provider") as HTMLSelectElement;
  const modelSel = $("agent-model") as HTMLSelectElement;

  const toggleDrawerBtn = $("btn-toggle-drawer");
  toggleDrawerBtn.addEventListener("click", () => {
    const shell = $("agent-shell");
    const open = shell.classList.toggle("drawer-open");
    toggleDrawerBtn.setAttribute("aria-expanded", open ? "true" : "false");
    $("agent-drawer").toggleAttribute("inert", !open);
  });

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

    const snap = await invoke<DashboardSnapshot>("configure_agent_provider", {
      provider: prov,
      apiKey,
      baseUrl,
      model: targetModel,
      autoInjectProjectRules: autoRules,
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
    await invoke("clear_agent_history").catch(() => {});
  });

  $("btn-delete-session").addEventListener("click", async () => {
    if (!currentSessionId) return;
    try {
      workspaceSessions = await invoke<ChatSession[]>("delete_chat_session", {
        sessionId: currentSessionId,
      });
      await startNewChatSession();
      setToast("Sesi chat dihapus.");
    } catch (err) {
      setToast(`Gagal menghapus sesi: ${String(err)}`);
    }
  });

  // File & Image Attachments
  const addPathAttachment = (): void => {
    const input = $("attach-file-path") as HTMLInputElement;
    const raw = input.value.trim().replace(/^@+/, "");
    if (!raw) return;
    pendingAttachments.push({
      kind: "file",
      name: raw,
      path: raw,
    });
    input.value = "";
    renderAttachmentChips();
  };

  $("btn-attach-file").addEventListener("click", addPathAttachment);
  ($("attach-file-path") as HTMLInputElement).addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      addPathAttachment();
    }
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
      chatInput.focus();
    });
  });

  const stopBtn = $("btn-stop-agent") as HTMLButtonElement;
  stopBtn.addEventListener("click", async () => {
    stopBtn.disabled = true;
    await invoke("cancel_local_agent").catch(() => {});
    $("agent-run-status").textContent = "Membatalkan...";
  });

  const chatForm = $("chat-form") as HTMLFormElement;
  chatInput.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      chatForm.requestSubmit();
    }
  });

  chatForm.addEventListener("submit", async (e) => {
    e.preventDefault();
    if (isTurnActive) return;

    const prompt = chatInput.value.trim();
    if (!prompt) return;

    const attachmentsToSend = [...pendingAttachments];
    const targetModel = getSelectedModelId();

    try {
      await saveAgentConfig();
    } catch (err) {
      setToast(`Config error: ${String(err)}`);
      return;
    }

    const attSuffix =
      attachmentsToSend.length > 0
        ? `\n\n[Attachments: ${attachmentsToSend.map((a) => (a.kind === "file" ? `@${a.path ?? a.name}` : a.name)).join(", ")}]`
        : "";

    chatHistory.push({
      role: "user",
      content: `${prompt}${attSuffix}`,
      timestamp: new Date().toLocaleTimeString(),
    });
    chatInput.value = "";
    pendingAttachments.length = 0;
    renderAttachmentChips();
    renderChat();
    createStreamingCard();

    const sendBtn = $("btn-send-chat") as HTMLButtonElement;
    isTurnActive = true;
    sendBtn.disabled = true;
    stopBtn.hidden = false;
    stopBtn.disabled = false;
    $("agent-run-status").textContent = `Streaming ${targetModel}...`;

    try {
      const res = await invoke<AgentTurnResponse>("run_local_agent_turn", {
        prompt,
        attachments: attachmentsToSend,
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
      onSnapshot(res.snapshot);
      await persistActiveChatSession();
      $("agent-run-status").textContent = res.cancelled
        ? "Dibatalkan"
        : `Selesai (${res.steps.length} tool step)`;
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
      $("agent-run-status").textContent = "Error";
    } finally {
      isTurnActive = false;
      sendBtn.disabled = false;
      stopBtn.hidden = true;
      stopBtn.disabled = false;
    }
  });
}
