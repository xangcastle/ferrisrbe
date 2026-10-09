import type { LiveEvent, LiveTopic } from "../types";
import { keycloak } from "../auth/keycloak";

type Handler = (event: LiveEvent) => void;
type StatusHandler = (status: LiveStatus) => void;
export type LiveStatus = "connecting" | "live" | "reconnecting";

const MAX_BACKOFF_MS = 15_000;

function topicKey(topic: LiveTopic) {
  return typeof topic === "string" ? topic : `build:${topic.build.id}`;
}

/**
 * One WebSocket per tab, multiplexing topic subscriptions. On reconnect it
 * resubscribes and emits a `resync` so hooks refetch their REST snapshot.
 */
class LiveClient {
  private socket: WebSocket | null = null;
  private connecting = false;
  private handlers = new Map<string, { topic: LiveTopic; fns: Set<Handler> }>();
  private statusHandlers = new Set<StatusHandler>();
  private backoff = 500;
  private timer: ReturnType<typeof setTimeout> | null = null;
  status: LiveStatus = "connecting";

  private async url() {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    try {
      await keycloak?.updateToken(30);
    } catch {
      /* token beyond refresh: connect anyway, the server rejects and we retry */
    }
    const token = keycloak?.token
      ? `?token=${encodeURIComponent(keycloak.token)}`
      : "";
    return `${proto}//${location.host}/api/ws${token}`;
  }

  private setStatus(status: LiveStatus) {
    this.status = status;
    this.statusHandlers.forEach((fn) => fn(status));
  }

  private scheduleReconnect() {
    this.socket = null;
    this.connecting = false;
    if (this.handlers.size === 0) return;
    this.setStatus("reconnecting");
    this.timer = setTimeout(() => this.connect(), this.backoff);
    this.backoff = Math.min(this.backoff * 2, MAX_BACKOFF_MS);
  }

  private connect() {
    if (this.socket || this.connecting || this.handlers.size === 0) return;
    this.connecting = true;
    this.url()
      .then((url) => {
        const socket = new WebSocket(url);
        this.socket = socket;
        socket.onopen = () => {
          this.connecting = false;
          const wasReconnect = this.backoff > 500;
          this.backoff = 500;
          this.setStatus("live");
          this.handlers.forEach(({ topic }) => this.send({ subscribe: topic }));
          if (wasReconnect) this.dispatch({ type: "resync" });
        };
        socket.onmessage = (msg) => {
          try {
            this.dispatch(JSON.parse(msg.data as string) as LiveEvent);
          } catch {
            /* ignore malformed frames */
          }
        };
        socket.onclose = () => this.scheduleReconnect();
        socket.onerror = () => socket.close();
      })
      .catch(() => this.scheduleReconnect());
  }

  private send(payload: unknown) {
    if (this.socket?.readyState === WebSocket.OPEN) {
      this.socket.send(JSON.stringify(payload));
    }
  }

  private dispatch(event: LiveEvent) {
    this.handlers.forEach(({ fns }) => fns.forEach((fn) => fn(event)));
  }

  subscribe(topic: LiveTopic, handler: Handler): () => void {
    const key = topicKey(topic);
    let entry = this.handlers.get(key);
    if (!entry) {
      entry = { topic, fns: new Set() };
      this.handlers.set(key, entry);
      this.send({ subscribe: topic });
    }
    entry.fns.add(handler);
    this.connect();
    return () => {
      const current = this.handlers.get(key);
      if (!current) return;
      current.fns.delete(handler);
      if (current.fns.size === 0) {
        this.handlers.delete(key);
        this.send({ unsubscribe: topic });
        if (this.handlers.size === 0 && this.socket) {
          if (this.timer) clearTimeout(this.timer);
          this.socket.close();
          this.socket = null;
        }
      }
    };
  }

  onStatus(handler: StatusHandler): () => void {
    this.statusHandlers.add(handler);
    handler(this.status);
    return () => {
      this.statusHandlers.delete(handler);
    };
  }
}

export const live = new LiveClient();
