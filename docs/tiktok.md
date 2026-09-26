# TikTok LIVE events — operator notes

`se-tiktok` turns a TikTok LIVE room's chat, gifts, likes, follows, shares, joins and subs into
engine events, so rules, alerts, patches and lights react to TikTok the same way they react to
Twitch. **TikTok has no official API for this.** The client speaks the reverse-engineered
protocol the community libraries use ([TikTok-Live-Connector], [TikTokLive]) and needs a third-party
*sign provider* ([Euler Stream] by default) to obtain the signed WebSocket URL. Expect it to break
when TikTok changes things; it is isolated so that nothing else is affected when it does.

It is **off by default**. Until you enable it, no client task exists and nothing touches the
network.

## Setup

```toml
# project.toml
[tiktok]
enabled = true
unique_id = "yourname"                    # your @handle (the "@" and full URLs are accepted)
# sign_url = "https://api.eulerstream.com"  # default; any Euler-Stream-compatible provider
# poll_offline = "60s"                      # how often to check whether you went live (30s–1h)
```

Optional but recommended: an Euler Stream API key (free "Community" account) raises the limits.
Store it in the system keyring — it never goes into project files:

```
stream do "tiktok.key.set <your-key>"   # stored as keyring secret `tiktok.sign_api_key`
stream do "tiktok.key.set ''"           # remove it again
```

Limits as of 2026-09-25: without a key the sign endpoint allows **5 connection requests per
minute, 30 per hour, 100 per day** (measured from `GET https://api.eulerstream.com/webcast/rate_limits`);
the free Community tier with a key allows **2,500 requests per day** ([pricing]). One request is
used per (re)connect, never while you are offline — the "are you live?" check talks to TikTok
directly.

## What you get

| Kind | Name | Payload |
|---|---|---|
| event | `tiktok.chat` | `message` (≤ 500 chars, plain data), `message_id` |
| event | `tiktok.gift` | `gift`, `gift_id`, `count`, `diamond_count`, `diamonds` (= diamond_count × count), `streak`, `to_user`? — one event per finished combo streak |
| event | `tiktok.like` | `count` (likes in the last second), `total` (room total), `likers` — at most one per second, attributed to the top liker |
| event | `tiktok.follow`, `tiktok.share`, `tiktok.join` | — |
| event | `tiktok.sub` | `months` |
| signal | `tiktok.viewers` | current viewer count |
| state | `tiktok.connected`, `tiktok.room_id`, `tiktok.status` | readonly |
| preflight | `health.tiktok` | pass (connected / disabled), warn (offline, reconnecting), fail (bad key, unknown user) |

Every event carries `user` and `user_id` plus an actor `{platform: "tiktok", id, name, roles}`;
roles are `mod` (room moderator/admin), `sub` (subscriber), `follower`, `owner` (you), else
`everyone`. Events have origin `chat` (viewer priority, like Twitch chat).

Example rule:

```toml
[[rule]]
name = "tiktok big gift"
when = "tiktok.gift"
if = "event.diamonds >= 100"
do = ["preset.fire confetti"]
cooldown = { global = "10s" }
```

Flood protection: chat is capped at 20 events/s (burst 40), joins at 5/s, gifts at 20/s,
follows/shares/subs at 10/s; the excess is dropped and counted (`stream query tiktok`).
History that TikTok replays on (re)connect never fires events.

## Controls

- `stream do "tiktok.connect [unique_id]"` — connect for this session even if `enabled = false`
  (optionally to another account).
- `stream do tiktok.disconnect` — stay disconnected for this session even if `enabled = true`.
- `stream query tiktok` — `{enabled, connected, state, room_id, unique_id, last_error, viewers,
  sign_auth, counts}`.

The overrides last until the engine restarts.

## Behaviour

- Offline: checks every `poll_offline`; connects as soon as you go live; when TikTok says the
  stream ended it disconnects and goes back to polling.
- Errors: reconnects with exponential backoff (2 s doubling to 5 min, jittered). A rate-limited
  sign request waits as long as the provider asks; a refused key retries every 15 min (setting a
  new key reconnects immediately).
- A crash inside the client is logged and the client restarts after a delay; the engine carries on.

## Troubleshooting

- `health.tiktok` fail "sign provider refused": the key is wrong or the plan lacks the route —
  `stream do "tiktok.key.set <key>"`.
- warn "sign provider rate limit": too many reconnects on the anonymous tier — add a key.
- warn "blocked by TikTok": TikTok served a captcha or rejected the socket; it retries with backoff.
  If it persists after a TikTok update, check the upstream projects for protocol changes.
- Smoke test outside the engine: `cargo run -p se-tiktok --example probe -- <handle>` (room
  lookup), add `--connect 60` to watch live events for a minute.

[TikTok-Live-Connector]: https://github.com/zerodytrash/TikTok-Live-Connector
[TikTokLive]: https://github.com/isaackogan/TikTokLive
[Euler Stream]: https://www.eulerstream.com
[pricing]: https://www.eulerstream.com/pricing
