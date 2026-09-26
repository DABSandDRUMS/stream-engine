import { SELF, env, runInDurableObject } from "cloudflare:test";
import { beforeEach, describe, expect, it } from "vitest";
import { relayStub } from "../src/env";
import { parseKofi } from "../src/kofi";

const SECRET = "test-relay-secret-0123456789abcdef0123456789abcdef";
const KOFI_TOKEN = "kofi-test-token-3f6c";
const BASE = "https://relay.test";

type Frame = Record<string, unknown> & { t?: string };

/** Ko-fi's documented test payload with our token and a chosen id. */
function kofiData(id: string, extra: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    verification_token: KOFI_TOKEN,
    message_id: id,
    timestamp: "2026-09-25T14:31:00Z",
    type: "Donation",
    is_public: true,
    from_name: "Jo Example",
    message: "Good luck with the integration!",
    amount: "3.00",
    url: "https://ko-fi.com/Home/CoffeeShop?txid=00000000-1111-2222-3333-444444444444",
    email: "jo.example@example.com",
    currency: "USD",
    is_subscription_payment: false,
    is_first_subscription_payment: false,
    kofi_transaction_id: "00000000-1111-2222-3333-444444444444",
    shop_items: null,
    tier_name: null,
    shipping: null,
    ...extra,
  };
}

function postKofi(data: unknown): Promise<Response> {
  return SELF.fetch(`${BASE}/hooks/kofi`, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({ data: JSON.stringify(data) }).toString(),
  });
}

/** A WebSocket client that records every frame; `until` resolves on the frame/close event that satisfies it. */
class Client {
  frames: Frame[] = [];
  raw: string[] = [];
  closed: { code: number; reason: string } | null = null;
  private waiters: { pred: () => boolean; resolve: () => void }[] = [];
  constructor(readonly ws: WebSocket) {
    ws.accept();
    ws.addEventListener("message", (e) => {
      const d = typeof e.data === "string" ? e.data : "";
      this.raw.push(d);
      try {
        this.frames.push(JSON.parse(d) as Frame);
      } catch {
        // bare "pong"
      }
      this.wake();
    });
    ws.addEventListener("close", (e) => {
      this.closed = { code: e.code, reason: e.reason };
      this.wake();
    });
  }
  private wake(): void {
    this.waiters = this.waiters.filter((w) => {
      if (!w.pred()) return true;
      w.resolve();
      return false;
    });
  }
  send(f: unknown): void {
    this.ws.send(typeof f === "string" ? f : JSON.stringify(f));
  }
  of(t: string): Frame[] {
    return this.frames.filter((f) => f.t === t);
  }
  /** Wait (bounded by the test timeout) until `pred` holds. */
  until(pred: () => boolean): Promise<void> {
    if (pred()) return Promise.resolve();
    const { promise, resolve } = Promise.withResolvers<void>();
    this.waiters.push({ pred, resolve });
    return promise;
  }
  /**
   * Round trip through the Durable Object: it handles one socket's frames in order, so once
   * the error reply to this non-JSON frame arrives, everything sent before it was processed.
   */
  async barrier(): Promise<void> {
    const n = this.of("error").length;
    this.send("barrier");
    await this.until(() => this.of("error").length > n);
  }
}

async function engine(secret = SECRET): Promise<Client> {
  const res = await SELF.fetch(`${BASE}/link`, { headers: { upgrade: "websocket", authorization: `Bearer ${secret}` } });
  expect(res.status).toBe(101);
  const c = new Client(res.webSocket as WebSocket);
  c.send({ t: "hello", v: 1, engine: "test" });
  await c.until(() => c.of("welcome").length > 0);
  return c;
}

async function viewer(): Promise<Client> {
  const res = await SELF.fetch(`${BASE}/queue/ws`, { headers: { upgrade: "websocket" } });
  expect(res.status).toBe(101);
  const c = new Client(res.webSocket as WebSocket);
  await c.until(() => c.of("queue").length > 0);
  return c;
}

async function pending(): Promise<string[]> {
  return runInDurableObject(relayStub(env), (_obj, state) =>
    state.storage.sql
      .exec<{ message_id: string }>("SELECT message_id FROM kofi_pending ORDER BY received_at, rowid")
      .toArray()
      .map((r) => r.message_id),
  );
}

beforeEach(async () => {
  await runInDurableObject(relayStub(env), (_obj, state) => {
    for (const ws of state.getWebSockets()) {
      try {
        ws.close(1000, "test reset");
      } catch {
        // closed
      }
    }
    state.storage.sql.exec("DELETE FROM kofi_seen; DELETE FROM kofi_pending; DELETE FROM kv;");
  });
});

describe("router", () => {
  it("404s unknown paths, 405s wrong methods, redirects /", async () => {
    expect((await SELF.fetch(`${BASE}/nope`)).status).toBe(404);
    const r = await SELF.fetch(`${BASE}/queue`, { method: "POST" });
    expect(r.status).toBe(405);
    expect(r.headers.get("allow")).toBe("GET");
    const home = await SELF.fetch(`${BASE}/`, { redirect: "manual" });
    expect(home.status).toBe(302);
    expect(home.headers.get("location")).toBe(`${BASE}/queue`);
    expect((await SELF.fetch(`${BASE}/hooks/kofi`)).status).toBe(405);
  });

  it("serves the queue page with a strict CSP and escaped title", async () => {
    const r = await SELF.fetch(`${BASE}/queue`);
    expect(r.status).toBe(200);
    expect(r.headers.get("content-security-policy")).toContain("default-src 'none'");
    const html = await r.text();
    expect(html).toContain("Drums &amp; &lt;Requests&gt;");
    expect(html).not.toContain("<Requests>");
    expect((await SELF.fetch(`${BASE}/queue.js`)).headers.get("content-type")).toContain("javascript");
    expect((await SELF.fetch(`${BASE}/queue/ws`)).status).toBe(426);
  });
});

describe("Ko-fi webhook", () => {
  it("verifies the token and never stores rejected payloads", async () => {
    const bad = await postKofi(kofiData("m-bad", { verification_token: "wrong" }));
    expect(bad.status).toBe(401);
    expect((await postKofi({ message_id: "x" })).status).toBe(401);
    const noData = await SELF.fetch(`${BASE}/hooks/kofi`, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: "other=1",
    });
    expect(noData.status).toBe(400);
    const notJson = await SELF.fetch(`${BASE}/hooks/kofi`, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: "data=%7Bnope",
    });
    expect(notJson.status).toBe(400);
    const wrongType = await SELF.fetch(`${BASE}/hooks/kofi`, { method: "POST", headers: { "content-type": "text/plain" }, body: "x" });
    expect(wrongType.status).toBe(415);
    expect(await pending()).toEqual([]);
  });

  it("dedupes on message_id", async () => {
    const a = await postKofi(kofiData("m-1"));
    expect(a.status).toBe(200);
    expect(await a.json()).toEqual({ status: "queued" });
    const b = await postKofi(kofiData("m-1", { amount: "999.00" }));
    expect(b.status).toBe(200);
    expect(await b.json()).toEqual({ status: "duplicate" });
    expect(await pending()).toEqual(["m-1"]);
  });

  it("buffers a bounded number while the engine is offline and reports drops", async () => {
    for (const id of ["b-1", "b-2", "b-3", "b-4", "b-5"]) expect((await postKofi(kofiData(id))).status).toBe(200);
    // KOFI_BUFFER_MAX = 3 in vitest.config.ts: the two oldest are dropped
    expect(await pending()).toEqual(["b-3", "b-4", "b-5"]);
    const e = await engine();
    expect(e.of("welcome")[0]).toMatchObject({ v: 1, pending: 3, dropped: 2 });
    await e.until(() => e.of("kofi").length === 3);
    expect(e.of("kofi").map((f) => f.id)).toEqual(["b-3", "b-4", "b-5"]);
    // not acknowledged yet → still buffered
    expect(await pending()).toHaveLength(3);
    e.send({ t: "ack", ids: ["b-3", "b-4"] });
    await e.barrier();
    expect(await pending()).toEqual(["b-5"]);
    e.ws.close(1000);
    // reconnect: the unacknowledged one is redelivered, drop counter was reset
    const e2 = await engine();
    expect(e2.of("welcome")[0]).toMatchObject({ pending: 1, dropped: 0 });
    await e2.until(() => e2.of("kofi").length === 1);
    expect(e2.of("kofi")[0]?.id).toBe("b-5");
  });

  it("delivers live payments without the token, e-mail or shipping", async () => {
    const e = await engine();
    const r = await postKofi(kofiData("live-1", { shipping: { street: "1 Main St" } }));
    expect(await r.json()).toEqual({ status: "delivered" });
    await e.until(() => e.of("kofi").length === 1);
    const f = e.of("kofi")[0] as { data: Record<string, unknown> };
    expect(f.data).toMatchObject({ message_id: "live-1", amount: "3.00", currency: "USD", from_name: "Jo Example", is_public: true, type: "Donation" });
    expect(f.data).not.toHaveProperty("verification_token");
    expect(f.data).not.toHaveProperty("email");
    expect(f.data).not.toHaveProperty("shipping");
  });

  it("parses form bodies directly", async () => {
    const req = new Request(`${BASE}/hooks/kofi`, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({ data: JSON.stringify(kofiData("p-1")) }).toString(),
    });
    const p = await parseKofi(req, KOFI_TOKEN);
    expect(p.ok && p.payment.message_id).toBe("p-1");
    const unconfigured = await parseKofi(new Request(`${BASE}/x`, { method: "POST", body: "" }), undefined);
    expect(unconfigured).toMatchObject({ ok: false, status: 503 });
  });
});

describe("engine link", () => {
  it("requires the shared secret and a WebSocket upgrade", async () => {
    expect((await SELF.fetch(`${BASE}/link`, { headers: { upgrade: "websocket" } })).status).toBe(401);
    expect((await SELF.fetch(`${BASE}/link`, { headers: { upgrade: "websocket", authorization: "Bearer nope" } })).status).toBe(401);
    expect((await SELF.fetch(`${BASE}/link`, { headers: { authorization: `Bearer ${SECRET}` } })).status).toBe(426);
  });

  it("answers ping with pong and rejects other protocol versions", async () => {
    const e = await engine();
    e.send("ping");
    await e.until(() => e.raw.includes("pong"));
    const res = await SELF.fetch(`${BASE}/link`, { headers: { upgrade: "websocket", authorization: `Bearer ${SECRET}` } });
    const old = new Client(res.webSocket as WebSocket);
    old.send({ t: "hello", v: 99 });
    await old.until(() => old.closed !== null);
    expect(old.closed?.code).toBe(4002);
  });

  it("relays queue snapshots to viewers and shows offline when the engine leaves", async () => {
    const v = await viewer();
    expect(v.of("queue")[0]).toMatchObject({ online: false, snapshot: null });
    const e = await engine();
    await v.until(() => v.of("status").some((f) => f.online === true));
    const snapshot = { v: 1, open: true, paused: false, now: { title: "Song A", user: "drumfan42", video: "dQw4w9WgXcQ", duration: 213, position: 10, playing: true, at: 1 }, upcoming: [{ pos: 1, title: "Song B", user: "x", duration: 180 }], length: 1 };
    e.send({ t: "queue", snapshot });
    await v.until(() => v.of("queue").length === 2);
    expect(v.of("queue")[1]).toMatchObject({ online: true, snapshot });
    expect(typeof v.of("queue")[1]?.now).toBe("number");
    const js = (await (await SELF.fetch(`${BASE}/queue.json`)).json()) as { online: boolean; snapshot: unknown };
    expect(js).toMatchObject({ online: true, snapshot });
    e.ws.close(1000, "bye");
    await v.until(() => v.of("status").some((f) => f.online === false));
    const late = await viewer();
    expect(late.of("queue")[0]).toMatchObject({ online: false, snapshot });
  });

  it("replaces an older engine connection", async () => {
    const a = await engine();
    const b = await engine();
    await a.until(() => a.closed !== null);
    expect(a.closed?.code).toBe(4001);
    expect(await relayStub(env).engineConnected()).toBe(true);
    await postKofi(kofiData("to-b"));
    await b.until(() => b.of("kofi").length === 1);
  });

  it("drops oversized frames", async () => {
    const e = await engine();
    e.send({ t: "queue", snapshot: { pad: "x".repeat(300 * 1024) } });
    await e.until(() => e.closed !== null);
    expect(e.closed?.code).toBe(1009);
  });

  it("carries request/response calls for extensions", async () => {
    const stub = relayStub(env);
    expect(await stub.engineCall("mod", { op: "x" })).toEqual({ ok: false, error: "engine offline" });
    const e = await engine();
    const call = stub.engineCall("mod", { op: "queue.skip" }, 2000);
    await e.until(() => e.of("mod.req").length === 1);
    const req = e.of("mod.req")[0] as { id: string; body: unknown };
    expect(req.body).toEqual({ op: "queue.skip" });
    e.send({ t: "mod.res", id: req.id, ok: true, result: { done: 1 } });
    expect(await call).toEqual({ ok: true, result: { done: 1 } });
    const failing = stub.engineCall("mod", {}, 2000);
    await e.until(() => e.of("mod.req").length === 2);
    e.send({ t: "mod.res", id: (e.of("mod.req")[1] as { id: string }).id, ok: false, error: "not a mod" });
    expect(await failing).toEqual({ ok: false, error: "not a mod" });
    expect(await stub.engineCall("Bad Kind", {})).toEqual({ ok: false, error: "bad call kind" });
  });
});
