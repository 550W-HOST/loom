import { projectThreadFrames } from "./loom-events.js";
import {
  createBrowserCursorStore,
  ThreadRelaySubscription,
  type ConnectionState,
} from "./relay.js";
import { renderThreadList, renderTimeline } from "./render.js";
import type {
  LoomProject,
  LoomProjectsResponse,
  LoomThread,
  LoomThreadsResponse,
  RelayEventFrame,
} from "./types.js";

const BASE = window.location.origin;
const WS_BASE = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}`;

const els = {
  connection: document.getElementById("connection")!,
  composer: document.getElementById("composer") as HTMLFormElement,
  message: document.getElementById("message") as HTMLTextAreaElement,
  newThread: document.getElementById("new-thread") as HTMLButtonElement,
  project: document.getElementById("project") as HTMLSelectElement,
  threadHeader: document.getElementById("thread-header")!,
  threadList: document.getElementById("thread-list")!,
  timeline: document.getElementById("timeline")!,
};

interface ThreadSession {
  thread: LoomThread;
  frames: RelayEventFrame[];
  notices: string[];
  subscription: ThreadRelaySubscription;
}

let threads: LoomThread[] = [];
let projects: LoomProject[] = [];
let current: ThreadSession | null = null;

function setConnection(state: ConnectionState, message: string): void {
  els.connection.className = `status status--${state}`;
  els.connection.textContent = message;
}

async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await fetch(`${BASE}${path}`, {
    ...init,
    headers: {
      "content-type": "application/json",
      ...(init.headers ?? {}),
    },
  });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new Error(typeof body.error === "string" ? body.error : `${response.status} ${response.statusText}`);
  }
  return response.json() as Promise<T>;
}

function statusText(status: LoomThread["status"]): string {
  return status === "working" ? "working" : status;
}

function renderHeader(session: ThreadSession | null): void {
  if (!session) {
    els.threadHeader.replaceChildren(textNode("Pick a thread, or create one."));
    els.composer.hidden = true;
    return;
  }
  els.threadHeader.replaceChildren(
    textNode(session.thread.title?.trim() || "Untitled thread", "header__title"),
    textNode(session.thread.id, "header__id"),
    textNode(statusText(session.thread.status), `header__status header__status--${session.thread.status}`),
  );
  els.composer.hidden = session.thread.status === "archived";
}

function textNode(text: string, className = "empty"): HTMLElement {
  const node = document.createElement("span");
  node.className = className;
  node.textContent = text;
  return node;
}

function renderSidebar(): void {
  renderThreadList(els.threadList, threads, current?.thread.id ?? null, (id) => {
    void openThread(id);
  });
}

function renderSession(session: ThreadSession): void {
  if (current !== session) return;
  try {
    const projected = projectThreadFrames({
      frames: session.frames,
      threadName: session.thread.title?.trim() || "Thread",
      threadStatus: session.thread.status,
    });
    if (projected.status && projected.status !== session.thread.status) {
      session.thread = { ...session.thread, status: projected.status as LoomThread["status"] };
      threads = threads.map((thread) => thread.id === session.thread.id ? session.thread : thread);
      renderSidebar();
    }
    renderHeader(session);
    renderTimeline(els.timeline, projected.rows, session.notices);
  } catch (error) {
    renderHeader(session);
    renderTimeline(els.timeline, [], [
      ...session.notices,
      `Timeline projection failed: ${error instanceof Error ? error.message : String(error)}`,
    ]);
  }
}

async function replayPage(threadId: string, since: string | null) {
  const query = new URLSearchParams({
    scope_kind: "thread",
    scope_id: threadId,
    // The no-cursor endpoint returns one newest window and has no older-page
    // cursor. Load the full retained window on a fresh thread; resumed pages
    // stay bounded and advance through `has_more`.
    limit: since ? "100" : "10000",
  });
  if (since) query.set("since", since);
  return api<{ frames: unknown[]; has_more: boolean }>(`/api/v1/replay?${query.toString()}`);
}

async function openThread(id: string): Promise<void> {
  current?.subscription.stop();
  const thread = threads.find((candidate) => candidate.id === id);
  if (!thread) return;
  const session: ThreadSession = {
    thread,
    frames: [],
    notices: [],
    subscription: null as unknown as ThreadRelaySubscription,
  };
  const subscription = new ThreadRelaySubscription({
    threadId: id,
    wsUrl: `${WS_BASE}/ws`,
    cursorStore: createBrowserCursorStore(),
    loadReplay: ({ threadId, since }) => replayPage(threadId, since),
    onFrames(frames) {
      session.frames = frames;
      renderSession(session);
    },
    onNotice(message) {
      session.notices = [...session.notices.slice(-2), message];
      renderSession(session);
    },
    onState(state) {
      if (state === "online") setConnection(state, "live");
      else if (state === "connecting") setConnection(state, "connecting...");
      else setConnection(state, "reconnecting...");
    },
  });
  session.subscription = subscription;
  current = session;
  els.timeline.replaceChildren();
  renderHeader(session);
  renderSidebar();
  subscription.start();
}

async function refreshThreads(): Promise<void> {
  const body = await api<LoomThreadsResponse>("/api/v1/threads");
  threads = Array.isArray(body) ? body : [];
  renderSidebar();
}

/**
 * Loads the project list and fills the create-thread selector.
 *
 * A thread must name a project, so this is not optional chrome: with no active
 * project the create button stays disabled rather than sending a request the
 * server would reject.
 */
async function refreshProjects(): Promise<void> {
  const body = await api<LoomProjectsResponse>("/api/v1/projects");
  projects = Array.isArray(body) ? body : [];
  els.project.replaceChildren(
    ...projects.map((project) => {
      const option = document.createElement("option");
      option.value = project.id;
      option.textContent = project.name;
      return option;
    }),
  );
  els.newThread.disabled = projects.length === 0;
  els.newThread.title = projects.length === 0
    ? "Create a project before opening a thread"
    : "New thread";
}

els.newThread.addEventListener("click", async () => {
  const projectId = els.project.value;
  if (!projectId) return;
  els.newThread.disabled = true;
  try {
    // `threads.create` requires the contract shape: `projectId`, `origin`,
    // `input` and `environment`. A fresh thread has no prompt, so `input` is
    // empty and the project decides the workspace.
    const body = await api<LoomThread>("/api/v1/threads", {
      method: "POST",
      body: JSON.stringify({
        projectId,
        origin: "app",
        input: [],
        environment: { type: "project-default" },
      }),
    });
    threads = [body, ...threads.filter((thread) => thread.id !== body.id)];
    renderSidebar();
    await openThread(body.id);
  } catch (error) {
    if (current) {
      current.notices = [...current.notices.slice(-2), error instanceof Error ? error.message : String(error)];
      renderSession(current);
    } else {
      setConnection("offline", error instanceof Error ? error.message : String(error));
    }
  } finally {
    els.newThread.disabled = projects.length === 0;
  }
});

els.composer.addEventListener("submit", async (event) => {
  event.preventDefault();
  const content = els.message.value.trim();
  if (!content || !current) return;
  const session = current;
  const submit = els.composer.querySelector("button[type=submit]") as HTMLButtonElement | null;
  if (submit) submit.disabled = true;
  els.message.value = "";
  try {
    await api(`/api/v1/threads/${encodeURIComponent(session.thread.id)}/messages`, {
      method: "POST",
      body: JSON.stringify({ content }),
    });
  } catch (error) {
    session.notices = [...session.notices.slice(-2), error instanceof Error ? error.message : String(error)];
    renderSession(session);
  } finally {
    if (submit) submit.disabled = false;
  }
});

async function main(): Promise<void> {
  try {
    await Promise.all([refreshProjects(), refreshThreads()]);
    setConnection("offline", "idle");
    if (threads[0]) await openThread(threads[0].id);
    else renderHeader(null);
  } catch (error) {
    setConnection("offline", error instanceof Error ? error.message : String(error));
    renderHeader(null);
  }
}

void main();
