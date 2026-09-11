/*
 * loom reference client.
 *
 * This is deliberately not a framework app. It exists to prove one path end to
 * end and to serve as the living specification for any client — including the
 * ported bb UI — that talks to loom-server:
 *
 *   1. The UI is a URL client. There is no server-address setting anywhere.
 *      `BASE` and the socket URL are derived from `window.location.origin`, so
 *      the browser, an installed PWA and the desktop shell's webview are the
 *      same client.
 *   2. The server is the only thing it talks to. It never talks to a daemon.
 *   3. Reconnect is subscribe-then-replay. The socket is (re)opened and the
 *      scope subscribed BEFORE asking for backlog; frames that arrive live in
 *      the meantime are merged with it and deduplicated by `event_id`. Asking
 *      for backlog first would miss anything published between the two calls.
 */

// Derived, never configured. See point 1 above.
const BASE = window.location.origin;
const WS_BASE = (location.protocol === "https:" ? "wss://" : "ws://") + location.host;

const LAST_EVENT_KEY = "loom:last-event:";
const scopeKey = (scope) => `${scope.kind}:${scope.id ?? ""}`;

const els = {
  connection: document.getElementById("connection"),
  threadList: document.getElementById("thread-list"),
  threadHeader: document.getElementById("thread-header"),
  timeline: document.getElementById("timeline"),
  composer: document.getElementById("composer"),
  message: document.getElementById("message"),
  newThread: document.getElementById("new-thread"),
};

/** The thread currently open, or null. */
let current = null;
/** Live frames that arrived before the merge finished, keyed nowhere (dedup later). */
let buffering = null;
/** Event ids already rendered for the life of this page. */
const seen = new Set();
/** A socket per connection attempt; replaced on every reconnect. */
let socket = null;
let reconnectAttempts = 0;
let reconnectTimer = null;

class ThreadView {
  constructor(id) {
    this.id = id;
    this.scope = { kind: "thread", id };
    this.timeline = [];
    this.runs = new Map();
    this.thread = null;
  }
}

function setConnection(state, text) {
  els.connection.className = `status status--${state}`;
  els.connection.textContent = text;
}

async function api(path, options = {}) {
  const response = await fetch(BASE + path, {
    headers: { "content-type": "application/json" },
    ...options,
  });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new Error(body.error || `${response.status} ${response.statusText}`);
  }
  if (response.status === 204) return null;
  return response.json();
}

/* ------------------------------------------------------------------ */
/* Thread list                                                         */
/* ------------------------------------------------------------------ */

async function refreshThreads() {
  const body = await api("/api/v1/threads");
  els.threadList.replaceChildren(
    ...body.threads.map((thread) => {
      const li = document.createElement("li");
      li.className = "thread" + (current && current.id === thread.id ? " thread--active" : "");
      li.innerHTML = `
        <span class="thread__title">${escapeHtml(thread.title || "Untitled thread")}</span>
        <span class="thread__meta"><span class="dot dot--${thread.status}"></span>${thread.status}</span>
      `;
      li.addEventListener("click", () => openThread(thread.id));
      return li;
    }),
  );
  if (!body.threads.length) {
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = "No threads yet.";
    els.threadList.append(li);
  }
}

/* ------------------------------------------------------------------ */
/* Conversation                                                        */
/* ------------------------------------------------------------------ */

function renderTimeline() {
  const view = current;
  els.timeline.replaceChildren(...view.timeline.map(renderEntry));
  els.timeline.scrollTop = els.timeline.scrollHeight;
}

function renderEntry(entry) {
  const li = document.createElement("li");
  li.className = `entry entry--${entry.className}`;
  if (entry.role) {
    const role = document.createElement("span");
    role.className = "entry__role";
    role.textContent = entry.role;
    li.append(role);
  }
  const body = document.createElement("div");
  body.className = "entry__body";
  body.textContent = entry.text;
  li.append(body);
  return li;
}

function pushEntry(entry) {
  const view = current;
  if (!view) return;
  view.timeline.push(entry);
  els.timeline.append(renderEntry(entry));
  els.timeline.scrollTop = els.timeline.scrollHeight;
}

/** Renders one `DomainEvent` carried in a relayed frame's payload. */
function renderDomainEvent(event) {
  switch (event.type) {
    case "thread_message_added":
      pushEntry({
        className: `message message--${event.message.role}`,
        role: event.message.role,
        text: event.message.content,
      });
      break;
    case "thread_status_changed":
      if (current) current.thread = { ...(current.thread || {}), status: event.to };
      pushEntry({ className: "status", text: `thread → ${event.to}` });
      break;
    case "thread_run_event":
      renderRunEvent(event);
      break;
    case "thread_created":
      refreshThreads().catch(() => {});
      break;
    default:
      break;
  }
}

function renderRunEvent(event) {
  const { run_id: runId, event: run } = event;
  switch (run.type) {
    case "output":
      pushEntry({ className: `run run--${run.stream}`, role: "assistant", text: run.text });
      break;
    case "tool_call":
      pushEntry({ className: "tool", text: `⚙ ${run.name} ${JSON.stringify(run.args)}` });
      break;
    case "tool_result":
      pushEntry({ className: "tool", text: `↳ ${run.name}: ${run.output}` });
      break;
    case "notice":
      pushEntry({ className: `notice notice--${run.level}`, text: run.message });
      break;
    case "started":
      if (current) current.runs.set(runId, true);
      break;
    case "finished":
      if (current) current.runs.delete(runId);
      break;
    default:
      break;
  }
}

function renderThreadHeader() {
  const thread = current.thread;
  if (!thread) {
    els.threadHeader.innerHTML = `<p class="empty">Pick a thread, or create one.</p>`;
    els.composer.hidden = true;
    return;
  }
  els.threadHeader.innerHTML = `
    <strong>${escapeHtml(thread.title || "Untitled thread")}</strong>
    <code>${thread.id}</code>
    <span class="dot dot--${thread.status}"></span>${thread.status}
  `;
  els.composer.hidden = false;
}

/* ------------------------------------------------------------------ */
/* The relayed frame path                                              */
/* ------------------------------------------------------------------ */

/**
 * Handles one server frame. Live delivery and replay produce the exact same
 * frame, so this is the single merge point.
 */
function handleEventFrame(frame) {
  if (frame.scope && frame.scope.id !== (current && current.id)) return;
  const key = scopeKey(frame.scope);
  if (seen.has(frame.event_id)) return; // a frame delivered twice (live + replay)
  seen.add(frame.event_id);
  localStorage.setItem(LAST_EVENT_KEY + key, frame.event_id);

  if (buffering) {
    buffering.push(frame);
    return;
  }
  renderFrame(frame);
}

function renderFrame(frame) {
  let payload;
  try {
    payload = JSON.parse(frame.payload);
  } catch {
    payload = { type: "raw", text: frame.payload };
  }
  if (payload.type === "raw") {
    pushEntry({ className: "raw", text: payload.text });
  } else {
    renderDomainEvent(payload);
  }
}

/** Event ids are fixed-width monotonic ULIDs, so lexicographic order is time order. */
function sortFrames(frames) {
  return [...frames].sort((a, b) => (a.event_id < b.event_id ? -1 : a.event_id > b.event_id ? 1 : 0));
}

async function replaySince(key, cursor) {
  const query = new URLSearchParams({ scope_kind: "thread", scope_id: current.id });
  if (cursor) query.set("since", cursor);
  const body = await api(`/api/v1/replay?${query}`);
  return body.frames.filter((frame) => frame.type === "event");
}

/* ------------------------------------------------------------------ */
/* Connection lifecycle                                                */
/* ------------------------------------------------------------------ */

async function openThread(id) {
  clearTimeout(reconnectTimer);
  if (socket) {
    socket.onclose = null;
    socket.close();
    socket = null;
  }
  current = new ThreadView(id);
  seen.clear();
  buffering = [];
  els.timeline.replaceChildren();
  renderThreadHeader();
  refreshThreads().catch(() => {});
  await connect();
}

async function connect() {
  if (!current) return;
  const view = current;
  setConnection("connecting", "connecting…");

  socket = new WebSocket(`${WS_BASE}/ws`);
  socket.onopen = async () => {
    if (current !== view) return;
    reconnectAttempts = 0;
    // Subscribe FIRST. Anything published from here on is queued on this
    // connection and will also be in (or newer than) the replay window.
    socket.send(JSON.stringify({ type: "subscribe", scope: view.scope }));
    setConnection("online", "live");
    try {
      // THEN replay the backlog. Merge by event id; ordering is restored below.
      const cursor = localStorage.getItem(LAST_EVENT_KEY + scopeKey(view.scope)) || "";
      const replayed = await replaySince(view.id, cursor);
      const merged = sortFrames([...replayed, ...(buffering || [])]);
      buffering = null;
      for (const frame of merged) {
        if (seen.has(frame.event_id)) continue;
        seen.add(frame.event_id);
        localStorage.setItem(LAST_EVENT_KEY + scopeKey(view.scope), frame.event_id);
        renderFrame(frame);
      }
    } catch (error) {
      pushEntry({ className: "notice notice--error", text: `replay failed: ${error.message}` });
      buffering = null;
    }
  };

  socket.onmessage = (message) => {
    if (current !== view) return;
    let frame;
    try {
      frame = JSON.parse(message.data);
    } catch {
      return;
    }
    if (frame.type === "event") {
      handleEventFrame(frame);
    } else if (frame.type === "error") {
      pushEntry({ className: "notice notice--error", text: frame.message });
    }
    // welcome / subscribed / pong need no rendering
  };

  socket.onclose = () => {
    if (current !== view) return;
    setConnection("offline", "reconnecting…");
    const delay = Math.min(1000 * 2 ** reconnectAttempts++, 10_000);
    reconnectTimer = setTimeout(() => connect(), delay);
  };

  socket.onerror = () => socket && socket.close();
}

/* ------------------------------------------------------------------ */
/* Commands                                                            */
/* ------------------------------------------------------------------ */

els.newThread.addEventListener("click", async () => {
  try {
    const body = await api("/api/v1/threads", { method: "POST", body: "{}" });
    await refreshThreads();
    await openThread(body.thread.id);
  } catch (error) {
    pushEntry({ className: "notice notice--error", text: error.message });
  }
});

els.composer.addEventListener("submit", async (event) => {
  event.preventDefault();
  const content = els.message.value.trim();
  if (!content || !current) return;
  els.message.value = "";
  try {
    // The reply arrives through the relay like any other frame; the HTTP
    // response is only an acknowledgement.
    await api(`/api/v1/threads/${current.id}/messages`, {
      method: "POST",
      body: JSON.stringify({ content }),
    });
  } catch (error) {
    pushEntry({ className: "notice notice--error", text: error.message });
  }
});

/* ------------------------------------------------------------------ */

function escapeHtml(value) {
  return String(value).replace(
    /[&<>"']/g,
    (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[char],
  );
}

async function main() {
  await refreshThreads().catch((error) => {
    setConnection("offline", error.message);
  });
  setConnection("offline", "idle");
}

main();
