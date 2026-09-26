# Extras: setup wizard, giveaways, credits, TTS, remote mods, backups

Operator notes for the M11 features. Config lives in `project.toml`; every section is optional
and hot-reloads (a broken section is logged and the previous settings stay active).

## First-run setup wizard

The UI opens **Setup wizard** by itself while `project.toml` lacks `[setup] done = true`
(state `setup.done`). Steps:

1. **Devices** — name each camera (`devices.rename`) and assign it to a scene source
   (`source.assign`); other devices (Studio 24c, DMX USB PRO, Stream Deck, MIDI) show their
   presence and preflight checks, and unknown devices can be marked as expected.
2. **Accounts** — Twitch device-code sign-in (`twitch.auth.start`; needs `[twitch] client_id`),
   optional bot account, YouTube Data API key (`youtube.key.set`, stored in the keyring).
3. **OBS** — plugin installed / connected / receiving frames (`health.obs`).
4. **Extras** — download the TTS voice model (`tts.model.fetch`), relay shared secret
   (`relay.secret.set`, with a generator for `npx wrangler secret put RELAY_SECRET`), remote mod
   switch, backup status.
5. **Finish** — `setup.complete` writes `[setup] done = true` (comments preserved). *Skip setup*
   does the same; `stream do setup.reopen` brings the wizard back.

`stream-engine new <dir>` creates the starter project the wizard configures.

## Giveaways

View **Giveaway** (Views menu), or actions:

| Action | Args |
|---|---|
| `giveaway.open` | `title`, `keyword` (default `[giveaway] keyword`), `min_role` (`everyone`…`owner`), `duration` (`5m`: auto-close) |
| `giveaway.close` | — |
| `giveaway.draw` | — (closes an open round; drawing again picks the next winner among those who haven't won) |
| `giveaway.reset` | — |
| `giveaway.remove` | `user` (name or `platform:id`) |
| `giveaway.enter` | `user`, `role`, `sub_months` (manual entry) |

Viewers enter by typing the keyword as the first word of a chat message. Entry weight = the
highest `[giveaway.weights]` value among the viewer's roles + `sub_month_bonus` × months
subscribed (from the chat badge), capped at `max_weight`; the win chance is weight / total weight
of everyone still in the draw. Bots (`exclude_users`) and the broadcaster (`exclude_roles`) are
never entered; banned or timed-out viewers are removed (`twitch.user.purge`). Open/close/winner
are announced through the chatbot (`announce_*` templates: `{title}`, `{keyword}`, `{entries}`,
`{user}`, `{chance}`). The round survives engine restarts.

State: `giveaway.{state,title,keyword,entries,tickets,winner,winners,closes_at}`; events
`giveaway.opened|closed|entered|drawn|reset` (use them in rules or overlay patches).

## Credits

`rules/credits.toml` triggers the credits patch when the show enters `outro`
(`mode.enter.outro` → `patch.credits.trigger`); the patch lists the session's subs, cheers,
raids, tips, follows, and top chatters.

## Text to speech (Kokoro-82M)

Local TTS on the CPU (ONNX Runtime) with phonemes from `espeak-ng` run as a separate process.
Audio goes to the `tts` audio slot → `tts` bus (ducks music per `[audio.duck]`).

* Model: `stream do tts.model.fetch` (or the wizard) downloads the checksum-verified model and
  voices into `~/.local/share/stream-engine/models/kokoro/`; the packaged script is
  `/usr/share/stream-engine/scripts/fetch-tts-model.sh`.
* Alerts call `tts.say {text, id, kind, user, amount, tier, voice?}` **after** the veto window
  with policy-filtered text; `[[tts.voices]]` rules pick a voice per tier/amount.
* Mods: `!skiptts` / `!cleartts` in chat, the TTS view, or the remote mod console
  (`tts.skip`, `tts.clear`). Deleted messages / banned users drop out of the queue.

## Remote mod console

Moderators open `https://<relay domain>/mod`, sign in with Twitch, and get the song queue
(skip, approve/reject, remove, open/close), alerts (hide, kill, pause), chat holds (policy
approvals, AutoMod allow/deny), TTS skip/clear, giveaways, and *Clean*. Never mixer, lights,
scenes, OBS, or presets.

How it is secured:

1. The relay validates the Twitch token (issued to your client id) and checks the channel is in
   the user's moderated channels (`user:read:moderated_channels`; the broadcaster is always
   allowed). The token is revoked right after the check and never stored.
2. The engine must agree: `[remote_mod] enabled = true`, not in `deny_users`, and in
   `allow_users` when that list is non-empty.
3. Every command is checked again by the engine against `[remote_mod] actions` and the API's
   mod scope, runs as the moderator (`Origin::Relay`, actor role Mod), and is rate-limited
   (`rate_per_min`). Active moderators show in **Settings → Accounts & app** (`remote_mod.active`), where
   the console can also be switched on (`[remote_mod] enabled`).
4. The console session is an HMAC-signed, HttpOnly, SameSite=Strict cookie (12 h by default,
   `MOD_SESSION_HOURS`), keyed from the relay's shared secret.

Owner setup: deploy the relay (docs in `relay/`), add `https://<domain>/mod/callback` as an
**OAuth Redirect URL** of the Twitch application whose client id is in `[twitch] client_id`, and
set `[remote_mod] enabled = true`.

Local test with a fake Twitch: `cd relay && npx wrangler dev --port 8787 --var
TWITCH_ID_BASE:http://127.0.0.1:8799 --var TWITCH_API_BASE:http://127.0.0.1:8799`, a dev engine
linked to it (`[relay] url = "ws://127.0.0.1:8787/link"`), then
`node test/e2e/mod-e2e.mjs --client-id <twitch.client_id> --broadcaster-id <twitch.broadcaster.id>
--stream "stream --socket …"`.

## Backups and retention

| What | Default | Config |
|---|---|---|
| Runtime DB backup | daily, keep 14, `~/.local/share/stream-engine/backups/runtime-<utc>.db.zst` (integrity-checked) | `[retention.backups]` `keep`, `interval`, `dir`, `enabled` |
| Session logs | delete after 90 days, always keep the newest 10 | `[retention]` `sessions_days`, `sessions_keep` |
| Recordings | 200 GB budget in OBS's recording folder, warn at 90 %, fail under 20 GB free | `[retention.recordings]` |

Backups are deferred while on air (unless two intervals overdue). Sessions containing a `.keep`
file (the clip pipeline adds it while clips are pending review) and the running session are never
deleted. Over-budget recordings are only **announced** (preflight warning + desktop notification
listing the oldest files); they are deleted by *Clean up old recordings* (press and hold) in
**Settings → Backups** (`retention.prune_recordings`), or — with `auto_delete = true` — after the warning has
stood for `grace_hours`. Nothing is deleted while OBS records or the show is on air.

Restore a runtime DB backup (engine stopped):

```sh
systemctl --user stop stream-engine
zstd -d ~/.local/share/stream-engine/backups/runtime-<stamp>.db.zst -o ~/.local/share/stream-engine/runtime.db -f
rm -f ~/.local/share/stream-engine/runtime.db-wal ~/.local/share/stream-engine/runtime.db-shm
systemctl --user start stream-engine
```

Actions: `retention.backup_now`, `retention.prune_sessions`, `retention.prune_recordings`,
`retention.scan`; query `retention`; preflight `health.backup`, `health.recordings`.

## TikTok LIVE (best effort)

Unofficial and off by default; see [tiktok.md](tiktok.md).
