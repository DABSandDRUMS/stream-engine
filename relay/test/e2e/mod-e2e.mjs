#!/usr/bin/env node
// End-to-end check of remote mod access: a fake Twitch OAuth provider + `wrangler dev` + a
// running dev engine linked to it. Drives the browser flow (login → Twitch authorize →
// callback fragment → session), then console calls, and verifies the effect in the engine
// through the `streamctl` CLI.
//
//   node test/e2e/mod-e2e.mjs --relay http://127.0.0.1:8787 --client-id fake-client \
//        --broadcaster-id 1000 --stream "streamctl --socket /tmp/se-extras.sock"
//
// wrangler dev must run with TWITCH_ID_BASE/TWITCH_API_BASE pointing at the fake provider
// (default http://127.0.0.1:8799), e.g.
//   npx wrangler dev --port 8787 --var TWITCH_ID_BASE:http://127.0.0.1:8799 --var TWITCH_API_BASE:http://127.0.0.1:8799
import { execSync } from "node:child_process";
import { parseArgs } from "node:util";
import { startFakeTwitch } from "./fake-twitch.mjs";

const { values: a } = parseArgs({
  options: {
    relay: { type: "string", default: "http://127.0.0.1:8787" },
    "fake-port": { type: "string", default: "8799" },
    "client-id": { type: "string" },
    "broadcaster-id": { type: "string" },
    stream: { type: "string", default: "streamctl" },
  },
});
if (!a["client-id"] || !a["broadcaster-id"]) {
  console.error("--client-id and --broadcaster-id must match the engine's twitch.client_id / twitch.broadcaster.id");
  process.exit(2);
}
const relay = a.relay.replace(/\/+$/, "");
const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  — ${detail}` : ""}`);
}
const engine = (args) => execSync(`${a.stream} ${args}`, { encoding: "utf8" }).trim();

const fake = await startFakeTwitch({
  port: Number(a["fake-port"]),
  clientId: a["client-id"],
  users: [
    { login: "modperson", id: "4242", moderates: ["77", a["broadcaster-id"]] },
    { login: "randomviewer", id: "5555", moderates: ["77"] },
  ],
});

async function signIn(login) {
  const l = await fetch(`${relay}/mod/login`, { redirect: "manual" });
  if (l.status !== 302) return { status: l.status, text: await l.text() };
  const stateCookie = l.headers.getSetCookie().find((c) => c.startsWith("se_mod_state=")).split(";")[0];
  const authorize = new URL(l.headers.get("Location"));
  authorize.searchParams.set("login", login);
  const tw = await fetch(authorize, { redirect: "manual" });
  const cb = new URL(tw.headers.get("Location"));
  const cbPage = await fetch(cb.origin + cb.pathname);
  const frag = new URLSearchParams(cb.hash.slice(1));
  const s = await fetch(`${relay}/mod/session`, {
    method: "POST",
    headers: { Origin: new URL(relay).origin, "X-SE-Mod": "1", "Content-Type": "application/json", Cookie: stateCookie },
    body: JSON.stringify({ access_token: frag.get("access_token"), state: frag.get("state") }),
  });
  const session = s.headers.getSetCookie().find((c) => c.startsWith("se_mod="))?.split(";")[0];
  return { status: s.status, body: await s.json(), session, authorize, callbackStatus: cbPage.status };
}

async function api(session, body) {
  const r = await fetch(`${relay}/mod/api`, {
    method: "POST",
    headers: { Origin: new URL(relay).origin, "X-SE-Mod": "1", "Content-Type": "application/json", ...(session ? { Cookie: session } : {}) },
    body: JSON.stringify(body),
  });
  return { status: r.status, body: await r.json() };
}

try {
  const idx = await (await fetch(`${relay}/mod`)).text();
  check("sign-in page offers Twitch login (engine linked)", idx.includes('href="/mod/login"'));

  const mod = await signIn("modperson");
  check("login redirects to the Twitch authorize endpoint", mod.authorize?.pathname === "/oauth2/authorize" && mod.authorize.searchParams.get("client_id") === a["client-id"]);
  check("callback page served", mod.callbackStatus === 200);
  check("moderator gets a session", mod.status === 200 && !!mod.session, JSON.stringify(mod.body));
  check("Twitch token revoked after verification", fake.revoked.length === 1);

  const st = await api(mod.session, { kind: "state" });
  check("console state comes from the engine", st.status === 200 && st.body.ok && Array.isArray(st.body.result.actions), JSON.stringify(st.body).slice(0, 160));

  engine("do giveaway.reset");
  const open = await api(mod.session, { kind: "cmd", action: "giveaway.open", args: { title: "e2e drumsticks", keyword: "!e2e" } });
  check("allowed command runs", open.status === 200 && open.body.ok, JSON.stringify(open.body));
  await new Promise((r) => setTimeout(r, 400));
  const gstate = engine("get giveaway.state");
  check("engine state changed by the moderator's command", gstate.includes("open"), gstate);
  await new Promise((r) => setTimeout(r, 300));
  const active = engine("get remote_mod.active");
  check("engine lists the moderator as active", active.includes("modperson"), active);

  const lights = await api(mod.session, { kind: "cmd", action: "lights.cue", args: { cue: "blackout" } });
  check("mixer/lights/scenes refused", lights.status === 403 && /not available/.test(lights.body.error), JSON.stringify(lights.body));
  const scene = await api(mod.session, { kind: "cmd", action: "scene.go" });
  check("scene change refused", scene.status === 403);

  const viewer = await signIn("randomviewer");
  check("non-moderator refused", viewer.status === 403 && !viewer.session, JSON.stringify(viewer.body));

  const anon = await api(undefined, { kind: "state" });
  check("console API requires a session", anon.status === 401);
  engine("do giveaway.reset");
} catch (e) {
  check("run", false, e.stack);
} finally {
  await fake.close();
}
const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
