# Public queue and optional relay (operator notes)

The public song queue can run entirely on the streaming machine. A tunnel supplies HTTPS
reachability; no Worker deployment, off-machine queue backend, or always-on machine is
required. The optional Cloudflare Worker backend also lives in `relay/` and shares the
same public page and Twitch-authenticated queue management controls.

## Local machine hosting

The local Node service uses two separate **loopback-only** listeners:

| Listener | Routes | Access |
|---|---|---|
| `127.0.0.1:8787` | `GET /`, `/queue`, `/queue.js`, `/queue.json`, `/queue/ws` | Read-only public page/data; `/` redirects to `/queue`. Tunnel only this listener. |
| `127.0.0.1:8787` | `/mod/login`, `/mod/callback`, `/mod/session`, `/mod/logout`, `/mod/api` | Optional Twitch login and authenticated queue-only management; `/mod` returns to `/queue#moderator`. |
| `127.0.0.1:8788` | `GET /link` WebSocket upgrade | Private engine link; bearer shared secret required. Never tunnel this listener. |

The queue page stays public and read-only until an authorized user opens its optional
**Moderator login** section. Ordinary viewers need no account, receive no login prompt,
and make no moderator requests while the section is closed. Public WebSocket messages
other than `ping` close the connection. Only approved public snapshot fields are forwarded:
page theme, request availability, pause state, current song, upcoming titles/requesters/durations,
and queue length. Account identity, pending requests, history and quota are private.
The local service does not expose the general operator API, player, moderator console,
or Ko-fi ingress. The page does not embed or play videos.

### Optional moderator login

Open **Moderator login → Sign in with Twitch** to manage songs on the same queue page.
The broadcaster and verified channel moderators can add a song name or YouTube link,
move waiting songs with **Up / Down** or the drag handle, remove requests, and approve
or reject pending requests. Pending requests must be approved before reordering.
The current song and playback controls are not changed by this panel. Changes appear
through the existing public live updates.
The moderation panel does not enable song approval. On this workstation, queue policy
`approval = off`: valid viewer requests enter the waiting queue without moderator approval.

Local setup:

1. Set `QUEUE_PUBLIC_ORIGIN` to the exact HTTPS queue origin in the queue service environment
   (on this workstation, `https://queue.dabsanddrums.com`).
2. Register `https://queue.dabsanddrums.com/mod/callback` as an **OAuth Redirect URL**
   for the Twitch application identified by `[twitch] client_id`. Other installations
   use their own exact queue origin followed by `/mod/callback`.
   Add the queue callback alongside any existing localhost callback; do not replace
   callbacks used by the engine's other authorization flows.
3. Enable `[remote_mod]` and restrict its actions to the intended queue controls:

   ```toml
   [remote_mod]
   enabled = true
   actions = ["queue.request", "queue.reorder", "queue.remove", "queue.approve", "queue.reject"]
   ```

The existing Twitch token validation, channel-moderator lookup, one-use OAuth state,
token revocation, and HMAC-signed HttpOnly session are reused. Every private queue read
and command is authorized again against the engine settings; the authenticated actor,
not browser-supplied user fields, determines attribution. Sessions expire after 12 hours
by default (`MOD_SESSION_HOURS`); **Sign out** removes the browser's session cookie.
Local routes additionally reject all other commands and private queries, even for the
broadcaster. Public viewers cannot mutate the queue by calling the API directly.

OAuth callbacks use the configured origin, not untrusted forwarded host headers.
A mismatched Host or forwarded protocol returns 403; correct `QUEUE_PUBLIC_ORIGIN`
and the tunnel configuration rather than weakening the origin check. Without that
setting, moderator routes accept only direct loopback HTTP hosts. A changing Quick
Tunnel hostname needs a matching origin setting and Twitch callback registration.
If Twitch sends the browser to `localhost/?error=redirect_mismatch`, its application
does not have the exact requested callback registered. In the Twitch developer console,
manage the app matching `twitch.client_id`, add the queue HTTPS callback and save, then
start a fresh sign-in from the queue. Do not run a localhost server or change the
queue callback to localhost to work around the registration mismatch.
An offline/replaced engine fails outstanding requests instead of replaying mutations
on reconnection. Command acknowledgement means submitted: YouTube lookup and queue
policy can still reject an addition. Watch the refreshed queue and engine song-request
logs for the final outcome.

Verification: `npm run typecheck`, `npm test`, and `npm run build-local`. The deployed
page was exercised anonymously and with a short-lived signed operator verification
session: add, remove, up/down, native drag/drop, approve, reject, logout, queue-only
permission denial, and mobile rendering in both themes. The original waiting order,
current song, paused state, approval policy, and theme were restored. After registering
the missing callback in the existing Twitch app, real broadcaster authorization was
completed in Brave: Twitch consent returned to `/queue#moderator`, which displayed
**Moderator access authorized** and **Signed in as dabsanddrums**.

### Page design and album art

The page defaults to a Windows 3.1 theme: teal desktop, grey beveled windows, navy
title bars, and an inset song list. In **Song requests → YouTube & web → Public song
list & tips**, turn **Windows 3.1 queue theme** off to restore the modern dark page
with artwork-driven accent colors and a blurred background. The choice is saved in
the runtime DB and updates connected viewers immediately, without a page reload.
It changes only the public page, not the Stream Engine UI or song playback.

Both themes show a large now-playing card with album art, artist, album, requester,
and progress. The upcoming list shows artwork, requester, duration, and estimated
wait. Both adapt for desktop and phone and honor reduced-motion settings; retro
window chrome is decorative, not a set of player controls.

CLI: `streamctl do queue.theme.set theme=win31` enables the theme;
`streamctl do queue.theme.set theme=modern` disables it. `queue.theme` reports the
saved choice; the `queue` query and public WebSocket snapshots include `theme`.

The local service adds album art after validating each engine snapshot. It searches the
iTunes Search API using only public YouTube titles and channels, then accepts a result
only when both artist and song match. The title must be `Artist - Song`, or contain the
artist and song together, or come from the artist's own `VEVO`/`- Topic` channel. Record
label channels never count as artists. Lookups are serialized, cached in memory, and
retried after failures. Missing or rejected matches fall back to YouTube thumbnails.
The Worker relay uses the same page and its thumbnail fallback; it does not run lookups.

Optional page settings:

| Setting | Effect |
|---|---|
| `QUEUE_TITLE` | Page heading; defaults to `Song queue`. |
| `QUEUE_CHANNEL` | Twitch login for the header link and **Open chat** button. |
| `QUEUE_PUBLIC_ORIGIN` | Exact HTTPS tunnel origin for local moderator routes and Twitch callbacks. |

This workstation sets `QUEUE_CHANNEL=dabsanddrums` in `stream-engine-queue.service`.
`npm run build-local` also verifies that the embedded browser script parses.

Build with Node 22+:

```sh
cd relay
npm ci
npm run build-local
```

`npm run start-local` starts the built service. It requires the matching `RELAY_SECRET`
from the environment, or a systemd credential named `RELAY_SECRET` under
`$CREDENTIALS_DIRECTORY`. On this workstation, the engine keeps its copy in the keyring
and `stream-engine-queue.service` loads a user-encrypted credential from
`~/.config/stream-engine/credentials/queue-relay.cred`. Do not print a secret, commit it,
or put it in process arguments/shell history. Set a new engine secret through
**Song requests → YouTube & web**, then encrypt the same bytes for the local service
with `systemd-creds encrypt --user --name=RELAY_SECRET`.

The engine's `project.toml` uses the private link:

```toml
[relay]
url = "ws://127.0.0.1:8788/link"
# queue_url is maintained by the local tunnel runner through project.write.
```

On this workstation, the user services and engine drop-in are installed under
`~/.config/systemd/user/`:

- `stream-engine.service.d/queue.conf` starts `stream-engine-queue.service` with the engine.
- `stream-engine-queue.service` is bound to the engine and starts the tunnel service.
- `stream-engine-queue-tunnel.service` is bound to the queue service and runs
  `node relay/src/tunnel.mjs`.

Stopping the engine stops both services. Stopping the queue service also stops the
tunnel. These services follow engine lifetime, not an OBS stream-state switch.

On boot, SDDM auto-login starts the graphical session, which starts the engine, the
queue service, and the tunnel. All three use `Restart=always` with
`StartLimitIntervalSec=0` and backoff of up to 30–60 s, so crashes, a late network,
or a Cloudflare outage are retried indefinitely. `loginctl` lingering is enabled for this user.

```sh
systemctl --user start stream-engine-queue.service
streamctl query relay.status
streamctl get queue.url
systemctl --user stop stream-engine-queue.service
```

#### Public page health (`health.queue_page`)

`health.relay` only proves the engine's link to the queue service. The engine also checks
what viewers see: every 60 s (first check 15 s after start, 10 s timeout) it fetches
`<origin of queue.url>/queue.json` through the public hostname, tunnel and DNS included.
The check runs on its own task and never delays queue handling; `queue.url` is re-read on
every check, so a new Quick Tunnel address is picked up by itself.

| Status | Detail (example) | Meaning |
|---|---|---|
| pass | `public queue page reachable (queue.dabsanddrums.com)` | HTTP 200 with `online: true`. |
| pass | `public queue page not configured` | `queue.url` is empty or not `https://`: nothing public to check. |
| warn | `public queue page (…) is up but shows the stream as offline: the engine's link to the queue service is down (see the relay check); song requests in chat still work` | The page answers with `online: false`. Check `health.relay`. |
| warn | `public queue page (…) did not answer: <reason>; checking again in a minute; song requests in chat still work` | One or two failed checks in a row (timeout, connection/DNS failure, HTTP error such as 530 from Cloudflare, or a page that isn't the queue). |
| fail | `viewers cannot open the public queue page (…): <reason>, N checks in a row; check the tunnel or DNS (stream-engine-queue-tunnel.service, stream-engine-queue.service); song requests in chat still work` | Three or more failed checks in a row. |

The engine log (`relay` target) gets one line per status change, not one per check. Live-safe
recovery is restarting only the tunnel or queue helper (`systemctl --user restart
stream-engine-queue-tunnel.service`, then `stream-engine-queue.service` if the page still
fails); neither touches the stream, the player, or chat requests.

### Tunnel address and custom domain

The default runner uses a foreground **Cloudflare Quick Tunnel** to
`http://127.0.0.1:8787`. This is a tunnel only, not a Worker; it needs no Cloudflare
account/login, domain purchase, DNS change, or inbound router port. Its generated
HTTPS hostname changes on restart. The runner updates `relay.queue_url` using the
engine's existing comment-preserving `project.write` action and waits for `queue.url`
to reflect it, so chat commands follow the current address.

[Quick Tunnels](https://developers.cloudflare.com/tunnel/get-started/quick-tunnels/)
have no uptime guarantee and a 200 in-flight-request limit; they are documented as
testing/development tunnels. A stable custom domain requires an authorized named
tunnel and DNS/hostname provisioning, not a CNAME to a random Quick Tunnel hostname.
GoDaddy can remain the registrar. Do not change nameservers or existing DNS records
without owner authorization.

For an already authorized named tunnel, configure its **only** public origin as
`http://127.0.0.1:8787`, provide `TUNNEL_TOKEN_FILE` pointing at a protected runtime
credential, and set `QUEUE_PUBLIC_URL` to its HTTPS `/queue` URL in the tunnel service's
drop-in. Both settings are required together. The token is read through
`cloudflared --token-file`, never placed in an argument. Never configure a public
origin for the operator API, desktop, or private engine link.

This workstation uses **https://queue.dabsanddrums.com/queue** on the Cloudflare
Free plan. Registration stays at GoDaddy; authoritative DNS uses
`piper.ns.cloudflare.com` and `yisroel.ns.cloudflare.com`. The existing apex A records
and `www` CNAME were retained. `_domainconnect` stays DNS-only. A Cloudflare redirect
rule preserves the apex and `www` permanent (301) redirect to
`http://twitch.tv/dabsanddrums`; it does not match the queue subdomain.

The named tunnel is `stream-engine-queue`
(`2eabbdc1-b977-46ce-a4dc-5d7d82e0ef2c`). Its ingress routes only
`queue.dabsanddrums.com` to `http://127.0.0.1:8787`, with a catch-all 404.
`stream-engine-queue-tunnel.service.d/domain.conf` loads the user-encrypted
`~/.config/stream-engine/credentials/queue-tunnel.cred` as `TUNNEL_TOKEN`, sets
`TUNNEL_TOKEN_FILE=%d/TUNNEL_TOKEN`, and publishes the stable HTTPS queue URL.
No Worker, router forwarding, or off-machine queue backend is deployed. Restarting
the tunnel preserves the address; the page is unavailable while the engine/queue
service is stopped. Queue hosting does not open requests, resume playback, or
verify the embedded YouTube account.

Do not add Funnel publication to this workstation's existing Tailscale Serve ports:
443, 8443, and 10000 already serve private services. Funnel publication is port-wide,
not isolated by `--set-path`. Those private routes must remain untouched.

### Chat link

In `commands/songs.toml`:

```toml
[[command]]
name = "!songlist"
aliases = ["!songqueue"]
role = "everyone"
cooldown = { global = "10s" }
reply = "Song queue: {queue.url}"
```

The command hot-reloads. Keep `!queue` status and mod open/close controls separate.

## Optional Cloudflare Worker backend

This alternative deploys a Worker plus one SQLite-backed Durable Object off-machine
(Workers Free plan, WebSocket Hibernation). It is **not** needed for local tunneled
queue hosting. Its additional Ko-fi/mod routes are:

| Route | What |
|---|---|
| `GET /queue` | Public song-queue page: now playing, upcoming, requester, position. Live over `/queue/ws`; shows **offline** while the engine isn't connected. `!songlist` links here (`queue.url`). |
| `GET /queue.json` | The same data as JSON (`{online, snapshot, updated_at}`). |
| `POST /hooks/kofi` | Ko-fi webhook. Verifies `verification_token`, dedupes `message_id`, buffers up to `KOFI_BUFFER_MAX` payments while the engine is offline. |
| `GET /link` | The engine's outbound WebSocket (`Authorization: Bearer <RELAY_SECRET>`). Nothing listens publicly on the streaming machine. |
| `/mod` | Remote mod console (Extras slice, `src/mod.ts`). |
| `GET /health` | `{ok, engine}` |

### Engine link protocol

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

### Deploy (owner, once)

Needs: a Cloudflare account and Node (for `npx wrangler`). A free `workers.dev` address
does not require a domain; a custom hostname additionally needs an authorized domain.

```sh
cd relay
npm ci
npx wrangler login                      # opens the browser once
# 1. attach the Worker to the domain: in wrangler.jsonc uncomment and edit
#    "routes": [{ "pattern": "relay.<your-domain>", "custom_domain": true }]
# 2. secrets (never in files):
npx wrangler secret put RELAY_SECRET              # enter the shared secret interactively
npx wrangler secret put KOFI_VERIFICATION_TOKEN   # from Ko-fi → Settings → API → Webhooks
# 3. deploy
npm test && npx wrangler deploy
```

Then enter the same shared secret on the streaming machine through
**Song requests → YouTube & web**. It lives in the keyring as `relay.secret`. Prefer
the UI to secret-bearing CLI arguments: engine redaction does not erase shell history
or hide process arguments.

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

### Worker local development (no Cloudflare account needed)

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

### Extending the Worker router

Routes are the `ROUTES` table in `src/index.ts` (`{method?, path | prefix, handler}`; first
match wins, other methods on a known path get 405). Extensions talk to the engine through the
Durable Object RPC `relayStub(env).engineCall(kind, body, timeoutMs?)`, which sends
`{t: kind + ".req", id, body}` over the link and resolves with the engine's
`{t: kind + ".res", id, ok, result|error}` (`{ok:false, error:"engine offline"}` when not
connected). The engine side maps `mod.req` to the `remote_mod.request` query.
