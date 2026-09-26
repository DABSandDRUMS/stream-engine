# Cloudflare relay (operator notes)

The relay (PLAN §14.6) is the only code stream-engine runs off this machine: a Cloudflare
Worker plus one SQLite-backed Durable Object (Workers Free plan, WebSocket Hibernation). It
lives in `relay/` (TypeScript).

| Route | What |
|---|---|
| `GET /queue` | Public song-queue page: now playing, upcoming, requester, position. Live over `/queue/ws`; shows **offline** while the engine isn't connected. `!queue` links here (`queue.url`). |
| `GET /queue.json` | The same data as JSON (`{online, snapshot, updated_at}`). |
| `POST /hooks/kofi` | Ko-fi webhook. Verifies `verification_token`, dedupes `message_id`, buffers up to `KOFI_BUFFER_MAX` payments while the engine is offline. |
| `GET /link` | The engine's outbound WebSocket (`Authorization: Bearer <RELAY_SECRET>`). Nothing listens publicly on the streaming machine. |
| `/mod` | Remote mod console (Extras slice, `src/mod.ts`). |
| `GET /health` | `{ok, engine}` |

## Engine link protocol

JSON text frames with a `t` field; bare `ping` → `pong` keepalive is answered by the runtime
without waking the Durable Object.

- engine → relay: `hello {v:1, engine}`, `queue {snapshot}`, `ack {ids}`, `<kind>.res {id, ok, result|error}`
- relay → engine: `welcome {v:1, pending, dropped}`, `kofi {id, received_at, data}`, `<kind>.req {id, body}`, `error {msg}`

Ko-fi payments stay buffered until the engine acknowledges them (`ack`), so a payment that
arrives while the engine is offline — or while the link drops mid-delivery — is delivered on the
next connection. The engine dedupes again on `message_id` and emits a `tip` event (same payload
as `streamctl sim tip`); private supporters (`is_public: false`) arrive as "Anonymous" with no
message. E-mail, shipping address and the verification token never leave the relay.

Only one engine connection is active; a new one replaces the old (close code 4001).

## Deploy (owner, once)

Needs: a Cloudflare account with the domain added, and Node (for `npx wrangler`).

```sh
cd relay
npm ci
npx wrangler login                      # opens the browser once
# 1. attach the Worker to the domain: in wrangler.jsonc uncomment and edit
#    "routes": [{ "pattern": "relay.<your-domain>", "custom_domain": true }]
# 2. secrets (never in files):
openssl rand -hex 32 | tee /dev/stderr | npx wrangler secret put RELAY_SECRET
npx wrangler secret put KOFI_VERIFICATION_TOKEN   # from Ko-fi → Settings → API → Webhooks
# 3. deploy
npm test && npx wrangler deploy
```

Then on the streaming machine:

```sh
streamctl do relay.secret.set <the same RELAY_SECRET>   # keyring `relay.secret`; redacted from logs
```

and in the project's `project.toml`:

```toml
[relay]
url = "wss://relay.<your-domain>/link"
```

The engine connects within a second (hot reload); preflight shows `health.relay` = pass
("connected to relay.<your-domain>"). `queue.url` becomes `https://relay.<your-domain>/queue`.

### Ko-fi

Ko-fi → Settings → API → Webhooks: set the URL to `https://relay.<your-domain>/hooks/kofi`,
copy the verification token into the `KOFI_VERIFICATION_TOKEN` secret, and press
**Send Test** — a `tip` event appears in the UI's event feed and fires the tip alert.

## Local development (no Cloudflare account needed)

```sh
cd relay
cp .dev.vars.example .dev.vars          # RELAY_SECRET / KOFI_VERIFICATION_TOKEN for wrangler dev
npm run dev                             # wrangler dev on http://127.0.0.1:8787
npm test                                # vitest in the Workers runtime (workerd)
npm run typecheck
```

Point a dev engine at it with `[relay] url = "ws://127.0.0.1:8787/link"` (plain `ws://` is
only accepted for localhost) and `streamctl do relay.secret.set <RELAY_SECRET from .dev.vars>`.
Simulate a Ko-fi payment:

```sh
curl -s http://127.0.0.1:8787/hooks/kofi \
  --data-urlencode 'data={"verification_token":"<KOFI_VERIFICATION_TOKEN>","message_id":"'$(uuidgen)'","type":"Donation","is_public":true,"from_name":"Jo Example","message":"Good luck!","amount":"3.00","currency":"USD"}'
```

## Extending the router

Routes are the `ROUTES` table in `src/index.ts` (`{method?, path | prefix, handler}`; first
match wins, other methods on a known path get 405). Extensions talk to the engine through the
Durable Object RPC `relayStub(env).engineCall(kind, body, timeoutMs?)`, which sends
`{t: kind + ".req", id, body}` over the link and resolves with the engine's
`{t: kind + ".res", id, ok, result|error}` (`{ok:false, error:"engine offline"}` when not
connected). The engine side maps `mod.req` to the `remote_mod.request` query.
