import { SELF, env as testEnv } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { createModHandler, matches, signSession, verifySession, type EngineReply, type ModDeps, type ModEnv, type RelayRpc, type Session } from "../src/mod";

const ORIGIN = "https://relay.example";
const SECRET = "0123456789abcdef0123456789abcdef";
const CLIENT = "client-abc";
const BROADCASTER = { id: "1000", login: "dabsanddrums" };

interface Call {
  kind: string;
  body: Record<string, unknown>;
}

interface Harness {
  calls: Call[];
  fetches: string[];
  call(path: string, init?: RequestInit): Promise<Response>;
  env: ModEnv;
  advance(ms: number): number;
}

function setup(opts: { queueOnly?: boolean; actions?: string[]; enabled?: boolean; connected?: boolean; mods?: string[][]; tokens?: Record<string, { user_id: string; login: string; client_id?: string; scopes?: string[] }>; engine?: (b: Record<string, unknown>) => EngineReply } = {}): Harness {
  const calls: Call[] = [];
  const fetches: string[] = [];
  let now = Date.parse("2026-09-25T20:00:00Z");
  const connected = opts.connected ?? true;
  const relay: RelayRpc = {
    async engineCall(kind, body) {
      const b = body as Record<string, unknown>;
      calls.push({ kind, body: b });
      if (!connected) return { ok: false, error: "engine offline" };
      if (b.kind === "hello") return { ok: true, result: { enabled: opts.enabled ?? true, client_id: CLIENT, broadcaster: BROADCASTER, actions: ["queue.**", "tts.skip"] } };
      if (opts.engine) return opts.engine(b);
      if (b.kind === "authorize") return { ok: true, result: { allowed: true, actions: opts.actions ?? ["queue.**", "tts.skip"] } };
      return { ok: true, result: { echoed: b } };
    },
    async engineConnected() {
      return connected;
    },
  };
  const tokens = opts.tokens ?? { "tok-mod": { user_id: "42", login: "ModPerson", scopes: ["user:read:moderated_channels"] } };
  // pages of moderated channels per user id "42"
  const pages = opts.mods ?? [["7", BROADCASTER.id]];
  const fakeFetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url);
    fetches.push(`${init?.method ?? "GET"} ${url.pathname}${url.search}`);
    const auth = new Headers(init?.headers).get("Authorization") ?? "";
    if (url.pathname === "/oauth2/validate") {
      const t = tokens[auth.replace(/^OAuth /, "")];
      if (!t) return new Response(JSON.stringify({ status: 401, message: "invalid access token" }), { status: 401 });
      return Response.json({ client_id: t.client_id ?? CLIENT, login: t.login.toLowerCase(), user_id: t.user_id, scopes: t.scopes ?? [], expires_in: 5000 });
    }
    if (url.pathname === "/helix/moderation/channels") {
      if (!tokens[auth.replace(/^Bearer /, "")]) return new Response("", { status: 401 });
      const after = Number(url.searchParams.get("after") ?? "0");
      const page = pages[after] ?? [];
      return Response.json({ data: page.map((id) => ({ broadcaster_id: id, broadcaster_login: `c${id}` })), pagination: after + 1 < pages.length ? { cursor: String(after + 1) } : {} });
    }
    if (url.pathname === "/oauth2/revoke") return new Response("", { status: 200 });
    return new Response("unexpected", { status: 599 });
  }) as typeof fetch;
  const deps: ModDeps = { relay: () => relay, fetch: fakeFetch, now: () => now };
  const env: ModEnv = { RELAY_SECRET: SECRET, MOD_QUEUE_ONLY: opts.queueOnly ? "1" : undefined, TWITCH_ID_BASE: "https://id.fake", TWITCH_API_BASE: "https://api.fake" };
  const handler = createModHandler(deps);
  const call = (path: string, init: RequestInit = {}) => handler(new Request(`${ORIGIN}${path}`, init), env);
  return { calls, fetches, call, env, advance: (ms: number) => (now += ms) };
}

function cookies(r: Response): Record<string, string> {
  const out: Record<string, string> = {};
  for (const c of r.headers.getSetCookie()) {
    const kv = c.split(";")[0] ?? "";
    const i = kv.indexOf("=");
    out[kv.slice(0, i)] = kv.slice(i + 1);
  }
  return out;
}

interface ApiBody {
  ok?: boolean;
  error?: string;
  login?: string;
  result?: { echoed?: unknown };
  actions?: string[];
}

async function body(r: Response): Promise<ApiBody> {
  return (await r.json()) as ApiBody;
}

/** Sign in as the default moderator and return the session cookie value. */
async function sessionOf(t: Harness): Promise<string> {
  const s = cookies(await signIn(t)).se_mod;
  if (!s) throw new Error("no session cookie");
  return s;
}

async function signIn(t: Harness, token = "tok-mod") {
  const login = await t.call("/mod/login");
  const state = new URL(login.headers.get("Location")!).searchParams.get("state")!;
  return t.call("/mod/session", {
    method: "POST",
    headers: { Origin: ORIGIN, "X-SE-Mod": "1", "Content-Type": "application/json", Cookie: `se_mod_state=${cookies(login).se_mod_state}` },
    body: JSON.stringify({ access_token: token, state }),
  });
}

function api(t: Harness, session: string, body: unknown, headers: Record<string, string> = {}) {
  return t.call("/mod/api", { method: "POST", headers: { Origin: ORIGIN, "X-SE-Mod": "1", Cookie: `se_mod=${session}`, ...headers }, body: JSON.stringify(body) });
}

describe("sign-in page and login redirect", () => {
  it("offers Twitch sign-in when the engine is online", async () => {
    const t = setup();
    const r = await t.call("/mod");
    expect(r.status).toBe(200);
    expect(r.headers.get("Content-Security-Policy")).toContain("frame-ancestors 'none'");
    expect(await r.text()).toContain('href="/mod/login"');
  });

  it("returns unavailable when the engine is offline", async () => {
    const t = setup({ connected: false });
    expect((await t.call("/mod/login")).status).toBe(503);
    expect(t.calls).toEqual([{ kind: "mod", body: { kind: "hello" } }]);
  });

  it("redirects to Twitch with our client id, callback, scope, and a state cookie", async () => {
    const t = setup();
    const r = await t.call("/mod/login");
    expect(r.status).toBe(302);
    const loc = new URL(r.headers.get("Location")!);
    expect(loc.origin + loc.pathname).toBe("https://id.fake/oauth2/authorize");
    expect(loc.searchParams.get("client_id")).toBe(CLIENT);
    expect(loc.searchParams.get("response_type")).toBe("token");
    expect(loc.searchParams.get("redirect_uri")).toBe(`${ORIGIN}/mod/callback`);
    expect(loc.searchParams.get("scope")).toBe("user:read:moderated_channels");
    expect(cookies(r).se_mod_state).toBe(loc.searchParams.get("state"));
    expect(r.headers.getSetCookie()[0]).toMatch(/HttpOnly; Secure; SameSite=Lax/);
  });

  it("refuses when remote mod access is disabled in the engine", async () => {
    const t = setup({ enabled: false });
    expect((await t.call("/mod/login")).status).toBe(403);
  });
});

describe("session creation", () => {
  it("signs in a channel moderator and revokes the Twitch token", async () => {
    const t = setup();
    const r = await signIn(t);
    expect(r.status).toBe(200);
    expect(await body(r)).toEqual({ ok: true, login: "modperson" });
    const c = cookies(r);
    expect(c.se_mod).toBeTruthy();
    expect(c.se_mod_state).toBe("");
    expect(r.headers.getSetCookie().find((x) => x.startsWith("se_mod="))).toMatch(/Path=\/mod; HttpOnly; Secure; SameSite=Strict/);
    expect(t.fetches).toContain("POST /oauth2/revoke");
    const auth = t.calls.find((x) => x.body.kind === "authorize");
    expect(auth?.body.user).toEqual({ id: "42", login: "modperson", broadcaster: false });
  });

  it("finds the channel on a later page of moderated channels", async () => {
    const t = setup({ mods: [["1", "2"], ["3"], ["4", BROADCASTER.id]] });
    expect((await signIn(t)).status).toBe(200);
    expect(t.fetches.filter((f) => f.startsWith("GET /helix/moderation/channels")).length).toBe(3);
  });

  it("rejects viewers who don't moderate the channel", async () => {
    const t = setup({ mods: [["7", "8"]] });
    const r = await signIn(t);
    expect(r.status).toBe(403);
    expect((await body(r)).error).toContain("not a moderator of dabsanddrums");
    expect(cookies(r).se_mod).toBeUndefined();
  });

  it("lets the broadcaster in without a moderator lookup", async () => {
    const t = setup({ tokens: { "tok-b": { user_id: BROADCASTER.id, login: "DABSandDRUMS" } } });
    expect((await signIn(t, "tok-b")).status).toBe(200);
    expect(t.fetches.some((f) => f.includes("/helix/"))).toBe(false);
    expect(t.calls.find((x) => x.body.kind === "authorize")?.body.user).toEqual({ id: BROADCASTER.id, login: "dabsanddrums", broadcaster: true });
  });

  it("rejects tokens issued to another application", async () => {
    const t = setup({ tokens: { "tok-x": { user_id: "42", login: "m", client_id: "someone-else", scopes: ["user:read:moderated_channels"] } } });
    expect((await signIn(t, "tok-x")).status).toBe(401);
  });

  it("rejects invalid tokens, state mismatches, and cross-origin posts", async () => {
    const t = setup();
    expect((await signIn(t, "tok-bogus")).status).toBe(401);
    const login = await t.call("/mod/login");
    const bad = await t.call("/mod/session", {
      method: "POST",
      headers: { Origin: ORIGIN, "X-SE-Mod": "1", Cookie: `se_mod_state=${cookies(login).se_mod_state}` },
      body: JSON.stringify({ access_token: "tok-mod", state: "not-the-state" }),
    });
    expect(bad.status).toBe(400);
    const xorigin = await t.call("/mod/session", { method: "POST", headers: { Origin: "https://evil.example", "X-SE-Mod": "1" }, body: "{}" });
    expect(xorigin.status).toBe(403);
  });

  it("honours an engine-side refusal (deny list)", async () => {
    const t = setup({ engine: (b) => (b.kind === "authorize" ? { ok: false, error: "not allowed" } : { ok: true, result: null }) });
    const r = await signIn(t);
    expect(r.status).toBe(403);
    expect((await body(r)).error).toBe("not allowed");
  });
});

describe("console API", () => {
  it("forwards state and commands with the signed-in moderator", async () => {
    const t = setup();
    const session = await sessionOf(t);
    const st = await api(t, session, { kind: "state" });
    expect(st.status).toBe(200);
    expect((await body(st)).result?.echoed).toEqual({ kind: "state", user: { id: "42", login: "modperson", broadcaster: false } });
    const cmd = await api(t, session, { kind: "cmd", action: "queue.skip", args: { id: 3 } });
    expect((await body(cmd)).result?.echoed).toEqual({ kind: "cmd", user: { id: "42", login: "modperson", broadcaster: false }, action: "queue.skip", args: { id: 3 } });
  });

  it("passes engine refusals through", async () => {
    const t = setup({ engine: (b) => (b.kind === "cmd" ? { ok: false, error: "`lights.cue` is not available to moderators" } : { ok: true, result: {} }) });
    const session = await sessionOf(t);
    const r = await api(t, session, { kind: "cmd", action: "lights.cue" });
    expect(r.status).toBe(403);
    expect((await body(r)).error).toContain("not available");
  });

  it("requires a valid, unexpired, untampered session", async () => {
    const t = setup();
    const session = await sessionOf(t);
    expect((await api(t, "", { kind: "state" })).status).toBe(401);
    const [p = "", s = ""] = session.split(".");
    const forged = btoa(JSON.stringify({ uid: "1000", login: "dabsanddrums", b: true, exp: 9e9 })).replace(/=+$/, "");
    expect((await api(t, `${forged}.${s}`, { kind: "state" })).status).toBe(401);
    expect((await api(t, `${p}.${s.slice(0, -2)}xx`, { kind: "state" })).status).toBe(401);
    t.advance(13 * 3600 * 1000);
    expect((await api(t, session, { kind: "state" })).status).toBe(401);
  });

  it("requires same-origin requests and well-formed bodies", async () => {
    const t = setup();
    const session = await sessionOf(t);
    expect((await api(t, session, { kind: "state" }, { Origin: "https://evil.example" })).status).toBe(403);
    expect((await api(t, session, { kind: "state" }, { "X-SE-Mod": "" })).status).toBe(403);
    expect((await api(t, session, { kind: "exec", action: "x" })).status).toBe(400);
    expect((await api(t, session, { kind: "cmd", action: "x".repeat(65) })).status).toBe(400);
    expect((await api(t, session, { kind: "cmd", action: "queue.skip", args: "y".repeat(20000) })).status).toBe(400);
  });

  it("reports the engine as offline", async () => {
    let online = true;
    const t = setup({ engine: (b) => (online || b.kind === "authorize" ? { ok: true, result: {} } : { ok: false, error: "engine offline" }) });
    const session = await sessionOf(t);
    online = false;
    expect((await api(t, session, { kind: "state" })).status).toBe(503);
  });

  it("clears the session on sign-out", async () => {
    const t = setup();
    const r = await t.call("/mod/logout");
    expect(r.status).toBe(302);
    expect(r.headers.getSetCookie()[0]).toMatch(/^se_mod=; .*Max-Age=0/);
  });
});

describe("queue-only moderator access", () => {
  it("redirects to the existing public queue without exposing console assets", async () => {
    const t = setup({ queueOnly: true });
    const r = await t.call("/mod");
    expect(r.status).toBe(302);
    expect(r.headers.get("Location")).toBe("/queue#moderator");
    expect(r.headers.get("Cache-Control")).toBe("no-store");
    expect((await t.call("/mod/app.js")).status).toBe(404);
    expect(t.calls).toEqual([]);
    const signedIn = await signIn(t);
    expect(signedIn.status).toBe(200);
    expect(await signedIn.json()).toEqual({ ok: true, login: "modperson", redirect: "/queue#moderator" });
    const s = await verifySession(SECRET, cookies(signedIn).se_mod!, Date.parse("2026-09-25T20:00:00Z"));
    expect(s).toEqual({ uid: "42", login: "modperson", b: false, exp: Date.parse("2026-09-26T08:00:00Z") / 1000 });
    expect(s).not.toHaveProperty("access_token");
  });

  it("requires authentication before any private queue read or mutation", async () => {
    const t = setup({ queueOnly: true });
    for (const request of [
      { kind: "query", name: "queue" },
      ...["request", "reorder", "remove", "approve", "reject"].map((action) => ({ kind: "cmd", action: `queue.${action}` })),
    ]) {
      const r = await api(t, "", request);
      expect(r.status).toBe(401);
      expect(r.headers.get("Cache-Control")).toBe("no-store");
    }
    expect(t.calls).toEqual([]);
  });

  it("reads only the real private queue and returns current engine permissions", async () => {
    let actions = ["queue.reorder", "queue.remove"];
    const snapshot = { upcoming: [{ id: 14, pos: 1, title: "Waiting song" }], pending: [{ id: 15, title: "Held song" }] };
    const t = setup({
      queueOnly: true,
      engine: (b) => ({ ok: true, result: b.kind === "authorize" ? { actions } : snapshot }),
    });
    const session = await sessionOf(t);
    t.calls.length = 0;
    const user = { id: "42", login: "modperson", broadcaster: false };
    const read = await api(t, session, { kind: "query", name: "queue", user: { id: BROADCASTER.id, broadcaster: true } });
    expect(read.status).toBe(200);
    expect(await read.json()).toEqual({ ok: true, result: snapshot, login: "modperson", actions });
    expect(t.calls).toEqual([
      { kind: "mod", body: { kind: "authorize", user } },
      { kind: "mod", body: { kind: "query", name: "queue", args: null, user } },
    ]);
    actions = ["queue.approve"];
    expect((await body(await api(t, session, { kind: "query", name: "queue" }))).actions).toEqual(actions);
  });

  it("refuses console state, unrelated/private queries and all other commands before forwarding", async () => {
    const t = setup({ queueOnly: true });
    const session = await sessionOf(t);
    t.calls.length = 0;
    const requests = [
      { kind: "state" },
      ...["queue.ui", "queue.history", "queue.account", "alerts", "tts", "giveaway", "remote_mod.request", "queue.*"].map((name) => ({ kind: "query", name })),
      ...["queue.skip", "queue.pause", "queue.play", "queue.ban_user", "queue.**", "queue.request.extra", "tts.skip", "clean", "constructor", "__proto__"].map((action) => ({ kind: "cmd", action })),
    ];
    for (const request of requests) expect((await api(t, session, request)).status).toBe(403);
    expect(t.calls).toEqual([]);
  });

  it("forwards only the five allowed commands with real arguments and signed identity", async () => {
    const t = setup({ queueOnly: true });
    const session = await sessionOf(t);
    t.calls.length = 0;
    const argsByAction: Record<string, unknown> = {
      "queue.request": { text: "https://www.youtube.com/watch?v=abcdefghijk", user: "modperson" },
      "queue.reorder": { id: 12, to: 2 },
      "queue.remove": { id: 12 },
      "queue.approve": { id: 13 },
      "queue.reject": { id: 14 },
    };
    for (const [action, args] of Object.entries(argsByAction)) {
      const r = await api(t, session, { kind: "cmd", action, args, user: { id: BROADCASTER.id, login: "owner", broadcaster: true } });
      expect(r.status).toBe(200);
      expect(t.calls.at(-1)).toEqual({
        kind: "mod", body: { kind: "cmd", action, args, user: { id: "42", login: "modperson", broadcaster: false } },
      });
    }
    expect(t.calls.length).toBe(5);
  });

  it("keeps engine policy authoritative after sign-in for both reads and commands", async () => {
    let denied = false;
    const t = setup({
      queueOnly: true,
      engine: (b) => denied ? { ok: false, error: "access disabled by engine" }
        : { ok: true, result: b.kind === "authorize" ? { actions: ["queue.**"] } : {} },
    });
    const session = await sessionOf(t);
    denied = true;
    t.calls.length = 0;
    expect((await api(t, session, { kind: "query", name: "queue" })).status).toBe(403);
    expect(t.calls.map((c) => c.body.kind)).toEqual(["authorize"]);
    expect((await api(t, session, { kind: "cmd", action: "queue.remove", args: { id: 12 } })).status).toBe(403);
    expect(t.calls.at(-1)?.body.kind).toBe("cmd");
  });

  it("fails closed when engine authorization does not provide valid action patterns", async () => {
    let result: unknown = { actions: ["queue.**"] };
    const t = setup({ queueOnly: true, engine: () => ({ ok: true, result }) });
    const session = await sessionOf(t);
    for (const invalid of [null, {}, { actions: "**" }, { actions: ["queue.**", 1] }]) {
      result = invalid;
      t.calls.length = 0;
      expect((await api(t, session, { kind: "query", name: "queue" })).status).toBe(503);
      expect(t.calls.map((c) => c.body.kind)).toEqual(["authorize"]);
    }
  });

  it("reports transport failure as unavailable without granting access", async () => {
    let error: string | null = null;
    const t = setup({
      queueOnly: true,
      engine: () => error ? { ok: false, error } : { ok: true, result: { actions: ["queue.**"] } },
    });
    const session = await sessionOf(t);
    for (const failure of ["engine offline", "engine did not answer", "engine busy", "service shutting down"]) {
      error = failure;
      expect((await api(t, session, { kind: "query", name: "queue" })).status).toBe(503);
      expect((await api(t, session, { kind: "cmd", action: "queue.remove", args: { id: 12 } })).status).toBe(503);
    }
  });

  it("rejects wrong-client tokens, missing moderator scope, and non-moderators without issuing sessions", async () => {
    const cases = [
      { tokens: { "tok-mod": { user_id: "42", login: "m", client_id: "other-app", scopes: ["user:read:moderated_channels"] } }, status: 401 },
      { tokens: { "tok-mod": { user_id: "42", login: "m", scopes: [] } }, status: 401 },
      { mods: [["another-channel"]], status: 403 },
    ];
    for (const test of cases) {
      const t = setup({ ...test, queueOnly: true });
      const r = await signIn(t);
      expect(r.status).toBe(test.status);
      expect(cookies(r).se_mod).toBeUndefined();
      expect(t.calls.some((c) => c.body.kind === "authorize")).toBe(false);
    }
  });

  it("rejects expired, tampered and wrong-key sessions before accessing private data", async () => {
    const t = setup({ queueOnly: true });
    const session = await sessionOf(t);
    const wrongKey = await signSession(`${SECRET}x`, { uid: BROADCASTER.id, login: BROADCASTER.login, b: true, exp: 9e9 });
    t.calls.length = 0;
    for (const token of [`${session}x`, wrongKey, "tok-mod"]) {
      expect((await api(t, token, { kind: "query", name: "queue" })).status).toBe(401);
      expect((await api(t, token, { kind: "cmd", action: "queue.remove", args: { id: 12 } })).status).toBe(401);
    }
    t.advance(12 * 3600 * 1000);
    expect((await api(t, session, { kind: "query", name: "queue" })).status).toBe(401);
    expect(t.calls).toEqual([]);
  });

  it("rejects cross-origin or missing-CSRF-header posts without engine access", async () => {
    const t = setup({ queueOnly: true });
    const session = await sessionOf(t);
    t.calls.length = 0;
    const invalidHeaders: Record<string, string>[] = [{ Origin: "https://evil.example" }, { Origin: "" }, { "X-SE-Mod": "" }];
    for (const headers of invalidHeaders) {
      expect((await api(t, session, { kind: "query", name: "queue" }, headers)).status).toBe(403);
      expect((await api(t, session, { kind: "cmd", action: "queue.request", args: { text: "song" } }, headers)).status).toBe(403);
    }
    expect(t.calls).toEqual([]);
  });

  it("enforces body byte limits even without Content-Length or with multibyte JSON", async () => {
    const t = setup({ queueOnly: true });
    const session = await sessionOf(t);
    t.calls.length = 0;
    const request = { kind: "cmd", action: "queue.request", args: { text: "界".repeat(6000) } };
    expect((await api(t, session, request)).status).toBe(400);
    expect((await api(t, session, { kind: "query", name: "queue" }, { "Content-Length": "20000" })).status).toBe(400);
    expect(t.calls).toEqual([]);
  });

  it("requires same-origin POST logout and clears the protected session cookie", async () => {
    const t = setup({ queueOnly: true });
    expect((await t.call("/mod/logout")).status).toBe(405);
    expect((await t.call("/mod/logout", { method: "POST", headers: { Origin: "https://evil.example", "X-SE-Mod": "1" } })).status).toBe(403);
    expect((await t.call("/mod/logout", { method: "POST", headers: { Origin: ORIGIN } })).status).toBe(403);
    const r = await t.call("/mod/logout", { method: "POST", headers: { Origin: ORIGIN, "X-SE-Mod": "1" } });
    expect(r.status).toBe(200);
    expect(await r.json()).toEqual({ ok: true });
    expect(r.headers.get("Cache-Control")).toBe("no-store");
    expect(r.headers.getSetCookie()).toEqual(["se_mod=; Path=/mod; HttpOnly; Secure; SameSite=Strict; Max-Age=0"]);
    const worker = setup();
    expect((await worker.call("/mod/logout", { method: "POST", headers: { Origin: ORIGIN, "X-SE-Mod": "1" } })).status).toBe(200);
    expect((await worker.call("/mod/logout")).status).toBe(302);
  });
});

describe("session tokens and patterns", () => {
  it("round-trips and rejects wrong secrets", async () => {
    const tok = await signSession(SECRET, { uid: "1", login: "a", b: false, exp: 2000 });
    expect(await verifySession(SECRET, tok, 1000 * 1000)).toEqual({ uid: "1", login: "a", b: false, exp: 2000 });
    expect(await verifySession(`${SECRET}x`, tok, 1000 * 1000)).toBeNull();
    expect(await verifySession(SECRET, tok, 2000 * 1000)).toBeNull();
    expect(await verifySession(SECRET, `${tok}.x`, 0)).toBeNull();
    await expect(signSession("short", { uid: "1", login: "a", b: false, exp: 1 })).rejects.toThrow(/RELAY_SECRET/);
  });

  it("rejects signed payloads with empty identities, missing roles or invalid expiry", async () => {
    const valid = { uid: "1", login: "a", b: false, exp: 2000 };
    for (const s of [
      { ...valid, uid: "" }, { ...valid, login: "" },
      { ...valid, b: undefined }, { ...valid, b: "true" },
      { ...valid, exp: Infinity }, { ...valid, exp: "2000" },
    ]) {
      const token = await signSession(SECRET, s as Session);
      expect(await verifySession(SECRET, token, 1000 * 1000)).toBeNull();
    }
  });

  it("matches command patterns like the engine", () => {
    expect(matches("queue.**", "queue.skip")).toBe(true);
    expect(matches("queue.**", "queue.ban_user")).toBe(true);
    expect(matches("queue.*", "queue.a.b")).toBe(false);
    expect(matches("tts.skip", "tts.say")).toBe(false);
    expect(matches("mod.automod.*", "mod.automod.approve")).toBe(true);
    expect(matches("giveaway.d*", "giveaway.draw")).toBe(true);
    expect(matches("clean", "clean")).toBe(true);
  });
});

describe("through the Worker and the relay Durable Object", () => {
  /** A fake engine on the real `/link` socket answering `mod.req` frames. */
  async function linkEngine(reply: (body: Record<string, unknown>) => unknown): Promise<{ ws: WebSocket; bodies: Record<string, unknown>[] }> {
    const res = await SELF.fetch("https://relay.test/link", { headers: { upgrade: "websocket", authorization: `Bearer ${testEnv.RELAY_SECRET}` } });
    expect(res.status).toBe(101);
    const ws = res.webSocket as WebSocket;
    ws.accept();
    const bodies: Record<string, unknown>[] = [];
    const welcomed = new Promise<void>((resolve) => {
      ws.addEventListener("message", (ev) => {
        const f = JSON.parse(String(ev.data)) as { t?: string; id?: string; body?: Record<string, unknown> };
        if (f.t === "welcome") resolve();
        if (f.t === "mod.req" && f.body) {
          bodies.push(f.body);
          ws.send(JSON.stringify({ t: "mod.res", id: f.id, ok: true, result: reply(f.body) }));
        }
      });
    });
    ws.send(JSON.stringify({ t: "hello", v: 1, engine: "mod-test" }));
    await welcomed;
    return { ws, bodies };
  }

  it("serves /mod from the Worker router and asks the linked engine for the login config", async () => {
    const { ws, bodies } = await linkEngine((b) => (b.kind === "hello" ? { enabled: true, client_id: "real-client", broadcaster: { id: "1", login: "chan" }, actions: [] } : null));
    try {
      expect(await (await SELF.fetch("https://relay.test/mod")).text()).toContain('href="/mod/login"');
      const login = await SELF.fetch("https://relay.test/mod/login", { redirect: "manual" });
      expect(login.status).toBe(302);
      const loc = new URL(login.headers.get("Location") ?? "");
      expect(loc.origin).toBe("https://id.twitch.tv");
      expect(loc.searchParams.get("client_id")).toBe("real-client");
      expect(bodies).toEqual([{ kind: "hello" }]);
      expect((await SELF.fetch("https://relay.test/mod/api", { method: "POST", headers: { Origin: "https://relay.test", "X-SE-Mod": "1" }, body: "{}" })).status).toBe(401);
      expect((await SELF.fetch("https://relay.test/mod/nope")).status).toBe(404);
    } finally {
      ws.close(1000, "done");
    }
  });
});
