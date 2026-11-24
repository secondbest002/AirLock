import { copyToClipboard } from "./dom";

type ColAlign = "left" | "center" | "right";

function parseSafeHttpUrl(rawUrl: string): string | null {
  const cleaned = rawUrl.trim().replace(/^<|>$/g, "");
  if (!cleaned) return null;
  try {
    const parsed = new URL(cleaned);
    if (parsed.protocol === "https:" || parsed.protocol === "http:") {
      return parsed.href;
    }
    return null;
  } catch {
    return null;
  }
}

function appendInlineFormatted(parent: HTMLElement, text: string, allowLinks = true): void {
  const tokenRe =
    /(`[^`]+`|\[[^\]]+\]\([^)\s]+(?:\s+"[^"]*")?\)|\*\*[^*]+\*\*|\*[^*]+\*|~~[^~]+~~)/g;
  let lastIdx = 0;
  let match: RegExpExecArray | null;

  while ((match = tokenRe.exec(text)) !== null) {
    if (match.index > lastIdx) {
      parent.appendChild(document.createTextNode(text.slice(lastIdx, match.index)));
    }
    const token = match[0];

    if (token.startsWith("`") && token.endsWith("`")) {
      const code = document.createElement("code");
      code.className = "md-inline-code";
      code.textContent = token.slice(1, -1);
      parent.appendChild(code);
    } else if (token.startsWith("[")) {
      const linkMatch = /^\[([^\]]+)\]\(([^)\s]+)(?:\s+"([^"]*)")?\)$/.exec(token);
      if (linkMatch) {
        const label = linkMatch[1];
        const href = linkMatch[2];
        const title = linkMatch[3];
        const safeUrl = allowLinks ? parseSafeHttpUrl(href) : null;
        if (safeUrl) {
          const a = document.createElement("a");
          a.className = "md-link";
          a.href = safeUrl;
          a.target = "_blank";
          a.rel = "noopener noreferrer";
          if (title) a.title = title;
          appendInlineFormatted(a, label, false);
          parent.appendChild(a);
        } else {
          parent.appendChild(document.createTextNode(`[${label}]`));
        }
      } else {
        parent.appendChild(document.createTextNode(token));
      }
    } else if (token.startsWith("**") && token.endsWith("**") && token.length > 4) {
      const strong = document.createElement("strong");
      appendInlineFormatted(strong, token.slice(2, -2), allowLinks);
      parent.appendChild(strong);
    } else if (token.startsWith("*") && token.endsWith("*") && token.length > 2) {
      const em = document.createElement("em");
      appendInlineFormatted(em, token.slice(1, -1), allowLinks);
      parent.appendChild(em);
    } else if (token.startsWith("~~") && token.endsWith("~~") && token.length > 4) {
      const del = document.createElement("del");
      del.className = "md-del";
      appendInlineFormatted(del, token.slice(2, -2), allowLinks);
      parent.appendChild(del);
    } else {
      parent.appendChild(document.createTextNode(token));
    }

    lastIdx = match.index + token.length;
  }

  if (lastIdx < text.length) {
    parent.appendChild(document.createTextNode(text.slice(lastIdx)));
  }
}

function splitTableRow(rawLine: string): string[] {
  let trimmed = rawLine.trim();
  if (trimmed.startsWith("|")) trimmed = trimmed.slice(1);
  if (trimmed.endsWith("|") && !trimmed.endsWith("\\|")) {
    trimmed = trimmed.slice(0, -1);
  }
  const cells: string[] = [];
  let cur = "";
  for (let i = 0; i < trimmed.length; i++) {
    const ch = trimmed[i];
    if (ch === "\\" && i + 1 < trimmed.length && trimmed[i + 1] === "|") {
      cur += "|";
      i++;
      continue;
    }
    if (ch === "|") {
      cells.push(cur.trim());
      cur = "";
      continue;
    }
    cur += ch;
  }
  cells.push(cur.trim());
  return cells;
}

function isTableSeparatorLine(line: string): boolean {
  const trimmed = line.trim();
  if (!trimmed.includes("|") && !trimmed.includes("-")) return false;
  const cells = splitTableRow(trimmed);
  if (cells.length === 0) return false;
  return cells.every((c) => /^:?-{2,}:?$/.test(c));
}

function parseTableAlignments(line: string): ColAlign[] {
  return splitTableRow(line).map((c) => {
    const starts = c.startsWith(":");
    const ends = c.endsWith(":");
    if (starts && ends) return "center";
    if (ends) return "right";
    return "left";
  });
}

function buildMarkdownTable(
  headerCells: string[],
  alignments: ColAlign[],
  bodyRows: string[][]
): HTMLElement {
  const wrapper = document.createElement("div");
  wrapper.className = "md-table-wrap";
  wrapper.tabIndex = 0;
  wrapper.setAttribute("role", "region");
  wrapper.setAttribute("aria-label", "Markdown table");

  const table = document.createElement("table");
  table.className = "md-table";

  const thead = document.createElement("thead");
  const headTr = document.createElement("tr");
  headerCells.forEach((cell, idx) => {
    const th = document.createElement("th");
    th.scope = "col";
    const align = alignments[idx] ?? "left";
    if (align !== "left") {
      th.className = `md-align-${align}`;
    }
    appendInlineFormatted(th, cell);
    headTr.appendChild(th);
  });
  thead.appendChild(headTr);
  table.appendChild(thead);

  const tbody = document.createElement("tbody");
  for (const row of bodyRows) {
    const tr = document.createElement("tr");
    for (let idx = 0; idx < headerCells.length; idx++) {
      const td = document.createElement("td");
      const align = alignments[idx] ?? "left";
      if (align !== "left") {
        td.className = `md-align-${align}`;
      }
      appendInlineFormatted(td, row[idx] ?? "");
      tr.appendChild(td);
    }
    tbody.appendChild(tr);
  }
  table.appendChild(tbody);

  wrapper.appendChild(table);
  return wrapper;
}

function renderProseBlock(container: HTMLElement, blockText: string): void {
  const lines = blockText.split("\n");
  let listEl: HTMLUListElement | HTMLOListElement | null = null;
  let listKind: "ul" | "ol" | null = null;

  const flushList = (): void => {
    if (listEl) {
      container.appendChild(listEl);
      listEl = null;
      listKind = null;
    }
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trimEnd();
    const trimmed = line.trim();
    if (!trimmed) {
      flushList();
      continue;
    }

    if (
      line.includes("|") &&
      i + 1 < lines.length &&
      isTableSeparatorLine(lines[i + 1])
    ) {
      flushList();
      const headers = splitTableRow(line);
      const alignments = parseTableAlignments(lines[i + 1]);
      i += 2;
      const bodyRows: string[][] = [];
      while (i < lines.length) {
        const rowLine = lines[i].trim();
        if (!rowLine || !rowLine.includes("|")) {
          i--;
          break;
        }
        bodyRows.push(splitTableRow(rowLine));
        i++;
      }
      container.appendChild(buildMarkdownTable(headers, alignments, bodyRows));
      continue;
    }

    if (/^(?:-{3,}|\*{3,}|_{3,})$/.test(trimmed)) {
      flushList();
      const hr = document.createElement("hr");
      hr.className = "md-hr";
      container.appendChild(hr);
      continue;
    }

    const headingMatch = /^(#{1,4})\s+(.+)$/.exec(line);
    if (headingMatch) {
      flushList();
      const level = headingMatch[1].length;
      const tag =
        level === 1 ? "h3" : level === 2 ? "h4" : level === 3 ? "h5" : "h6";
      const h = document.createElement(tag);
      h.className = "md-heading";
      appendInlineFormatted(h, headingMatch[2]);
      container.appendChild(h);
      continue;
    }

    if (line.startsWith("> ")) {
      flushList();
      const bq = document.createElement("blockquote");
      bq.className = "md-quote";
      appendInlineFormatted(bq, line.slice(2));
      container.appendChild(bq);
      continue;
    }

    const taskMatch = /^\s*[-*]\s+\[([ xX])\]\s+(.+)$/.exec(line);
    if (taskMatch) {
      if (listKind !== "ul") {
        flushList();
        listEl = document.createElement("ul");
        listEl.className = "md-list md-task-list";
        listKind = "ul";
      }
      const li = document.createElement("li");
      li.className = "md-task-item";
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.className = "md-task-checkbox";
      cb.disabled = true;
      cb.checked = taskMatch[1].toLowerCase() === "x";
      cb.setAttribute("aria-label", taskMatch[1].toLowerCase() === "x" ? "Selesai" : "Belum selesai");
      const labelSpan = document.createElement("span");
      appendInlineFormatted(labelSpan, taskMatch[2]);
      li.append(cb, labelSpan);
      listEl!.appendChild(li);
      continue;
    }

    const ulMatch = /^\s*[-*]\s+(.+)$/.exec(line);
    if (ulMatch) {
      if (listKind !== "ul") {
        flushList();
        listEl = document.createElement("ul");
        listEl.className = "md-list";
        listKind = "ul";
      }
      const li = document.createElement("li");
      appendInlineFormatted(li, ulMatch[1]);
      listEl!.appendChild(li);
      continue;
    }

    const olMatch = /^\s*\d+\.\s+(.+)$/.exec(line);
    if (olMatch) {
      if (listKind !== "ol") {
        flushList();
        listEl = document.createElement("ol");
        listEl.className = "md-list";
        listKind = "ol";
      }
      const li = document.createElement("li");
      appendInlineFormatted(li, olMatch[1]);
      listEl!.appendChild(li);
      continue;
    }

    flushList();
    const p = document.createElement("p");
    p.className = "md-para";
    appendInlineFormatted(p, line);
    container.appendChild(p);
  }

  flushList();
}

function buildCodeBlock(lang: string, codeText: string): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "md-code-block";

  const hdr = document.createElement("div");
  hdr.className = "md-code-hdr";

  const langLabel = document.createElement("span");
  langLabel.textContent = lang || "text";

  const copyBtn = document.createElement("button");
  copyBtn.type = "button";
  copyBtn.className = "btn btn-secondary btn-xs";
  copyBtn.textContent = "Copy";
  copyBtn.addEventListener("click", () => {
    void copyToClipboard(codeText, "Code block");
  });

  hdr.append(langLabel, copyBtn);

  const pre = document.createElement("pre");
  const code = document.createElement("code");
  code.textContent = codeText;
  pre.appendChild(code);

  wrap.append(hdr, pre);
  return wrap;
}

export function renderMarkdownInto(container: HTMLElement, raw: string): void {
  container.replaceChildren();
  const normalized = raw.replace(/\r\n/g, "\n");
  const lines = normalized.split("\n");

  let inFence = false;
  let fenceLang = "";
  let fenceLines: string[] = [];
  let proseLines: string[] = [];

  const flushProse = (): void => {
    if (proseLines.length > 0) {
      renderProseBlock(container, proseLines.join("\n"));
      proseLines = [];
    }
  };

  for (const line of lines) {
    const trimmed = line.trim();
    if (!inFence && trimmed.startsWith("```")) {
      flushProse();
      inFence = true;
      fenceLang = trimmed.slice(3).trim();
      fenceLines = [];
    } else if (inFence && trimmed === "```") {
      container.appendChild(buildCodeBlock(fenceLang, fenceLines.join("\n")));
      inFence = false;
      fenceLang = "";
      fenceLines = [];
    } else if (inFence) {
      fenceLines.push(line);
    } else {
      proseLines.push(line);
    }
  }

  if (inFence) {
    container.appendChild(buildCodeBlock(fenceLang, fenceLines.join("\n")));
  } else {
    flushProse();
  }
}
