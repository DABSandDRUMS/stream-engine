# Twitch and the policy layer (operator notes)

`se-twitch` connects the engine to Twitch (§11): OAuth device code login with tokens in the
keyring, EventSub over WebSocket, and Helix calls. What viewers' chat, bits, and channel points
may *do* is decided by the policy pipeline in the core (`se_core::policy`, §12.1).

## 1. One-time setup (owner)

1. Enable 2FA on the Twitch account, then open <https://dev.twitch.tv/console/apps> →
   **Register Your Application**:
   - Name: anything unique (e.g. `dabsanddrums-stream-engine`)
   - OAuth Redirect URLs: `http://localhost` (unused by the device flow, but required)
   - Category: *Other*; **Client Type: Public** (the device code flow needs a public client)
2. Copy the **Client ID** into the project's `project.toml`:

   ```toml
   [twitch]
   client_id = "abcd1234…"
   ```

   The engine reloads it live; `health.twitch` changes from "no Client ID" to "not authorized".
3. Authorize: in the UI **Twitch** view (or Settings) press *Authorize with Twitch*, or run
   `streamctl do twitch.auth.start`. The engine shows a code and a link
   (`twitch.auth.user_code`, `twitch.auth.verification_uri`, also logged):
   open <https://www.twitch.tv/activate>, sign in as the **broadcaster**, enter the code, and
   approve the listed permissions. Within a few seconds `twitch.auth.status` becomes
   `authorized`, EventSub connects, rewards are created, emotes load.
4. Optional bot account for chat replies: set `bot = true` (and `chat_as = "bot"`), then run
   `streamctl do twitch.auth.start account=bot` and approve while signed in as the bot account
   (use a private window). Make the bot a moderator in the channel for the higher chat rate
   limit (`chat_rate = 100`).

Refresh tokens live only in the keyring (`stream-engine` / `twitch.refresh_token`,
`twitch.bot_refresh_token`); access tokens are refreshed automatically and validated hourly.
`streamctl do twitch.auth.logout` forgets them.

Scopes requested (broadcaster): `user:read:chat user:write:chat bits:read
channel:read:subscriptions channel:read:hype_train channel:read:redemptions
channel:manage:redemptions channel:manage:polls channel:manage:predictions
channel:manage:broadcast channel:manage:raids channel:read:ads channel:manage:ads
channel:moderate moderator:read:followers moderator:manage:shoutouts
moderator:manage:banned_users moderator:manage:chat_messages moderator:manage:automod
moderator:manage:blocked_terms moderation:read channel:read:vips` (`channel:moderate` is
needed for the `channel.ban` / `channel.unban` EventSub types; `moderation:read` and
`channel:read:vips` read the moderator and VIP lists for the roles cache, §3). Bot:
`user:read:chat user:write:chat`. If a later version adds scopes, `twitch.auth.missing_scopes`
lists them and preflight warns: authorize again (until then the roles cache uses chat badges
only).

## 2. `[twitch]` reference

| key | default | meaning |
|---|---|---|
| `client_id` | — | app Client ID (required) |
| `bot` | `false` | authorize a bot account for chat |
| `chat_as` | `broadcaster` | default sender for `twitch.chat.send` |
| `sub_source` | `chat` | `chat`: subs/resubs/gifts from `channel.chat.notification` (gift recipients linked to their bomb exactly); `eventsub`: from `channel.subscribe`/`.subscription.*` (recipients attributed to the open bomb) |
| `emotes` | `["7tv","bttv","ffz"]` | third-party emote providers |
| `viewer_poll` / `ads_poll` | `30s` / `60s` | Helix polling |
| `ad_warning` | `60s` | `twitch.ad.upcoming {in_s, duration}` fires this long before a scheduled ad |
| `latency` | `2.5s` | viewer video latency estimate added to the chat → screen delay |
| `chat_rate` | `20` | messages per 30 s the sender allows itself |
| `sync_rewards` | `true` | create/update rewards from `rewards/*.toml` |
| `secrets` | `twitch` | keyring name prefix for the tokens |
| `eventsub_url`, `subscriptions_url`, `helix_url`, `auth_url` | Twitch | endpoints (point at mocks for testing, §6) |

## 3. Events (normalized, same shapes as the simulator)

`twitch.chat {message, message_id, fragments, user, user_id, login, color, badges, sub_months,
follow_age_s, reply_to?, bits?, bot, shared_from?}` — actor roles come from badges (owner, mod,
VIP, sub) plus *follower* when the cached follow is at least `[policy] follower_min_age` old.
Fragments carry Twitch emotes (CDN urls) and 7TV/BTTV/FFZ emote words already split out.

**Roles for events without badges** (cheers, redemptions, subs, raids, gift recipients) come
from the users/roles cache in the runtime DB (`twitch_users`): the roles each viewer's chat badges
showed last, plus the channel's moderators and VIPs from Helix (read when Twitch connects and
every 10 minutes; `twitch.users.refresh` reads them now). When a badge and a list disagree (a new
VIP who hasn't chatted, a removed mod with an old badge) the one seen last wins. Viewers not seen
for 90 days are forgotten; listed moderators and VIPs are kept.

`twitch.cheer {bits, message, user, anonymous}`, `twitch.sub {tier, months, is_gift, message,
user, gift_id?, gifter?}`, `twitch.resub {tier, months, streak, message}`, `twitch.gift {count,
tier, total, gift_id, user}` followed by one `twitch.sub` per recipient with the same `gift_id`,
`twitch.follow`, `twitch.raid {viewers, from, from_login}`, `twitch.redeem {reward, reward_id,
redemption_id, cost, input, status, managed, reward_key?}`, `twitch.poll.begin|progress|end`,
`twitch.prediction.begin|progress|lock|end`, `twitch.hype_train.begin|progress|end`,
`twitch.ad_break {duration, automatic}`, `twitch.ad.upcoming`, `twitch.stream.online|offline`,
`twitch.channel.update`, `twitch.automod.hold|update`, `twitch.announcement`,
`twitch.marker.created`, `twitch.emotes.updated`.

Deletion sync: `twitch.chat.delete {message_id}`, `twitch.user.purge {user_id, user}` (every ban,
timeout, or clear-user), `twitch.chat.clear`; plus `twitch.ban` / `twitch.timeout {duration_s}` /
`twitch.unban`. Overlays, TTS, and the alert queue remove matching content; the policy drops held
items from that message or user.

## 4. Actions

`twitch.auth.start|cancel|logout {account}` · `twitch.chat.send {text, reply_to?, as?}` ·
`twitch.refund|fulfill {redemption_id, reward_id?}` · `twitch.marker {description}` ·
`twitch.poll.start {title, choices: "a|b|c", duration: "2m", points?}` · `twitch.poll.end
{archive?}` · `twitch.prediction.start {title, outcomes, window}` · `twitch.prediction.lock` ·
`twitch.prediction.resolve {winner: id | title | number}` · `twitch.prediction.cancel` ·
`twitch.shoutout {user}` · `twitch.raid {user}` · `twitch.raid.cancel` · `twitch.ad.snooze` ·
`twitch.rewards.sync` · `twitch.emotes.refresh` · `twitch.users.refresh` · `twitch.delay.set {ms}`
(negative = measured).

Moderation: `mod.ban {user|user_id, reason?}`, `mod.unban`, `mod.timeout {user|user_id,
duration_s, reason?}`, `mod.delete {message_id}`, `mod.automod.approve|deny {message_id}`,
`mod.blocked_terms.add {text}`, `mod.blocked_terms.remove {id}`; the policy queue:
`mod.approve {id}`, `mod.reject {id, reason?}`. Commands a viewer submits directly (origin
`chat`/`relay`, e.g. `!ban` through the bot) run `mod.*`/`twitch.*` only for moderators.

In `rehearsal` mode every Twitch-changing action is a dry run (`twitch.dry_run` event, audit
entry) while events still flow.

State for the UI and overlays: `twitch.auth.*`, `twitch.eventsub.*`, `twitch.stream.{live,
started_at, title, category, viewers}`, `twitch.ad.{next_in_s, duration_s, snoozes, …}`,
`twitch.poll`, `twitch.prediction`, `twitch.hype_train.*`, `twitch.automod.{queue, held}`,
`twitch.rewards`, `twitch.delay_ms`; signals `twitch.viewers`, `twitch.chat_rate` (messages in
the last minute). Queries: `emotes` → `{badges: {"set/id": url}, emotes: {code: {id, url,
provider, animated, zero_width}}}`, `twitch.follow_age {user_id}`, `twitch.scopes`,
`twitch.users` → `{moderators, vips: [{id, login}], moderators_synced_at, vips_synced_at, known}`,
`twitch.users {user_id}` → `{known, roles}`.

Preflight `health.twitch`: fail without Client ID / authorization / EventSub; warn on missing
scopes, refused subscriptions, reward sync problems, an unauthorized bot, or a token close to
expiry.

Stream delay (§3.2): `twitch.delay_ms` = the channel's stream delay setting + `latency` +
measured chat transit (time from sending a chat message to its EventSub echo). It sets the master
clock's `twitch_delay_ms` mapping (`Clock::chat_to_screen`). `twitch.delay.set {ms}` pins it.

## 5. Policy (§12.1)

Every event from Twitch, the relay, chat, or the simulator passes the policy before it is
published or rules see it:

- **Text**: `message`/`input`/fragments are made display-safe (controls/bidi/invisible
  characters removed, zalgo capped at 2 marks, whitespace collapsed, cut to `max_len`).
- **Blocklist** (`[policy] blocklist`): matching sees through case, homoglyphs (Cyrillic/Greek/
  fullwidth/math letters, small caps), leetspeak, s p a c e d or d.o.t.t.e.d letters, and
  stretched letters. A blocked chat message becomes `policy.filtered` (never shown); blocked text
  in a cheer/sub/tip is stripped (the event still counts).
- **AutoMod**: messages AutoMod holds are never shown; held messages are listed in
  `twitch.automod.queue` for `mod.automod.approve|deny`.
- **Veto window** (`[policy.veto]`): cheers ≥ `min_bits`, tips ≥ `min_tip`, resub messages, and
  managed redemptions with input wait `ms` in `policy.pending` (kind `veto`). Nobody acts → the
  event is delivered with `vetted: true`. `mod.reject {id}` → delivered without its text
  (`vetoed: true`), or dropped with `on_reject = "drop"`; a vetoed redemption is refunded.
- **Chat effects** (set/animate/trigger/preset.fire and `lights.`/`audio.`/`patch.`/`fx.`/
  `source.`/`clips.` actions at chat priority) only run in `effect_modes` (default live and
  rehearsal; simulator events are exempt), at priority 100, auto-expiring (`[safety] chat_ttl`),
  cleared by `clean`. Bot replies, alerts, TTS, and the queue are not effects.
- **Rules** triggered by viewers can add `role = "vip"` (minimum role) and `approval = true`
  (the firing waits in `policy.pending` for `mod.approve`).
- **Ad breaks**: opt in with `[policy] ad_break_mode = true` to have `twitch.ad_break` switch
  to mode `ad_break` (from any mode but those in `ad_break_skip`, default offline), then return
  after the break unless someone changed the mode meanwhile. By default the event is available
  to your rules without changing the show mode.
- **Audit**: every decision is a `policy.accepted|rejected|filtered|pending|approved` event and
  an `audit` row (query `audit`).

### Rewards (`rewards/*.toml`)

```toml
title = "HYPE"            # ≤ 45 chars, unique on the channel
cost = 2000
prompt = "…"
color = "#E82424"
cooldown = "5m"           # global (also enforced by Twitch)
user_cooldown = "30m"     # per viewer (ours)
max_per_stream = 10
max_per_user_per_stream = 3
input_required = false
role = "everyone"         # minimum role
approval = false          # a mod approves each redemption
fires = "preset.hype"     # command or list; `preset.x` = `preset.fire x`; templated from the redemption
on_reject = "refund"      # or "keep"
fulfill = "auto"          # "manual": the fired handler fulfills/refunds itself
modes = ["live"]          # default: [policy] effect_modes
enabled = true
paused = false
```

The app creates and updates these on Twitch (they must be created by our Client ID to be
refundable) and disables rewards whose file was removed. Redemptions stay *unfulfilled* until the
policy decides. It rejects them for role, cost, cooldowns, limits, missing or blocked input, the
wrong mode, a mod's rejection, or an approval timeout (→ `twitch.refund`). An accepted
redemption runs its `fires` in order; the first command that fails — e.g. a preset with
`conflict = "reject"` that is already active — stops the rest and the redemption is refunded
too (`policy.rejected` with that reason; the cooldowns and limits it took are given back). Only
when every command ran does it get `twitch.fulfill` and the `twitch.redeem` event reach rules
and alerts. Commands after a `wait` are only scheduled, so they can't refund it.

### For other subsystems

Pure helpers: `se_core::policy::{role_allows, Cooldowns, CooldownSpec, FilterCfg::from_config,
filter_text, display_text}`. In-core gate for ad-hoc chat actions: submit an action
`policy.run {key, role?, cooldown: {global?, per_user?}, approval?, filter?, event?: {…}, do: […]}`
with origin `chat` and the viewer as actor; the ack error carries the rejection reason.

## 6. Testing without a Twitch account

The Twitch CLI (`twitch-cli` release binary, installed to `~/.local/share/stream-engine/bin/twitch`)
runs a mock EventSub WebSocket server; `se-twitch` ships a mock of id.twitch.tv + Helix:

```sh
twitch event websocket start-server --port 8080
cargo run -p se-twitch --features mock --example mock_twitch -- serve 127.0.0.1:18090
```

Point a copy of the project at them:

```toml
[twitch]
client_id = "mock-client"
secrets = "twitch-mock"
eventsub_url = "ws://127.0.0.1:8080/ws"
subscriptions_url = "http://127.0.0.1:8080/eventsub/subscriptions"
helix_url = "http://127.0.0.1:18090/helix"
auth_url = "http://127.0.0.1:18090/oauth2"
```

`streamctl do twitch.auth.start`, then `curl http://127.0.0.1:18090/activate` plays the viewer entering
the code. Fire events with `twitch event trigger channel.cheer -C 1000 -T websocket` (and
`channel.subscribe`, `channel.subscription.gift -C 50`, `channel.channel_points_custom_reward_redemption.add
-i <reward id> -n HYPE -C 2000`, `channel.ban`, `channel.poll.begin`, …); payloads the CLI can't
generate (chat messages, deletes, chat notifications, AutoMod holds, ad breaks of any length) go
through `mock_twitch fire crates/se-twitch/tests/fixtures/<file>.json`. `twitch event websocket
reconnect` exercises `session_reconnect`. Helix calls the engine made (refunds, fulfillments,
chat, markers) are at `http://127.0.0.1:18090/_mock/calls`.

The automated version: `cargo test -p se-twitch --features mock --test mock_e2e -- --ignored`.
