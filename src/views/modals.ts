import { invoke } from "@tauri-apps/api/core";
import { closeTopLayer, copyToClipboard, $, setToast, setupDialogLightDismiss } from "../dom";
import { generateClientPreset } from "../presets";
import type {
  CommandPaletteAction,
  DashboardSnapshot,
  DiagnosticReport,
  DownstreamMcpConfigInput,
  HttpTrafficEntry,
  JobSummary,
  PendingOAuthInfo,
  PendingToolApproval,
} from "../types";

let activeOAuthRequestId: string | null = null;
const pendingOAuthQueue: PendingOAuthInfo[] = [];
let activeToolRequestId: string | null = null;
const pendingToolQueue: PendingToolApproval[] = [];
let activeLogJobId: string | null = null;
let pendingForceRevertAuditId: string | null = null;
let revertConflictConfirmHandler: ((auditId: string) => Promise<void>) | null = null;

let paletteActionsProvider: (() => CommandPaletteAction[]) | null = null;
let filteredPaletteActions: CommandPaletteAction[] = [];
let selectedPaletteIdx = 0;

export function enqueueOAuthConsent(info: PendingOAuthInfo): void {
  if (
    activeOAuthRequestId === info.request_id ||
    pendingOAuthQueue.some((q) => q.request_id === info.request_id)
  ) {
    return;
  }
  pendingOAuthQueue.push(info);
  if (!activeOAuthRequestId) {
    showNextOAuthConsent();
  }
}

export function handleOAuthResolvedEvent(resolvedId: string): void {
  const idx = pendingOAuthQueue.findIndex((q) => q.request_id === resolvedId);
  if (idx !== -1) {
    pendingOAuthQueue.splice(idx, 1);
  }
  if (activeOAuthRequestId === resolvedId) {
    activeOAuthRequestId = null;
    const dialog = $("oauth-consent-dialog") as HTMLDialogElement;
    if (dialog.open) {
      dialog.close();
    }
    showNextOAuthConsent();
  }
}

function showNextOAuthConsent(): void {
  const dialog = $("oauth-consent-dialog") as HTMLDialogElement;
  if (activeOAuthRequestId || pendingOAuthQueue.length === 0) {
    return;
  }
  const next = pendingOAuthQueue.shift()!;
  activeOAuthRequestId = next.request_id;
  $("oauth-req-client").textContent = next.client_name
    ? `${next.client_name} (${next.client_id})`
    : next.client_id || "(unregistered client)";
  $("oauth-req-redirect").textContent = next.redirect_uri;
  $("oauth-req-scope").textContent = next.scope || "mcp";

  if (!dialog.open) {
    dialog.showModal();
  }
}

async function submitOAuthDecision(
  decision: "approve_once" | "approve_and_trust" | "deny",
  onSnapshot: (snap: DashboardSnapshot) => void
): Promise<void> {
  const reqId = activeOAuthRequestId;
  const dialog = $("oauth-consent-dialog") as HTMLDialogElement;
  if (dialog.open) {
    dialog.close();
  }
  activeOAuthRequestId = null;

  if (reqId) {
    try {
      const snap = await invoke<DashboardSnapshot>("resolve_oauth_consent", {
        requestId: reqId,
        decision,
      });
      onSnapshot(snap);
      const label =
        decision === "deny"
          ? "Permintaan OAuth ditolak."
          : decision === "approve_and_trust"
            ? "Client OAuth disetujui dan dipercaya permanen."
            : "Permintaan OAuth disetujui sekali.";
      setToast(label);
    } catch (err) {
      setToast(`OAuth consent: ${String(err)}`);
    }
  }

  showNextOAuthConsent();
}

export function enqueueToolApproval(info: PendingToolApproval): void {
  if (
    activeToolRequestId === info.request_id ||
    pendingToolQueue.some((q) => q.request_id === info.request_id)
  ) {
    return;
  }
  pendingToolQueue.push(info);
  if (!activeToolRequestId) {
    showNextToolApproval();
  }
}

export function handleToolApprovalResolvedEvent(resolvedId: string): void {
  const idx = pendingToolQueue.findIndex((q) => q.request_id === resolvedId);
  if (idx !== -1) {
    pendingToolQueue.splice(idx, 1);
  }
  if (activeToolRequestId === resolvedId) {
    activeToolRequestId = null;
    const dialog = $("tool-approval-dialog") as HTMLDialogElement;
    if (dialog.open) {
      dialog.close();
    }
    showNextToolApproval();
  }
}

function showNextToolApproval(): void {
  const dialog = $("tool-approval-dialog") as HTMLDialogElement;
  if (activeToolRequestId || pendingToolQueue.length === 0) {
    return;
  }
  const next = pendingToolQueue.shift()!;
  activeToolRequestId = next.request_id;
  $("tool-req-source").textContent = next.source;
  $("tool-req-name").textContent = next.tool_name;
  $("tool-req-summary").textContent = next.summary;
  $("tool-req-args").textContent = next.args_preview || "{}";
  if (!dialog.open) {
    dialog.showModal();
  }
}

async function submitToolDecision(
  decision: "approve_once" | "approve_for_session" | "deny",
  onSnapshot: (snap: DashboardSnapshot) => void
): Promise<void> {
  const reqId = activeToolRequestId;
  const dialog = $("tool-approval-dialog") as HTMLDialogElement;
  if (dialog.open) {
    dialog.close();
  }
  activeToolRequestId = null;

  if (reqId) {
    try {
      const snap = await invoke<DashboardSnapshot>("resolve_tool_approval", {
        requestId: reqId,
        decision,
      });
      onSnapshot(snap);
      const label =
        decision === "deny"
          ? "Eksekusi tool ditolak."
          : decision === "approve_for_session"
            ? "Tool disetujui untuk seluruh sesi ini."
            : "Eksekusi tool disetujui sekali.";
      setToast(label);
    } catch (err) {
      setToast(`Tool approval: ${String(err)}`);
    }
  }

  showNextToolApproval();
}

export function openRevertConflictModal(
  auditId: string,
  message: string,
  onConfirmForce: (id: string) => Promise<void>
): void {
  pendingForceRevertAuditId = auditId;
  revertConflictConfirmHandler = onConfirmForce;
  $("revert-conflict-message").textContent = message.replace(/^CONFLICT:\s*/, "");
  ($("revert-conflict-dialog") as HTMLDialogElement).showModal();
}

export function openTrafficInspectorModal(entry: HttpTrafficEntry): void {
  const rpcPart = entry.rpc_method ? ` (${entry.rpc_method})` : "";
  $("traffic-insp-meta").textContent =
    `${entry.timestamp} · ${entry.method} ${entry.path}${rpcPart} · HTTP ${entry.status} · ${entry.duration_ms}ms · ${entry.note}`;
  $("traffic-insp-req").textContent = entry.request_preview || "(tidak ada payload request)";
  $("traffic-insp-res").textContent = entry.response_preview || "(tidak ada payload response)";
  ($("traffic-inspector-dialog") as HTMLDialogElement).showModal();
}

export function openPresetPreviewModal(
  presetId: string,
  getSnap: () => DashboardSnapshot | null
): void {
  const snap = getSnap();
  if (!snap) return;

  const tunnelToggle = $("preset-modal-use-tunnel") as HTMLInputElement;
  tunnelToggle.disabled = !snap.tunnel_url;
  tunnelToggle.checked = Boolean(snap.tunnel_url);

  const renderPresetContent = (): void => {
    const latestSnap = getSnap();
    if (!latestSnap) return;
    const generated = generateClientPreset(presetId, latestSnap, tunnelToggle.checked);
    $("preset-modal-title").textContent = generated.title;
    $("preset-modal-file").textContent = generated.filename;
    $("preset-modal-desc").textContent = generated.instructions;
    $("preset-modal-code").textContent = generated.content;
  };

  tunnelToggle.onchange = renderPresetContent;
  renderPresetContent();

  ($("preset-preview-dialog") as HTMLDialogElement).showModal();
}

export function openDiagnosticModal(report: DiagnosticReport): void {
  const grid = $("diagnostic-checks-grid");
  grid.innerHTML = "";

  const checks: Array<{ label: string; ok: boolean; value: string }> = [
    {
      label: "MCP Server",
      ok: report.server_running,
      value: report.server_running ? `ONLINE (Port ${report.port})` : "OFFLINE",
    },
    {
      label: "GET /health",
      ok: report.health_ok,
      value: report.health_ok ? "OK" : "FAIL",
    },
    {
      label: "MCP initialize",
      ok: report.mcp_initialize_ok,
      value: report.mcp_initialize_ok ? "OK" : "FAIL",
    },
    {
      label: "Tools Exposed",
      ok: report.tools_count >= 14,
      value:
        report.tools_count > 14
          ? `${report.tools_count} tools (14 + ${report.tools_count - 14} hub)`
          : `${report.tools_count} / 14 tools`,
    },
    {
      label: "Loopback Latency",
      ok: report.latency_ms < 500,
      value: `${report.latency_ms} ms`,
    },
    {
      label: "Remote Tunnel",
      ok: report.tunnel_active && Boolean(report.tunnel_url),
      value: report.tunnel_active ? (report.tunnel_url ? "CONNECTED" : "STARTING") : "IDLE",
    },
  ];

  for (const chk of checks) {
    const item = document.createElement("div");
    item.className = "diag-item";
    const lbl = document.createElement("span");
    lbl.className = "kv-label";
    lbl.textContent = chk.label;
    const val = document.createElement("span");
    val.className = `status-dot ${chk.ok ? "status-online" : "status-warn"}`;
    val.textContent = chk.value;
    item.append(lbl, val);
    grid.appendChild(item);
  }

  const notesList = $("diagnostic-notes-list");
  notesList.innerHTML = "";
  for (const note of report.notes) {
    const li = document.createElement("li");
    li.textContent = note;
    notesList.appendChild(li);
  }

  ($("diagnostic-dialog") as HTMLDialogElement).showModal();
}

async function refreshActiveJobLogs(): Promise<void> {
  if (!activeLogJobId) return;
  try {
    const raw = await invoke<string>("get_background_job_logs", { jobId: activeLogJobId });
    $("job-logs-pre").textContent = raw || "(belum ada output)";
  } catch (err) {
    $("job-logs-pre").textContent = `Gagal memuat log: ${String(err)}`;
  }
}

export async function openJobLogsModal(job: JobSummary): Promise<void> {
  activeLogJobId = job.job_id;
  $("job-logs-title").textContent = `Job Output: ${job.label || job.job_id}`;
  $("job-logs-meta").textContent = `${job.job_id} · ${job.command} · Status: ${job.status} (${job.elapsed_secs}s)`;
  $("job-logs-pre").textContent = "Memuat log...";
  ($("job-logs-dialog") as HTMLDialogElement).showModal();
  await refreshActiveJobLogs();
}

export function openAddMcpModal(): void {
  const form = $("add-mcp-form") as HTMLFormElement;
  form.reset();
  ($("mcp-transport") as HTMLSelectElement).value = "http";
  $("mcp-http-fields").hidden = false;
  $("mcp-stdio-fields").hidden = true;
  ($("add-mcp-dialog") as HTMLDialogElement).showModal();
}

function renderPaletteList(): void {
  const list = $("palette-results");
  list.innerHTML = "";

  if (filteredPaletteActions.length === 0) {
    const empty = document.createElement("div");
    empty.className = "palette-empty";
    empty.textContent = "Tidak ada perintah yang cocok.";
    list.appendChild(empty);
    return;
  }

  if (selectedPaletteIdx >= filteredPaletteActions.length) {
    selectedPaletteIdx = 0;
  }

  filteredPaletteActions.forEach((act, idx) => {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = `palette-item ${idx === selectedPaletteIdx ? "selected" : ""}`;
    btn.addEventListener("click", () => {
      executePaletteAction(act);
    });

    const lbl = document.createElement("span");
    lbl.className = "palette-label";
    lbl.textContent = act.label;

    const hint = document.createElement("span");
    hint.className = "palette-hint";
    hint.textContent = act.hint;

    btn.append(lbl, hint);
    list.appendChild(btn);
  });
}

function executePaletteAction(act: CommandPaletteAction): void {
  const dialog = $("command-palette-dialog") as HTMLDialogElement;
  void closeTopLayer(dialog);
  void act.run();
}

export function openCommandPalette(): void {
  if (!paletteActionsProvider) return;
  const dialog = $("command-palette-dialog") as HTMLDialogElement;
  const input = $("palette-search-input") as HTMLInputElement;
  input.value = "";
  selectedPaletteIdx = 0;
  filteredPaletteActions = paletteActionsProvider();
  renderPaletteList();
  if (!dialog.open) {
    dialog.showModal();
  }
  input.focus();
}

export function initModals(
  onSnapshot: (snap: DashboardSnapshot) => void,
  actionsProvider: () => CommandPaletteAction[]
): void {
  paletteActionsProvider = actionsProvider;
  document.querySelectorAll<HTMLDialogElement>("dialog").forEach(setupDialogLightDismiss);

  const oauthDialog = $("oauth-consent-dialog") as HTMLDialogElement;
  $("btn-oauth-deny").addEventListener("click", () => void submitOAuthDecision("deny", onSnapshot));
  $("btn-oauth-once").addEventListener(
    "click",
    () => void submitOAuthDecision("approve_once", onSnapshot)
  );
  $("btn-oauth-trust").addEventListener(
    "click",
    () => void submitOAuthDecision("approve_and_trust", onSnapshot)
  );
  oauthDialog.addEventListener("cancel", (e) => {
    e.preventDefault();
    void submitOAuthDecision("deny", onSnapshot);
  });

  const revertDialog = $("revert-conflict-dialog") as HTMLDialogElement;
  $("btn-revert-cancel").addEventListener("click", () => {
    pendingForceRevertAuditId = null;
    if (revertDialog.open) revertDialog.close();
  });
  revertDialog.addEventListener("cancel", () => {
    pendingForceRevertAuditId = null;
  });
  $("btn-revert-force").addEventListener("click", async () => {
    const targetId = pendingForceRevertAuditId;
    const handler = revertConflictConfirmHandler;
    pendingForceRevertAuditId = null;
    revertConflictConfirmHandler = null;
    if (revertDialog.open) revertDialog.close();
    if (targetId && handler) {
      await handler(targetId);
    }
  });

  const trafficDialog = $("traffic-inspector-dialog") as HTMLDialogElement;
  $("btn-close-traffic-insp").addEventListener("click", () => {
    void closeTopLayer(trafficDialog);
  });
  $("btn-copy-insp-req").addEventListener("click", () => {
    void copyToClipboard($("traffic-insp-req").textContent ?? "", "Request Payload");
  });
  $("btn-copy-insp-res").addEventListener("click", () => {
    void copyToClipboard($("traffic-insp-res").textContent ?? "", "Response Payload");
  });

  const presetDialog = $("preset-preview-dialog") as HTMLDialogElement;
  $("btn-close-preset-modal").addEventListener("click", () => {
    void closeTopLayer(presetDialog);
  });
  $("btn-preset-modal-copy").addEventListener("click", () => {
    void copyToClipboard($("preset-modal-code").textContent ?? "", "Preset Config");
  });

  const diagDialog = $("diagnostic-dialog") as HTMLDialogElement;
  $("btn-close-diag").addEventListener("click", () => {
    void closeTopLayer(diagDialog);
  });

  const toolDialog = $("tool-approval-dialog") as HTMLDialogElement;
  $("btn-tool-deny").addEventListener("click", () => void submitToolDecision("deny", onSnapshot));
  $("btn-tool-once").addEventListener(
    "click",
    () => void submitToolDecision("approve_once", onSnapshot)
  );
  $("btn-tool-session").addEventListener(
    "click",
    () => void submitToolDecision("approve_for_session", onSnapshot)
  );
  toolDialog.addEventListener("cancel", (e) => {
    e.preventDefault();
    void submitToolDecision("deny", onSnapshot);
  });

  const jobLogsDialog = $("job-logs-dialog") as HTMLDialogElement;
  $("btn-refresh-job-logs").addEventListener("click", () => void refreshActiveJobLogs());
  $("btn-copy-job-logs").addEventListener("click", () => {
    void copyToClipboard($("job-logs-pre").textContent ?? "", "Job Logs");
  });
  $("btn-close-job-logs").addEventListener("click", () => {
    activeLogJobId = null;
    void closeTopLayer(jobLogsDialog);
  });
  jobLogsDialog.addEventListener("cancel", () => {
    activeLogJobId = null;
  });

  const addMcpDialog = $("add-mcp-dialog") as HTMLDialogElement;
  const mcpTransportSel = $("mcp-transport") as HTMLSelectElement;
  mcpTransportSel.addEventListener("change", () => {
    const isStdio = mcpTransportSel.value === "stdio";
    $("mcp-http-fields").hidden = isStdio;
    $("mcp-stdio-fields").hidden = !isStdio;
  });
  $("btn-cancel-add-mcp").addEventListener("click", () => {
    void closeTopLayer(addMcpDialog);
  });
  ($("add-mcp-form") as HTMLFormElement).addEventListener("submit", async (e) => {
    e.preventDefault();
    const name = ($("mcp-name") as HTMLInputElement).value.trim();
    const namespace = ($("mcp-namespace") as HTMLInputElement).value.trim();
    const transport = mcpTransportSel.value === "stdio" ? "stdio" : "http";
    const url = ($("mcp-url") as HTMLInputElement).value.trim();
    const bearer = ($("mcp-bearer") as HTMLInputElement).value.trim();
    const command = ($("mcp-command") as HTMLInputElement).value.trim();
    const rawArgs = ($("mcp-args") as HTMLInputElement).value.trim();
    const args = rawArgs
      ? rawArgs
          .split(/[\s,]+/)
          .map((s) => s.trim())
          .filter(Boolean)
      : [];

    const cfg: DownstreamMcpConfigInput = {
      id: "",
      name,
      namespace,
      transport,
      url,
      bearer_token: bearer,
      command,
      args,
      env: {},
      enabled: true,
    };

    const submitBtn = $("btn-save-add-mcp") as HTMLButtonElement;
    submitBtn.disabled = true;
    try {
      const snap = await invoke<DashboardSnapshot>("upsert_downstream_mcp", { cfg });
      onSnapshot(snap);
      void closeTopLayer(addMcpDialog);
      setToast(`Downstream MCP '${name}' disimpan.`);
    } catch (err) {
      setToast(`Gagal menyimpan downstream MCP: ${String(err)}`);
    } finally {
      submitBtn.disabled = false;
    }
  });

  const paletteDialog = $("command-palette-dialog") as HTMLDialogElement;
  const paletteInput = $("palette-search-input") as HTMLInputElement;
  $("btn-close-palette").addEventListener("click", () => {
    void closeTopLayer(paletteDialog);
  });
  paletteInput.addEventListener("input", () => {
    const q = paletteInput.value.trim().toLowerCase();
    const all = paletteActionsProvider ? paletteActionsProvider() : [];
    filteredPaletteActions = q
      ? all.filter(
          (a) => a.label.toLowerCase().includes(q) || a.hint.toLowerCase().includes(q)
        )
      : all;
    selectedPaletteIdx = 0;
    renderPaletteList();
  });
  paletteInput.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      if (filteredPaletteActions.length > 0) {
        selectedPaletteIdx = (selectedPaletteIdx + 1) % filteredPaletteActions.length;
        renderPaletteList();
      }
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (filteredPaletteActions.length > 0) {
        selectedPaletteIdx =
          (selectedPaletteIdx - 1 + filteredPaletteActions.length) %
          filteredPaletteActions.length;
        renderPaletteList();
      }
    } else if (e.key === "Enter") {
      e.preventDefault();
      const target = filteredPaletteActions[selectedPaletteIdx];
      if (target) executePaletteAction(target);
    }
  });
}
