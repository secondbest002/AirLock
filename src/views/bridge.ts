import { invoke } from "@tauri-apps/api/core";
import { $, copyToClipboard, formatDurationCompact, maskToken, setToast } from "../dom";
import { generateClientPreset, MCP_CLIENT_PRESETS } from "../presets";
import type {
  DashboardSnapshot,
  DiagnosticReport,
  DownstreamMcpSummary,
  HttpTrafficEntry,
  JobSummary,
  LspServerSummary,
  OAuthClientSummary,
} from "../types";
import {
  openAddMcpModal,
  openDiagnosticModal,
  openJobLogsModal,
  openPresetPreviewModal,
  openTrafficInspectorModal,
} from "./modals";

const SIM_DEFAULTS: Record<string, string> = {
  list_workspaces: JSON.stringify({}, null, 2),
  project_overview: JSON.stringify({}, null, 2),
  smart_search: JSON.stringify({ query: "fn main", max_results: 20 }, null, 2),
  read_file_range: JSON.stringify({ path: "package.json", start_line: 1, end_line: 40 }, null, 2),
  batch_inspect: JSON.stringify(
    {
      include_overview: true,
      overview_depth: 2,
      outline_paths: ["src/main.ts"],
      searches: [{ query: "renderSnapshot", max_results: 10 }],
    },
    null,
    2
  ),
  surgical_edit: JSON.stringify(
    {
      path: "README.md",
      edits: [{ search: "old text", replace: "new text" }],
    },
    null,
    2
  ),
  multi_surgical_edit: JSON.stringify(
    {
      files: [
        { path: "README.md", edits: [{ search: "old", replace: "new" }] },
        { path: "notes/a.txt", edits: [{ search: "v1", replace: "v2" }] },
      ],
    },
    null,
    2
  ),
  patch_and_verify: JSON.stringify(
    {
      path: "src/main.ts",
      edits: [{ search: "old text", replace: "new text" }],
      verify_command: "npm run build",
    },
    null,
    2
  ),
  run_command_compressed: JSON.stringify({ command: "git status -s" }, null, 2),
  write_file_jailed: JSON.stringify(
    {
      path: "notes/scratch.txt",
      content: "hello from simulator\n",
    },
    null,
    2
  ),
  find_files: JSON.stringify({ pattern: "*.rs", max_results: 40 }, null, 2),
  code_outline: JSON.stringify({ path: "src/main.ts", max_symbols: 80 }, null, 2),
  read_symbol: JSON.stringify({ path: "src/main.ts", symbol: "init", include_docs: true }, null, 2),
  lsp_diagnostics: JSON.stringify({ path: "src/main.ts", severity: "all" }, null, 2),
  lsp_definition: JSON.stringify({ path: "src/main.ts", line: 1, character: 1 }, null, 2),
  lsp_references: JSON.stringify({ path: "src/main.ts", line: 1, character: 1, max_results: 20 }, null, 2),
  lsp_hover: JSON.stringify({ path: "src/main.ts", line: 1, character: 1 }, null, 2),
  git_ops: JSON.stringify({ op: "status" }, null, 2),
  file_ops: JSON.stringify({ action: "mkdir", path: "notes/tmp" }, null, 2),
  start_command_job: JSON.stringify({ command: "npm run build", label: "Frontend Build" }, null, 2),
  poll_command_job: JSON.stringify({ job_id: "job-..." }, null, 2),
  stop_command_job: JSON.stringify({ job_id: "job-..." }, null, 2),
  list_skills: JSON.stringify({}, null, 2),
  load_skill: JSON.stringify({ name: "tauri-security-audit" }, null, 2),
  read_skill_resource: JSON.stringify(
    { name: "tauri-security-audit", path: "references/checklist.md" },
    null,
    2
  ),
  upsert_artifact: JSON.stringify(
    {
      kind: "implementation_plan",
      title: "Architecture Plan",
      content_md: "# Plan\n1. Inspect codebase\n2. Implement fix\n",
    },
    null,
    2
  ),
  request_plan_approval: JSON.stringify(
    { summary: "Ready for review and approval" },
    null,
    2
  ),
  spawn_subagent: JSON.stringify(
    { goal: "Inspect security boundaries and error handling", max_steps: 4 },
    null,
    2
  ),
};

let secretsVisible = false;
let trafficSearchQuery = "";
let trafficStatusFilter = "all";

function renderSecrets(snap: DashboardSnapshot): void {
  const tokenEl = $("mcp-auth-token");
  tokenEl.textContent = secretsVisible ? snap.auth_token : maskToken(snap.auth_token);

  const tunnelUrlEl = $("public-tunnel-url");
  if (snap.tunnel_url) {
    tunnelUrlEl.textContent = secretsVisible
      ? `${snap.tunnel_url}/mcp/${snap.auth_token}`
      : `${snap.tunnel_url}/mcp/${maskToken(snap.auth_token)}`;
  } else {
    tunnelUrlEl.textContent = snap.tunnel_status;
  }

  const toggleBtn = $("btn-toggle-secrets");
  toggleBtn.setAttribute("aria-pressed", secretsVisible ? "true" : "false");
  toggleBtn.innerHTML = secretsVisible
    ? `<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19m-6.72-1.07a3 3 0 1 1-4.24-4.24"></path><line x1="1" y1="1" x2="23" y2="23"></line></svg> Hide`
    : `<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"></path><circle cx="12" cy="12" r="3"></circle></svg> Show`;
}

function updateCloudflareNamedVisibility(): void {
  const provider = ($("tunnel-provider-select") as HTMLSelectElement).value;
  $("cf-named-fields").hidden = provider !== "cloudflare_named";
}

function matchesTrafficFilter(item: HttpTrafficEntry): boolean {
  if (trafficStatusFilter === "2xx" && item.status >= 300) {
    return false;
  }
  if (trafficStatusFilter === "err" && item.status < 400) {
    return false;
  }
  const q = trafficSearchQuery.trim().toLowerCase();
  if (!q) return true;
  const hay = `${item.method} ${item.path} ${item.rpc_method ?? ""} ${item.note} ${item.status}`.toLowerCase();
  return hay.includes(q);
}

function buildTrafficRow(item: HttpTrafficEntry): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.className = "traffic-row-clickable";
  tr.tabIndex = 0;
  tr.title = "Klik untuk melihat raw request & response payload";
  const openModal = (): void => openTrafficInspectorModal(item);
  tr.addEventListener("click", openModal);
  tr.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      openModal();
    }
  });

  const tdTime = document.createElement("td");
  tdTime.textContent = item.timestamp;

  const tdMethod = document.createElement("td");
  tdMethod.textContent = item.method;

  const tdRpc = document.createElement("td");
  tdRpc.textContent = item.rpc_method ? `${item.path} · ${item.rpc_method}` : item.path;

  const tdStatus = document.createElement("td");
  tdStatus.textContent = String(item.status);
  tdStatus.className =
    item.status < 300 ? "http-ok" : item.status < 400 ? "http-warn" : "http-err";

  const tdDur = document.createElement("td");
  tdDur.textContent = `${item.duration_ms}ms`;

  const tdNote = document.createElement("td");
  tdNote.textContent = item.note;

  tr.append(tdTime, tdMethod, tdRpc, tdStatus, tdDur, tdNote);
  return tr;
}

function renderOAuthClients(
  clients: OAuthClientSummary[],
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const tbody = $("oauth-clients-body");
  tbody.innerHTML = "";

  const revokeAllBtn = $("btn-revoke-all-oauth") as HTMLButtonElement;
  revokeAllBtn.disabled = clients.length === 0;

  if (clients.length === 0) {
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 5;
    td.className = "traffic-empty";
    td.textContent = "Belum ada OAuth client yang terdaftar.";
    tr.appendChild(td);
    tbody.appendChild(tr);
    return;
  }

  for (const client of clients) {
    const tr = document.createElement("tr");

    const tdClient = document.createElement("td");
    const titleSpan = document.createElement("span");
    titleSpan.textContent = client.client_name ? client.client_name : client.client_id;
    tdClient.appendChild(titleSpan);
    if (client.client_name) {
      const idSub = document.createElement("span");
      idSub.className = "client-sub";
      idSub.textContent = client.client_id;
      tdClient.appendChild(idSub);
    }

    const tdTrust = document.createElement("td");
    const badge = document.createElement("span");
    badge.className = `status-dot ${client.trusted ? "status-online" : "status-warn"}`;
    badge.textContent = client.trusted ? "TRUSTED" : "ONCE";
    tdTrust.appendChild(badge);

    const tdTokens = document.createElement("td");
    tdTokens.textContent = `${client.active_access_tokens} access · ${client.active_refresh_tokens} refresh`;

    const tdRedirect = document.createElement("td");
    tdRedirect.textContent =
      client.redirect_uris.length > 0 ? client.redirect_uris.join(", ") : "—";

    const tdAction = document.createElement("td");
    const revokeBtn = document.createElement("button");
    revokeBtn.type = "button";
    revokeBtn.className = "btn btn-danger btn-xs";
    revokeBtn.textContent = "Revoke";
    revokeBtn.addEventListener("click", async () => {
      try {
        const snap = await invoke<DashboardSnapshot>("revoke_oauth_client", {
          clientId: client.client_id,
        });
        onSnapshot(snap);
        setToast(`Client OAuth ${client.client_id} dicabut.`);
      } catch (err) {
        setToast(`Gagal mencabut client: ${String(err)}`);
      }
    });
    tdAction.appendChild(revokeBtn);

    tr.append(tdClient, tdTrust, tdTokens, tdRedirect, tdAction);
    tbody.appendChild(tr);
  }
}

export function renderHttpTraffic(traffic: HttpTrafficEntry[]): void {
  const tbody = $("http-traffic-body") as HTMLTableSectionElement;
  const filtered = [...traffic].reverse().filter(matchesTrafficFilter);
  $("traffic-count-label").textContent =
    filtered.length === traffic.length
      ? `${traffic.length} requests`
      : `${filtered.length} / ${traffic.length} requests`;
  const railMon = document.getElementById("rail-monitor-count");
  if (railMon) railMon.textContent = String(traffic.length);

  tbody.innerHTML = "";

  if (filtered.length === 0) {
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 6;
    td.className = "traffic-empty";
    td.textContent =
      traffic.length === 0
        ? "Belum ada traffic HTTP masuk pada sesi ini."
        : "Tidak ada traffic yang cocok dengan filter.";
    tr.appendChild(td);
    tbody.appendChild(tr);
    return;
  }

  for (const item of filtered) {
    tbody.appendChild(buildTrafficRow(item));
  }
}

export function appendTrafficIncremental(
  snap: DashboardSnapshot | null,
  entry: HttpTrafficEntry
): void {
  if (snap) {
    snap.http_traffic.push(entry);
    if (snap.http_traffic.length > 60) {
      snap.http_traffic.splice(0, snap.http_traffic.length - 60);
    }
    const visibleCount = snap.http_traffic.filter(matchesTrafficFilter).length;
    $("traffic-count-label").textContent =
      visibleCount === snap.http_traffic.length
        ? `${snap.http_traffic.length} requests`
        : `${visibleCount} / ${snap.http_traffic.length} requests`;
    const railMon = document.getElementById("rail-monitor-count");
    if (railMon) railMon.textContent = String(snap.http_traffic.length);
  }

  if (!matchesTrafficFilter(entry)) {
    return;
  }

  const tbody = $("http-traffic-body") as HTMLTableSectionElement;
  const emptyCell = tbody.querySelector(".traffic-empty");
  if (emptyCell) {
    tbody.innerHTML = "";
  }

  tbody.prepend(buildTrafficRow(entry));
  while (tbody.rows.length > 60) {
    tbody.deleteRow(tbody.rows.length - 1);
  }
}

export function appendTunnelLogIncremental(
  snap: DashboardSnapshot | null,
  line: string
): void {
  if (snap) {
    snap.tunnel_logs.push(line);
    if (snap.tunnel_logs.length > 120) {
      snap.tunnel_logs.splice(0, snap.tunnel_logs.length - 120);
    }
  }
  const pre = $("tunnel-logs-pre");
  if (pre.textContent === "Belum ada log tunnel.") {
    pre.textContent = line;
  } else {
    pre.textContent = `${pre.textContent}\n${line}`;
  }
  pre.scrollTop = pre.scrollHeight;
}

const latestJobsById = new Map<string, JobSummary>();

function renderStatusbarJobs(jobs: JobSummary[]): void {
  const running = jobs.filter((j) => j.status === "running");
  const pill = document.getElementById("statusbar-jobs-pill");
  const pillText = document.getElementById("statusbar-jobs-text");
  if (pill && pillText) {
    pill.hidden = running.length === 0;
    if (running.length > 0) {
      const maxElapsed = Math.max(...running.map((j) => j.elapsed_secs));
      const totalLines = running.reduce((sum, j) => sum + j.line_count, 0);
      pillText.textContent =
        running.length === 1
          ? `1 job running · ${formatDurationCompact(maxElapsed)} · ${totalLines} lines`
          : `${running.length} jobs running · ${formatDurationCompact(maxElapsed)} · ${totalLines} lines`;
    }
  }

  const countBadge = document.getElementById("jobs-running-count");
  if (countBadge) {
    countBadge.textContent = `${running.length} active`;
    countBadge.className = `badge ${running.length > 0 ? "badge-warn" : "badge-muted"}`;
  }
}

function patchJobRow(tr: HTMLTableRowElement, job: JobSummary): void {
  const tdId = tr.cells[0];
  tdId.textContent = job.job_id;

  const tdCmd = tr.cells[1];
  const cmdHeader = tdCmd.querySelector<HTMLElement>('[data-role="job-cmd-header"]');
  const lastLineEl = tdCmd.querySelector<HTMLElement>('[data-role="job-last-line"]');
  if (cmdHeader) {
    cmdHeader.replaceChildren();
    const cmdText = job.label ? `${job.label} (${job.command})` : job.command;
    if (job.mcp_task_id) {
      const taskBadge = document.createElement("span");
      taskBadge.className = "badge badge-info mcp-task-badge";
      taskBadge.textContent = "MCP Task";
      taskBadge.title = `MCP Task: ${job.mcp_task_id}`;
      cmdHeader.appendChild(taskBadge);
      cmdHeader.appendChild(document.createTextNode(" "));
    }
    if (job.workspace_alias) {
      const wsPill = document.createElement("span");
      wsPill.className = "audit-ws-pill";
      wsPill.textContent = `@${job.workspace_alias}`;
      cmdHeader.appendChild(wsPill);
      cmdHeader.appendChild(document.createTextNode(" "));
    }
    const titleSpan = document.createElement("span");
    titleSpan.className = "job-cmd-title";
    titleSpan.textContent = cmdText;
    cmdHeader.appendChild(titleSpan);
  }
  if (lastLineEl) {
    const preview = job.last_line?.trim();
    if (preview) {
      lastLineEl.textContent = `↳ ${preview}`;
      lastLineEl.title = preview;
      lastLineEl.classList.remove("is-empty");
    } else {
      lastLineEl.textContent =
        job.status === "running" ? "↳ (menunggu output...)" : "↳ (tanpa output)";
      lastLineEl.title = "";
      lastLineEl.classList.add("is-empty");
    }
  }

  const tdStatus = tr.cells[2];
  const badge = tdStatus.querySelector<HTMLElement>('[data-role="job-status-badge"]');
  const idleHint = tdStatus.querySelector<HTMLElement>('[data-role="job-idle-hint"]');
  if (badge) {
    badge.className = `status-dot ${
      job.status === "running"
        ? "status-warn"
        : job.status === "completed"
          ? "status-online"
          : "status-offline"
    }`;
    badge.textContent = job.exit_code !== null ? `${job.status} (${job.exit_code})` : job.status;
  }
  const idleSecs = job.idle_secs ?? 0;
  const stallHint = job.stall_hint?.trim();
  if (idleHint) {
    if (job.status === "running") {
      idleHint.hidden = false;
      if (stallHint && idleSecs < 15) {
        idleHint.textContent = "lock/prompt!";
        idleHint.className = "job-idle-hint is-silent";
        idleHint.title = stallHint;
      } else if (idleSecs >= 15) {
        idleHint.textContent = `silent ${formatDurationCompact(idleSecs)}`;
        idleHint.className = "job-idle-hint is-silent";
        idleHint.title =
          stallHint ||
          "Belum ada baris output baru — kemungkinan sedang kompilasi berat atau menunggu prompt.";
      } else if (job.line_count > 0) {
        idleHint.textContent = "active";
        idleHint.className = "job-idle-hint is-active";
        idleHint.title = `Output terakhir ${idleSecs}s lalu`;
      } else {
        idleHint.textContent = "starting";
        idleHint.className = "job-idle-hint";
        idleHint.title = "Proses baru dijalankan";
      }
    } else {
      idleHint.hidden = true;
      idleHint.textContent = "";
    }
  }

  const tdRuntime = tr.cells[3];
  const runtimeText = tdRuntime.querySelector<HTMLElement>('[data-role="job-runtime-text"]');
  const progressTrack = tdRuntime.querySelector<HTMLElement>('[data-role="job-progress-track"]');
  const progressFill = tdRuntime.querySelector<HTMLElement>('[data-role="job-progress-fill"]');
  const timeoutSecs = job.timeout_secs ?? 0;
  if (runtimeText) {
    const elapsedPart =
      timeoutSecs > 0
        ? `${formatDurationCompact(job.elapsed_secs)} / ${formatDurationCompact(timeoutSecs)}`
        : formatDurationCompact(job.elapsed_secs);
    runtimeText.textContent = `${elapsedPart} · ${job.line_count} lines`;
  }
  if (progressTrack && progressFill) {
    const pct =
      job.status === "running"
        ? timeoutSecs > 0
          ? Math.min(100, Math.max(2, Math.round((job.elapsed_secs / timeoutSecs) * 100)))
          : 0
        : 100;
    progressTrack.hidden = job.status === "running" && timeoutSecs <= 0;
    progressTrack.title =
      timeoutSecs > 0
        ? `Durasi: ${job.elapsed_secs}s dari batas timeout ${timeoutSecs}s`
        : `Durasi: ${job.elapsed_secs}s`;
    progressFill.style.width = `${pct}%`;
    progressFill.className = `job-progress-fill ${
      job.status === "running"
        ? idleSecs >= 15
          ? "is-silent"
          : ""
        : job.status === "completed"
          ? "is-ok"
          : "is-err"
    }`.trim();
  }

  const tdAction = tr.cells[4];
  const logsBtn = tdAction.querySelector<HTMLButtonElement>('[data-role="btn-job-logs"]');
  const stopBtn = tdAction.querySelector<HTMLButtonElement>('[data-role="btn-job-stop"]');
  if (logsBtn) {
    logsBtn.textContent = job.status === "running" ? "Live Logs" : "Logs";
  }
  if (stopBtn) {
    stopBtn.hidden = job.status !== "running";
  }
}

function buildJobRow(
  job: JobSummary,
  onSnapshot: (snap: DashboardSnapshot) => void
): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.dataset.jobId = job.job_id;

  const tdId = document.createElement("td");
  tdId.className = "mono-cell";

  const tdCmd = document.createElement("td");
  tdCmd.className = "mono-cell";
  const cmdHeader = document.createElement("div");
  cmdHeader.dataset.role = "job-cmd-header";
  const lastLineEl = document.createElement("span");
  lastLineEl.className = "job-last-line";
  lastLineEl.dataset.role = "job-last-line";
  tdCmd.append(cmdHeader, lastLineEl);

  const tdStatus = document.createElement("td");
  const statusStack = document.createElement("div");
  statusStack.className = "job-status-stack";
  const badge = document.createElement("span");
  badge.dataset.role = "job-status-badge";
  const idleHint = document.createElement("span");
  idleHint.className = "job-idle-hint";
  idleHint.dataset.role = "job-idle-hint";
  statusStack.append(badge, idleHint);
  tdStatus.appendChild(statusStack);

  const tdRuntime = document.createElement("td");
  tdRuntime.className = "mono-cell";
  const runtimeWrap = document.createElement("div");
  runtimeWrap.className = "job-runtime-wrap";
  const runtimeText = document.createElement("span");
  runtimeText.className = "job-runtime-text";
  runtimeText.dataset.role = "job-runtime-text";
  const progressTrack = document.createElement("div");
  progressTrack.className = "job-progress-track";
  progressTrack.dataset.role = "job-progress-track";
  progressTrack.setAttribute("aria-hidden", "true");
  const progressFill = document.createElement("div");
  progressFill.className = "job-progress-fill";
  progressFill.dataset.role = "job-progress-fill";
  progressTrack.appendChild(progressFill);
  runtimeWrap.append(runtimeText, progressTrack);
  tdRuntime.appendChild(runtimeWrap);

  const tdAction = document.createElement("td");
  const actWrap = document.createElement("div");
  actWrap.className = "btn-row";

  const logsBtn = document.createElement("button");
  logsBtn.type = "button";
  logsBtn.className = "btn btn-secondary btn-xs";
  logsBtn.dataset.role = "btn-job-logs";
  logsBtn.addEventListener("click", () => {
    const latest = latestJobsById.get(job.job_id) ?? job;
    void openJobLogsModal(latest);
  });
  actWrap.appendChild(logsBtn);

  const stopBtn = document.createElement("button");
  stopBtn.type = "button";
  stopBtn.className = "btn btn-danger btn-xs";
  stopBtn.textContent = "Stop";
  stopBtn.dataset.role = "btn-job-stop";
  stopBtn.addEventListener("click", async () => {
    try {
      await invoke("stop_background_job", { jobId: job.job_id });
      const next = await invoke<DashboardSnapshot>("get_dashboard_state");
      onSnapshot(next);
      setToast(`Job ${job.job_id} dihentikan.`);
    } catch (err) {
      setToast(`Gagal stop job: ${String(err)}`);
    }
  });
  actWrap.appendChild(stopBtn);

  tdAction.appendChild(actWrap);
  tr.append(tdId, tdCmd, tdStatus, tdRuntime, tdAction);
  patchJobRow(tr, job);
  return tr;
}

function renderBackgroundJobs(
  jobs: JobSummary[],
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  latestJobsById.clear();
  for (const job of jobs) {
    latestJobsById.set(job.job_id, job);
  }
  renderStatusbarJobs(jobs);

  const tbody = $("jobs-body") as HTMLTableSectionElement;
  if (jobs.length === 0) {
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 5;
    td.className = "traffic-empty";
    td.textContent = "Belum ada background command job yang dijalankan.";
    tr.appendChild(td);
    tbody.replaceChildren(tr);
    return;
  }

  const existingRows = Array.from(tbody.rows);
  const sameOrder =
    existingRows.length === jobs.length &&
    existingRows.every((row, idx) => row.dataset.jobId === jobs[idx].job_id);

  if (sameOrder) {
    for (let i = 0; i < jobs.length; i++) {
      patchJobRow(existingRows[i], jobs[i]);
    }
    return;
  }

  const nextRows = jobs.map((job) => buildJobRow(job, onSnapshot));
  tbody.replaceChildren(...nextRows);
}

function renderActiveLspServers(
  servers: LspServerSummary[],
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const host = $("lsp-servers-list");
  host.replaceChildren();
  const stopAllBtn = $("btn-stop-all-lsp") as HTMLButtonElement;
  stopAllBtn.disabled = servers.length === 0;

  if (servers.length === 0) {
    const empty = document.createElement("div");
    empty.className = "traffic-empty";
    empty.textContent = "Belum ada LSP server aktif (lazy spawn saat tool lsp_* dipanggil).";
    host.appendChild(empty);
    return;
  }

  for (const srv of servers) {
    const chip = document.createElement("div");
    chip.className = "lsp-chip-card";

    const main = document.createElement("div");
    main.className = "lsp-chip-main";

    const wsPill = document.createElement("span");
    wsPill.className = "audit-ws-pill";
    wsPill.textContent = `@${srv.workspace_alias}`;

    const langCode = document.createElement("code");
    langCode.className = "md-inline-code";
    langCode.textContent = `${srv.lang_id} (${srv.command})`;

    const statusBadge = document.createElement("span");
    statusBadge.className = `badge ${srv.status === "indexing" ? "badge-warn" : "badge-ok"}`;
    statusBadge.textContent = srv.status;

    main.append(wsPill, langCode, statusBadge);

    const meta = document.createElement("div");
    meta.className = "lsp-chip-meta mono-muted";
    meta.textContent = `idle ${srv.idle_secs}s · ${srv.open_docs_count} docs · ${srv.diagnostics_count} diags`;

    const stopBtn = document.createElement("button");
    stopBtn.type = "button";
    stopBtn.className = "btn btn-danger btn-xs";
    stopBtn.textContent = "Stop LSP";
    stopBtn.addEventListener("click", async () => {
      stopBtn.disabled = true;
      try {
        const snap = await invoke<DashboardSnapshot>("stop_lsp_server", { key: srv.key });
        onSnapshot(snap);
        setToast(`LSP '${srv.lang_id}' (@${srv.workspace_alias}) dihentikan.`);
      } catch (err) {
        setToast(`Gagal menghentikan LSP: ${String(err)}`);
        stopBtn.disabled = false;
      }
    });

    chip.append(main, meta, stopBtn);
    host.appendChild(chip);
  }
}

export function updateJobIncremental(
  snap: DashboardSnapshot | null,
  job: JobSummary,
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  if (!snap) return;
  const idx = snap.background_jobs.findIndex((j) => j.job_id === job.job_id);
  if (idx >= 0) {
    snap.background_jobs[idx] = job;
  } else {
    snap.background_jobs.unshift(job);
  }
  renderBackgroundJobs(snap.background_jobs, onSnapshot);
}

function renderDownstreamMcps(
  servers: DownstreamMcpSummary[],
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const tbody = $("mcp-hub-body") as HTMLTableSectionElement;
  tbody.innerHTML = "";
  if (servers.length === 0) {
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 6;
    td.className = "traffic-empty";
    td.textContent = "Belum ada downstream MCP server terdaftar.";
    tr.appendChild(td);
    tbody.appendChild(tr);
    return;
  }

  for (const srv of servers) {
    const tr = document.createElement("tr");

    const tdName = document.createElement("td");
    const nameTitle = document.createElement("div");
    nameTitle.className = "client-name";
    nameTitle.textContent = srv.name;
    if (srv.plugin_id) {
      const plgBadge = document.createElement("span");
      plgBadge.className = "plugin-managed-pill";
      plgBadge.textContent = "Plugin";
      nameTitle.appendChild(document.createTextNode(" "));
      nameTitle.appendChild(plgBadge);
    }
    tdName.appendChild(nameTitle);
    const endpointSub = document.createElement("div");
    endpointSub.className = "client-sub";
    endpointSub.textContent =
      srv.transport === "stdio"
        ? `${srv.command} ${srv.args.join(" ")}`.trim()
        : srv.url;
    tdName.appendChild(endpointSub);
    if (srv.last_error) {
      const errDiv = document.createElement("div");
      errDiv.className = "mcp-err-note";
      errDiv.title = srv.last_error;
      errDiv.textContent = srv.last_error;
      tdName.appendChild(errDiv);
    }

    const tdNs = document.createElement("td");
    tdNs.className = "mono-cell";
    tdNs.textContent = `${srv.namespace}__`;

    const tdTr = document.createElement("td");
    tdTr.className = "mono-cell";
    tdTr.textContent = srv.transport.toUpperCase();

    const tdTools = document.createElement("td");
    tdTools.className = "mono-cell";
    tdTools.textContent = String(srv.tools_count);

    const tdStatus = document.createElement("td");
    const badge = document.createElement("span");
    badge.className = `status-dot ${
      srv.status === "connected"
        ? "status-online"
        : srv.status === "error"
          ? "status-offline"
          : "status-warn"
    }`;
    badge.textContent = srv.status.toUpperCase();
    tdStatus.appendChild(badge);

    const tdAct = document.createElement("td");
    const actWrap = document.createElement("div");
    actWrap.className = "btn-row";

    const refBtn = document.createElement("button");
    refBtn.type = "button";
    refBtn.className = "btn btn-secondary btn-xs";
    refBtn.textContent = "Refresh";
    refBtn.addEventListener("click", async () => {
      try {
        const next = await invoke<DashboardSnapshot>("refresh_downstream_mcp", { id: srv.id });
        onSnapshot(next);
        setToast(`Server '${srv.name}' diperbarui.`);
      } catch (err) {
        setToast(`Refresh gagal: ${String(err)}`);
      }
    });

    const delBtn = document.createElement("button");
    delBtn.type = "button";
    delBtn.className = "btn btn-danger btn-xs";
    delBtn.textContent = "Remove";
    if (srv.plugin_id) {
      delBtn.disabled = true;
      delBtn.title = "Dikelola oleh plugin — hapus lewat panel Plugins";
    } else {
      delBtn.addEventListener("click", async () => {
        try {
          const next = await invoke<DashboardSnapshot>("remove_downstream_mcp", { id: srv.id });
          onSnapshot(next);
          setToast(`Server '${srv.name}' dihapus.`);
        } catch (err) {
          setToast(`Remove gagal: ${String(err)}`);
        }
      });
    }

    actWrap.append(refBtn, delBtn);
    tdAct.appendChild(actWrap);
    tr.append(tdName, tdNs, tdTr, tdTools, tdStatus, tdAct);
    tbody.appendChild(tr);
  }
}

export function renderBridgeView(
  snap: DashboardSnapshot,
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const hubSub = document.getElementById("hub-tools-subtitle");
  if (hubSub) {
    const nsCode = document.createElement("code");
    nsCode.className = "md-inline-code";
    nsCode.textContent = "namespace__tool";
    hubSub.replaceChildren(
      document.createTextNode("Gabungkan downstream MCP servers ("),
      nsCode,
      document.createTextNode(
        `), kelola Agent Plugins & Skills, dan uji ${snap.builtin_tools_count ?? 20} tool hemat token.`
      )
    );
  }
  $("local-mcp-url").textContent = `http://127.0.0.1:${snap.server_port}/mcp`;

  const portInput = $("server-port-input") as HTMLInputElement;
  if (document.activeElement !== portInput) {
    portInput.value = String(snap.server_port);
  }

  renderSecrets(snap);

  const btnServer = $("btn-toggle-server");
  btnServer.textContent = snap.server_running ? "Stop Server" : "Start Server";
  btnServer.className = `btn ${snap.server_running ? "btn-danger" : "btn-primary"}`;

  const btnTunnel = $("btn-toggle-tunnel");
  btnTunnel.textContent = snap.tunnel_running ? "Stop Tunnel" : "Start Tunnel";
  btnTunnel.className = `btn ${snap.tunnel_running ? "btn-danger" : "btn-secondary"}`;

  const providerSel = $("tunnel-provider-select") as HTMLSelectElement;
  if (snap.tunnel_running || document.activeElement !== providerSel) {
    providerSel.value = snap.tunnel_provider;
  }
  providerSel.disabled = snap.tunnel_running;
  updateCloudflareNamedVisibility();

  const cfTokenInput = $("cf-named-token") as HTMLInputElement;
  cfTokenInput.placeholder = snap.has_cloudflare_token
    ? "Token tersimpan (isi baru untuk mengganti)"
    : "eyJhIjoi...";
  const cfUrlInput = $("cf-named-url") as HTMLInputElement;
  if (document.activeElement !== cfUrlInput) {
    cfUrlInput.value = snap.cloudflare_public_url;
  }

  const logsPre = $("tunnel-logs-pre");
  logsPre.textContent =
    snap.tunnel_logs.length > 0 ? snap.tunnel_logs.join("\n") : "Belum ada log tunnel.";
  logsPre.scrollTop = logsPre.scrollHeight;

  ($("idle-timeout-select") as HTMLSelectElement).value = String(snap.idle_timeout_mins);
  ($("toggle-close-to-tray") as HTMLInputElement).checked = snap.close_to_tray;
  ($("toggle-sensitive-guard") as HTMLInputElement).checked = !snap.allow_sensitive_reads;

  const cmdPolicy = snap.command_execution_policy ?? "guarded";
  ($("command-policy-select") as HTMLSelectElement).value = cmdPolicy;
  $("command-allowlist-box").hidden = cmdPolicy !== "allowlist";
  const allowlistInput = $("command-allowlist-input") as HTMLTextAreaElement;
  if (document.activeElement !== allowlistInput) {
    allowlistInput.value = (snap.command_allowlist ?? []).join(", ");
  }

  renderOAuthClients(snap.oauth_clients ?? [], onSnapshot);
  renderActiveLspServers(snap.active_lsp_servers ?? [], onSnapshot);
  renderBackgroundJobs(snap.background_jobs ?? [], onSnapshot);
  renderDownstreamMcps(snap.downstream_mcps ?? [], onSnapshot);
  renderHttpTraffic(snap.http_traffic ?? []);
}

export function initBridgeView(
  getSnap: () => DashboardSnapshot | null,
  onSnapshot: (snap: DashboardSnapshot) => void,
  onRefresh: () => Promise<void>
): void {
  const presetSel = $("mcp-preset-select") as HTMLSelectElement;
  presetSel.innerHTML = "";
  for (const opt of MCP_CLIENT_PRESETS) {
    const el = document.createElement("option");
    el.value = opt.id;
    el.textContent = opt.label;
    presetSel.appendChild(el);
  }

  $("btn-toggle-server").addEventListener("click", async () => {
    try {
      const running = getSnap()?.server_running ?? false;
      const snap = await invoke<DashboardSnapshot>(running ? "stop_mcp_server" : "start_mcp_server");
      onSnapshot(snap);
      setToast(
        snap.server_running
          ? `MCP Server aktif di 127.0.0.1:${snap.server_port}`
          : "MCP Server dimatikan."
      );
    } catch (err) {
      setToast(`Error server: ${String(err)}`);
    }
  });

  $("btn-apply-port").addEventListener("click", async () => {
    const portVal = Number(($("server-port-input") as HTMLInputElement).value);
    if (!Number.isInteger(portVal) || portVal < 1024 || portVal > 65535) {
      setToast("Port harus berupa angka 1024–65535.");
      return;
    }
    try {
      const snap = await invoke<DashboardSnapshot>("set_server_port", { port: portVal });
      onSnapshot(snap);
      setToast(`MCP Server dipindahkan ke port ${snap.server_port}.`);
    } catch (err) {
      setToast(`Gagal mengubah port: ${String(err)}`);
    }
  });

  $("tunnel-provider-select").addEventListener("change", () => {
    updateCloudflareNamedVisibility();
  });

  $("btn-save-cf-named").addEventListener("click", async () => {
    const tokenEl = $("cf-named-token") as HTMLInputElement;
    const urlEl = $("cf-named-url") as HTMLInputElement;
    try {
      const snap = await invoke<DashboardSnapshot>("configure_cloudflare_named", {
        token: tokenEl.value.trim(),
        publicUrl: urlEl.value.trim(),
      });
      tokenEl.value = "";
      onSnapshot(snap);
      setToast("Konfigurasi Cloudflare Named Tunnel disimpan.");
    } catch (err) {
      setToast(`Gagal menyimpan Named Tunnel: ${String(err)}`);
    }
  });

  $("btn-clear-cf-named").addEventListener("click", async () => {
    try {
      const snap = await invoke<DashboardSnapshot>("configure_cloudflare_named", {
        token: "__CLEAR__",
        publicUrl: "",
      });
      ($("cf-named-token") as HTMLInputElement).value = "";
      ($("cf-named-url") as HTMLInputElement).value = "";
      onSnapshot(snap);
      setToast("Token Cloudflare Named Tunnel dihapus.");
    } catch (err) {
      setToast(`Gagal menghapus token: ${String(err)}`);
    }
  });

  $("btn-clear-tunnel-logs").addEventListener("click", async () => {
    const snap = await invoke<DashboardSnapshot>("clear_tunnel_logs");
    onSnapshot(snap);
    setToast("Log tunnel dibersihkan.");
  });

  $("btn-toggle-tunnel").addEventListener("click", async () => {
    try {
      const running = getSnap()?.tunnel_running ?? false;
      const provider = ($("tunnel-provider-select") as HTMLSelectElement).value;
      const snap = await invoke<DashboardSnapshot>(
        running ? "stop_tunnel" : "start_tunnel",
        running ? {} : { provider }
      );
      onSnapshot(snap);
      setToast(snap.tunnel_running ? `Tunnel (${provider}) aktif.` : "Tunnel dimatikan.");
    } catch (err) {
      setToast(`Error tunnel: ${String(err)}`);
    }
  });

  $("btn-toggle-secrets").addEventListener("click", () => {
    secretsVisible = !secretsVisible;
    const snap = getSnap();
    if (snap) renderSecrets(snap);
  });

  $("btn-rotate-token").addEventListener("click", async () => {
    const snap = await invoke<DashboardSnapshot>("rotate_auth_token");
    onSnapshot(snap);
    setToast("Bearer Token baru dibuat.");
  });

  $("idle-timeout-select").addEventListener("change", async (e) => {
    const mins = Number((e.target as HTMLSelectElement).value);
    const snap = await invoke<DashboardSnapshot>("set_idle_timeout", { mins });
    onSnapshot(snap);
    setToast(mins > 0 ? `Auto-Kill Idle diset ke ${mins} menit.` : "Auto-Kill Idle dinonaktifkan.");
  });

  $("toggle-close-to-tray").addEventListener("change", async (e) => {
    const closeToTray = (e.target as HTMLInputElement).checked;
    try {
      const snap = await invoke<DashboardSnapshot>("update_app_settings", { closeToTray });
      onSnapshot(snap);
      setToast(
        closeToTray
          ? "Minimize to Tray saat tutup jendela diaktifkan."
          : "Tutup jendela akan menghentikan aplikasi."
      );
    } catch (err) {
      setToast(`Gagal menyimpan pengaturan: ${String(err)}`);
    }
  });

  $("toggle-sensitive-guard").addEventListener("change", async (e) => {
    const guardEnabled = (e.target as HTMLInputElement).checked;
    try {
      const snap = await invoke<DashboardSnapshot>("update_app_settings", {
        allowSensitiveReads: !guardEnabled,
      });
      onSnapshot(snap);
      setToast(
        guardEnabled
          ? "Sensitive File Guard aktif (.env, *.key, auth.json diblokir)."
          : "Peringatan: Sensitive File Guard dinonaktifkan."
      );
    } catch (err) {
      setToast(`Gagal menyimpan pengaturan: ${String(err)}`);
    }
  });

  $("command-policy-select").addEventListener("change", async (e) => {
    const policy = (e.target as HTMLSelectElement).value;
    $("command-allowlist-box").hidden = policy !== "allowlist";
    try {
      const snap = await invoke<DashboardSnapshot>("update_app_settings", {
        commandExecutionPolicy: policy,
      });
      onSnapshot(snap);
      const label =
        policy === "allowlist"
          ? "Strict Allowlist (hanya prefix aman & blokir chaining)"
          : policy === "hitl"
            ? "Always HITL (wajib konfirmasi shell)"
            : "Guarded (hardened blocklist)";
      setToast(`Command Policy: ${label}`, policy === "guarded" ? "info" : "warn");
    } catch (err) {
      setToast(`Gagal menyimpan Command Policy: ${String(err)}`, "warn");
    }
  });

  $("btn-save-command-allowlist").addEventListener("click", async () => {
    const raw = ($("command-allowlist-input") as HTMLTextAreaElement).value;
    const entries = raw
      .split(/[,\n]/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
    try {
      const snap = await invoke<DashboardSnapshot>("update_app_settings", {
        commandAllowlist: entries,
      });
      onSnapshot(snap);
      setToast(`Command Allowlist disimpan (${snap.command_allowlist.length} prefix).`, "info");
    } catch (err) {
      setToast(`Gagal menyimpan Command Allowlist: ${String(err)}`, "warn");
    }
  });

  $("btn-reset-command-allowlist").addEventListener("click", async () => {
    try {
      const snap = await invoke<DashboardSnapshot>("update_app_settings", {
        commandAllowlist: [],
      });
      onSnapshot(snap);
      ($("command-allowlist-input") as HTMLTextAreaElement).value =
        snap.command_allowlist.join(", ");
      setToast("Command Allowlist dikembalikan ke daftar default.", "info");
    } catch (err) {
      setToast(`Gagal reset Command Allowlist: ${String(err)}`, "warn");
    }
  });

  $("btn-revoke-all-oauth").addEventListener("click", async () => {
    try {
      const snap = await invoke<DashboardSnapshot>("revoke_all_oauth");
      onSnapshot(snap);
      setToast("Seluruh client dan token OAuth telah dicabut.");
    } catch (err) {
      setToast(`Gagal mencabut semua OAuth: ${String(err)}`);
    }
  });

  $("btn-copy-mcp-url").addEventListener("click", async () => {
    const snap = getSnap();
    if (!snap) return;
    const baseUrl = snap.tunnel_url ?? `http://127.0.0.1:${snap.server_port}`;
    await copyToClipboard(`${baseUrl}/mcp/${snap.auth_token}`, "MCP Endpoint + Token");
  });

  $("btn-copy-preset").addEventListener("click", async () => {
    const snap = getSnap();
    if (!snap) return;
    const preset = generateClientPreset(presetSel.value, snap, Boolean(snap.tunnel_url));
    await copyToClipboard(preset.content, preset.title);
  });

  $("btn-preview-preset").addEventListener("click", () => {
    openPresetPreviewModal(presetSel.value, getSnap);
  });

  $("btn-self-diag").addEventListener("click", async () => {
    try {
      setToast("Menjalankan self-diagnostic MCP...");
      const report = await invoke<DiagnosticReport>("run_self_diagnostic");
      openDiagnosticModal(report);
      setToast(`Diagnostik selesai (${report.latency_ms}ms).`);
    } catch (err) {
      setToast(`Diagnostik gagal: ${String(err)}`);
    }
  });

  $("traffic-filter-input").addEventListener("input", (e) => {
    trafficSearchQuery = (e.target as HTMLInputElement).value;
    const snap = getSnap();
    if (snap) renderHttpTraffic(snap.http_traffic);
  });

  $("traffic-status-filter").addEventListener("change", (e) => {
    trafficStatusFilter = (e.target as HTMLSelectElement).value;
    const snap = getSnap();
    if (snap) renderHttpTraffic(snap.http_traffic);
  });

  $("btn-clear-traffic").addEventListener("click", async () => {
    const snap = await invoke<DashboardSnapshot>("clear_http_traffic");
    onSnapshot(snap);
    setToast("Riwayat HTTP traffic dibersihkan.");
  });

  $("btn-clear-finished-jobs").addEventListener("click", async () => {
    try {
      const snap = await invoke<DashboardSnapshot>("clear_finished_jobs");
      onSnapshot(snap);
      setToast("Daftar background job yang selesai dibersihkan.");
    } catch (err) {
      setToast(`Gagal membersihkan job: ${String(err)}`);
    }
  });

  $("btn-stop-all-lsp").addEventListener("click", async () => {
    try {
      const snap = await invoke<DashboardSnapshot>("stop_lsp_server", { key: null });
      onSnapshot(snap);
      setToast("Seluruh LSP server aktif telah dihentikan.");
    } catch (err) {
      setToast(`Gagal menghentikan LSP servers: ${String(err)}`);
    }
  });

  $("btn-add-mcp").addEventListener("click", () => {
    openAddMcpModal();
  });

  const simSelect = $("sim-tool-select") as HTMLSelectElement;
  const simArgs = $("sim-args-input") as HTMLTextAreaElement;
  simArgs.value = SIM_DEFAULTS[simSelect.value] ?? "{}";

  simSelect.addEventListener("change", () => {
    simArgs.value = SIM_DEFAULTS[simSelect.value] ?? "{}";
  });

  $("btn-run-sim").addEventListener("click", async () => {
    try {
      const parsed = JSON.parse(simArgs.value);
      const output = await invoke<string>("run_tool_direct", {
        toolName: simSelect.value,
        args: parsed,
      });
      $("sim-output").textContent = output;
      await onRefresh();
      setToast(`Tool ${simSelect.value} selesai dieksekusi.`);
    } catch (err) {
      $("sim-output").textContent = `Error: ${String(err)}`;
    }
  });

  $("btn-copy-sim-rpc").addEventListener("click", async () => {
    try {
      const parsed = JSON.parse(simArgs.value);
      const rpc = {
        jsonrpc: "2.0",
        id: 1,
        method: "tools/call",
        params: {
          name: simSelect.value,
          arguments: parsed,
        },
      };
      await copyToClipboard(JSON.stringify(rpc, null, 2), "JSON-RPC Payload");
    } catch (err) {
      setToast(`JSON argumen tidak valid: ${String(err)}`);
    }
  });
}
