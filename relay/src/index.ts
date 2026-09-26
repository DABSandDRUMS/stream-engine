// stream-engine relay Worker (PLAN §14.6). Routes live in `ROUTES`; extensions (e.g. remote
// mod access at `/mod`) add their own entry here and talk to the engine through
// `relayStub(env).engineCall(kind, body)`.

import { type Env, relayStub } from "./env";
import { kofiWebhook } from "./kofi";
import { queuePage, queueScript } from "./queue-page";
import { handleMod } from "./mod";
import { Relay } from "./relay";
import { type Route, dispatch } from "./router";
import { bearer, isWebSocketUpgrade, json, safeEqual, text } from "./util";

export { Relay };

/** `GET /link`: the engine's outbound WebSocket, authenticated with the shared secret. */
async function engineLink(req: Request, env: Env): Promise<Response> {
  if (!env.RELAY_SECRET) return text("relay secret not configured (wrangler secret put RELAY_SECRET)", 503);
  const token = bearer(req);
  if (!token || !safeEqual(token, env.RELAY_SECRET)) return text("unauthorized", 401, { "www-authenticate": "Bearer" });
  if (!isWebSocketUpgrade(req)) return text("expected a WebSocket upgrade", 426);
  return relayStub(env).fetch(req);
}

export const ROUTES: Route[] = [
  { method: "GET", path: "/", handler: (_req, _env, _ctx, url) => Response.redirect(`${url.origin}/queue`, 302) },
  { method: "GET", path: "/queue", handler: (_req, env) => queuePage(env) },
  { method: "GET", path: "/queue.js", handler: () => queueScript() },
  { method: "GET", path: "/queue.json", handler: async (_req, env) => json(await relayStub(env).snapshot(), 200, { "access-control-allow-origin": "*" }) },
  {
    method: "GET",
    path: "/queue/ws",
    handler: (req, env) => (isWebSocketUpgrade(req) ? relayStub(env).fetch(req) : text("expected a WebSocket upgrade", 426)),
  },
  { method: "POST", path: "/hooks/kofi", handler: kofiWebhook },
  { method: "GET", path: "/link", handler: engineLink },
  { method: "GET", path: "/health", handler: async (_req, env) => json({ ok: true, engine: await relayStub(env).engineConnected() }) },
  { prefix: "/mod", handler: handleMod },
];

export default {
  fetch(req, env, ctx) {
    return dispatch(ROUTES, req, env, ctx);
  },
} satisfies ExportedHandler<Env>;
