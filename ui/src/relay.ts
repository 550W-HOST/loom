import type { RelayEventFrame, ReplayPage } from "./types.js";
import { isRelayEventFrame } from "./types.js";

export function scopeKey(threadId: string): string {
  return `thread:${threadId}`;
}

export function sortRelayFrames(frames: readonly RelayEventFrame[]): RelayEventFrame[] {
  return [...frames].sort((left, right) =>
    left.event_id < right.event_id ? -1 : left.event_id > right.event_id ? 1 : 0,
  );
}

export function mergeRelayFrames(frames: readonly RelayEventFrame[]): RelayEventFrame[] {
  const byId = new Map<string, RelayEventFrame>();
  for (const frame of frames) {
    if (!byId.has(frame.event_id)) {
      byId.set(frame.event_id, frame);
    }
  }
  return sortRelayFrames([...byId.values()]);
}

export type ReplayPageLoader = (args: {
  threadId: string;
  since: string | null;
}) => Promise<ReplayPage>;

export async function replayAllPages(
  loadPage: ReplayPageLoader,
  threadId: string,
  cursor: string | null,
): Promise<RelayEventFrame[]> {
  const frames: RelayEventFrame[] = [];
  let since = cursor;
  while (true) {
    const page = await loadPage({ threadId, since });
    const pageFrames = page.frames.filter(isRelayEventFrame);
    frames.push(...pageFrames);
    if (!page.has_more || pageFrames.length === 0) {
      return mergeRelayFrames(frames);
    }
    since = pageFrames[pageFrames.length - 1]!.event_id;
  }
}

export interface CursorStore {
  get(key: string): string | null;
  set(key: string, value: string): void;
}

export function createBrowserCursorStore(): CursorStore {
  return {
    get(key) {
      try {
        return window.localStorage.getItem(key);
      } catch {
        return null;
      }
    },
    set(key, value) {
      try {
        window.localStorage.setItem(key, value);
      } catch {
        // A private browsing context may reject persistent storage.
      }
    },
  };
}

function isSubscribedForThread(value: unknown, threadId: string): boolean {
  if (
    typeof value !== "object" ||
    value === null ||
    Array.isArray(value) ||
    (value as { type?: unknown }).type !== "subscribed"
  ) {
    return false;
  }
  const scope = (value as { scope?: unknown }).scope;
  return (
    typeof scope === "object" &&
    scope !== null &&
    !Array.isArray(scope) &&
    (scope as { kind?: unknown }).kind === "thread" &&
    (scope as { id?: unknown }).id === threadId
  );
}

export interface WebSocketLike {
  onopen: (() => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
  send(data: string): void;
  close(): void;
}

export type ConnectionState = "connecting" | "online" | "offline";

export interface ThreadRelaySubscriptionOptions {
  threadId: string;
  wsUrl: string;
  loadReplay: ReplayPageLoader;
  cursorStore: CursorStore;
  createSocket?: (url: string) => WebSocketLike;
  onFrames: (frames: RelayEventFrame[]) => void;
  onNotice: (message: string) => void;
  onState: (state: ConnectionState) => void;
}

class RelayFrameStore {
  private readonly byId = new Map<string, RelayEventFrame>();

  has(eventId: string): boolean {
    return this.byId.has(eventId);
  }

  merge(frames: readonly RelayEventFrame[]): boolean {
    let changed = false;
    for (const frame of frames) {
      if (!this.byId.has(frame.event_id)) {
        this.byId.set(frame.event_id, frame);
        changed = true;
      }
    }
    return changed;
  }

  all(): RelayEventFrame[] {
    return sortRelayFrames([...this.byId.values()]);
  }

  hasFrames(): boolean {
    return this.byId.size > 0;
  }
}

export class ThreadRelaySubscription {
  private readonly options: ThreadRelaySubscriptionOptions;
  private readonly store = new RelayFrameStore();
  private socket: WebSocketLike | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private reconnectAttempts = 0;
  private generation = 0;
  private stopped = true;

  constructor(options: ThreadRelaySubscriptionOptions) {
    this.options = options;
  }

  start(): void {
    this.stopped = false;
    this.connect();
  }

  stop(): void {
    this.stopped = true;
    this.generation += 1;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.socket) {
      this.socket.onopen = null;
      this.socket.onmessage = null;
      this.socket.onclose = null;
      this.socket.onerror = null;
      this.socket.close();
      this.socket = null;
    }
  }

  private connect(): void {
    if (this.stopped) return;
    const generation = ++this.generation;
    const socket = (this.options.createSocket ?? ((url) => new WebSocket(url) as unknown as WebSocketLike))(
      this.options.wsUrl,
    );
    this.socket = socket;
    this.options.onState("connecting");
    let buffering: RelayEventFrame[] | null = null;
    const bufferedIds = new Set<string>();
    let replayStarted = false;
    const isCurrent = (): boolean =>
      !this.stopped && this.generation === generation && this.socket === socket;
    const startReplay = (): void => {
      if (replayStarted || !isCurrent()) return;
      replayStarted = true;
      void this.replayAfterSubscribe({
        socket,
        isCurrent,
        getBuffer: () => buffering,
        clearBuffer: () => {
          buffering = null;
        },
      });
    };

    socket.onopen = () => {
      if (!isCurrent()) return;
      this.reconnectAttempts = 0;
      buffering = [];
      socket.send(
        JSON.stringify({
          type: "subscribe",
          scope: { kind: "thread", id: this.options.threadId },
        }),
      );
      this.options.onState("online");
    };

    socket.onmessage = (message) => {
      if (!isCurrent()) return;
      let value: unknown;
      try {
        value = typeof message.data === "string" ? JSON.parse(message.data) : message.data;
      } catch {
        return;
      }
      if (isSubscribedForThread(value, this.options.threadId)) {
        startReplay();
        return;
      }
      if (!isRelayEventFrame(value)) return;
      if (value.scope.kind !== "thread" || value.scope.id !== this.options.threadId) return;
      if (this.store.has(value.event_id) || bufferedIds.has(value.event_id)) return;
      if (buffering) {
        buffering.push(value);
        bufferedIds.add(value.event_id);
        return;
      }
      this.publishFrames([value]);
    };

    socket.onclose = () => {
      if (!isCurrent()) return;
      this.socket = null;
      this.options.onState("offline");
      const delay = Math.min(1_000 * 2 ** this.reconnectAttempts, 10_000);
      this.reconnectAttempts += 1;
      this.reconnectTimer = setTimeout(() => {
        this.reconnectTimer = null;
        this.connect();
      }, delay);
    };

    socket.onerror = () => socket.close();
  }

  private async replayAfterSubscribe(args: {
    socket: WebSocketLike;
    isCurrent: () => boolean;
    getBuffer: () => RelayEventFrame[] | null;
    clearBuffer: () => void;
  }): Promise<void> {
    try {
      // A fresh page has no in-memory timeline to merge into. Replaying from
      // its persisted cursor would render only future events and leave the
      // existing thread blank; the cursor is for reconnecting this instance.
      const cursor = this.store.hasFrames()
        ? this.options.cursorStore.get(scopeKey(this.options.threadId))
        : null;
      const replayed = await replayAllPages(this.options.loadReplay, this.options.threadId, cursor);
      if (!args.isCurrent()) return;
      const live = args.getBuffer() ?? [];
      args.clearBuffer();
      this.publishFrames([...replayed, ...live]);
    } catch (error) {
      if (!args.isCurrent()) return;
      const live = args.getBuffer() ?? [];
      args.clearBuffer();
      this.publishFrames(live);
      this.options.onNotice(`Replay failed: ${error instanceof Error ? error.message : String(error)}`);
      args.socket.close();
    }
  }

  private publishFrames(frames: readonly RelayEventFrame[]): void {
    if (!this.store.merge(frames)) return;
    const ordered = this.store.all();
    const last = ordered[ordered.length - 1];
    if (last) {
      this.options.cursorStore.set(scopeKey(this.options.threadId), last.event_id);
    }
    this.options.onFrames(ordered);
  }
}
