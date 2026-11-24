import { invoke } from "@tauri-apps/api/core";
import { $, copyToClipboard, setToast } from "../dom";
import { renderMarkdownInto } from "../markdown";
import type {
  AgentRunSummary,
  ArtifactDocument,
  ArtifactFeedbackResult,
  ArtifactKind,
  ArtifactStatus,
  ExecutionMode,
} from "../types";

type ArtifactsTabKey = "plan" | "tasks" | "walkthrough" | "runs";

const TAB_ORDER: ArtifactsTabKey[] = ["plan", "tasks", "walkthrough", "runs"];

const TAB_IDS: Record<ArtifactsTabKey, { tabId: string; panelId: string }> = {
  plan: { tabId: "tab-art-plan", panelId: "panel-art-plan" },
  tasks: { tabId: "tab-art-tasks", panelId: "panel-art-tasks" },
  walkthrough: { tabId: "tab-art-walkthrough", panelId: "panel-art-walkthrough" },
  runs: { tabId: "tab-art-runs", panelId: "panel-art-runs" },
};

const TAB_TO_KIND: Partial<Record<ArtifactsTabKey, ArtifactKind>> = {
  plan: "implementation_plan",
  tasks: "task_list",
  walkthrough: "walkthrough",
};

let activeTab: ArtifactsTabKey = "plan";
let cachedArtifacts: ArtifactDocument[] = [];
let cachedRuns: AgentRunSummary[] = [];
let getSessionIdFn: () => string = () => "default";
let runPromptFn: (prompt: string, mode?: ExecutionMode) => Promise<void> = async () => {};

function statusBadgeClass(status: ArtifactStatus): string {
  switch (status) {
    case "approved":
    case "completed":
      return "badge badge-ok";
    case "awaiting_approval":
      return "badge badge-warn";
    case "changes_requested":
      return "badge badge-danger";
    default:
      return "badge badge-muted";
  }
}

function statusLabel(status: ArtifactStatus): string {
  switch (status) {
    case "awaiting_approval":
      return "Awaiting Approval";
    case "changes_requested":
      return "Changes Requested";
    case "approved":
      return "Approved";
    case "completed":
      return "Completed";
    default:
      return "Draft";
  }
}

function runStatusBadgeClass(status: AgentRunSummary["status"]): string {
  switch (status) {
    case "running":
      return "badge badge-info";
    case "awaiting_approval":
      return "badge badge-warn";
    case "completed":
      return "badge badge-ok";
    case "failed":
      return "badge badge-danger";
    default:
      return "badge badge-muted";
  }
}

function getArtifactForTab(tab: ArtifactsTabKey): ArtifactDocument | null {
  const kind = TAB_TO_KIND[tab];
  if (!kind) return null;
  return cachedArtifacts.find((d) => d.kind === kind) ?? null;
}

function updateArtifactsBadge(): void {
  const badge = document.getElementById("artifacts-count-badge");
  if (!badge) return;
  badge.textContent = String(cachedArtifacts.length);
  const hasAwaiting = cachedArtifacts.some((d) => d.status === "awaiting_approval");
  badge.className = hasAwaiting ? "badge badge-warn" : "badge badge-muted";
}

function approximateLineAnchor(md: string, needle: string): number | null {
  const clean = needle.trim().split("\n")[0]?.trim() ?? "";
  if (clean.length < 3) return null;
  const snippet = clean.slice(0, 48).toLowerCase();
  const lines = md.replace(/\r\n/g, "\n").split("\n");
  const idx = lines.findIndex((line) => line.toLowerCase().includes(snippet));
  return idx >= 0 ? idx + 1 : null;
}

function syncFooterState(): void {
  const footer = $("artifacts-footer");
  const doc = getArtifactForTab(activeTab);
  if (!doc) {
    footer.hidden = true;
    return;
  }
  footer.hidden = false;

  const approveBtn = $("btn-art-approve") as HTMLButtonElement;
  const reqChangesBtn = $("btn-art-request-changes") as HTMLButtonElement;
  const isPlan = doc.kind === "implementation_plan";

  approveBtn.hidden = !isPlan || doc.status === "approved" || doc.status === "completed";
  reqChangesBtn.hidden = !isPlan;

  const openComments = doc.comments.filter((c) => !c.resolved).length;
  reqChangesBtn.disabled = openComments === 0;
  reqChangesBtn.title =
    openComments === 0
      ? "Tambahkan minimal 1 komentar review yang belum resolved terlebih dahulu"
      : `Kirim ${openComments} komentar review untuk merevisi rencana`;
}

function upsertCachedArtifact(doc: ArtifactDocument): void {
  const idx = cachedArtifacts.findIndex((d) => d.id === doc.id || d.kind === doc.kind);
  if (idx >= 0) {
    cachedArtifacts[idx] = doc;
  } else {
    cachedArtifacts.push(doc);
  }
}

function renderArtifactPane(tab: ArtifactsTabKey): void {
  const info = TAB_IDS[tab];
  const panel = $(info.panelId);
  const sid = getSessionIdFn();
  $("artifacts-session-label").textContent = `Session: ${sid}`;

  if (tab === "runs") {
    renderRunsPanelContent(panel);
    syncFooterState();
    return;
  }

  const doc = getArtifactForTab(tab);
  if (!doc) {
    const empty = document.createElement("div");
    empty.className = "art-empty";
    const msg = document.createElement("p");
    msg.textContent =
      tab === "plan"
        ? "Belum ada Implementation Plan pada sesi ini. Aktifkan mode Plan First atau ketik /plan."
        : tab === "tasks"
          ? "Belum ada Task List checklist untuk sesi ini."
          : "Belum ada Walkthrough ringkasan verifikasi untuk sesi ini.";
    empty.appendChild(msg);
    panel.replaceChildren(empty);
    syncFooterState();
    return;
  }

  const wrap = document.createElement("div");
  wrap.className = "art-doc-wrap";

  // Header metadata row
  const head = document.createElement("div");
  head.className = "art-doc-head";
  const titleWrap = document.createElement("div");
  const h3 = document.createElement("h3");
  h3.className = "art-doc-title";
  h3.textContent = doc.title;
  const sub = document.createElement("div");
  sub.className = "art-doc-meta mono-muted";
  sub.textContent = `v${doc.version} · @${doc.workspace_alias} · ${doc.updated_at}`;
  titleWrap.append(h3, sub);

  const badge = document.createElement("span");
  badge.className = statusBadgeClass(doc.status);
  badge.textContent = statusLabel(doc.status);
  head.append(titleWrap, badge);

  // Markdown preview (clickable blocks for inline review comments)
  const mdWrap = document.createElement("div");
  mdWrap.className = "art-md-body";
  mdWrap.title = "Klik paragraf/heading/baris mana saja untuk menautkan kutipan ke komentar review";
  renderMarkdownInto(mdWrap, doc.content_md);

  // Comment composer elements referenced on block click
  const commentsSection = document.createElement("div");
  commentsSection.className = "art-comments-sec";

  const commentsHead = document.createElement("div");
  commentsHead.className = "art-comments-head";
  const commentsLabel = document.createElement("span");
  commentsLabel.className = "bar-label";
  const openCount = doc.comments.filter((c) => !c.resolved).length;
  commentsLabel.textContent = `Review Comments (${openCount} open / ${doc.comments.length} total)`;
  commentsHead.appendChild(commentsLabel);

  const commentsList = document.createElement("div");
  commentsList.className = "art-comments-list";
  if (doc.comments.length === 0) {
    const none = document.createElement("div");
    none.className = "mono-muted";
    none.style.fontSize = "11.5px";
    none.textContent = "Klik bagian mana pun di atas atau tulis komentar review di bawah.";
    commentsList.appendChild(none);
  } else {
    for (const cmt of doc.comments) {
      const cCard = document.createElement("div");
      cCard.className = `art-comment-card ${cmt.resolved ? "resolved" : ""}`;

      const cTop = document.createElement("div");
      cTop.className = "art-comment-top";
      const cAnchor = document.createElement("span");
      cAnchor.className = "mono-muted";
      cAnchor.textContent = `${cmt.line_anchor !== null ? `Line ${cmt.line_anchor}` : "General"} · ${cmt.created_at}`;

      const resBtn = document.createElement("button");
      resBtn.type = "button";
      resBtn.className = "btn btn-secondary btn-xs";
      resBtn.textContent = cmt.resolved ? "Reopen" : "Resolve";
      resBtn.addEventListener("click", async () => {
        try {
          const updated = await invoke<ArtifactDocument>("resolve_artifact_comment", {
            sessionId: sid,
            artifactId: doc.id,
            commentId: cmt.id,
            resolved: !cmt.resolved,
          });
          upsertCachedArtifact(updated);
          renderArtifactPane(tab);
        } catch (err) {
          setToast(`Gagal mengubah status komentar: ${String(err)}`, "warn");
        }
      });
      cTop.append(cAnchor, resBtn);
      cCard.appendChild(cTop);

      if (cmt.quote) {
        const qEl = document.createElement("blockquote");
        qEl.className = "art-comment-quote";
        qEl.textContent = cmt.quote;
        cCard.appendChild(qEl);
      }

      const bEl = document.createElement("div");
      bEl.className = "art-comment-body";
      bEl.textContent = cmt.body;
      cCard.appendChild(bEl);
      commentsList.appendChild(cCard);
    }
  }

  // Inline comment form
  const form = document.createElement("form");
  form.className = "art-comment-form";

  const anchorRow = document.createElement("div");
  anchorRow.className = "art-comment-anchor-row";

  const lineInput = document.createElement("input");
  lineInput.type = "number";
  lineInput.min = "1";
  lineInput.placeholder = "Line #";
  lineInput.className = "mono-input art-line-input";
  lineInput.setAttribute("aria-label", "Nomor baris opsional");

  const quoteInput = document.createElement("input");
  quoteInput.type = "text";
  quoteInput.placeholder = "Kutipan (klik teks di atas atau kosongkan)";
  quoteInput.className = "mono-input art-quote-input";
  quoteInput.setAttribute("aria-label", "Kutipan teks target komentar");

  anchorRow.append(lineInput, quoteInput);

  const bodyRow = document.createElement("div");
  bodyRow.className = "art-comment-submit-row";

  const bodyInput = document.createElement("textarea");
  bodyInput.rows = 2;
  bodyInput.className = "mono-textarea art-body-input";
  bodyInput.placeholder = "Tulis catatan review atau koreksi untuk bagian ini...";
  bodyInput.setAttribute("aria-label", "Isi komentar review");

  const addBtn = document.createElement("button");
  addBtn.type = "submit";
  addBtn.className = "btn btn-secondary btn-xs";
  addBtn.textContent = "Add Comment";

  bodyRow.append(bodyInput, addBtn);
  form.append(anchorRow, bodyRow);

  mdWrap.addEventListener("click", (e) => {
    const target = e.target instanceof Element ? e.target.closest("p, h1, h2, h3, h4, li, pre") : null;
    if (!target) return;
    const rawText = (target.textContent ?? "").trim().replace(/\s+/g, " ");
    if (!rawText) return;
    quoteInput.value = rawText.slice(0, 120);
    const guessed = approximateLineAnchor(doc.content_md, rawText);
    if (guessed !== null) {
      lineInput.value = String(guessed);
    }
    bodyInput.focus();
  });

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const commentText = bodyInput.value.trim();
    if (!commentText) return;
    const parsedLine = Number.parseInt(lineInput.value, 10);
    const lineAnchor = Number.isFinite(parsedLine) && parsedLine > 0 ? parsedLine : null;
    const quote = quoteInput.value.trim() || null;

    addBtn.disabled = true;
    try {
      const updated = await invoke<ArtifactDocument>("add_artifact_comment", {
        sessionId: sid,
        artifactId: doc.id,
        lineAnchor,
        quote,
        body: commentText,
      });
      upsertCachedArtifact(updated);
      renderArtifactPane(tab);
      setToast("Komentar review ditambahkan.");
    } catch (err) {
      setToast(`Gagal menambah komentar: ${String(err)}`, "warn");
    } finally {
      addBtn.disabled = false;
    }
  });

  commentsSection.append(commentsHead, commentsList, form);

  wrap.append(head, mdWrap, commentsSection);

  // If Plan tab also has a review_note artifact, show it compactly below
  if (tab === "plan") {
    const reviewDoc = cachedArtifacts.find((d) => d.kind === "review_note");
    if (reviewDoc) {
      const noteBox = document.createElement("details");
      noteBox.className = "thought-box";
      noteBox.style.marginTop = "12px";
      const sum = document.createElement("summary");
      sum.textContent = `Review Note: ${reviewDoc.title} (v${reviewDoc.version})`;
      const noteBody = document.createElement("div");
      noteBody.className = "art-md-body";
      noteBody.style.marginTop = "8px";
      renderMarkdownInto(noteBody, reviewDoc.content_md);
      noteBox.append(sum, noteBody);
      wrap.appendChild(noteBox);
    }
  }

  panel.replaceChildren(wrap);
  syncFooterState();
}

function renderRunsPanelContent(panel: HTMLElement): void {
  if (cachedRuns.length === 0) {
    const empty = document.createElement("div");
    empty.className = "art-empty";
    const p = document.createElement("p");
    p.textContent = "Belum ada eksekusi agent atau subagent yang tercatat pada sesi aplikasi ini.";
    empty.appendChild(p);
    panel.replaceChildren(empty);
    return;
  }

  const list = document.createElement("div");
  list.className = "art-runs-list";

  const primaries = cachedRuns.filter((r) => !r.parent_run_id);
  const childrenByParent = new Map<string, AgentRunSummary[]>();
  for (const r of cachedRuns) {
    if (r.parent_run_id) {
      const arr = childrenByParent.get(r.parent_run_id) ?? [];
      arr.push(r);
      childrenByParent.set(r.parent_run_id, arr);
    }
  }

  const appendRunCard = (run: AgentRunSummary, isChild: boolean): void => {
    const card = document.createElement("div");
    card.className = `art-run-card ${isChild ? "art-run-child" : ""}`;

    const top = document.createElement("div");
    top.className = "art-run-top";

    const title = document.createElement("span");
    title.className = "art-run-label";
    title.textContent = `${isChild ? "↳ " : ""}${run.label}`;

    const right = document.createElement("div");
    right.className = "btn-row";

    const badge = document.createElement("span");
    badge.className = runStatusBadgeClass(run.status);
    badge.textContent = run.status.replace("_", " ");
    right.appendChild(badge);

    if (run.status === "running") {
      const stopBtn = document.createElement("button");
      stopBtn.type = "button";
      stopBtn.className = "btn btn-danger btn-xs";
      stopBtn.textContent = "Stop";
      stopBtn.addEventListener("click", async () => {
        stopBtn.disabled = true;
        try {
          const updated = await invoke<AgentRunSummary[]>("cancel_agent_run", {
            runId: run.run_id,
          });
          renderRunsTab(updated);
        } catch (err) {
          setToast(`Gagal menghentikan run: ${String(err)}`, "warn");
          stopBtn.disabled = false;
        }
      });
      right.appendChild(stopBtn);
    }

    top.append(title, right);

    const meta = document.createElement("div");
    meta.className = "mono-muted art-run-meta";
    meta.textContent = `@${run.workspace_alias} · ${run.execution_mode} · ${run.provider}/${run.model} · step ${run.current_step}/${run.max_steps} · ${run.elapsed_secs}s`;

    const activity = document.createElement("div");
    activity.className = "art-run-activity";
    activity.textContent = run.last_activity_text;

    card.append(top, meta, activity);
    list.appendChild(card);
  };

  for (const primary of primaries) {
    appendRunCard(primary, false);
    const children = childrenByParent.get(primary.run_id) ?? [];
    for (const ch of children) {
      appendRunCard(ch, true);
    }
  }

  // Fallback for orphan child runs if primary was evicted from ring buffer
  const primaryIds = new Set(primaries.map((p) => p.run_id));
  for (const r of cachedRuns) {
    if (r.parent_run_id && !primaryIds.has(r.parent_run_id)) {
      appendRunCard(r, true);
    }
  }

  panel.replaceChildren(list);
}

function selectArtifactsTab(nextTab: ArtifactsTabKey, focusTabBtn = false): void {
  activeTab = nextTab;
  for (const key of TAB_ORDER) {
    const info = TAB_IDS[key];
    const btn = $(info.tabId) as HTMLButtonElement;
    const panel = $(info.panelId);
    const selected = key === nextTab;
    btn.setAttribute("aria-selected", selected ? "true" : "false");
    btn.tabIndex = selected ? 0 : -1;
    panel.hidden = !selected;
    if (selected && focusTabBtn) {
      btn.focus();
    }
  }
  renderArtifactPane(nextTab);
}

export function isArtifactsPanelOpen(): boolean {
  return $("agent-shell").classList.contains("artifacts-open");
}

export function toggleArtifactsPanel(forceOpen?: boolean): void {
  const shell = $("agent-shell");
  const panel = $("artifacts-panel");
  const toggleBtn = $("btn-toggle-artifacts");
  const nextOpen =
    typeof forceOpen === "boolean" ? forceOpen : !shell.classList.contains("artifacts-open");

  if (nextOpen && shell.classList.contains("drawer-open")) {
    shell.classList.remove("drawer-open");
    $("agent-drawer").toggleAttribute("inert", true);
    $("btn-toggle-drawer").setAttribute("aria-expanded", "false");
  }

  shell.classList.toggle("artifacts-open", nextOpen);
  panel.toggleAttribute("inert", !nextOpen);
  toggleBtn.setAttribute("aria-expanded", nextOpen ? "true" : "false");

  if (nextOpen) {
    renderArtifactPane(activeTab);
  }
}

export function openRunsTab(): void {
  toggleArtifactsPanel(true);
  selectArtifactsTab("runs");
}

export async function loadSessionArtifacts(sessionId: string): Promise<void> {
  const cleanSid = sessionId.trim() || "default";
  try {
    cachedArtifacts = await invoke<ArtifactDocument[]>("list_session_artifacts", {
      sessionId: cleanSid,
    });
  } catch {
    cachedArtifacts = [];
  }
  updateArtifactsBadge();
  renderArtifactPane(activeTab);
}

export function handleArtifactUpdatedEvent(doc: ArtifactDocument): void {
  const activeSid = getSessionIdFn();
  if (doc.session_id !== activeSid && !(activeSid === "default" && doc.session_id === "webmcp-latest")) {
    return;
  }
  upsertCachedArtifact(doc);
  updateArtifactsBadge();

  if (doc.kind === "implementation_plan" && doc.status === "awaiting_approval") {
    toggleArtifactsPanel(true);
    selectArtifactsTab("plan");
    setToast("Implementation Plan siap direview & disetujui.", "info");
    return;
  }

  const targetTab: ArtifactsTabKey =
    doc.kind === "task_list"
      ? "tasks"
      : doc.kind === "walkthrough"
        ? "walkthrough"
        : "plan";
  if (activeTab === targetTab) {
    renderArtifactPane(targetTab);
  } else {
    syncFooterState();
  }
}

export function renderRunsTab(runs: AgentRunSummary[]): void {
  cachedRuns = runs;
  const runsTabBtn = $("tab-art-runs");
  const runningCount = runs.filter((r) => r.status === "running").length;
  runsTabBtn.textContent = runningCount > 0 ? `Runs (${runningCount})` : "Runs";
  if (activeTab === "runs") {
    renderArtifactPane("runs");
  }
}

export function initArtifactsView(options: {
  getSessionId: () => string;
  onRunPrompt: (prompt: string, mode?: ExecutionMode) => Promise<void>;
}): void {
  getSessionIdFn = options.getSessionId;
  runPromptFn = options.onRunPrompt;

  $("btn-toggle-artifacts").addEventListener("click", () => {
    toggleArtifactsPanel();
  });

  $("btn-close-artifacts").addEventListener("click", () => {
    toggleArtifactsPanel(false);
    $("btn-toggle-artifacts").focus();
  });

  const tablist = document.querySelector<HTMLElement>(".art-tablist");
  for (const key of TAB_ORDER) {
    const btn = $(TAB_IDS[key].tabId);
    btn.addEventListener("click", () => {
      selectArtifactsTab(key);
    });
  }

  if (tablist) {
    tablist.addEventListener("keydown", (e) => {
      const idx = TAB_ORDER.indexOf(activeTab);
      if (idx === -1) return;
      let nextIdx: number | null = null;
      if (e.key === "ArrowRight") {
        nextIdx = (idx + 1) % TAB_ORDER.length;
      } else if (e.key === "ArrowLeft") {
        nextIdx = (idx - 1 + TAB_ORDER.length) % TAB_ORDER.length;
      } else if (e.key === "Home") {
        nextIdx = 0;
      } else if (e.key === "End") {
        nextIdx = TAB_ORDER.length - 1;
      }
      if (nextIdx !== null) {
        e.preventDefault();
        selectArtifactsTab(TAB_ORDER[nextIdx], true);
      }
    });
  }

  $("btn-art-copy").addEventListener("click", async () => {
    const doc = getArtifactForTab(activeTab);
    if (!doc) return;
    await copyToClipboard(doc.content_md, doc.title);
  });

  $("btn-art-export").addEventListener("click", async () => {
    const doc = getArtifactForTab(activeTab);
    if (!doc) return;
    try {
      const savedPath = await invoke<string>("export_artifact_to_workspace", {
        sessionId: getSessionIdFn(),
        artifactId: doc.id,
      });
      setToast(`Artifact diekspor ke ${savedPath}`);
    } catch (err) {
      setToast(`Gagal mengekspor artifact: ${String(err)}`, "warn");
    }
  });

  $("btn-art-approve").addEventListener("click", async () => {
    const doc = getArtifactForTab(activeTab);
    if (!doc) return;
    const btn = $("btn-art-approve") as HTMLButtonElement;
    btn.disabled = true;
    try {
      const updated = await invoke<ArtifactDocument>("approve_artifact", {
        sessionId: getSessionIdFn(),
        artifactId: doc.id,
      });
      upsertCachedArtifact(updated);
      renderArtifactPane(activeTab);
      await runPromptFn(
        "Rencana disetujui. Laksanakan seluruh implementation_plan langkah demi langkah, perbarui artifact task_list, lalu buat walkthrough setelah verifikasi.",
        "plan_first"
      );
    } catch (err) {
      setToast(`Gagal menyetujui artifact: ${String(err)}`, "warn");
    } finally {
      btn.disabled = false;
    }
  });

  $("btn-art-request-changes").addEventListener("click", async () => {
    const doc = getArtifactForTab(activeTab);
    if (!doc) return;
    const btn = $("btn-art-request-changes") as HTMLButtonElement;
    btn.disabled = true;
    try {
      const res = await invoke<ArtifactFeedbackResult>("request_artifact_changes", {
        sessionId: getSessionIdFn(),
        artifactId: doc.id,
      });
      upsertCachedArtifact(res.artifact);
      renderArtifactPane(activeTab);
      await runPromptFn(res.feedback_prompt, "plan_first");
    } catch (err) {
      setToast(`Gagal mengirim permintaan revisi: ${String(err)}`, "warn");
    } finally {
      btn.disabled = false;
    }
  });

  selectArtifactsTab("plan");
}
