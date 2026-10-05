// Local queue-only relay. Tunnel the public listener, never the private engine listener.
import { createHash, timingSafeEqual } from "node:crypto";
import { readFile } from "node:fs/promises";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { Duplex } from "node:stream";
import { join } from "node:path";
import { setTimeout, clearTimeout } from "node:timers";
import { WebSocket, WebSocketServer } from "ws";
import { ArtLookup, parseVideo } from "./art";
import { queuePage, queueScript } from "./queue-page";
import { createModHandler, MAX_MOD_BODY, type EngineReply, type ModEnv, type RelayRpc } from "./mod";
import { json, text } from "./util";

const PROTOCOL = 1;
const MAX_FRAME = 256 * 1024;
const MAX_VIEWERS = 2000;
const MAX_BUFFER = 1024 * 1024;
const MAX_MOD_CALLS = 64;
const HOST = "127.0.0.1";
const PUBLIC_PORT = 8787;
const PRIVATE_PORT = 8788;

type RecordValue = Record<string, unknown>;
function record(value: unknown): value is RecordValue {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// Keep the se-songs public_snapshot contract explicit: never forward queue.ui or
// private account/history/pending/quota fields, even if the engine sends extras.
function publicEntry(value: unknown, current: boolean): RecordValue | null {
  if (!record(value)) return null;
  const entry: RecordValue = {};
  for (const key of ["title", "channel", "user", "video"]) {
    if (typeof value[key] !== "string") return null;
    entry[key] = value[key];
  }
  for (const key of current ? ["duration", "position", "at"] : ["duration", "pos"]) {
    if (typeof value[key] !== "number" || !Number.isFinite(value[key])) return null;
    entry[key] = value[key];
  }
  if (current) {
    if (typeof value.playing !== "boolean") return null;
    entry.playing = value.playing;
  }
  return entry;
}

function publicSnapshot(value: unknown): RecordValue | null {
  if (!record(value) || value.v !== PROTOCOL || typeof value.open !== "boolean" ||
      typeof value.paused !== "boolean" || !Array.isArray(value.upcoming) ||
      value.upcoming.length > 50 || typeof value.length !== "number" || !Number.isFinite(value.length)) return null;
  const now = value.now === null ? null : publicEntry(value.now, true);
  if (value.now !== null && now === null) return null;
  const upcoming: RecordValue[] = [];
  for (const valueEntry of value.upcoming) {
    const entry = publicEntry(valueEntry, false);
    if (!entry) return null;
    upcoming.push(entry);
  }
  return {
    v: PROTOCOL,
    theme: value.theme === "modern" ? "modern" : "win31",
    open: value.open,
    paused: value.paused,
    now,
    upcoming,
    length: value.length,
  };
}

// Display names and album art are added after the engine snapshot is validated; the
// engine contract itself stays unchanged.
function decorate(snapshot: RecordValue, art: ArtLookup): RecordValue {
  const entry = (value: unknown): unknown => {
    if (!record(value)) return value;
    const found = art.art(value.video as string);
    if (found) return { ...value, art: found.url, artist: found.artist, song: found.song, album: found.album };
    const parsed = parseVideo(value.title as string);
    return parsed ? { ...value, artist: parsed.artist, song: parsed.song } : value;
  };
  return { ...snapshot, now: entry(snapshot.now), upcoming: (snapshot.upcoming as unknown[]).map(entry) };
}

async function sendResponse(res: ServerResponse, response: Response): Promise<void> {
  const body = await response.text();
  const headers: Record<string, string | string[]> = Object.fromEntries(response.headers);
  const cookies = response.headers.getSetCookie();
  if (cookies.length) headers["set-cookie"] = cookies;
  res.writeHead(response.status, headers);
  res.end(body);
}

function pathname(req: IncomingMessage): string {
  try {
    return new URL(req.url ?? "/", `http://${HOST}`).pathname;
  } catch {
    return "";
  }
}

/** The tunnel must retain Host; never trust client-supplied forwarded host/origin. */
async function modRequest(req: IncomingMessage, publicOrigin: URL | null): Promise<Request | Response> {
  const host = req.headers.host?.toLowerCase();
  let origin: string;
  if (publicOrigin) {
    if (host !== publicOrigin.host) return text("untrusted host", 403, { "cache-control": "no-store" });
    const proto = req.headers["x-forwarded-proto"];
    if (proto !== undefined && proto !== publicOrigin.protocol.slice(0, -1)) {
      return text("untrusted forwarded protocol", 403, { "cache-control": "no-store" });
    }
    origin = publicOrigin.origin;
  } else {
    if (host !== `${HOST}:${PUBLIC_PORT}` && host !== `localhost:${PUBLIC_PORT}`) {
      return text("QUEUE_PUBLIC_ORIGIN is required for moderator access through a tunnel", 503, { "cache-control": "no-store" });
    }
    if (req.headers["x-forwarded-proto"] !== undefined && req.headers["x-forwarded-proto"] !== "http") {
      return text("untrusted forwarded protocol", 403, { "cache-control": "no-store" });
    }
    origin = `http://${host}`;
  }
  const target = req.url ?? "/";
  if (!target.startsWith("/") || target.startsWith("//")) return text("bad request target", 400, { "cache-control": "no-store" });
  const headers = new Headers();
  for (const [key, value] of Object.entries(req.headers)) {
    if (Array.isArray(value)) for (const item of value) headers.append(key, item);
    else if (value !== undefined) headers.set(key, value);
  }
  const method = req.method ?? "GET";
  let body: string | undefined;
  if (method !== "GET" && method !== "HEAD") {
    const declared = Number(req.headers["content-length"] ?? "0");
    if (!Number.isFinite(declared) || declared < 0 || declared > MAX_MOD_BODY) {
      req.resume();
      return text("request body too large", 413, { "cache-control": "no-store" });
    }
    const chunks: Buffer[] = [];
    let size = 0;
    for await (const chunk of req.iterator({ destroyOnReturn: false })) {
      const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
      size += bytes.byteLength;
      if (size > MAX_MOD_BODY) {
        req.resume();
        return text("request body too large", 413, { "cache-control": "no-store" });
      }
      chunks.push(bytes);
    }
    body = Buffer.concat(chunks, size).toString("utf8");
  }
  return new Request(`${origin}${target}`, { method, headers, body });
}

function rejectUpgrade(socket: Duplex, status: number, reason: string): void {
  socket.end(`HTTP/1.1 ${status} ${reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`);
}

function send(ws: WebSocket, frame: string): void {
  if (ws.readyState !== WebSocket.OPEN) return;
  if (ws.bufferedAmount + Buffer.byteLength(frame) > MAX_BUFFER) {
    ws.terminate();
    return;
  }
  ws.send(frame, (error) => { if (error) ws.terminate(); });
}

function listen(server: Server, port: number): Promise<void> {
  const { promise, resolve, reject } = Promise.withResolvers<void>();
  server.once("error", reject);
  server.listen(port, HOST, () => {
    server.off("error", reject);
    resolve();
  });
  return promise;
}

async function main(): Promise<void> {
  let secret: string;
  try {
    secret = process.env.CREDENTIALS_DIRECTORY
      ? (await readFile(join(process.env.CREDENTIALS_DIRECTORY, "RELAY_SECRET"), "utf8")).trim()
      : (process.env.RELAY_SECRET ?? "").trim();
  } catch {
    throw new Error("cannot read RELAY_SECRET credential");
  }
  if (secret.length < 16) throw new Error("RELAY_SECRET must contain at least 16 characters");
  const modEnv: ModEnv = {
    RELAY_SECRET: secret,
    MOD_QUEUE_ONLY: "1",
    TWITCH_CLIENT_ID: process.env.TWITCH_CLIENT_ID,
    MOD_SESSION_HOURS: process.env.MOD_SESSION_HOURS,
  };
  const expected = createHash("sha256").update(secret).digest();
  secret = "";
  const publicOrigin = process.env.QUEUE_PUBLIC_ORIGIN ? new URL(process.env.QUEUE_PUBLIC_ORIGIN) : null;
  if (publicOrigin && (publicOrigin.protocol !== "https:" || publicOrigin.username || publicOrigin.password ||
      publicOrigin.pathname !== "/" || publicOrigin.search || publicOrigin.hash)) {
    throw new Error("QUEUE_PUBLIC_ORIGIN must be an HTTPS origin without a path, credentials, query or fragment");
  }
  const authenticated = (req: IncomingMessage): boolean => {
    const token = /^Bearer\s+(\S+)\s*$/i.exec(req.headers.authorization ?? "")?.[1] ?? "";
    return timingSafeEqual(expected, createHash("sha256").update(token).digest());
  };

  const viewers = new WebSocketServer({ noServer: true, maxPayload: MAX_FRAME, perMessageDeflate: false });
  const engines = new WebSocketServer({ noServer: true, maxPayload: MAX_FRAME, perMessageDeflate: false });
  let engine: WebSocket | null = null;
  let snapshot: RecordValue | null = null;
  let engineSnapshot: RecordValue | null = null;
  let updatedAt: number | null = null;
  let stopping = false;
  const calls = new Map<string, {
    engine: WebSocket;
    resolve(reply: EngineReply): void;
    timer: NodeJS.Timeout;
  }>();
  const finishCall = (id: string, reply: EngineReply) => {
    const call = calls.get(id);
    if (!call) return;
    calls.delete(id);
    clearTimeout(call.timer);
    call.resolve(reply);
  };
  const failCalls = (ws: WebSocket | null, error: string) => {
    for (const [id, call] of calls) if (!ws || call.engine === ws) finishCall(id, { ok: false, error });
  };
  const rpc: RelayRpc = {
    async engineConnected() { return !stopping && engine?.readyState === WebSocket.OPEN; },
    async engineCall(kind, body, timeoutMs = 8000) {
      if (stopping) return { ok: false, error: "service shutting down" };
      if (kind !== "mod") return { ok: false, error: "bad call kind" };
      const ws = engine;
      if (!ws || ws.readyState !== WebSocket.OPEN) return { ok: false, error: "engine offline" };
      if (calls.size >= MAX_MOD_CALLS) return { ok: false, error: "engine busy" };
      const id = crypto.randomUUID();
      const frame = JSON.stringify({ t: "mod.req", id, body });
      if (Buffer.byteLength(frame) > MAX_FRAME || ws.bufferedAmount + Buffer.byteLength(frame) > MAX_BUFFER) {
        return { ok: false, error: "engine busy" };
      }
      const { promise, resolve } = Promise.withResolvers<EngineReply>();
      const timeout = Number.isFinite(timeoutMs) ? Math.min(Math.max(timeoutMs, 1), 30_000) : 8000;
      const timer = setTimeout(() => finishCall(id, { ok: false, error: "engine did not answer" }), timeout);
      timer.unref();
      calls.set(id, { engine: ws, resolve, timer });
      try {
        ws.send(frame, (error) => { if (error) finishCall(id, { ok: false, error: "engine offline" }); });
      } catch {
        finishCall(id, { ok: false, error: "engine offline" });
      }
      return promise;
    },
  };
  const handleMod = createModHandler({ relay: () => rpc, fetch: (input, init) => fetch(input, init), now: () => Date.now() });
  const state = () => ({ online: engine?.readyState === WebSocket.OPEN, snapshot, updated_at: updatedAt });
  const broadcast = (message: RecordValue) => {
    const frame = JSON.stringify({ ...message, now: Date.now() });
    for (const viewer of viewers.clients) send(viewer, frame);
  };
  const art = new ArtLookup(() => {
    if (!engineSnapshot) return;
    snapshot = decorate(engineSnapshot, art);
    broadcast({ t: "queue", ...state() });
  });
  const publicServer = createServer((req, res) => {
    const path = pathname(req);
    if (path === "/mod" || path.startsWith("/mod/")) {
      void (async () => {
        if (stopping) return text("service shutting down", 503, { "cache-control": "no-store" });
        const request = await modRequest(req, publicOrigin);
        return request instanceof Response ? request : handleMod(request, modEnv);
      })().then((response) => sendResponse(res, response)).catch(() => {
        if (res.headersSent) res.destroy();
        else void sendResponse(res, text("bad request", 400, { "cache-control": "no-store" })).catch(() => res.destroy());
      });
      return;
    }
    let response: Response;
    if (!["/", "/queue", "/queue.js", "/queue.json", "/queue/ws"].includes(path)) {
      response = text("not found", 404);
    } else if (req.method !== "GET") {
      response = text("method not allowed", 405, { allow: "GET" });
    } else if (path === "/") {
      response = new Response(null, { status: 302, headers: { location: "/queue" } });
    } else if (path === "/queue") {
      response = queuePage({ QUEUE_TITLE: process.env.QUEUE_TITLE, QUEUE_CHANNEL: process.env.QUEUE_CHANNEL });
    } else if (path === "/queue.js") {
      response = queueScript();
    } else if (path === "/queue.json") {
      response = json(state(), 200, { "access-control-allow-origin": "*" });
    } else {
      response = text("WebSocket upgrade required", 426, { upgrade: "websocket" });
    }
    void sendResponse(res, response).catch(() => res.destroy());
  });
  const privateServer = createServer((req, res) => {
    const response = pathname(req) !== "/link" ? text("not found", 404)
      : req.method !== "GET" ? text("method not allowed", 405, { allow: "GET" })
      : !authenticated(req) ? text("unauthorized", 401)
      : text("WebSocket upgrade required", 426, { upgrade: "websocket" });
    void sendResponse(res, response).catch(() => res.destroy());
  });
  const sockets = new Set<Duplex>();
  for (const server of [publicServer, privateServer]) {
    server.on("connection", (socket) => {
      sockets.add(socket);
      socket.on("error", () => socket.destroy());
      socket.on("close", () => sockets.delete(socket));
    });
  }


  publicServer.on("upgrade", (req, socket, head) => {
    if (stopping) return rejectUpgrade(socket, 503, "Service Unavailable");
    if (pathname(req) !== "/queue/ws") return rejectUpgrade(socket, 404, "Not Found");
    if (req.method !== "GET") return rejectUpgrade(socket, 405, "Method Not Allowed");
    if (viewers.clients.size >= MAX_VIEWERS) return rejectUpgrade(socket, 503, "Service Unavailable");
    viewers.handleUpgrade(req, socket, head, (ws) => viewers.emit("connection", ws));
  });
  privateServer.on("upgrade", (req, socket, head) => {
    if (stopping) return rejectUpgrade(socket, 503, "Service Unavailable");
    if (pathname(req) !== "/link") return rejectUpgrade(socket, 404, "Not Found");
    if (req.method !== "GET") return rejectUpgrade(socket, 405, "Method Not Allowed");
    if (!authenticated(req)) return rejectUpgrade(socket, 401, "Unauthorized");
    engines.handleUpgrade(req, socket, head, (ws) => engines.emit("connection", ws));
  });

  viewers.on("connection", (ws: WebSocket) => {
    ws.on("error", () => { if (ws.readyState === WebSocket.OPEN) ws.terminate(); });
    ws.on("message", (data, binary) => {
      if (!binary && data.toString() === "ping") send(ws, "pong");
      else ws.close(1008, "viewers are read-only");
    });
    send(ws, JSON.stringify({ t: "queue", ...state(), now: Date.now() }));
  });
  engines.on("connection", (ws: WebSocket) => {
    let welcomed = false;
    const helloDeadline = setTimeout(() => ws.close(1008, "hello required"), 10_000);
    helloDeadline.unref();
    ws.on("error", () => { if (ws.readyState === WebSocket.OPEN) ws.terminate(); });
    ws.on("close", () => {
      clearTimeout(helloDeadline);
      if (engine !== ws) return;
      engine = null;
      failCalls(ws, "engine offline");
      broadcast({ t: "status", online: false });
    });
    ws.on("message", (data, binary) => {
      if (ws.readyState !== WebSocket.OPEN) return;
      if (binary) return ws.close(1003, "text frames required");
      const message = data.toString();
      if (message === "ping") return send(ws, "pong");
      let frame: unknown;
      try {
        frame = JSON.parse(message);
      } catch {
        return send(ws, JSON.stringify({ t: "error", msg: "frames must be JSON" }));
      }
      if (!record(frame)) return;
      if (frame.t === "hello") {
        if (frame.v !== PROTOCOL) {
          send(ws, JSON.stringify({ t: "error", msg: `unsupported protocol (relay speaks ${PROTOCOL})` }));
          ws.close(4002, "protocol mismatch");
          return;
        }
        if (welcomed) return;
        welcomed = true;
        clearTimeout(helloDeadline);
        const previous = engine;
        engine = ws;
        if (previous && previous !== ws) previous.close(4001, "replaced by a newer engine connection");
        if (previous && previous !== ws) failCalls(previous, "engine offline");
        // Queue-only service: there is no local payment ingress or payment buffer.
        send(ws, JSON.stringify({ t: "welcome", v: PROTOCOL, pending: 0, dropped: 0 }));
        broadcast({ t: "status", online: true });
      } else if (frame.t === "mod.res" && welcomed && engine === ws && typeof frame.id === "string") {
        const call = calls.get(frame.id);
        if (!call || call.engine !== ws) return;
        if (frame.ok === true) finishCall(frame.id, { ok: true, result: frame.result });
        else if (frame.ok === false && typeof frame.error === "string") finishCall(frame.id, { ok: false, error: frame.error });
      } else if (frame.t === "queue" && welcomed && engine === ws) {
        const next = publicSnapshot(frame.snapshot);
        if (!next) return send(ws, JSON.stringify({ t: "error", msg: "invalid public snapshot" }));
        engineSnapshot = next;
        snapshot = decorate(next, art);
        updatedAt = Date.now();
        art.want([next.now, ...(next.upcoming as unknown[])].filter(record).map((e) => ({
          video: e.video as string, title: e.title as string, channel: e.channel as string,
        })));
        broadcast({ t: "queue", ...state() });
      }
      // Payment acknowledgments have no consumer in this queue-only service.
    });
  });

  let shutdownPromise: Promise<void> | null = null;
  const shutdown = (): Promise<void> => {
    if (shutdownPromise) return shutdownPromise;
    stopping = true;
    failCalls(null, "service shutting down");
    shutdownPromise = (async () => {
      const timer = setTimeout(() => {
        for (const ws of [...viewers.clients, ...engines.clients]) ws.terminate();
        for (const socket of sockets) socket.destroy();
      }, 1500);
      for (const ws of [...viewers.clients, ...engines.clients]) ws.close(1001, "service shutting down");
      const closed = [publicServer, privateServer, viewers, engines].map((server) => {
        const { promise, resolve } = Promise.withResolvers<void>();
        server.close(() => resolve());
        return promise;
      });
      await Promise.all(closed);
      clearTimeout(timer);
    })();
    return shutdownPromise;
  };
  try {
    await listen(privateServer, PRIVATE_PORT);
    await listen(publicServer, PUBLIC_PORT);
  } catch (error) {
    await shutdown();
    throw error;
  }
  const failed = (error: NodeJS.ErrnoException) => {
    console.error(`local queue listener failed: ${error.code ?? "unknown error"}`);
    process.exitCode = 1;
    void shutdown();
  };
  publicServer.on("error", failed);
  privateServer.on("error", failed);
  for (const signal of ["SIGTERM", "SIGINT"] as const) process.on(signal, () => { void shutdown(); });
  console.log(`local queue ready: public http://${HOST}:${PUBLIC_PORT}/queue; private engine ws://${HOST}:${PRIVATE_PORT}/link`);
}

await main().catch((error: unknown) => {
  const code = record(error) && typeof error.code === "string" ? error.code : null;
  console.error(`local queue startup failed: ${code ?? (error instanceof Error ? error.message : "unknown error")}`);
  process.exitCode = 1;
});
