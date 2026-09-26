// The relay Durable Object (PLAN §14.6): one instance ("main") holding
//   * the engine link — one outbound WebSocket from the engine (tag "engine"),
//   * public queue viewers (tag "viewer"),
//   * Ko-fi dedupe + a bounded buffer of payments not yet acknowledged by the engine,
//   * the latest public queue snapshot.
// WebSocket Hibernation keeps idle connections free; bare "ping" frames are answered with
// "pong" by the runtime without waking the object.
//
// Engine protocol (JSON text frames, field `t`):
//   engine → relay: hello {v, engine} · queue {snapshot} · ack {ids} · <kind>.res {id, ok, result|error}
//   relay → engine: welcome {v, pending, dropped} · kofi {id, received_at, data} · <kind>.req {id, body} · error {msg}
// Viewer protocol: relay → viewer: queue {online, snapshot, updated_at} · status {online}

import { DurableObject } from "cloudflare:workers";
import type { Env } from "./env";
import type { KofiPayment } from "./kofi";

export const PROTOCOL = 1;
const MAX_FRAME = 256 * 1024;
const MAX_VIEWERS = 2000;
const SEEN_TTL_MS = 30 * 24 * 3600 * 1000;
const DEFAULT_BUFFER = 500;

export type KofiStatus = "delivered" | "queued" | "duplicate";
export type CallResult = { ok: true; result: unknown } | { ok: false; error: string };

/** What viewers and `/queue.json` get: the latest snapshot and whether the engine is online. */
export interface QueueState {
  online: boolean;
  snapshot: unknown;
  updated_at: number | null;
}

interface Attachment {
  kind: "engine" | "viewer";
  at: number;
}

interface PendingCall {
  resolve: (r: CallResult) => void;
  timer: number;
}

export class Relay extends DurableObject<Env> {
  private readonly sql: SqlStorage;
  private readonly calls = new Map<string, PendingCall>();

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec(`
      CREATE TABLE IF NOT EXISTS kofi_seen (message_id TEXT PRIMARY KEY, received_at INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS kofi_pending (message_id TEXT PRIMARY KEY, received_at INTEGER NOT NULL, payload TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    `);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  // ---- HTTP (WebSocket upgrades forwarded by the Worker) ---------------------------------

  override async fetch(req: Request): Promise<Response> {
    const { pathname } = new URL(req.url);
    if (pathname === "/link") return this.acceptEngine();
    if (pathname === "/queue/ws") return this.acceptViewer();
    return new Response("not found", { status: 404 });
  }

  private acceptEngine(): Response {
    const [client, server] = Object.values(new WebSocketPair()) as [WebSocket, WebSocket];
    for (const old of this.ctx.getWebSockets("engine")) {
      try {
        old.close(4001, "replaced by a newer engine connection");
      } catch {
        // already closing
      }
    }
    this.ctx.acceptWebSocket(server, ["engine"]);
    server.serializeAttachment({ kind: "engine", at: Date.now() } satisfies Attachment);
    this.broadcast({ t: "status", online: true });
    return new Response(null, { status: 101, webSocket: client });
  }

  private acceptViewer(): Response {
    if (this.ctx.getWebSockets("viewer").length >= MAX_VIEWERS) return new Response("too many viewers", { status: 503 });
    const [client, server] = Object.values(new WebSocketPair()) as [WebSocket, WebSocket];
    this.ctx.acceptWebSocket(server, ["viewer"]);
    server.serializeAttachment({ kind: "viewer", at: Date.now() } satisfies Attachment);
    server.send(JSON.stringify({ t: "queue", ...this.current(), now: Date.now() }));
    return new Response(null, { status: 101, webSocket: client });
  }

  // ---- RPC (called by the Worker and by extensions such as /mod) --------------------------

  /** Store (dedupe + buffer) a verified Ko-fi payment and deliver it if the engine is online. */
  async kofi(payment: KofiPayment): Promise<KofiStatus> {
    const now = Date.now();
    const id = payment.message_id;
    this.sql.exec("DELETE FROM kofi_seen WHERE received_at < ?", now - SEEN_TTL_MS);
    const fresh = this.sql.exec("INSERT OR IGNORE INTO kofi_seen (message_id, received_at) VALUES (?, ?)", id, now).rowsWritten;
    if (fresh === 0) return "duplicate";
    this.sql.exec("INSERT INTO kofi_pending (message_id, received_at, payload) VALUES (?, ?, ?)", id, now, JSON.stringify(payment));
    const max = Math.max(1, Number.parseInt(this.env.KOFI_BUFFER_MAX ?? "", 10) || DEFAULT_BUFFER);
    const count = this.sql.exec<{ n: number }>("SELECT COUNT(*) AS n FROM kofi_pending").one().n;
    if (count > max) {
      const over = count - max;
      this.sql.exec("DELETE FROM kofi_pending WHERE message_id IN (SELECT message_id FROM kofi_pending ORDER BY received_at, rowid LIMIT ?)", over);
      this.bump("dropped", over);
      console.warn(`kofi buffer full: dropped ${over} oldest payment(s)`);
    }
    const engine = this.engine();
    if (!engine) return "queued";
    engine.send(JSON.stringify({ t: "kofi", id, received_at: now, data: payment }));
    return "delivered";
  }

  /** Latest public snapshot and whether the engine is connected. */
  async snapshot(): Promise<QueueState> {
    return this.current();
  }

  async engineConnected(): Promise<boolean> {
    return this.engine() !== null;
  }

  /**
   * Request/response over the engine link for extensions: sends `{t: kind + ".req", id, body}`
   * and resolves with the engine's `{t: kind + ".res", id, ok, result | error}`.
   */
  async engineCall(kind: string, body: unknown, timeoutMs = 10_000): Promise<CallResult> {
    if (!/^[a-z][a-z0-9_]*$/.test(kind)) return { ok: false, error: "bad call kind" };
    const engine = this.engine();
    if (!engine) return { ok: false, error: "engine offline" };
    const id = crypto.randomUUID();
    const { promise, resolve } = Promise.withResolvers<CallResult>();
    const timer = setTimeout(() => {
      this.calls.delete(id);
      resolve({ ok: false, error: "engine did not answer" });
    }, timeoutMs);
    this.calls.set(id, { resolve, timer });
    engine.send(JSON.stringify({ t: `${kind}.req`, id, body }));
    return promise;
  }

  // ---- WebSocket events ----------------------------------------------------------------

  override async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const att = ws.deserializeAttachment() as Attachment | null;
    if (att?.kind !== "engine") return; // viewers only ping (auto-answered)
    if (typeof message !== "string") return;
    if (message.length > MAX_FRAME) {
      ws.close(1009, "frame too large");
      return;
    }
    let f: Record<string, unknown>;
    try {
      f = JSON.parse(message) as Record<string, unknown>;
    } catch {
      ws.send(JSON.stringify({ t: "error", msg: "frames must be JSON" }));
      return;
    }
    const t = typeof f.t === "string" ? f.t : "";
    if (t === "hello") {
      if (f.v !== PROTOCOL) {
        ws.send(JSON.stringify({ t: "error", msg: `unsupported protocol ${String(f.v)} (relay speaks ${PROTOCOL})` }));
        ws.close(4002, "protocol mismatch");
        return;
      }
      const pending = this.sql.exec<{ n: number }>("SELECT COUNT(*) AS n FROM kofi_pending").one().n;
      ws.send(JSON.stringify({ t: "welcome", v: PROTOCOL, pending, dropped: this.take("dropped") }));
      for (const row of this.sql.exec<{ message_id: string; received_at: number; payload: string }>(
        "SELECT message_id, received_at, payload FROM kofi_pending ORDER BY received_at, rowid",
      )) {
        ws.send(JSON.stringify({ t: "kofi", id: row.message_id, received_at: row.received_at, data: JSON.parse(row.payload) }));
      }
    } else if (t === "queue") {
      if (typeof f.snapshot !== "object" || f.snapshot === null) return;
      const now = Date.now();
      this.sql.exec("INSERT INTO kv (key, value) VALUES ('snapshot', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value", JSON.stringify(f.snapshot));
      this.sql.exec("INSERT INTO kv (key, value) VALUES ('snapshot_at', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value", String(now));
      this.broadcast({ t: "queue", online: true, snapshot: f.snapshot, updated_at: now });
    } else if (t === "ack") {
      const ids = Array.isArray(f.ids) ? f.ids.filter((x): x is string => typeof x === "string") : [];
      for (const id of ids) this.sql.exec("DELETE FROM kofi_pending WHERE message_id = ?", id);
    } else if (t.endsWith(".res") && typeof f.id === "string") {
      const call = this.calls.get(f.id);
      if (!call) return;
      this.calls.delete(f.id);
      clearTimeout(call.timer);
      call.resolve(f.ok === true ? { ok: true, result: f.result ?? null } : { ok: false, error: typeof f.error === "string" ? f.error : "failed" });
    }
  }

  override async webSocketClose(ws: WebSocket, code: number, reason: string): Promise<void> {
    this.closed(ws);
    try {
      ws.close(code === 1005 || code === 1006 ? 1000 : code, reason);
    } catch {
      // already closed
    }
  }

  override async webSocketError(ws: WebSocket): Promise<void> {
    this.closed(ws);
  }

  private closed(ws: WebSocket): void {
    const att = ws.deserializeAttachment() as Attachment | null;
    if (att?.kind !== "engine") return;
    if (this.engine(ws)) return; // a newer engine connection is still up
    for (const [id, call] of this.calls) {
      clearTimeout(call.timer);
      call.resolve({ ok: false, error: "engine disconnected" });
      this.calls.delete(id);
    }
    this.broadcast({ t: "status", online: false });
  }

  // ---- helpers -------------------------------------------------------------------------

  /** The newest open engine socket (ignoring `except`). */
  private engine(except?: WebSocket): WebSocket | null {
    let best: WebSocket | null = null;
    let bestAt = -1;
    for (const ws of this.ctx.getWebSockets("engine")) {
      if (ws === except || ws.readyState !== WebSocket.OPEN) continue;
      const at = (ws.deserializeAttachment() as Attachment | null)?.at ?? 0;
      if (at > bestAt) {
        best = ws;
        bestAt = at;
      }
    }
    return best;
  }

  private current(): QueueState {
    const rows = this.sql.exec<{ key: string; value: string }>("SELECT key, value FROM kv WHERE key IN ('snapshot', 'snapshot_at')").toArray();
    const get = (k: string) => rows.find((r) => r.key === k)?.value;
    const snap = get("snapshot");
    const at = get("snapshot_at");
    return { online: this.engine() !== null, snapshot: snap ? JSON.parse(snap) : null, updated_at: at ? Number(at) : null };
  }

  /** Send to every viewer, stamped with the relay clock (`now`) so pages can correct skew. */
  private broadcast(msg: Record<string, unknown>): void {
    const s = JSON.stringify({ ...msg, now: Date.now() });
    for (const ws of this.ctx.getWebSockets("viewer")) {
      try {
        ws.send(s);
      } catch {
        // closing viewer
      }
    }
  }

  private bump(key: string, by: number): void {
    this.sql.exec("INSERT INTO kv (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + ?", key, String(by), by);
  }

  /** Read and reset a counter. */
  private take(key: string): number {
    const row = this.sql.exec<{ value: string }>("SELECT value FROM kv WHERE key = ?", key).toArray()[0];
    this.sql.exec("DELETE FROM kv WHERE key = ?", key);
    return row ? Number(row.value) : 0;
  }
}
