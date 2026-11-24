import { invoke } from "@tauri-apps/api/core";
import {
  $,
  closeTopLayer,
  copyToClipboard,
  formatDurationCompact,
  setToast,
  setupDialogLightDismiss,
} from "../dom";
import { generateClientPreset } from "../presets";
import type {
  CommandPaletteAction,
  DashboardSnapshot,
  DiagnosticReport,
  DownstreamMcpConfigInput,
  ElicitationAction,
  HooksStatusSummary,
  HttpTrafficEntry,
  JobLogsDelta,
  JobSummary,
  PendingElicitation,
  PendingOAuthInfo,
  PendingToolApproval,
} from "../types";

interface ElicitationFieldBinding {
  key: string;
  focusTarget: HTMLElement;
  validate: (forceShow: boolean) => boolean;
  readValue: () => { present: boolean; value: unknown };
}

let activeOAuthRequestId: string | null = null;
const pendingOAuthQueue: PendingOAuthInfo[] = [];
let activeToolRequestId: string | null = null;
const pendingToolQueue: PendingToolApproval[] = [];
let activeElicitationRequestId: string | null = null;
const pendingElicitationQueue: PendingElicitation[] = [];
let activeElicitationBindings: ElicitationFieldBinding[] = [];
let elicitationSubmitting = false;
let activeLogJobId: string | null = null;
let activeLogNextLine = 0;
let activeLogFetching = false;
let activeLogPendingFetch = false;
let activeLogPendingReset = false;
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

function isPlainRecord(val: unknown): val is Record<string, unknown> {
  return typeof val === "object" && val !== null && !Array.isArray(val);
}

function parseEnumChoices(
  prop: Record<string, unknown>
): Array<{ value: string | number; label: string }> {
  if (Array.isArray(prop.enum)) {
    const names = Array.isArray(prop.enumNames) ? prop.enumNames : [];
    const out: Array<{ value: string | number; label: string }> = [];
    prop.enum.forEach((item, idx) => {
      if (typeof item === "string" || typeof item === "number") {
        const lbl = typeof names[idx] === "string" ? String(names[idx]) : String(item);
        out.push({ value: item, label: lbl });
      }
    });
    return out;
  }
  if (Array.isArray(prop.oneOf)) {
    const out: Array<{ value: string | number; label: string }> = [];
    for (const item of prop.oneOf) {
      if (isPlainRecord(item) && (typeof item.const === "string" || typeof item.const === "number")) {
        const lbl = typeof item.title === "string" ? item.title : String(item.const);
        out.push({ value: item.const, label: lbl });
      }
    }
    return out;
  }
  return [];
}

function syncFieldDescribedBy(
  control: HTMLElement,
  descId: string,
  errId: string,
  showError: boolean
): void {
  const tokens = [descId, showError ? errId : ""].filter(Boolean);
  if (tokens.length > 0) {
    control.setAttribute("aria-describedby", tokens.join(" "));
  } else {
    control.removeAttribute("aria-describedby");
  }
}

function renderElicitationFields(
  container: HTMLElement,
  schema: Record<string, unknown> | null
): ElicitationFieldBinding[] {
  container.replaceChildren();
  const bindings: ElicitationFieldBinding[] = [];

  const properties = schema && isPlainRecord(schema.properties) ? schema.properties : {};
  const requiredSet = new Set(
    schema && Array.isArray(schema.required)
      ? schema.required.filter((x): x is string => typeof x === "string")
      : []
  );

  const entries = Object.entries(properties);
  if (entries.length === 0) {
    const note = document.createElement("p");
    note.className = "mono-muted";
    note.textContent =
      "Server tidak meminta field form tambahan. Tekan Submit untuk menyetujui, Decline untuk menolak, atau Cancel untuk membatalkan.";
    container.appendChild(note);
    return bindings;
  }

  entries.forEach(([key, rawProp], idx) => {
    const prop = isPlainRecord(rawProp) ? rawProp : {};
    const slug = `${key.replace(/[^a-zA-Z0-9_-]/g, "_")}_${idx}`;
    const errId = `elic-err-${slug}`;
    const descId = typeof prop.description === "string" && prop.description.trim() ? `elic-desc-${slug}` : "";
    const title = typeof prop.title === "string" && prop.title.trim() ? prop.title.trim() : key;
    const isRequired = requiredSet.has(key);
    const labelText = isRequired ? `${title} *` : title;
    const rawType = typeof prop.type === "string" ? prop.type : "string";
    const choices = parseEnumChoices(prop);

    const errEl = document.createElement("p");
    errEl.id = errId;
    errEl.className = "elic-field-error";
    errEl.hidden = true;

    const appendDesc = (parent: HTMLElement): void => {
      if (!descId) return;
      const descEl = document.createElement("p");
      descEl.id = descId;
      descEl.className = "elic-field-desc";
      descEl.textContent = String(prop.description).trim();
      parent.appendChild(descEl);
    };

    if (rawType === "boolean") {
      const wrap = document.createElement("div");
      wrap.className = "elic-field";

      const row = document.createElement("div");
      row.className = "elic-check-row";

      const checkbox = document.createElement("input");
      checkbox.type = "checkbox";
      checkbox.id = `elic-in-${slug}`;
      checkbox.name = key;
      checkbox.checked = prop.default === true;
      checkbox.required = prop.const === true;
      checkbox.setAttribute("aria-invalid", "false");
      if (descId) checkbox.setAttribute("aria-describedby", descId);

      const label = document.createElement("label");
      label.htmlFor = checkbox.id;
      label.textContent = prop.const === true ? `${title} *` : title;

      // Label placed inline immediately after checkbox control per accessible forms guidance.
      row.append(checkbox, label);
      wrap.appendChild(row);
      appendDesc(wrap);
      wrap.appendChild(errEl);
      container.appendChild(wrap);

      let interacted = false;
      let validatedOnce = false;
      const validate = (forceShow: boolean): boolean => {
        if (!forceShow && !interacted && !validatedOnce) return true;
        validatedOnce = true;
        checkbox.setCustomValidity("");
        if (prop.const === true && !checkbox.checked) {
          checkbox.setCustomValidity("Opsi ini wajib dicentang sebelum melanjutkan.");
        }
        const ok = checkbox.checkValidity();
        checkbox.setAttribute("aria-invalid", ok ? "false" : "true");
        checkbox.classList.toggle("user-invalid-fallback", !ok);
        errEl.hidden = ok;
        errEl.textContent = ok ? "" : checkbox.validationMessage;
        syncFieldDescribedBy(checkbox, descId, errId, !ok);
        return ok;
      };

      checkbox.addEventListener("change", () => {
        interacted = true;
        if (validatedOnce) validate(false);
      });
      checkbox.addEventListener("blur", () => {
        if (interacted) validate(false);
      });

      bindings.push({
        key,
        focusTarget: checkbox,
        validate,
        readValue: () => ({ present: true, value: checkbox.checked }),
      });
      return;
    }

    if (choices.length >= 2 && choices.length <= 5) {
      const fieldset = document.createElement("fieldset");
      fieldset.className = "elic-field elic-radio-group";
      fieldset.setAttribute("aria-invalid", "false");
      if (descId) fieldset.setAttribute("aria-describedby", descId);

      const legend = document.createElement("legend");
      legend.textContent = labelText;
      fieldset.appendChild(legend);
      appendDesc(fieldset);

      const groupName = `elic_radio_${slug}`;
      const radios: HTMLInputElement[] = [];

      choices.forEach((choice, cIdx) => {
        const row = document.createElement("div");
        row.className = "elic-radio-row";

        const radio = document.createElement("input");
        radio.type = "radio";
        radio.id = `elic-in-${slug}-${cIdx}`;
        radio.name = groupName;
        radio.value = String(choice.value);
        if (cIdx === 0 && isRequired) {
          radio.required = true;
        }
        if (prop.default !== undefined && String(prop.default) === String(choice.value)) {
          radio.checked = true;
        }
        radio.setAttribute("aria-invalid", "false");

        const label = document.createElement("label");
        label.htmlFor = radio.id;
        label.textContent = choice.label;

        // Label placed inline immediately after radio input for alignment and click target clarity.
        row.append(radio, label);
        fieldset.appendChild(row);
        radios.push(radio);
      });

      fieldset.appendChild(errEl);
      container.appendChild(fieldset);

      let interacted = false;
      let validatedOnce = false;
      const validate = (forceShow: boolean): boolean => {
        if (!forceShow && !interacted && !validatedOnce) return true;
        validatedOnce = true;
        const selected = radios.find((r) => r.checked);
        const ok = !isRequired || Boolean(selected);
        fieldset.setAttribute("aria-invalid", ok ? "false" : "true");
        fieldset.classList.toggle("user-invalid-fallback", !ok);
        radios.forEach((r) => r.setAttribute("aria-invalid", ok ? "false" : "true"));
        errEl.hidden = ok;
        errEl.textContent = ok ? "" : `Pilih salah satu opsi untuk ${title}.`;
        syncFieldDescribedBy(fieldset, descId, errId, !ok);
        return ok;
      };

      radios.forEach((r) => {
        r.addEventListener("change", () => {
          interacted = true;
          if (validatedOnce) validate(false);
        });
      });
      fieldset.addEventListener("focusout", (e) => {
        const nextFocus = e.relatedTarget as Node | null;
        if (!fieldset.contains(nextFocus) && interacted) {
          validate(false);
        }
      });

      bindings.push({
        key,
        focusTarget: radios[0],
        validate,
        readValue: () => {
          const picked = radios.find((r) => r.checked);
          if (!picked) return { present: false, value: undefined };
          const matched = choices.find((c) => String(c.value) === picked.value);
          return { present: true, value: matched ? matched.value : picked.value };
        },
      });
      return;
    }

    if (choices.length > 0) {
      const wrap = document.createElement("div");
      wrap.className = "form-group elic-field";

      const label = document.createElement("label");
      label.htmlFor = `elic-in-${slug}`;
      label.textContent = labelText;

      const select = document.createElement("select");
      select.id = `elic-in-${slug}`;
      select.className = "mono-select";
      select.required = isRequired;
      select.setAttribute("aria-invalid", "false");
      if (descId) select.setAttribute("aria-describedby", descId);

      const placeholderOpt = document.createElement("option");
      placeholderOpt.value = "";
      placeholderOpt.textContent = isRequired ? "-- Pilih opsi (wajib) --" : "-- Opsional --";
      select.appendChild(placeholderOpt);

      choices.forEach((choice) => {
        const opt = document.createElement("option");
        opt.value = String(choice.value);
        opt.textContent = choice.label;
        if (prop.default !== undefined && String(prop.default) === String(choice.value)) {
          opt.selected = true;
        }
        select.appendChild(opt);
      });

      wrap.append(label, select);
      appendDesc(wrap);
      wrap.appendChild(errEl);
      container.appendChild(wrap);

      let interacted = false;
      let validatedOnce = false;
      const validate = (forceShow: boolean): boolean => {
        if (!forceShow && !interacted && !validatedOnce) return true;
        validatedOnce = true;
        select.setCustomValidity("");
        const ok = select.checkValidity();
        select.setAttribute("aria-invalid", ok ? "false" : "true");
        select.classList.toggle("user-invalid-fallback", !ok);
        errEl.hidden = ok;
        errEl.textContent = ok ? "" : select.validationMessage || `Pilih nilai untuk ${title}.`;
        syncFieldDescribedBy(select, descId, errId, !ok);
        return ok;
      };

      select.addEventListener("change", () => {
        interacted = true;
        if (validatedOnce) validate(false);
      });
      select.addEventListener("blur", () => {
        if (interacted) validate(false);
      });

      bindings.push({
        key,
        focusTarget: select,
        validate,
        readValue: () => {
          if (!select.value) return { present: false, value: undefined };
          const matched = choices.find((c) => String(c.value) === select.value);
          return { present: true, value: matched ? matched.value : select.value };
        },
      });
      return;
    }

    const wrap = document.createElement("div");
    wrap.className = "form-group elic-field";

    const label = document.createElement("label");
    label.htmlFor = `elic-in-${slug}`;
    label.textContent = labelText;

    const input = document.createElement("input");
    input.id = `elic-in-${slug}`;
    input.className = "mono-input";
    input.required = isRequired;
    input.setAttribute("aria-invalid", "false");
    if (descId) input.setAttribute("aria-describedby", descId);

    if (rawType === "integer" || rawType === "number") {
      input.type = "number";
      input.step = rawType === "integer" ? "1" : "any";
      if (typeof prop.minimum === "number") input.min = String(prop.minimum);
      if (typeof prop.maximum === "number") input.max = String(prop.maximum);
      if (typeof prop.default === "number") input.value = String(prop.default);
    } else {
      const fmt = typeof prop.format === "string" ? prop.format : "";
      input.type = fmt === "email" ? "email" : fmt === "uri" || fmt === "url" ? "url" : fmt === "date" ? "date" : "text";
      if (typeof prop.minLength === "number") input.minLength = prop.minLength;
      if (typeof prop.maxLength === "number") input.maxLength = prop.maxLength;
      if (typeof prop.pattern === "string") input.pattern = prop.pattern;
      if (typeof prop.default === "string") input.value = prop.default;
      input.spellcheck = false;
    }

    wrap.append(label, input);
    appendDesc(wrap);
    wrap.appendChild(errEl);
    container.appendChild(wrap);

    let interacted = false;
    let validatedOnce = false;
    const validate = (forceShow: boolean): boolean => {
      if (!forceShow && !interacted && !validatedOnce) return true;
      validatedOnce = true;
      input.setCustomValidity("");
      const trimmed = input.value.trim();
      if (rawType === "integer" && trimmed !== "") {
        const parsed = Number(trimmed);
        if (!Number.isInteger(parsed)) {
          input.setCustomValidity("Masukkan bilangan bulat (integer) yang valid.");
        }
      }
      if (rawType === "string" && isRequired && trimmed === "" && input.value.length > 0) {
        input.setCustomValidity("Field ini tidak boleh hanya berisi spasi.");
      }
      const ok = input.checkValidity();
      input.setAttribute("aria-invalid", ok ? "false" : "true");
      input.classList.toggle("user-invalid-fallback", !ok);
      errEl.hidden = ok;
      errEl.textContent = ok ? "" : input.validationMessage || "Nilai tidak valid.";
      syncFieldDescribedBy(input, descId, errId, !ok);
      return ok;
    };

    input.addEventListener("input", () => {
      interacted = true;
      if (validatedOnce) validate(false);
    });
    input.addEventListener("blur", () => {
      if (interacted) validate(false);
    });

    bindings.push({
      key,
      focusTarget: input,
      validate,
      readValue: () => {
        if (rawType === "integer" || rawType === "number") {
          const raw = input.value.trim();
          if (raw === "") return { present: false, value: undefined };
          return { present: true, value: Number(raw) };
        }
        const val = input.value;
        if (!isRequired && val === "") return { present: false, value: undefined };
        return { present: true, value: val };
      },
    });
  });

  return bindings;
}

export function enqueueMcpElicitation(info: PendingElicitation): void {
  if (
    activeElicitationRequestId === info.request_id ||
    pendingElicitationQueue.some((q) => q.request_id === info.request_id)
  ) {
    return;
  }
  pendingElicitationQueue.push(info);
  if (!activeElicitationRequestId) {
    showNextMcpElicitation();
  }
}

export function handleMcpElicitationResolvedEvent(resolvedId: string): void {
  const idx = pendingElicitationQueue.findIndex((q) => q.request_id === resolvedId);
  if (idx !== -1) {
    pendingElicitationQueue.splice(idx, 1);
  }
  if (activeElicitationRequestId === resolvedId) {
    activeElicitationRequestId = null;
    activeElicitationBindings = [];
    const dialog = $("mcp-elicitation-dialog") as HTMLDialogElement;
    if (dialog.open) {
      dialog.close();
    }
    showNextMcpElicitation();
  }
}

function showNextMcpElicitation(): void {
  const dialog = $("mcp-elicitation-dialog") as HTMLDialogElement;
  if (activeElicitationRequestId || pendingElicitationQueue.length === 0) {
    return;
  }
  const next = pendingElicitationQueue.shift()!;
  activeElicitationRequestId = next.request_id;
  $("mcp-elic-server").textContent = `${next.server_name} (${next.server_id})`;
  $("mcp-elic-req-id").textContent = next.request_id;
  $("mcp-elic-message").textContent = next.message;
  activeElicitationBindings = renderElicitationFields($("mcp-elic-fields"), next.requested_schema);

  if (!dialog.open) {
    dialog.showModal();
  }
  if (activeElicitationBindings.length > 0) {
    activeElicitationBindings[0].focusTarget.focus();
  }
}

async function submitElicitationDecision(
  action: ElicitationAction,
  onSnapshot: (snap: DashboardSnapshot) => void
): Promise<void> {
  if (elicitationSubmitting || !activeElicitationRequestId) return;

  let content: Record<string, unknown> | null = null;
  if (action === "accept") {
    let firstInvalid: ElicitationFieldBinding | null = null;
    for (const binding of activeElicitationBindings) {
      const ok = binding.validate(true);
      if (!ok && !firstInvalid) {
        firstInvalid = binding;
      }
    }
    if (firstInvalid) {
      firstInvalid.focusTarget.focus();
      return;
    }
    content = {};
    for (const binding of activeElicitationBindings) {
      const res = binding.readValue();
      if (res.present) {
        content[binding.key] = res.value;
      }
    }
  }

  const reqId = activeElicitationRequestId;
  const dialog = $("mcp-elicitation-dialog") as HTMLDialogElement;
  const acceptBtn = $("btn-elic-accept") as HTMLButtonElement;
  const declineBtn = $("btn-elic-decline") as HTMLButtonElement;
  const cancelBtn = $("btn-elic-cancel") as HTMLButtonElement;

  elicitationSubmitting = true;
  acceptBtn.disabled = true;
  declineBtn.disabled = true;
  cancelBtn.disabled = true;

  try {
    const snap = await invoke<DashboardSnapshot>("resolve_mcp_elicitation", {
      requestId: reqId,
      action,
      content,
    });
    activeElicitationRequestId = null;
    activeElicitationBindings = [];
    if (dialog.open) {
      dialog.close();
    }
    onSnapshot(snap);
    const toastMsg =
      action === "accept"
        ? "Input MCP Elicitation dikirim ke downstream server."
        : action === "decline"
          ? "Permintaan MCP Elicitation ditolak (Decline)."
          : "Permintaan MCP Elicitation dibatalkan (Cancel).";
    setToast(toastMsg);
    showNextMcpElicitation();
  } catch (err) {
    const msg = String(err);
    setToast(`MCP Elicitation: ${msg}`, "warn");
    if (msg.includes("kadaluarsa") || msg.includes("tidak ditemukan")) {
      activeElicitationRequestId = null;
      activeElicitationBindings = [];
      if (dialog.open) {
        dialog.close();
      }
      showNextMcpElicitation();
    }
  } finally {
    elicitationSubmitting = false;
    acceptBtn.disabled = false;
    declineBtn.disabled = false;
    cancelBtn.disabled = false;
  }
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

let pendingHooksTrustStatus: HooksStatusSummary | null = null;
let hooksTrustOnSnapshot: ((snap: DashboardSnapshot) => void) | null = null;

export function openHooksTrustModal(
  status: HooksStatusSummary,
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  pendingHooksTrustStatus = status;
  hooksTrustOnSnapshot = onSnapshot;
  const dialog = $("hooks-trust-dialog") as HTMLDialogElement;

  $("hooks-trust-meta").textContent =
    `Workspace @${status.workspace_alias} memiliki konfigurasi .airlock/hooks.json (${status.hooks.length} hook).`;
  $("hooks-trust-path").textContent = status.workspace_path;
  $("hooks-trust-sha").textContent = status.current_sha256 ?? "-";

  const warnBox = $("hooks-trust-warning");
  if (status.parse_error) {
    warnBox.hidden = false;
    warnBox.textContent = `Format .airlock/hooks.json bermasalah: ${status.parse_error}`;
  } else if (status.hash_changed) {
    warnBox.hidden = false;
    warnBox.textContent =
      "PERINGATAN: Isi .airlock/hooks.json berubah sejak terakhir di-trust (SHA-256 mismatch). Kepercayaan sebelumnya otomatis dicabut.";
  } else if (!status.trusted && status.hooks_file_exists) {
    warnBox.hidden = false;
    warnBox.textContent =
      "Hooks workspace ini belum di-trust. Perintah shell di bawah hanya akan berjalan setelah Anda menyetujui SHA-256 digest saat ini.";
  } else {
    warnBox.hidden = true;
    warnBox.textContent = "";
  }

  const list = $("hooks-trust-list");
  if (status.hooks.length === 0) {
    const empty = document.createElement("div");
    empty.className = "mono-muted";
    empty.textContent = "Tidak ada hook terdefinisi di .airlock/hooks.json.";
    list.replaceChildren(empty);
  } else {
    const rows: HTMLElement[] = [];
    for (const h of status.hooks) {
      const card = document.createElement("div");
      card.className = "hook-def-card";

      const top = document.createElement("div");
      top.className = "hook-def-top";
      const idCode = document.createElement("code");
      idCode.textContent = h.id;
      const meta = document.createElement("span");
      meta.className = "badge badge-muted";
      meta.textContent = `${h.event}${h.file_glob ? ` (${h.file_glob})` : ""} · ${h.timeout_secs}s · ${h.on_failure}`;
      top.append(idCode, meta);

      const cmdPre = document.createElement("pre");
      cmdPre.className = "mono-pre hook-cmd-pre";
      cmdPre.textContent = h.command;

      card.append(top, cmdPre);
      rows.push(card);
    }
    list.replaceChildren(...rows);
  }

  const confirmBtn = $("btn-confirm-hooks-trust") as HTMLButtonElement;
  confirmBtn.disabled = !status.current_sha256 || Boolean(status.parse_error) || status.trusted;
  confirmBtn.textContent = status.trusted ? "Already Trusted" : "Trust & Enable Hooks";

  if (!dialog.open) {
    dialog.showModal();
  }
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

function clearActiveJobLogsState(): void {
  activeLogJobId = null;
  activeLogNextLine = 0;
  activeLogFetching = false;
  activeLogPendingFetch = false;
  activeLogPendingReset = false;
}

function updateJobLogsModalHeader(job: JobSummary): void {
  $("job-logs-title").textContent = `Job Output: ${job.label || job.job_id}`;
  const statusBadge = $("job-logs-status-badge");
  statusBadge.className = `status-dot ${
    job.status === "running"
      ? "status-warn"
      : job.status === "completed"
        ? "status-online"
        : "status-offline"
  }`;
  statusBadge.textContent = job.exit_code !== null ? `${job.status} (${job.exit_code})` : job.status;

  const timeoutPart =
    job.timeout_secs && job.timeout_secs > 0
      ? ` / ${formatDurationCompact(job.timeout_secs)}`
      : "";
  const idlePart =
    job.status === "running"
      ? job.stall_hint
        ? ` · ${job.stall_hint}`
        : (job.idle_secs ?? 0) >= 10
          ? ` · silent ${formatDurationCompact(job.idle_secs ?? 0)}`
          : " · active"
      : "";
  $("job-logs-meta").textContent =
    `${job.job_id} · ${job.command} · ${formatDurationCompact(job.elapsed_secs)}${timeoutPart} · ${job.line_count} lines${idlePart}`;

  $("job-logs-stream-hint").textContent =
    job.status === "running" ? "LIVE STREAM (AUTO-UPDATING)" : "FINAL OUTPUT";
  ($("btn-stop-job-modal") as HTMLButtonElement).hidden = job.status !== "running";
}

export function handleJobUpdatedForModal(job: JobSummary): void {
  if (activeLogJobId !== job.job_id) return;
  updateJobLogsModalHeader(job);
  if (job.line_count > activeLogNextLine || job.status !== "running") {
    void refreshActiveJobLogs(false);
  }
}

async function refreshActiveJobLogs(reset = false): Promise<void> {
  if (!activeLogJobId) return;
  if (activeLogFetching) {
    activeLogPendingFetch = true;
    activeLogPendingReset = activeLogPendingReset || reset;
    return;
  }

  const targetJobId = activeLogJobId;
  const fromLine = reset ? 0 : activeLogNextLine;
  activeLogFetching = true;

  try {
    const delta = await invoke<JobLogsDelta>("get_background_job_logs_delta", {
      jobId: targetJobId,
      fromLine,
    });
    if (activeLogJobId !== targetJobId) return;

    updateJobLogsModalHeader(delta.summary);

    const pre = $("job-logs-pre");
    if (reset || delta.truncated_earlier || fromLine === 0) {
      pre.textContent =
        delta.lines.length > 0 ? delta.lines.join("\n") : "(belum ada output log)";
    } else if (delta.lines.length > 0) {
      const existing = pre.textContent ?? "";
      const isPlaceholder =
        existing === "Memuat log..." || existing === "(belum ada output log)" || existing === "";
      pre.textContent = isPlaceholder
        ? delta.lines.join("\n")
        : `${existing}\n${delta.lines.join("\n")}`;
    }

    activeLogNextLine = delta.next_from_line;
    const autoScroll = ($("chk-job-logs-autoscroll") as HTMLInputElement).checked;
    if (autoScroll) {
      pre.scrollTop = pre.scrollHeight;
    }
  } catch (err) {
    if (activeLogJobId === targetJobId && fromLine === 0) {
      $("job-logs-pre").textContent = `Gagal memuat log: ${String(err)}`;
    }
  } finally {
    activeLogFetching = false;
    if (activeLogPendingFetch && activeLogJobId === targetJobId) {
      const nextReset = activeLogPendingReset;
      activeLogPendingFetch = false;
      activeLogPendingReset = false;
      void refreshActiveJobLogs(nextReset);
    } else {
      activeLogPendingFetch = false;
      activeLogPendingReset = false;
    }
  }
}

export async function openJobLogsModal(job: JobSummary): Promise<void> {
  activeLogJobId = job.job_id;
  activeLogNextLine = 0;
  activeLogPendingFetch = false;
  activeLogPendingReset = false;
  updateJobLogsModalHeader(job);
  $("job-logs-pre").textContent = "Memuat log...";
  ($("job-logs-dialog") as HTMLDialogElement).showModal();
  await refreshActiveJobLogs(true);
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

  const elicDialog = $("mcp-elicitation-dialog") as HTMLDialogElement;
  const elicForm = $("mcp-elic-form") as HTMLFormElement;
  elicForm.addEventListener("submit", (e) => {
    e.preventDefault();
    void submitElicitationDecision("accept", onSnapshot);
  });
  $("btn-elic-decline").addEventListener(
    "click",
    () => void submitElicitationDecision("decline", onSnapshot)
  );
  $("btn-elic-cancel").addEventListener(
    "click",
    () => void submitElicitationDecision("cancel", onSnapshot)
  );
  elicDialog.addEventListener("cancel", (e) => {
    e.preventDefault();
    void submitElicitationDecision("cancel", onSnapshot);
  });

  const jobLogsDialog = $("job-logs-dialog") as HTMLDialogElement;
  $("btn-stop-job-modal").addEventListener("click", async () => {
    const targetId = activeLogJobId;
    if (!targetId) return;
    const stopBtn = $("btn-stop-job-modal") as HTMLButtonElement;
    stopBtn.disabled = true;
    try {
      await invoke("stop_background_job", { jobId: targetId });
      const next = await invoke<DashboardSnapshot>("get_dashboard_state");
      onSnapshot(next);
      await refreshActiveJobLogs(false);
      setToast(`Job ${targetId} dihentikan.`);
    } catch (err) {
      setToast(`Gagal stop job: ${String(err)}`, "warn");
    } finally {
      stopBtn.disabled = false;
    }
  });
  $("btn-refresh-job-logs").addEventListener("click", () => void refreshActiveJobLogs(false));
  $("chk-job-logs-autoscroll").addEventListener("change", (e) => {
    if ((e.target as HTMLInputElement).checked) {
      const pre = $("job-logs-pre");
      pre.scrollTop = pre.scrollHeight;
    }
  });
  $("btn-copy-job-logs").addEventListener("click", () => {
    void copyToClipboard($("job-logs-pre").textContent ?? "", "Job Logs");
  });
  $("btn-close-job-logs").addEventListener("click", () => {
    clearActiveJobLogsState();
    void closeTopLayer(jobLogsDialog);
  });
  jobLogsDialog.addEventListener("close", () => {
    clearActiveJobLogsState();
  });
  jobLogsDialog.addEventListener("cancel", () => {
    clearActiveJobLogsState();
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

  const hooksDialog = $("hooks-trust-dialog") as HTMLDialogElement;
  hooksDialog.addEventListener("cancel", (e) => {
    e.preventDefault();
    pendingHooksTrustStatus = null;
    void closeTopLayer(hooksDialog);
  });
  $("btn-cancel-hooks-trust").addEventListener("click", () => {
    pendingHooksTrustStatus = null;
    void closeTopLayer(hooksDialog);
  });
  $("btn-confirm-hooks-trust").addEventListener("click", async () => {
    const target = pendingHooksTrustStatus;
    if (!target || !target.current_sha256) return;
    const btn = $("btn-confirm-hooks-trust") as HTMLButtonElement;
    btn.disabled = true;
    try {
      const nextStatus = await invoke<HooksStatusSummary>("trust_workspace_hooks", {
        workspaceAlias: target.workspace_alias,
        expectedSha256: target.current_sha256,
      });
      pendingHooksTrustStatus = null;
      void closeTopLayer(hooksDialog);
      if (hooksTrustOnSnapshot) {
        const snap = await invoke<DashboardSnapshot>("get_dashboard_state");
        hooksTrustOnSnapshot(snap);
      }
      setToast(`Hooks @${nextStatus.workspace_alias} di-trust & diaktifkan.`);
    } catch (err) {
      setToast(`Gagal memverifikasi trust hooks: ${String(err)}`, "warn");
    } finally {
      btn.disabled = false;
    }
  });
}
