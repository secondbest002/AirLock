import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { $, closeTopLayer, setToast, setupDialogLightDismiss } from "../dom";
import { renderMarkdownInto } from "../markdown";
import type {
  DashboardSnapshot,
  PluginInstallPreview,
  PluginSummary,
  SkillDetail,
  SkillScope,
  SkillSummary,
} from "../types";

type SkillFilter = "all" | SkillScope;

let skillFilter: SkillFilter = "all";
let sourceMode: "local" | "git" = "local";
let activePreview: PluginInstallPreview | null = null;
let editingSecretsPluginId: string | null = null;
let armedUninstallId: string | null = null;
let armedUninstallTimer: number | null = null;

function setInstallStep(step: 1 | 2): void {
  const step1 = $("plugin-step-1");
  const step2 = $("plugin-step-2");
  const backBtn = $("btn-plg-back");
  const inspectBtn = $("btn-plg-inspect");
  const confirmBtn = $("btn-plg-confirm");
  if (step === 1) {
    step1.hidden = false;
    step2.hidden = true;
    backBtn.hidden = true;
    inspectBtn.hidden = false;
    confirmBtn.hidden = true;
  } else {
    step1.hidden = true;
    step2.hidden = false;
    backBtn.hidden = false;
    inspectBtn.hidden = true;
    confirmBtn.hidden = false;
  }
}

function setSourceMode(mode: "local" | "git"): void {
  sourceMode = mode;
  const localBtn = $("btn-plg-src-local");
  const gitBtn = $("btn-plg-src-git");
  const localGroup = $("plugin-local-group");
  const gitGroup = $("plugin-git-group");

  localBtn.classList.toggle("active", mode === "local");
  localBtn.setAttribute("aria-pressed", mode === "local" ? "true" : "false");
  gitBtn.classList.toggle("active", mode === "git");
  gitBtn.setAttribute("aria-pressed", mode === "git" ? "true" : "false");

  localGroup.hidden = mode !== "local";
  gitGroup.hidden = mode !== "git";
}

async function releaseActiveStaging(): Promise<void> {
  if (!activePreview) return;
  const stagingId = activePreview.staging_id;
  activePreview = null;
  try {
    await invoke("cancel_plugin_staging", { stagingId });
  } catch {
    // ignore cleanup failures if already consumed
  }
}

export function openInstallPluginModal(): void {
  void releaseActiveStaging();
  setSourceMode("local");
  setInstallStep(1);
  ($("plugin-local-path") as HTMLInputElement).value = "";
  ($("plugin-git-url") as HTMLInputElement).value = "";
  ($("plugin-subdir") as HTMLInputElement).value = "";
  ($("plugin-enable-immediately") as HTMLInputElement).checked = true;
  const dialog = $("install-plugin-dialog") as HTMLDialogElement;
  if (!dialog.open) {
    dialog.showModal();
  }
}

export function openCreateSkillModal(): void {
  ($("new-skill-name") as HTMLInputElement).value = "";
  ($("new-skill-desc") as HTMLInputElement).value = "";
  ($("new-skill-body") as HTMLTextAreaElement).value = "";
  const dialog = $("create-skill-dialog") as HTMLDialogElement;
  if (!dialog.open) {
    dialog.showModal();
  }
}

function renderPluginPreviewStep2(preview: PluginInstallPreview): void {
  const metaGrid = $("plugin-prev-meta");
  metaGrid.replaceChildren();

  const makeKv = (label: string, val: string): HTMLElement => {
    const item = document.createElement("div");
    item.className = "kv-item";
    const lbl = document.createElement("span");
    lbl.className = "kv-label";
    lbl.textContent = label;
    const code = document.createElement("code");
    code.className = "kv-value";
    code.textContent = val;
    item.append(lbl, code);
    return item;
  };

  metaGrid.append(
    makeKv("Plugin Name", `${preview.manifest.name} v${preview.manifest.version}`),
    makeKv("Manifest SHA-256", `${preview.manifest_sha256.slice(0, 16)}...`),
    makeKv("Source", `${preview.source_type}: ${preview.location}`)
  );

  const warnList = $("plugin-prev-warnings");
  warnList.replaceChildren();
  if (preview.warnings.length === 0) {
    const li = document.createElement("li");
    li.textContent = "Tidak ada command stdio atau endpoint HTTP eksternal terdeteksi (skill-only plugin).";
    warnList.appendChild(li);
  } else {
    for (const w of preview.warnings) {
      const li = document.createElement("li");
      li.textContent = w;
      warnList.appendChild(li);
    }
  }

  const assetsList = $("plugin-prev-assets");
  assetsList.replaceChildren();
  for (const srv of preview.mcp_servers) {
    const li = document.createElement("li");
    const targetDesc =
      srv.transport === "stdio"
        ? `${srv.command} ${srv.args.join(" ")}`.trim()
        : srv.url;
    li.textContent = `MCP [${srv.server_key}] (${srv.transport.toUpperCase()}): ${targetDesc}`;
    assetsList.appendChild(li);
  }
  for (const sk of preview.skills) {
    const li = document.createElement("li");
    li.textContent = `Skill [${sk.name}]: ${sk.description}`;
    assetsList.appendChild(li);
  }
  if (assetsList.children.length === 0) {
    const li = document.createElement("li");
    li.textContent = "Belum ada MCP server atau skill terdeteksi.";
    assetsList.appendChild(li);
  }

  const secWrap = $("plugin-prev-secrets-wrap");
  const secInputs = $("plugin-prev-secrets-inputs");
  secInputs.replaceChildren();
  if (preview.required_secrets.length === 0) {
    secWrap.hidden = true;
  } else {
    secWrap.hidden = false;
    for (const key of preview.required_secrets) {
      const grp = document.createElement("div");
      grp.className = "form-group";
      const inputId = `plg-install-sec-${key}`;
      const lbl = document.createElement("label");
      lbl.htmlFor = inputId;
      lbl.textContent = `Secret: ${key}`;
      const input = document.createElement("input");
      input.id = inputId;
      input.type = "password";
      input.className = "mono-input plugin-secret-input";
      input.dataset.secretKey = key;
      input.autocomplete = "off";
      input.placeholder = "Isi nilai secret (opsional, bisa diisi nanti)...";
      grp.append(lbl, input);
      secInputs.appendChild(grp);
    }
  }
}

function openPluginSecretsModal(plugin: PluginSummary): void {
  editingSecretsPluginId = plugin.id;
  $("plugin-secrets-meta").textContent =
    `${plugin.name} v${plugin.version} — nilai disimpan terenkripsi DPAPI (secrets.dpapi) dan tidak pernah ditulis ke config.json.`;

  const fields = $("plugin-secrets-fields");
  fields.replaceChildren();

  const keys = Array.from(
    new Set([...plugin.required_secrets, ...plugin.configured_secrets])
  ).sort();

  if (keys.length === 0) {
    const note = document.createElement("p");
    note.className = "card-desc";
    note.textContent = "Plugin ini tidak mendeklarasikan placeholder ${SECRET:...} pada mcp.json.";
    fields.appendChild(note);
  } else {
    for (const key of keys) {
      const grp = document.createElement("div");
      grp.className = "form-group";
      const inputId = `plg-edit-sec-${key}`;
      const isConfigured = plugin.configured_secrets.includes(key);
      const lbl = document.createElement("label");
      lbl.htmlFor = inputId;
      lbl.textContent = `${key} ${isConfigured ? "(Tersimpan)" : "(Kosong)"}`;
      const input = document.createElement("input");
      input.id = inputId;
      input.type = "password";
      input.className = "mono-input plugin-edit-secret-input";
      input.dataset.secretKey = key;
      input.autocomplete = "off";
      input.placeholder = isConfigured
        ? "Biarkan kosong untuk tetap pakai yang lama, atau ketik __CLEAR__"
        : "Masukkan secret baru...";
      grp.append(lbl, input);
      fields.appendChild(grp);
    }
  }

  const dialog = $("plugin-secrets-dialog") as HTMLDialogElement;
  if (!dialog.open) {
    dialog.showModal();
  }
}

async function openSkillInspector(name: string): Promise<void> {
  try {
    const detail = await invoke<SkillDetail>("get_skill_detail", { name });
    const s = detail.summary;
    $("skill-detail-title").textContent = `Skill: ${s.name}`;
    $("skill-detail-meta").textContent =
      `${s.source_label} · ~${s.catalog_tokens_est} tok catalog / ~${s.body_tokens_est} tok full · ${s.dir_path}`;

    const resWrap = $("skill-detail-resources");
    resWrap.replaceChildren();

    const makePill = (txt: string, cls = "badge badge-muted"): HTMLElement => {
      const span = document.createElement("span");
      span.className = cls;
      span.textContent = txt;
      return span;
    };

    resWrap.appendChild(makePill(s.scope.toUpperCase(), "skill-scope-badge"));
    if (s.parse_error) {
      resWrap.appendChild(makePill(`Parse Error: ${s.parse_error}`, "badge badge-warn"));
    }
    if (s.allowed_tools.length > 0) {
      resWrap.appendChild(makePill(`Allowed: ${s.allowed_tools.join(", ")}`));
    }
    for (const rel of s.resource_files) {
      resWrap.appendChild(makePill(`res: ${rel}`));
    }

    const mdHost = $("skill-detail-md");
    const contentToRender = detail.body_md.trim() ? detail.body_md : detail.raw_md;
    renderMarkdownInto(mdHost, contentToRender || "(SKILL.md kosong)");

    const dialog = $("skill-detail-dialog") as HTMLDialogElement;
    if (!dialog.open) {
      dialog.showModal();
    }
  } catch (err) {
    setToast(`Gagal memuat detail skill: ${String(err)}`, "warn");
  }
}

function renderPluginsTable(
  plugins: PluginSummary[],
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const tbody = $("plugins-body") as HTMLTableSectionElement;
  tbody.replaceChildren();

  if (plugins.length === 0) {
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 6;
    td.className = "traffic-empty";
    td.textContent = "Belum ada Agent Plugin yang terinstal.";
    tr.appendChild(td);
    tbody.appendChild(tr);
    return;
  }

  for (const plg of plugins) {
    const tr = document.createElement("tr");

    const tdName = document.createElement("td");
    const title = document.createElement("div");
    title.className = "client-name";
    title.textContent = `${plg.name} v${plg.version}`;
    const desc = document.createElement("div");
    desc.className = "client-sub";
    desc.textContent = plg.description || plg.id;
    tdName.append(title, desc);

    const tdSrc = document.createElement("td");
    tdSrc.className = "mono-cell";
    tdSrc.title = plg.source;
    tdSrc.textContent = plg.source;

    const tdAssets = document.createElement("td");
    tdAssets.className = "mono-cell";
    tdAssets.textContent = `${plg.mcp_servers_count} MCP · ${plg.skills_count} Skill`;

    const tdSecrets = document.createElement("td");
    const secBadge = document.createElement("span");
    if (plg.missing_secrets.length > 0) {
      secBadge.className = "badge badge-warn";
      secBadge.textContent = `Missing ${plg.missing_secrets.length}`;
      secBadge.title = `Belum diisi: ${plg.missing_secrets.join(", ")}`;
    } else if (plg.required_secrets.length > 0) {
      secBadge.className = "badge badge-ok";
      secBadge.textContent = `${plg.configured_secrets.length} DPAPI OK`;
    } else {
      secBadge.className = "badge badge-muted";
      secBadge.textContent = "None";
    }
    tdSecrets.appendChild(secBadge);

    const tdStatus = document.createElement("td");
    const stDot = document.createElement("span");
    stDot.className = `status-dot ${plg.enabled ? "status-online" : "status-offline"}`;
    stDot.textContent = plg.enabled ? "ENABLED" : "DISABLED";
    tdStatus.appendChild(stDot);

    const tdAct = document.createElement("td");
    const actWrap = document.createElement("div");
    actWrap.className = "btn-row";

    const toggleBtn = document.createElement("button");
    toggleBtn.type = "button";
    toggleBtn.className = "btn btn-secondary btn-xs";
    toggleBtn.textContent = plg.enabled ? "Disable" : "Enable";
    toggleBtn.setAttribute(
      "aria-label",
      `${plg.enabled ? "Disable" : "Enable"} plugin ${plg.name}`
    );
    toggleBtn.addEventListener("click", async () => {
      try {
        const next = await invoke<DashboardSnapshot>("toggle_plugin", {
          pluginId: plg.id,
          enabled: !plg.enabled,
        });
        onSnapshot(next);
        setToast(`Plugin '${plg.name}' ${!plg.enabled ? "diaktifkan" : "dinonaktifkan"}.`);
      } catch (err) {
        setToast(`Gagal toggle plugin: ${String(err)}`, "warn");
      }
    });
    actWrap.appendChild(toggleBtn);

    if (plg.required_secrets.length > 0 || plg.configured_secrets.length > 0) {
      const secBtn = document.createElement("button");
      secBtn.type = "button";
      secBtn.className = "btn btn-secondary btn-xs";
      secBtn.textContent = "Secrets";
      secBtn.setAttribute("aria-label", `Configure secrets for plugin ${plg.name}`);
      secBtn.addEventListener("click", () => {
        openPluginSecretsModal(plg);
      });
      actWrap.appendChild(secBtn);
    }

    const uninstBtn = document.createElement("button");
    uninstBtn.type = "button";
    uninstBtn.className = "btn btn-danger btn-xs";
    const isArmed = armedUninstallId === plg.id;
    uninstBtn.textContent = isArmed ? "Confirm?" : "Uninstall";
    uninstBtn.setAttribute(
      "aria-label",
      isArmed ? `Confirm uninstall plugin ${plg.name}` : `Uninstall plugin ${plg.name}`
    );
    uninstBtn.addEventListener("click", async () => {
      if (armedUninstallId !== plg.id) {
        armedUninstallId = plg.id;
        uninstBtn.textContent = "Confirm?";
        uninstBtn.setAttribute("aria-label", `Confirm uninstall plugin ${plg.name}`);
        if (armedUninstallTimer !== null) {
          window.clearTimeout(armedUninstallTimer);
        }
        armedUninstallTimer = window.setTimeout(() => {
          armedUninstallId = null;
          uninstBtn.textContent = "Uninstall";
          uninstBtn.setAttribute("aria-label", `Uninstall plugin ${plg.name}`);
        }, 4000);
        return;
      }
      armedUninstallId = null;
      if (armedUninstallTimer !== null) {
        window.clearTimeout(armedUninstallTimer);
        armedUninstallTimer = null;
      }
      try {
        const next = await invoke<DashboardSnapshot>("uninstall_plugin", { pluginId: plg.id });
        onSnapshot(next);
        setToast(`Plugin '${plg.name}' telah dihapus bersih.`);
      } catch (err) {
        setToast(`Uninstall plugin gagal: ${String(err)}`, "warn");
      }
    });
    actWrap.appendChild(uninstBtn);

    tdAct.appendChild(actWrap);
    tr.append(tdName, tdSrc, tdAssets, tdSecrets, tdStatus, tdAct);
    tbody.appendChild(tr);
  }
}

function renderSkillsTable(
  skills: SkillSummary[],
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const tbody = $("skills-body") as HTMLTableSectionElement;
  tbody.replaceChildren();

  const filtered =
    skillFilter === "all" ? skills : skills.filter((s) => s.scope === skillFilter);

  if (filtered.length === 0) {
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 5;
    td.className = "traffic-empty";
    td.textContent =
      skillFilter === "all"
        ? "Belum ada Agent Skill (SKILL.md) yang ditemukan."
        : `Tidak ada skill untuk scope '${skillFilter}'.`;
    tr.appendChild(td);
    tbody.appendChild(tr);
    return;
  }

  for (const sk of filtered) {
    const tr = document.createElement("tr");

    const tdSkill = document.createElement("td");
    const nameRow = document.createElement("div");
    nameRow.className = "client-name";
    nameRow.textContent = sk.name;
    const descRow = document.createElement("div");
    descRow.className = "client-sub";
    descRow.textContent = sk.parse_error
      ? `Parse error: ${sk.parse_error}`
      : sk.description;
    tdSkill.append(nameRow, descRow);

    const tdScope = document.createElement("td");
    const scopeBadge = document.createElement("span");
    scopeBadge.className = `skill-scope-badge skill-scope-${sk.scope}`;
    scopeBadge.textContent = sk.source_label;
    tdScope.appendChild(scopeBadge);
    if (sk.disable_model_invocation) {
      const manualBadge = document.createElement("span");
      manualBadge.className = "skill-scope-badge";
      manualBadge.title = "disable-model-invocation: true (load manually)";
      manualBadge.textContent = "manual";
      tdScope.appendChild(document.createTextNode(" "));
      tdScope.appendChild(manualBadge);
    }

    const tdTokens = document.createElement("td");
    tdTokens.className = "mono-cell skill-token-meta";
    tdTokens.textContent = `~${sk.catalog_tokens_est} / ~${sk.body_tokens_est} tok`;

    const tdState = document.createElement("td");
    const toggle = document.createElement("input");
    toggle.type = "checkbox";
    toggle.className = "tactile-switch";
    toggle.checked = sk.enabled;
    toggle.disabled = Boolean(sk.parse_error);
    toggle.setAttribute("aria-label", `Toggle skill ${sk.name}`);
    toggle.addEventListener("change", async () => {
      try {
        const next = await invoke<DashboardSnapshot>("toggle_skill_enabled", {
          name: sk.name,
          enabled: toggle.checked,
        });
        onSnapshot(next);
        setToast(`Skill '${sk.name}' ${toggle.checked ? "diaktifkan" : "dinonaktifkan"}.`);
      } catch (err) {
        toggle.checked = !toggle.checked;
        setToast(`Gagal mengubah status skill: ${String(err)}`, "warn");
      }
    });
    tdState.appendChild(toggle);

    const tdAct = document.createElement("td");
    const inspectBtn = document.createElement("button");
    inspectBtn.type = "button";
    inspectBtn.className = "btn btn-secondary btn-xs";
    inspectBtn.textContent = "Inspect SKILL.md";
    inspectBtn.setAttribute("aria-label", `Inspect skill ${sk.name}`);
    inspectBtn.addEventListener("click", () => {
      void openSkillInspector(sk.name);
    });
    tdAct.appendChild(inspectBtn);

    tr.append(tdSkill, tdScope, tdTokens, tdState, tdAct);
    tbody.appendChild(tr);
  }
}

export function renderPluginsView(
  snap: DashboardSnapshot,
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const pluginsCount = document.getElementById("hub-plugins-count");
  if (pluginsCount) pluginsCount.textContent = String((snap.plugins ?? []).length);
  const skillsCount = document.getElementById("hub-skills-count");
  if (skillsCount) skillsCount.textContent = String((snap.skills ?? []).length);

  renderPluginsTable(snap.plugins ?? [], onSnapshot);
  renderSkillsTable(snap.skills ?? [], onSnapshot);
}

export function initPluginsView(
  getSnap: () => DashboardSnapshot | null,
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const installDialog = $("install-plugin-dialog") as HTMLDialogElement;
  const secretsDialog = $("plugin-secrets-dialog") as HTMLDialogElement;
  const detailDialog = $("skill-detail-dialog") as HTMLDialogElement;
  const createDialog = $("create-skill-dialog") as HTMLDialogElement;

  setupDialogLightDismiss(detailDialog);

  installDialog.addEventListener("cancel", (e) => {
    e.preventDefault();
    void releaseActiveStaging();
    void closeTopLayer(installDialog);
  });

  secretsDialog.addEventListener("cancel", (e) => {
    e.preventDefault();
    editingSecretsPluginId = null;
    void closeTopLayer(secretsDialog);
  });

  createDialog.addEventListener("cancel", (e) => {
    e.preventDefault();
    void closeTopLayer(createDialog);
  });

  $("btn-install-plugin").addEventListener("click", () => openInstallPluginModal());
  $("btn-plg-src-local").addEventListener("click", () => setSourceMode("local"));
  $("btn-plg-src-git").addEventListener("click", () => setSourceMode("git"));

  $("btn-browse-plugin-dir").addEventListener("click", async () => {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "Pilih Folder Agent Plugin (plugin.json / mcp.json / skills)",
    });
    if (typeof selected === "string" && selected) {
      ($("plugin-local-path") as HTMLInputElement).value = selected;
    }
  });

  $("btn-plg-back").addEventListener("click", () => {
    void releaseActiveStaging();
    setInstallStep(1);
  });

  $("btn-plg-cancel").addEventListener("click", async () => {
    await releaseActiveStaging();
    await closeTopLayer(installDialog);
  });

  $("btn-plg-inspect").addEventListener("click", async () => {
    const location =
      sourceMode === "local"
        ? ($("plugin-local-path") as HTMLInputElement).value.trim()
        : ($("plugin-git-url") as HTMLInputElement).value.trim();
    const subdirVal = ($("plugin-subdir") as HTMLInputElement).value.trim();
    if (!location) {
      setToast("Isi lokasi folder atau Git URL plugin terlebih dahulu.", "warn");
      return;
    }
    const inspectBtn = $("btn-plg-inspect") as HTMLButtonElement;
    inspectBtn.disabled = true;
    inspectBtn.textContent = "Inspecting...";
    try {
      const preview = await invoke<PluginInstallPreview>("inspect_plugin_source", {
        sourceType: sourceMode,
        location,
        subdir: subdirVal || null,
      });
      activePreview = preview;
      renderPluginPreviewStep2(preview);
      setInstallStep(2);
    } catch (err) {
      setToast(`Inspect plugin gagal: ${String(err)}`, "warn");
    } finally {
      inspectBtn.disabled = false;
      inspectBtn.textContent = "Inspect Plugin";
    }
  });

  $("btn-plg-confirm").addEventListener("click", async () => {
    if (!activePreview) return;
    const stagingId = activePreview.staging_id;
    const enableImmediately = ($("plugin-enable-immediately") as HTMLInputElement).checked;
    const secrets: Record<string, string> = {};
    document
      .querySelectorAll<HTMLInputElement>("#plugin-prev-secrets-inputs .plugin-secret-input")
      .forEach((input) => {
        const key = input.dataset.secretKey;
        if (key && input.value.trim()) {
          secrets[key] = input.value.trim();
        }
      });

    const confirmBtn = $("btn-plg-confirm") as HTMLButtonElement;
    confirmBtn.disabled = true;
    confirmBtn.textContent = "Installing...";
    try {
      const next = await invoke<DashboardSnapshot>("confirm_install_plugin", {
        stagingId,
        enableImmediately,
        secrets,
      });
      activePreview = null;
      await closeTopLayer(installDialog);
      onSnapshot(next);
      setToast("Agent Plugin berhasil diinstal & diverifikasi.");
    } catch (err) {
      setToast(`Instalasi plugin gagal: ${String(err)}`, "warn");
    } finally {
      confirmBtn.disabled = false;
      confirmBtn.textContent = "Trust & Install";
    }
  });

  $("btn-cancel-plugin-secrets").addEventListener("click", async () => {
    editingSecretsPluginId = null;
    await closeTopLayer(secretsDialog);
  });

  $("plugin-secrets-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    if (!editingSecretsPluginId) return;
    const secrets: Record<string, string> = {};
    document
      .querySelectorAll<HTMLInputElement>("#plugin-secrets-fields .plugin-edit-secret-input")
      .forEach((input) => {
        const key = input.dataset.secretKey;
        if (key && input.value.trim()) {
          secrets[key] = input.value.trim();
        }
      });
    try {
      const next = await invoke<DashboardSnapshot>("update_plugin_secrets", {
        pluginId: editingSecretsPluginId,
        secrets,
      });
      editingSecretsPluginId = null;
      await closeTopLayer(secretsDialog);
      onSnapshot(next);
      setToast("Secrets plugin berhasil disimpan di DPAPI Vault.");
    } catch (err) {
      setToast(`Gagal menyimpan secrets: ${String(err)}`, "warn");
    }
  });

  $("btn-close-skill-detail").addEventListener("click", () => {
    void closeTopLayer(detailDialog);
  });

  document.querySelectorAll<HTMLButtonElement>(".skill-filter-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      const targetFilter = (btn.dataset.skillFilter as SkillFilter | undefined) ?? "all";
      skillFilter = targetFilter;
      document.querySelectorAll<HTMLButtonElement>(".skill-filter-btn").forEach((b) => {
        const active = b === btn;
        b.classList.toggle("active", active);
        b.setAttribute("aria-pressed", active ? "true" : "false");
      });
      const snap = getSnap();
      if (snap) {
        renderSkillsTable(snap.skills ?? [], onSnapshot);
      }
    });
  });

  $("btn-rescan-skills").addEventListener("click", async () => {
    try {
      const next = await invoke<DashboardSnapshot>("rescan_skills");
      onSnapshot(next);
      setToast(`Skills dipindai ulang (${next.skills.length} skill terdeteksi).`);
    } catch (err) {
      setToast(`Rescan skills gagal: ${String(err)}`, "warn");
    }
  });

  $("btn-new-skill").addEventListener("click", () => openCreateSkillModal());

  $("btn-cancel-create-skill").addEventListener("click", () => {
    void closeTopLayer(createDialog);
  });

  $("create-skill-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const name = ($("new-skill-name") as HTMLInputElement).value.trim();
    const description = ($("new-skill-desc") as HTMLInputElement).value.trim();
    const bodyMd = ($("new-skill-body") as HTMLTextAreaElement).value;
    try {
      const next = await invoke<DashboardSnapshot>("create_global_skill", {
        name,
        description,
        bodyMd,
      });
      await closeTopLayer(createDialog);
      onSnapshot(next);
      setToast(`Global skill '${name}' berhasil dibuat.`);
    } catch (err) {
      setToast(`Gagal membuat skill: ${String(err)}`, "warn");
    }
  });
}
