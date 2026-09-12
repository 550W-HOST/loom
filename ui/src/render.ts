import {
  buildTimelineRowTitle,
  type ThreadTimelineViewRow,
} from "@bb/thread-view";

const TITLE_OPTIONS = {
  summaryStyle: "bundle",
  workStyle: "default",
} as const;

function textElement(tag: string, className: string, text: string): HTMLElement {
  const element = document.createElement(tag);
  element.className = className;
  element.textContent = text;
  return element;
}

function statusLabel(status: string | null | undefined): string | null {
  switch (status) {
    case "pending":
      return "running";
    case "completed":
      return "completed";
    case "error":
      return "error";
    case "interrupted":
      return "interrupted";
    default:
      return null;
  }
}

function appendStatus(header: HTMLElement, status: string | null | undefined): void {
  const label = statusLabel(status);
  if (label) header.append(textElement("span", `row__status row__status--${status}`, label));
}

function rowTitle(row: ThreadTimelineViewRow): string {
  if (row.kind === "conversation") return row.role === "user" ? "User" : "Assistant";
  try {
    return buildTimelineRowTitle(row, TITLE_OPTIONS).plain;
  } catch {
    return row.kind === "system" ? row.title : row.kind === "turn" ? "Turn" : row.kind;
  }
}

function appendDetail(parent: HTMLElement, text: string | null | undefined, className = "row__detail"): void {
  if (!text) return;
  parent.append(textElement("pre", className, text));
}

function appendWorkBody(parent: HTMLElement, row: Extract<ThreadTimelineViewRow, { kind: "work" }>): void {
  switch (row.workKind) {
    case "command":
      appendDetail(parent, `$ ${row.command}`, "row__command");
      appendDetail(parent, row.output);
      if (row.cwd) appendDetail(parent, `cwd ${row.cwd}`, "row__meta");
      return;
    case "tool":
      if (row.toolArgs) appendDetail(parent, JSON.stringify(row.toolArgs, null, 2), "row__meta");
      appendDetail(parent, row.output);
      return;
    case "file-change":
      appendDetail(parent, row.change.diff);
      return;
    case "file-read":
      appendDetail(parent, row.path, "row__meta");
      return;
    case "search":
      appendDetail(parent, `${row.query}${row.path ? ` in ${row.path}` : ""}`, "row__meta");
      return;
    case "web-search":
      appendDetail(parent, row.queries.join(", "), "row__meta");
      return;
    case "web-fetch":
      appendDetail(parent, row.url, "row__meta");
      appendDetail(parent, row.prompt);
      return;
    case "image-view":
      appendDetail(parent, row.path, "row__meta");
      return;
    case "image-generation":
      appendDetail(parent, row.prompt);
      appendDetail(parent, row.error, "row__error");
      return;
    case "plan-steps":
      for (const step of row.steps) {
        parent.append(textElement("div", "row__plan-step", `${step.status === "completed" ? "[x]" : step.status === "active" ? "[>]" : step.status === "failed" ? "[!]" : "[ ]"} ${step.step}`));
      }
      return;
    case "delegation":
      appendDetail(parent, row.description);
      if (row.childRows.length > 0) parent.append(renderRows(row.childRows));
      return;
    case "approval":
      if ("statusReason" in row) appendDetail(parent, row.statusReason, "row__meta");
      return;
    case "question":
      appendDetail(parent, `${row.questions.length} question${row.questions.length === 1 ? "" : "s"}`, "row__meta");
      return;
    case "workflow":
      appendDetail(parent, row.description, "row__meta");
      appendDetail(parent, row.summary);
      appendDetail(parent, row.error, "row__error");
      return;
    default:
      return;
  }
}

function renderRow(row: ThreadTimelineViewRow): HTMLLIElement {
  const item = document.createElement("li");
  item.className = `timeline-row timeline-row--${row.kind}`;
  item.dataset.rowId = row.id;
  const header = document.createElement("div");
  header.className = "row__header";
  header.append(textElement("span", "row__title", rowTitle(row)));

  if (row.kind === "conversation") {
    item.classList.add(`timeline-row--${row.role}`);
    item.append(header, textElement("div", "row__content", row.text));
    return item;
  }
  if (row.kind === "system") {
    appendStatus(header, row.status);
    item.append(header);
    appendDetail(item, row.detail, row.systemKind === "error" ? "row__error" : "row__detail");
    return item;
  }
  appendStatus(header, row.status);
  item.append(header);
  if (row.kind === "work") {
    appendWorkBody(item, row);
  } else if (row.kind === "turn" && row.children) {
    item.append(renderRows(row.children));
  } else if ((row.kind === "bundle-summary" || row.kind === "step-summary") && row.children.length > 0) {
    item.append(renderRows(row.children));
  }
  return item;
}

export function renderRows(rows: readonly ThreadTimelineViewRow[]): HTMLOListElement {
  const list = document.createElement("ol");
  list.className = "timeline timeline--nested";
  for (const row of rows) list.append(renderRow(row));
  return list;
}

export function renderTimeline(
  target: HTMLElement,
  rows: readonly ThreadTimelineViewRow[],
  notices: readonly string[] = [],
): void {
  const shouldStickToBottom = target.scrollHeight - target.scrollTop - target.clientHeight < 72;
  const list = renderRows(rows);
  for (const notice of notices) {
    const item = document.createElement("li");
    item.className = "timeline-row timeline-row--notice";
    item.append(textElement("div", "row__error", notice));
    list.append(item);
  }
  target.replaceChildren(...list.children);
  if (shouldStickToBottom || rows.length === 0) target.scrollTop = target.scrollHeight;
}

export function renderThreadList(
  target: HTMLElement,
  threads: readonly { id: string; title: string | null; status: string }[],
  activeId: string | null,
  onSelect: (id: string) => void,
): void {
  target.replaceChildren();
  if (threads.length === 0) {
    target.append(textElement("li", "empty", "No threads yet."));
    return;
  }
  for (const thread of threads) {
    const item = document.createElement("li");
    item.className = `thread${thread.id === activeId ? " thread--active" : ""}`;
    item.tabIndex = 0;
    item.setAttribute("role", "button");
    item.append(
      textElement("span", "thread__title", thread.title?.trim() || "Untitled thread"),
      textElement("span", "thread__meta", `${thread.status}`),
    );
    item.addEventListener("click", () => onSelect(thread.id));
    item.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        onSelect(thread.id);
      }
    });
    target.append(item);
  }
}
