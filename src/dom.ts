import { writeText } from "@tauri-apps/plugin-clipboard-manager";

export type ToastKind = "info" | "warn" | "offline";
export type AccentTheme = "blue" | "mint" | "amber" | "mono";

export const ACCENT_ORDER: readonly AccentTheme[] = ["blue", "mint", "amber", "mono"];

export const ACCENT_LABELS: Record<AccentTheme, string> = {
  blue: "Precision Blue",
  mint: "Electric Mint",
  amber: "Solar Amber",
  mono: "Carbon Mono",
};

export function nextAccentTheme(current?: string): AccentTheme {
  const idx = ACCENT_ORDER.indexOf((current ?? "blue") as AccentTheme);
  return ACCENT_ORDER[(idx + 1) % ACCENT_ORDER.length];
}

const ACCENT_STORAGE_KEY = "airlock_accent";

export function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`Missing element #${id}`);
  return el;
}

export function setToast(msg: string, kind: ToastKind = "info"): void {
  $("toast-region").textContent = msg;
  const stack = document.getElementById("toast-stack");
  if (!stack || !msg.trim()) return;

  while (stack.children.length >= 4 && stack.firstElementChild) {
    stack.firstElementChild.remove();
  }

  const item = document.createElement("div");
  item.className = "toast-item";

  const dot = document.createElement("span");
  dot.className = `live-dot ${kind === "warn" ? "warn" : kind === "offline" ? "offline" : "online"}`;
  dot.setAttribute("aria-hidden", "true");

  const text = document.createElement("span");
  text.textContent = msg;

  item.append(dot, text);
  stack.appendChild(item);

  window.setTimeout(() => {
    item.remove();
  }, 3400);
}

export async function copyToClipboard(text: string, label: string): Promise<void> {
  try {
    await writeText(text);
    setToast(`${label} disalin ke clipboard.`);
  } catch {
    await navigator.clipboard.writeText(text);
    setToast(`${label} disalin ke clipboard.`);
  }
}

export function maskToken(token: string): string {
  if (!token) return "---";
  if (token.length <= 6) return "••••••";
  return `${"•".repeat(12)}${token.slice(-4)}`;
}

export function escapeHtml(raw: string): string {
  return raw
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

export function formatDurationCompact(secs: number): string {
  const safe = Math.max(0, Math.floor(secs));
  if (safe < 60) return `${safe}s`;
  const mins = Math.floor(safe / 60);
  const remSecs = safe % 60;
  if (mins < 60) return remSecs > 0 ? `${mins}m ${remSecs}s` : `${mins}m`;
  const hours = Math.floor(mins / 60);
  const remMins = mins % 60;
  return remMins > 0 ? `${hours}h ${remMins}m` : `${hours}h`;
}

export function syncRailIndicator(): void {
  const activeBtn = document.querySelector<HTMLElement>(".rail-btn.active");
  const indicator = document.getElementById("rail-indicator");
  if (!activeBtn || !indicator) return;
  const y = activeBtn.offsetTop + 8;
  indicator.style.transform = `translateY(${y}px)`;
}

export function positionGuardrailGlider(): void {
  const track = document.getElementById("guardrail-track");
  const glider = document.getElementById("guardrail-glider");
  const activeBtn = track?.querySelector<HTMLElement>(".guardrail-btn.active");
  if (!track || !glider || !activeBtn) return;
  const trackW = track.clientWidth;
  if (!trackW) return;
  const x = activeBtn.offsetLeft;
  const sx = activeBtn.offsetWidth / trackW;
  glider.style.transform = `translateX(${x}px) scaleX(${sx})`;
}

export function switchTab(tabName: string): void {
  const currentActive = document.querySelector<HTMLElement>(".view-panel.active");
  if (currentActive?.id === `panel-${tabName}`) {
    return;
  }

  const applySwitch = (): void => {
    document.querySelectorAll<HTMLButtonElement>(".rail-btn").forEach((btn) => {
      const active = btn.dataset.tab === tabName;
      btn.classList.toggle("active", active);
      btn.setAttribute("aria-selected", active ? "true" : "false");
    });
    document.querySelectorAll<HTMLElement>(".view-panel").forEach((panel) => {
      panel.classList.toggle("active", panel.id === `panel-${tabName}`);
    });
    syncRailIndicator();
  };

  const targetHeading = document.getElementById(`heading-${tabName}`);
  const doc = document as Document & {
    startViewTransition?: (cb: () => void) => {
      ready?: Promise<void>;
      finished: Promise<void>;
    };
  };
  const reducedMotion = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;

  if (!doc.startViewTransition || reducedMotion) {
    applySwitch();
    targetHeading?.focus({ preventScroll: true });
    return;
  }

  const transition = doc.startViewTransition(applySwitch);
  void transition.ready?.catch(() => {});
  void transition.finished
    .catch(() => {
      // Ignore skipped/aborted view transitions on rapid tab switches or viewport resizes
    })
    .finally(() => {
      targetHeading?.focus({ preventScroll: true });
    });
}

let supportsDisplayTransition: boolean | undefined;

function canTransitionDisplay(): boolean {
  if (supportsDisplayTransition !== undefined) return supportsDisplayTransition;
  if (!window.CSS?.supports?.("transition-behavior", "allow-discrete") || !document.body) {
    supportsDisplayTransition = false;
    return false;
  }
  const probe = document.createElement("div");
  probe.style.cssText = "transition: display 1s allow-discrete !important; display: block;";
  document.body.appendChild(probe);
  getComputedStyle(probe).display;
  probe.style.display = "none";
  supportsDisplayTransition = getComputedStyle(probe).display === "block";
  probe.remove();
  return supportsDisplayTransition;
}

function supportsTopLayerExit(): boolean {
  return Boolean(window.CSS?.supports?.("overlay", "auto") && canTransitionDisplay());
}

export async function closeTopLayer(dialog: HTMLDialogElement): Promise<void> {
  if (!dialog.open) return;
  if (!supportsTopLayerExit()) {
    dialog.setAttribute("data-closing", "");
    const animations = dialog.getAnimations({ subtree: true });
    if (animations.length > 0) {
      await Promise.race([
        Promise.allSettled(animations.map((a) => a.finished)),
        new Promise<void>((resolve) => window.setTimeout(resolve, 600)),
      ]);
    }
    if (!dialog.hasAttribute("data-closing")) return;
    dialog.removeAttribute("data-closing");
  }
  dialog.close();
}

export function setupDialogLightDismiss(dialog: HTMLDialogElement): void {
  const closedBy = dialog.getAttribute("closedby");
  if (closedBy === "any") {
    dialog.addEventListener("cancel", (event) => {
      if (!supportsTopLayerExit()) {
        event.preventDefault();
        void closeTopLayer(dialog);
      }
    });

    if (!("closedBy" in HTMLDialogElement.prototype)) {
      dialog.addEventListener("click", (event) => {
        if (event.target !== dialog) return;
        const rect = dialog.getBoundingClientRect();
        const inside =
          rect.top <= event.clientY &&
          event.clientY <= rect.top + rect.height &&
          rect.left <= event.clientX &&
          event.clientX <= rect.left + rect.width;
        if (!inside) {
          void closeTopLayer(dialog);
        }
      });
    }
  }
}

export function setAccentTheme(accent: AccentTheme, announce = true): void {
  document.documentElement.dataset.accent = accent;
  try {
    localStorage.setItem(ACCENT_STORAGE_KEY, accent);
  } catch {
    // ignore storage errors
  }
  const display = ACCENT_LABELS[accent];
  const label = document.getElementById("accent-name-label");
  if (label) {
    label.textContent = display;
  }
  const btn = document.getElementById("btn-toggle-accent");
  if (btn) {
    btn.title = `Accent: ${display} — klik untuk ganti`;
  }
  if (announce) {
    setToast(`Accent Obsidian Studio: ${display}`);
  }
}

export function initAccentTheme(): AccentTheme {
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(ACCENT_STORAGE_KEY);
  } catch {
    stored = null;
  }
  const normalized = stored === "cyan" ? "blue" : stored;
  const accent: AccentTheme = ACCENT_ORDER.includes(normalized as AccentTheme)
    ? (normalized as AccentTheme)
    : "blue";
  setAccentTheme(accent, false);
  return accent;
}

export function activateSubTab(groupName: string, key: string): void {
  const tabs = Array.from(
    document.querySelectorAll<HTMLButtonElement>(`[data-subtab-group="${groupName}"]`)
  );
  const panels = Array.from(
    document.querySelectorAll<HTMLElement>(`[data-subpanel-group="${groupName}"]`)
  );
  if (tabs.length === 0) return;

  for (const tab of tabs) {
    const active = tab.dataset.subtabKey === key;
    tab.classList.toggle("active", active);
    tab.setAttribute("aria-selected", active ? "true" : "false");
    tab.setAttribute("tabindex", active ? "0" : "-1");
  }
  for (const panel of panels) {
    panel.hidden = panel.dataset.subpanelKey !== key;
  }
}

export function initSubTabs(groupName: string): void {
  const tabs = Array.from(
    document.querySelectorAll<HTMLButtonElement>(`[data-subtab-group="${groupName}"]`)
  );
  if (tabs.length === 0) return;

  tabs.forEach((tab, idx) => {
    tab.addEventListener("click", () => {
      const key = tab.dataset.subtabKey;
      if (key) activateSubTab(groupName, key);
    });
    tab.addEventListener("keydown", (e) => {
      let nextIdx: number | null = null;
      if (e.key === "ArrowRight") nextIdx = (idx + 1) % tabs.length;
      else if (e.key === "ArrowLeft") nextIdx = (idx - 1 + tabs.length) % tabs.length;
      else if (e.key === "Home") nextIdx = 0;
      else if (e.key === "End") nextIdx = tabs.length - 1;
      if (nextIdx === null) return;
      e.preventDefault();
      const target = tabs[nextIdx];
      const key = target?.dataset.subtabKey;
      if (target && key) {
        activateSubTab(groupName, key);
        target.focus();
      }
    });
  });

  const initial = tabs.find((t) => t.getAttribute("aria-selected") === "true") ?? tabs[0];
  if (initial?.dataset.subtabKey) {
    activateSubTab(groupName, initial.dataset.subtabKey);
  }
}
