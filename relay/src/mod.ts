// Remote mod access (PLAN §12.3, §14.6, §19).
//
// Moderators open https://<domain>/mod, sign in with Twitch (implicit grant: the token comes
// back in the URL fragment, the page posts it here once, and we never store it), and get a
// console with a restricted command set. Verification:
//   1. token validated at id.twitch.tv/oauth2/validate and issued to OUR client id,
//   2. the user is the broadcaster, or the broadcaster's channel appears in the user's
//      moderated channels (Helix GET /moderation/channels, scope user:read:moderated_channels),
//   3. the engine agrees (`authorize`: [remote_mod] enabled, allow/deny lists).
// The session is a stateless HMAC-signed cookie (key derived from RELAY_SECRET). Every
// console request is forwarded over the engine link as `mod.req`; the engine enforces the
// command set again (Scope::Mod + [remote_mod] actions) and runs commands as the moderator.

import { type Env, relayStub } from "./env";

export interface ModEnv extends Omit<Env, "RELAY"> {
  /** Only the Worker default adapter needs a Durable Object binding. */
  RELAY?: Env["RELAY"];
  /** Local queue deployment: never expose the general moderator console. */
  MOD_QUEUE_ONLY?: string;
  /** Twitch identity/API bases; overridable for local tests with a fake OAuth provider. */
  TWITCH_ID_BASE?: string;
  TWITCH_API_BASE?: string;
  /** Fallback when the engine doesn't report one (normally the engine's [twitch] client_id). */
  TWITCH_CLIENT_ID?: string;
  /** Console session lifetime in hours (default 12). */
  MOD_SESSION_HOURS?: string;
}

export type EngineReply = { ok: true; result: unknown } | { ok: false; error: string };

/** What this module needs from the relay Durable Object (see relay.ts). */
export interface RelayRpc {
  engineCall(kind: string, body: unknown, timeoutMs?: number): Promise<EngineReply>;
  engineConnected(): Promise<boolean>;
}

export interface ModDeps {
  relay(env: ModEnv): RelayRpc;
  fetch: typeof fetch;
  now(): number;
}

const defaultDeps: ModDeps = {
  relay: (env) => {
    if (!env.RELAY) throw new Error("relay binding is not configured");
    return relayStub({ ...env, RELAY: env.RELAY });
  },
  fetch: (input, init) => fetch(input, init),
  now: () => Date.now(),
};

export const SESSION_COOKIE = "se_mod";
export const STATE_COOKIE = "se_mod_state";
const SCOPE = "user:read:moderated_channels";
export const MAX_MOD_BODY = 16 * 1024;
const ENGINE_TIMEOUT_MS = 8000;
const QUEUE_ACTIONS: Record<string, true> = {
  "queue.request": true, "queue.reorder": true, "queue.remove": true, "queue.approve": true, "queue.reject": true,
};

function queueOnly(env: ModEnv): boolean {
  return env.MOD_QUEUE_ONLY === "1";
}

interface Hello {
  enabled: boolean;
  client_id: string;
  broadcaster: { id: string; login: string };
  actions: string[];
}

export interface Session {
  uid: string;
  login: string;
  b: boolean;
  exp: number;
}

// ------------------------------------------------------------------------------------------
// crypto helpers

const enc = new TextEncoder();

function b64url(bytes: ArrayBuffer | Uint8Array): string {
  const u = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  let s = "";
  for (const b of u) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function unb64url(s: string): Uint8Array | null {
  if (!/^[A-Za-z0-9_-]*$/.test(s)) return null;
  const pad = s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4);
  try {
    const bin = atob(pad);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  } catch {
    return null;
  }
}

async function sessionKey(secret: string | undefined): Promise<CryptoKey> {
  if (!secret || secret.length < 16) throw new Error("RELAY_SECRET is not set (or shorter than 16 characters)");
  const master = await crypto.subtle.importKey("raw", enc.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const derived = await crypto.subtle.sign("HMAC", master, enc.encode("stream-engine mod session v1"));
  return crypto.subtle.importKey("raw", derived, { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]);
}

export async function signSession(secret: string | undefined, s: Session): Promise<string> {
  const payload = b64url(enc.encode(JSON.stringify(s)));
  const sig = await crypto.subtle.sign("HMAC", await sessionKey(secret), enc.encode(payload));
  return `${payload}.${b64url(sig)}`;
}

export async function verifySession(secret: string | undefined, token: string, nowMs: number): Promise<Session | null> {
  const parts = token.split(".");
  const payload = parts[0];
  const sig = parts[1];
  if (!payload || !sig || parts.length !== 2) return null;
  const sigBytes = unb64url(sig);
  if (!sigBytes) return null;
  let ok = false;
  try {
    ok = await crypto.subtle.verify("HMAC", await sessionKey(secret), sigBytes, enc.encode(payload));
  } catch {
    return null;
  }
  if (!ok) return null;
  const raw = unb64url(payload);
  if (!raw) return null;
  try {
    const s = JSON.parse(new TextDecoder().decode(raw)) as Session;
    if (typeof s.uid !== "string" || !s.uid || typeof s.login !== "string" || !s.login ||
        typeof s.b !== "boolean" || typeof s.exp !== "number" || !Number.isFinite(s.exp)) return null;
    return s.exp * 1000 > nowMs ? s : null;
  } catch {
    return null;
  }
}

function randomHex(n: number): string {
  const b = new Uint8Array(n);
  crypto.getRandomValues(b);
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
}

function safeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let r = 0;
  for (let i = 0; i < a.length; i++) r |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return r === 0;
}

export function readCookie(req: Request, name: string): string | null {
  const h = req.headers.get("Cookie");
  if (!h) return null;
  for (const part of h.split(";")) {
    const i = part.indexOf("=");
    if (i < 0) continue;
    if (part.slice(0, i).trim() === name) return part.slice(i + 1).trim();
  }
  return null;
}

function cookie(name: string, value: string, maxAge: number, sameSite: "Strict" | "Lax"): string {
  return `${name}=${value}; Path=/mod; HttpOnly; Secure; SameSite=${sameSite}; Max-Age=${maxAge}`;
}

// ------------------------------------------------------------------------------------------
// command patterns (same semantics as the engine's address globs)

function globSeg(p: string, s: string): boolean {
  if (!p.includes("*")) return p === s;
  const re = new RegExp("^" + p.split("*").map((x) => x.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join(".*") + "$");
  return re.test(s);
}

export function matches(pattern: string, addr: string): boolean {
  const p = pattern.split(".");
  const a = addr.split(".");
  const go = (i: number, j: number): boolean => {
    if (i === p.length) return j === a.length;
    if (p[i] === "**") {
      for (let k = j; k <= a.length; k++) if (go(i + 1, k)) return true;
      return false;
    }
    const ps = p[i];
    const as = a[j];
    return ps !== undefined && as !== undefined && globSeg(ps, as) && go(i + 1, j + 1);
  };
  return go(0, 0);
}

// ------------------------------------------------------------------------------------------
// responses

const SECURITY_HEADERS: Record<string, string> = {
  "Cache-Control": "no-store",
  "Referrer-Policy": "no-referrer",
  "X-Content-Type-Options": "nosniff",
  "X-Frame-Options": "DENY",
};

const CSP = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

function html(body: string, status = 200, extra: Record<string, string> = {}): Response {
  return new Response(body, { status, headers: { "Content-Type": "text/html; charset=utf-8", "Content-Security-Policy": CSP, ...SECURITY_HEADERS, ...extra } });
}

function json(v: unknown, status = 200, extra: HeadersInit = {}): Response {
  const h = new Headers({ "Content-Type": "application/json", ...SECURITY_HEADERS });
  new Headers(extra).forEach((val, k) => h.append(k, val));
  return new Response(JSON.stringify(v), { status, headers: h });
}

function esc(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] ?? c);
}

function page(title: string, main: string, script?: string): string {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>${esc(title)}</title><link rel="stylesheet" href="/mod/style.css"></head>
<body><main>${main}</main>${script ? `<script src="${script}"></script>` : ""}</body></html>`;
}

function messagePage(title: string, msg: string, status: number): Response {
  return html(page(title, `<h1>${esc(title)}</h1><p class="msg">${esc(msg)}</p><p><a class="btn" href="/mod">Back</a></p>`), status);
}

// ------------------------------------------------------------------------------------------
// Twitch

async function engineHello(env: ModEnv, deps: ModDeps): Promise<Hello | string> {
  let r: EngineReply;
  try {
    r = await deps.relay(env).engineCall("mod", { kind: "hello" }, ENGINE_TIMEOUT_MS);
  } catch (e) {
    return `engine unreachable: ${(e as Error).message}`;
  }
  if (!r.ok) return r.error === "engine offline" ? "The stream engine is offline. Try again when it is running." : r.error;
  const h = r.result as Hello;
  return { ...h, client_id: h.client_id || env.TWITCH_CLIENT_ID || "" };
}

interface Validated {
  client_id: string;
  login: string;
  user_id: string;
  scopes: string[];
}

async function validateToken(env: ModEnv, deps: ModDeps, token: string): Promise<Validated | null> {
  const r = await deps.fetch(`${idBase(env)}/oauth2/validate`, { headers: { Authorization: `OAuth ${token}` } });
  if (r.status !== 200) return null;
  const v = (await r.json()) as Validated;
  return typeof v.user_id === "string" && typeof v.login === "string" ? v : null;
}

/** True when `broadcasterId` is among the channels the token's user moderates. */
async function moderates(env: ModEnv, deps: ModDeps, token: string, clientId: string, userId: string, broadcasterId: string): Promise<boolean> {
  let cursor = "";
  for (let page = 0; page < 20; page++) {
    const u = new URL(`${(env.TWITCH_API_BASE || "https://api.twitch.tv").replace(/\/+$/, "")}/helix/moderation/channels`);
    u.searchParams.set("user_id", userId);
    u.searchParams.set("first", "100");
    if (cursor) u.searchParams.set("after", cursor);
    const r = await deps.fetch(u.toString(), { headers: { Authorization: `Bearer ${token}`, "Client-Id": clientId } });
    if (r.status !== 200) throw new Error(`Twitch moderation lookup failed (${r.status})`);
    const body = (await r.json()) as { data?: { broadcaster_id: string }[]; pagination?: { cursor?: string } };
    if ((body.data ?? []).some((c) => c.broadcaster_id === broadcasterId)) return true;
    cursor = body.pagination?.cursor ?? "";
    if (!cursor) return false;
  }
  return false;
}

function idBase(env: ModEnv): string {
  return (env.TWITCH_ID_BASE || "https://id.twitch.tv").replace(/\/+$/, "");
}

// ------------------------------------------------------------------------------------------
// request guards

function sameOrigin(req: Request, url: URL): boolean {
  const o = req.headers.get("Origin");
  return o === url.origin && req.headers.get("X-SE-Mod") === "1";
}

async function readJson(req: Request): Promise<Record<string, unknown> | null> {
  const len = Number(req.headers.get("Content-Length") ?? "0");
  if (!Number.isFinite(len) || len < 0 || len > MAX_MOD_BODY) return null;
  if (!req.body) return null;
  const reader = req.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > MAX_MOD_BODY) {
        await reader.cancel();
        return null;
      }
      chunks.push(value);
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    const v = JSON.parse(new TextDecoder().decode(bytes));
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  } finally {
    reader.releaseLock();
  }
}

async function currentSession(req: Request, env: ModEnv, deps: ModDeps): Promise<Session | null> {
  const c = readCookie(req, SESSION_COOKIE);
  return c ? verifySession(env.RELAY_SECRET, c, deps.now()) : null;
}

// ------------------------------------------------------------------------------------------
// routes

export function createModHandler(deps: ModDeps = defaultDeps) {
  return async function handleMod(req: Request, env: ModEnv, _ctx?: ExecutionContext): Promise<Response> {
    const url = new URL(req.url);
    const path = url.pathname.replace(/\/+$/, "") || "/mod";
    const get = req.method === "GET" || req.method === "HEAD";
    try {
      if (path === "/mod" && get) {
        if (queueOnly(env)) return new Response(null, { status: 302, headers: { Location: "/queue#moderator", ...SECURITY_HEADERS } });
        return await index(req, env, deps);
      }
      if (path === "/mod/style.css" && get) return new Response(STYLE, { headers: { "Content-Type": "text/css; charset=utf-8", ...SECURITY_HEADERS } });
      if (path === "/mod/app.js" && get && !queueOnly(env)) return new Response(APP_JS, { headers: { "Content-Type": "text/javascript; charset=utf-8", ...SECURITY_HEADERS } });
      if (path === "/mod/callback.js" && get) return new Response(CALLBACK_JS, { headers: { "Content-Type": "text/javascript; charset=utf-8", ...SECURITY_HEADERS } });
      if (path === "/mod/login" && get) return await login(req, env, deps, url);
      if (path === "/mod/callback" && get) return html(page("Signing in…", `<h1>Signing in…</h1><p class="msg" id="msg">Checking your Twitch account.</p>`, "/mod/callback.js"));
      if (path === "/mod/session" && req.method === "POST") return await createSession(req, env, deps, url);
      if (path === "/mod/api" && req.method === "POST") return await api(req, env, deps, url);
      if (path === "/mod/logout" && (queueOnly(env) || req.method === "POST")) {
        if (req.method !== "POST") return json({ ok: false, error: "method not allowed" }, 405, { Allow: "POST" });
        if (!sameOrigin(req, url)) return json({ ok: false, error: "bad origin" }, 403);
        return json({ ok: true }, 200, { "Set-Cookie": cookie(SESSION_COOKIE, "", 0, "Strict") });
      }
      if (path === "/mod/logout" && get) {
        return new Response(null, { status: 302, headers: { Location: "/mod", "Set-Cookie": cookie(SESSION_COOKIE, "", 0, "Strict"), ...SECURITY_HEADERS } });
      }
      return new Response("not found", { status: 404, headers: SECURITY_HEADERS });
    } catch (e) {
      console.error("mod:", e);
      return json({ ok: false, error: "internal error" }, 500);
    }
  };
}

export const handleMod = createModHandler();

async function index(req: Request, env: ModEnv, deps: ModDeps): Promise<Response> {
  const s = await currentSession(req, env, deps);
  if (s) {
    return html(
      page(
        "Mod console",
        `<header><h1>Mod console</h1><span id="engine" class="pill">connecting…</span><span class="who">${esc(s.login)}${s.b ? " (broadcaster)" : ""}</span><a href="/mod/logout">Sign out</a></header>
<section id="console" aria-live="polite"></section>`,
        "/mod/app.js",
      ),
    );
  }
  const connected = await deps.relay(env).engineConnected().catch(() => false);
  return html(
    page(
      "Mod console — sign in",
      `<h1>Mod console</h1><p class="msg">Moderators of the channel can manage the song queue, alerts, TTS, and chat holds here.</p>
${connected ? `<p><a class="btn primary" href="/mod/login">Sign in with Twitch</a></p>` : `<p class="msg warn">The stream engine is offline right now.</p>`}`,
    ),
  );
}

async function login(_req: Request, env: ModEnv, deps: ModDeps, url: URL): Promise<Response> {
  const hello = await engineHello(env, deps);
  if (typeof hello === "string") return messagePage("Can't sign in", hello, 503);
  if (!hello.enabled) return messagePage("Can't sign in", "Remote mod access is turned off for this channel.", 403);
  if (!hello.client_id) return messagePage("Can't sign in", "The channel's Twitch app isn't configured yet.", 503);
  const state = randomHex(16);
  const auth = new URL(`${idBase(env)}/oauth2/authorize`);
  auth.searchParams.set("response_type", "token");
  auth.searchParams.set("client_id", hello.client_id);
  auth.searchParams.set("redirect_uri", `${url.origin}/mod/callback`);
  auth.searchParams.set("scope", SCOPE);
  auth.searchParams.set("state", state);
  return new Response(null, { status: 302, headers: { Location: auth.toString(), "Set-Cookie": cookie(STATE_COOKIE, state, 600, "Lax"), ...SECURITY_HEADERS } });
}

async function createSession(req: Request, env: ModEnv, deps: ModDeps, url: URL): Promise<Response> {
  if (!sameOrigin(req, url)) return json({ ok: false, error: "bad origin" }, 403);
  const body = await readJson(req);
  const token = typeof body?.access_token === "string" ? body.access_token : "";
  const state = typeof body?.state === "string" ? body.state : "";
  const expected = readCookie(req, STATE_COOKIE) ?? "";
  const clearState = cookie(STATE_COOKIE, "", 0, "Lax");
  if (!token || !state || !expected || !safeEqual(state, expected)) return json({ ok: false, error: "sign-in expired, please try again" }, 400, { "Set-Cookie": clearState });
  const hello = await engineHello(env, deps);
  if (typeof hello === "string") return json({ ok: false, error: hello }, 503);
  if (!hello.enabled) return json({ ok: false, error: "remote mod access is turned off" }, 403);
  const v = await validateToken(env, deps, token);
  if (!v) return json({ ok: false, error: "Twitch did not accept the sign-in" }, 401, { "Set-Cookie": clearState });
  if (!hello.client_id || v.client_id !== hello.client_id) return json({ ok: false, error: "token was issued to another application" }, 401, { "Set-Cookie": clearState });
  const bid = hello.broadcaster?.id ?? "";
  if (!bid) return json({ ok: false, error: "the engine has no Twitch broadcaster signed in yet" }, 503);
  const isBroadcaster = v.user_id === bid;
  if (!isBroadcaster) {
    if (!v.scopes?.includes(SCOPE)) return json({ ok: false, error: "missing permission to check moderator status" }, 401, { "Set-Cookie": clearState });
    const ok = await moderates(env, deps, token, hello.client_id, v.user_id, bid);
    if (!ok) return json({ ok: false, error: `${v.login} is not a moderator of ${hello.broadcaster.login || "this channel"}` }, 403, { "Set-Cookie": clearState });
  }
  // the Twitch token isn't needed any more
  deps
    .fetch(`${idBase(env)}/oauth2/revoke`, { method: "POST", headers: { "Content-Type": "application/x-www-form-urlencoded" }, body: new URLSearchParams({ client_id: hello.client_id, token }) })
    .catch(() => undefined);
  const user = { id: v.user_id, login: v.login.toLowerCase(), broadcaster: isBroadcaster };
  const auth = await deps.relay(env).engineCall("mod", { kind: "authorize", user }, ENGINE_TIMEOUT_MS);
  if (!auth.ok) return json({ ok: false, error: auth.error }, 403, { "Set-Cookie": clearState });
  const hours = Math.min(Math.max(Number(env.MOD_SESSION_HOURS ?? "12") || 12, 0.1), 72);
  const exp = Math.floor(deps.now() / 1000 + hours * 3600);
  const session = await signSession(env.RELAY_SECRET, { uid: user.id, login: user.login, b: isBroadcaster, exp });
  const h = new Headers();
  h.append("Set-Cookie", cookie(SESSION_COOKIE, session, Math.floor(hours * 3600), "Strict"));
  h.append("Set-Cookie", clearState);
  return json({ ok: true, login: user.login, ...(queueOnly(env) ? { redirect: "/queue#moderator" } : {}) }, 200, h);
}

async function api(req: Request, env: ModEnv, deps: ModDeps, url: URL): Promise<Response> {
  if (!sameOrigin(req, url)) return json({ ok: false, error: "bad origin" }, 403);
  const s = await currentSession(req, env, deps);
  if (!s) return json({ ok: false, error: "signed out" }, 401);
  const body = await readJson(req);
  if (!body) return json({ ok: false, error: "bad request" }, 400);
  if (queueOnly(env) && !(
    (body.kind === "query" && body.name === "queue") ||
    (body.kind === "cmd" && typeof body.action === "string" && QUEUE_ACTIONS[body.action] === true)
  )) return json({ ok: false, error: "only queue management is available" }, 403);
  const kind = body.kind;
  const user = { id: s.uid, login: s.login, broadcaster: s.b };
  let msg: Record<string, unknown>;
  if (kind === "state") msg = { kind, user };
  else if (kind === "cmd" && typeof body.action === "string" && body.action.length <= 64) msg = { kind, user, action: body.action, args: body.args ?? null };
  else if (kind === "query" && typeof body.name === "string" && body.name.length <= 64) msg = { kind, user, name: body.name, args: body.args ?? null };
  else return json({ ok: false, error: "bad request" }, 400);
  let actions: string[] = [];
  if (kind === "query" && body.name === "queue") {
    const auth = await deps.relay(env).engineCall("mod", { kind: "authorize", user }, ENGINE_TIMEOUT_MS);
    if (!auth.ok) return json(auth, ["engine offline", "engine did not answer", "engine busy", "service shutting down"].includes(auth.error) ? 503 : 403);
    const allowed = (auth.result as { actions?: unknown } | null)?.actions;
    if (!Array.isArray(allowed) || !allowed.every((action) => typeof action === "string")) {
      return json({ ok: false, error: "invalid engine authorization reply" }, 503);
    }
    actions = allowed;
  }
  const r = await deps.relay(env).engineCall("mod", msg, ENGINE_TIMEOUT_MS);
  return json(r.ok && kind === "query" && body.name === "queue" ? { ...r, login: s.login, actions } : r,
    r.ok ? 200 : ["engine offline", "engine did not answer", "engine busy", "service shutting down"].includes(r.error) ? 503 : 403);
}

// ------------------------------------------------------------------------------------------
// static assets

const STYLE = `
:root{--bg:#16161e;--panel:#1f2130;--fg:#e6e6f0;--dim:#9aa0b8;--accent:#7aa2f7;--red:#f7768e;--yellow:#e0af68;--green:#9ece6a}
@media (prefers-color-scheme: light){:root{--bg:#f4f4f8;--panel:#fff;--fg:#1d1f2b;--dim:#5b6078;--accent:#2e5bd8;--red:#c4314b;--yellow:#a36b00;--green:#3b7a1d}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 system-ui,sans-serif}
main{max-width:1100px;margin:0 auto;padding:16px}h1{font-size:20px;margin:0 12px 0 0}h2{font-size:13px;letter-spacing:.08em;text-transform:uppercase;color:var(--dim);margin:0 0 8px}
header{display:flex;align-items:center;gap:12px;margin-bottom:16px;flex-wrap:wrap}header a{color:var(--dim)}.who{margin-left:auto;color:var(--dim)}
.msg{color:var(--dim)}.warn{color:var(--yellow)}.err{color:var(--red)}
.btn,button{display:inline-block;border:1px solid var(--dim);background:var(--panel);color:var(--fg);border-radius:6px;padding:6px 12px;font:inherit;cursor:pointer;text-decoration:none}
button:hover,.btn:hover{border-color:var(--accent)}button:disabled{opacity:.4;cursor:default}.primary{border-color:var(--accent);color:var(--accent)}.danger{border-color:var(--red);color:var(--red)}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(320px,1fr));gap:12px}.card{background:var(--panel);border-radius:8px;padding:12px}
.card>button,.card>span{margin:0 6px 8px 0;vertical-align:middle}
.row{display:flex;align-items:center;gap:8px;padding:4px 0;border-bottom:1px solid rgba(128,128,128,.15)}.row:last-child{border-bottom:0}.row .t{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis}
.sub{color:var(--dim);font-size:13px}.pill{border-radius:10px;padding:2px 8px;font-size:12px;border:1px solid var(--dim)}.pill.ok{border-color:var(--green);color:var(--green)}.pill.bad{border-color:var(--red);color:var(--red)}
#toast{position:fixed;right:16px;bottom:16px;background:var(--panel);border:1px solid var(--red);padding:8px 12px;border-radius:6px;display:none}
`;

const CALLBACK_JS = `(function(){
  var msg=document.getElementById('msg');
  function fail(t){msg.textContent=t;msg.className='msg err';var a=document.createElement('a');a.href='/mod';a.className='btn';a.textContent='Back';msg.after(a);}
  var q=new URLSearchParams(location.search);
  if(q.get('error')){fail('Sign-in cancelled: '+(q.get('error_description')||q.get('error')));return;}
  var h=new URLSearchParams(location.hash.slice(1));
  history.replaceState(null,'',location.pathname);
  var token=h.get('access_token'),state=h.get('state');
  if(!token||!state){fail('Twitch did not return a sign-in token.');return;}
  fetch('/mod/session',{method:'POST',credentials:'same-origin',headers:{'Content-Type':'application/json','X-SE-Mod':'1'},body:JSON.stringify({access_token:token,state:state})})
    .then(function(r){return r.json();})
    .then(function(r){if(r.ok){location.replace(r.redirect||'/mod');}else{fail(r.error||'Sign-in failed.');}})
    .catch(function(){fail('Network error while signing in.');});
})();`;

const APP_JS = `(function(){
  'use strict';
  var root=document.getElementById('console'),pill=document.getElementById('engine');
  var toast=document.createElement('div');toast.id='toast';document.body.appendChild(toast);
  var actions=[],busy=false;
  function seg(p,s){if(p.indexOf('*')<0)return p===s;return new RegExp('^'+p.split('*').map(function(x){return x.replace(/[.+?^$\${}()|[\\]\\\\]/g,'\\\\$&');}).join('.*')+'$').test(s);}
  function match(p,a){p=p.split('.');a=a.split('.');function go(i,j){if(i===p.length)return j===a.length;if(p[i]==='**'){for(var k=j;k<=a.length;k++)if(go(i+1,k))return true;return false;}return j<a.length&&seg(p[i],a[j])&&go(i+1,j+1);}return go(0,0);}
  function allowed(a){return actions.some(function(p){return match(p,a);});}
  function el(tag,cls,text){var e=document.createElement(tag);if(cls)e.className=cls;if(text!=null)e.textContent=String(text);return e;}
  function say(t){toast.textContent=t;toast.style.display='block';clearTimeout(say.t);say.t=setTimeout(function(){toast.style.display='none';},4000);}
  function post(body){return fetch('/mod/api',{method:'POST',credentials:'same-origin',headers:{'Content-Type':'application/json','X-SE-Mod':'1'},body:JSON.stringify(body)}).then(function(r){if(r.status===401){location.replace('/mod');}return r.json();});}
  function cmd(action,args,label){if(!allowed(action))return;post({kind:'cmd',action:action,args:args||null}).then(function(r){if(!r.ok)say((label||action)+': '+r.error);refresh();}).catch(function(){say('network error');});}
  function btn(label,action,args,cls){var b=el('button',cls||'',label);b.disabled=!allowed(action);b.onclick=function(){cmd(action,args,label);};return b;}
  function row(main,sub,buttons){var r=el('div','row');var t=el('div','t');t.appendChild(el('div','',main));if(sub)t.appendChild(el('div','sub',sub));r.appendChild(t);(buttons||[]).forEach(function(b){r.appendChild(b);});return r;}
  function card(title,head){var c=el('div','card');var h=el('h2','',title);c.appendChild(h);if(head)head.forEach(function(b){c.appendChild(b);});return c;}
  function render(s){
    actions=s.actions||[];root.textContent='';var g=el('div','grid');root.appendChild(g);
    var st=s.state||{};
    // queue
    var q=s.queue;
    if(q){var c=card('Song queue '+(q.open?'(open)':'(closed)'),[q.open?btn('Close queue','queue.close'):btn('Open queue','queue.open')]);
      if(q.now){c.appendChild(row('▶ '+q.now.title,'requested by '+q.now.user,[btn('Skip','queue.skip',{}),btn('Ban song','queue.ban_song',{id:q.now.id},'danger')]));}
      (q.pending||[]).forEach(function(e){c.appendChild(row('⏳ '+e.title,'pending · '+e.user,[btn('Approve','queue.approve',{id:e.id},'primary'),btn('Reject','queue.reject',{id:e.id})]));});
      (q.upcoming||[]).forEach(function(e){c.appendChild(row((e.pos||'')+'. '+e.title,e.user,[btn('Remove','queue.remove',{id:e.id})]));});
      if(!q.now&&!(q.upcoming||[]).length&&!(q.pending||[]).length)c.appendChild(el('div','sub','queue is empty'));
      g.appendChild(c);}
    // alerts
    var a=s.alerts;
    if(a){var c2=card('Alerts'+(a.paused?' (paused)':''),[a.paused?btn('Resume','alerts.resume'):btn('Pause','alerts.pause')]);
      if(a.current)c2.appendChild(row('on screen: '+(a.current.title||a.current.kind),(a.current.user||'')+(a.current.message?' — '+a.current.message:''),[btn('Hide','alerts.skip'),btn('Kill','alerts.veto',{id:a.current.id},'danger')]));
      (a.queue||[]).forEach(function(x){var b=[btn('Kill','alerts.veto',{id:x.id},'danger')];if(x.veto_remaining_ms>0)b.unshift(btn('OK now','alerts.approve',{id:x.id}));c2.appendChild(row(x.title||x.kind,(x.user||'')+(x.message?' — '+x.message:'')+(x.veto_remaining_ms>0?' · veto '+Math.ceil(x.veto_remaining_ms/1000)+' s':''),b));});
      if(!a.current&&!(a.queue||[]).length)c2.appendChild(el('div','sub','no alerts queued'));
      g.appendChild(c2);}
    // chat holds
    var pend=st['policy.pending']||[],am=st['twitch.automod.queue']||[];
    var c3=card('Chat holds');
    pend.forEach(function(p){c3.appendChild(row((p.kind==='veto'?'veto: ':'approval: ')+(p.text||p.reward||p.type||''),(p.user||'')+(p.amount?' · '+p.amount:''),[btn('Approve','mod.approve',{id:p.id},'primary'),btn('Reject','mod.reject',{id:p.id},'danger')]));});
    am.forEach(function(m){c3.appendChild(row('AutoMod: '+m.text,(m.user||'')+(m.category?' · '+m.category:''),[btn('Allow','mod.automod.approve',{message_id:m.message_id}),btn('Deny','mod.automod.deny',{message_id:m.message_id},'danger')]));});
    if(!pend.length&&!am.length)c3.appendChild(el('div','sub','nothing held'));
    g.appendChild(c3);
    // tts
    var t=s.tts;
    if(t){var c4=card('Text to speech',[btn('Skip','tts.skip'),btn('Clear','tts.clear',null,'danger')]);
      if(t.current)c4.appendChild(row('speaking: '+(t.current.text||''),t.current.user||''));
      (t.queue||[]).forEach(function(x){c4.appendChild(row(x.text||'',x.user||'',[btn('Drop','tts.skip',{id:x.id})]));});
      if(!t.current&&!(t.queue||[]).length)c4.appendChild(el('div','sub','silent'));
      g.appendChild(c4);}
    // giveaway
    var gw=s.giveaway;
    if(gw&&gw.state&&gw.state!=='idle'){var c5=card('Giveaway: '+(gw.title||''),[gw.state==='open'?btn('Close','giveaway.close'):el('span','sub','closed'),btn('Draw','giveaway.draw',null,'primary')]);
      c5.appendChild(el('div','sub',(gw.entries||[]).length+' entries · keyword '+gw.keyword));
      (gw.winners||[]).forEach(function(w){c5.appendChild(row('🏆 '+w.user,(Math.round(w.chance*1000)/10)+'% chance'));});
      g.appendChild(c5);}
    var c6=card('Chat effects');c6.appendChild(row('Clear every chat-triggered effect on stream',st['show.mode']?'mode: '+st['show.mode']:'',[btn('Clean','clean',null,'danger')]));g.appendChild(c6);
  }
  function refresh(){if(busy)return;busy=true;post({kind:'state'}).then(function(r){busy=false;if(r.ok){pill.textContent='engine online';pill.className='pill ok';render(r.result||{});}else{pill.textContent=r.error==='engine offline'?'engine offline':'error';pill.className='pill bad';if(r.error!=='engine offline')say(r.error);}}).catch(function(){busy=false;pill.textContent='network error';pill.className='pill bad';});}
  refresh();setInterval(refresh,2000);
})();`;
