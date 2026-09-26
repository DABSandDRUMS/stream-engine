# Chatbot, alerts, goals, stats, and overlays

Operator and author guide for `se-bot` (§14.3) and `se-alerts` (§14.1–14.2, §12.1 veto and
deletion sync), plus the overlay patches in `project-example/patches/`.

Everything is plain TOML in the project, hot-reloaded, and editable from the UI
(**Views → Chatbot** and **Views → Alerts, goals & stats**). UI and chat edits are written back
with `toml_edit`, so comments and ordering survive. A file that fails to parse or validate keeps
its last good version live; the error shows in the view, in `health.bot` / `health.alerts`, and
in the console.

## Chat commands — `commands/*.toml`

```toml
[[command]]
name     = "!discord"                       # trigger word (one word, case-insensitive)
aliases  = ["!dc"]
reply    = "Join the Discord: https://discord.gg/xxxx"
role     = "everyone"                       # everyone < follower < sub < vip < mod < owner
cooldown = { global = "30s", per_user = "2m" }
modes    = ["live"]                         # optional: only in these modes
enabled  = true
```

A command does **one** of: `reply` only, `do = ["…"]` (one-line engine commands, e.g.
`preset.fire hype`), `action = "queue.request"` + `args = { … }` (one subsystem action), or
`builtin = "…"`; `reply` can accompany any of them. Other fields:

| field | meaning |
|---|---|
| `min_args`, `usage` | fewer arguments than `min_args` → reply `usage` instead (no cooldown used) |
| `deny_reply` | said when the role gate refuses (default: silent) |
| `counter` | counter behind `{count}` (default: the command name, when the reply uses `{count}`) |
| `thread` | reply as a Twitch reply thread to the invoking message |
| `as` | `"bot"` or `"broadcaster"` (default `[twitch] chat_as`) |
| `sub.<word>` | subcommands, e.g. `sub.open = { role = "mod", action = "queue.open" }` for `!queue open`; they inherit the parent's role |

**Placeholders** (replies, `do` tokens, `args` values): `{user}`, `{touser}` (first argument
without `@`, else the caller), `{args}` (everything after the command), `{1}`…`{9}`, `{count}`,
`{uptime}` (Twitch stream uptime, else time since the show went `live`), `{song}` (now playing),
`{random:a|b|c}`, and **any state address** (`{goals.subs.current}`, `{stats.latest.follow.user}`,
`{queue.url}`, `{bot.counter.deaths}`). Unknown names stay literal so mistakes are visible. Text a
viewer typed is inserted once and never re-scanned, and it passes the policy content filter
(`[policy] blocklist`, length caps, zalgo/homoglyph folding); a blocked value withholds the reply.
A lone `{1}` in `args` whose value is a number (`3`, `#12`) arrives as an integer.

**Gates** run through the core policy (`se_core::policy`): role check (follow age is applied by
the Twitch adapter), then cooldowns (global and per user; mods and the broadcaster skip them —
`[bot] mods_bypass_cooldowns`). Commands submitted by the bot carry origin `chat` and the viewer
as actor, so chat priority, chat caps, `chat_ttl`, and `[policy] effect_modes` apply exactly as
for rules; the broadcaster's own commands run at preset priority.

**Built-ins** (`commands/mod.toml` in the example; rename or re-gate freely):

| builtin | chat | notes |
|---|---|---|
| `addcom` | `!addcom !name reply…` | writes to `[bot] custom_file` (`commands/custom.toml`) with a `# added from chat by <mod>` comment |
| `editcom` | `!editcom !name reply…` | edits the reply in whichever file defines it |
| `delcom` | `!delcom !name` | removes it from its file |
| `quote` | `!quote`, `!quote 12`, `!quote words` | random / by number / search |
| `addquote`, `delquote` | `!addquote text`, `!delquote 12` | quote numbers are never reused |
| `setcounter` | `!setcounter deaths 5`, `… +1`, `… -1` | |
| `commands` | `!commands` | lists what the caller may use |

Chat edits only touch plain reply commands (no `do`/`action`/`builtin`/`sub`), so a mod can't
break or delete the tools themselves. Replies of built-ins default to short confirmations;
set `reply` to change them (`{command}`, `{quote}`, `{quote_id}`, `{quote_date}`, `{quote_by}`,
`{counter}`, `{value}`, `{commands}`).

**Timers** — same files:

```toml
[[timer]]
name = "requests"
every = "20m"              # at least 1m
min_chat_lines = 10        # …and at least this much chat since the last run
modes = ["live"]           # default; the clock starts when the mode is entered
reply = "Song requests are open: !sr <song or link>"
do = []                    # optional commands
```

**Counters and quotes** live in the runtime DB (`bot_counters`, `bot_quotes`). Counters are also
state `bot.counter.<name>` for rules and overlays.

**Replies and safety.** Everything the bot sends (`bot.say` from rules, other subsystems, and
commands) is cleaned: control and bidi characters removed, line breaks folded, capped at 500
characters, and a reply starting with `/ . \ !` gets an invisible guard so it is never read as
a command (by Twitch, other bots, or this bot). The bot also ignores echoes of its own recent
messages. It then fires `twitch.chat.send {text, reply_to?, as?}` (the Twitch adapter owns rate
limits and which account sends) and emits `bot.said {text, source, reply_to}`.

`[bot]` in `project.toml`: `custom_file`, `mods_bypass_cooldowns`, `max_reply`,
`max_custom_reply`, `echo_window`.

## Alerts — `alerts/*.toml`

```toml
[[alert]]
name     = "cheer"
when     = "twitch.cheer"          # event pattern (globs ok)
if       = "amount >= 1"           # optional expression
title    = "{user} cheered {amount} bits!"
message  = "{message}"             # the viewer's text, content-filtered
sound    = "alert_cheer"           # → audio.play {sound}; files in assets/sounds/
duration = "6s"
priority = 40
tts      = true                    # → tts.say after the veto window
tts_text = "{user} says: {message}"
voice    = "af_heart"              # optional; otherwise the TTS picks by kind/tier/amount
image    = "images/cheer.gif"      # optional; relative to assets/
do       = ["preset.fire confetti"] # optional commands when it shows
veto     = true                    # viewer text waits for the veto window (default)
interrupt = true                   # may interrupt smaller alerts

[[alert.variation]]                # first matching variation overrides fields
name = "big"
if   = "amount >= 1000"
sound = "alert_cheer_big"
duration = "10s"
priority = 75
```

The **first** enabled alert whose `when` matches (and `if` holds) handles an event. Expressions
and templates see normalized fields — `user`, `amount` (bits / tip amount / raid viewers / gift
count / sub months / redeem cost), `tier` (1–3), `money` (`$5.00`), `message`, `role`, `mode` —
plus `event.<payload field>` and state addresses. Templates can also use payload fields directly
(`{months}`, `{viewers}`, `{count}`, `{gifter}`); text fields are content-filtered.

### Queue policy — `[queue]` (one per project, `alerts/queue.toml` in the example)

| key | default | |
|---|---|---|
| `min_spacing` | `1s` | gap between alerts |
| `max_on_screen` | `15s` | cap for any alert |
| `max_queue` | `200` | beyond this the lowest-priority newest alert is dropped |
| `interrupt`, `interrupt_margin` | `true`, `30` | an alert at least this much higher priority cuts the current one short |
| `on_interrupt` | `requeue` | or `drop` |
| `pause_modes` | `["ad_break", "brb"]` | hold the queue; the alert on screen is taken down and replays in full afterwards |
| `veto_window` | `3s` | see below |
| `gift_window` | `3s` | gifted subs within this window join their gift bomb |
| `max_message` | `300` | viewer text cap |

The queue also holds on **panic** (until **clean**) and on the manual `alerts.pause`.

### Gift bombs

`twitch.gift {count, gift_id, user}` becomes **one** alert; the gifted `twitch.sub {is_gift,
gift_id, gifter}` events that follow (or arrive first — EventSub doesn't order them) are
folded into it as `recipients` (the overlay lists them live via `alert.update`). Gifted subs
whose gift event never arrives are combined after `gift_window` (or shown as a single gifted-sub
alert). Stats and goals still count every sub.

### Veto window and deletion sync (§12.1)

The core policy already holds large viewer-text events (`[policy] veto`, e.g. cheers ≥ 100 bits,
tips ≥ $5) before anything sees them and marks them `vetted`. Alerts with viewer text that were
**not** vetted wait `veto_window` in the queue: state `alerts.veto` (list) and
`alerts.veto.<id>` (seconds left) drive the UI countdown; `alerts.veto {id}` kills it (queued or
on screen, and skips its TTS), `alerts.approve {id}` lets it through now. Mods can do the same
from chat with `!veto <id>` / `!skipalert`.

Deletion sync: `twitch.chat.delete {message_id}` strips that text from its alert (and skips its
TTS); `twitch.user.purge {user_id}` (ban/timeout) removes the user's queued and on-screen alerts;
the chat feed drops their messages (below).

### Driving overlays

Events `alert.show {id, kind, variation, event, title, message, user, amount, currency, tier,
sound, image, duration, priority, recipients, count, sim}`, `alert.update {…}`,
`alert.hide {id, reason}` (`done | skipped | vetoed | interrupted | paused | purged`), and state
`alerts.current` (null when nothing shows). Queue state: `alerts.queue`, `alerts.length`,
`alerts.paused`, `alerts.pause_reason`, `alerts.veto`, `alerts.veto_pending`.
`alerts.enabled` (user-settable) is the master switch: off = counted but not shown.

Actions: `alerts.veto|approve|replay {id}`, `alerts.skip`, `alerts.pause|resume`,
`alerts.clear`; editing (UI/CLI only): `alerts.edit {file, path, fields}`,
`alerts.add {file, path, key: alert|variation, fields}`, `alerts.remove {file, path, key, index}`,
`alerts.policy {fields}`. Queries: `alerts` (current, queue, history), `alerts.config`.

## Stats — `stats.*`

Every follow, sub (incl. gifted), gift bomb, cheer, tip, and raid is a row in the runtime DB
tagged with the engine session (one stream: it rotates when the show returns to `offline`).
Published: `stats.latest.{follow,sub,gift,cheer,tip,raid}.*` (user plus tier/months/count/amount/
currency/viewers), `stats.top.{cheer,tip,gift}.{session,alltime}.{user,amount}`, and
`stats.session.{follows,subs,gifts,bits,tips,raids}`. Private Ko-fi tips appear as “Anonymous”.
Queries: `stats`, `stats.recent {n}`, `credits {chatters}` (session subs, gifts, cheers, tips,
raids, follows, top chatters — the credits roll).

`[stats]`: `count_simulated` (default `true`; simulator rows are marked and can be removed with
**Purge simulated data** / `stats.purge_simulated`), `ignore_chatters` (bots),
`exclude_broadcaster`.

## Goals — `project.toml [goals.<name>]`

```toml
[goals.subs]
label  = "Sub goal"
counts = "subs"        # follows | subs | sub_points | bits | tips | gifts | raids
target = 50

[goals.hype]
when = "twitch.redeem" # or any event…
if   = "event.reward == 'HYPE'"
add  = "1"             # …with an amount expression
target = 20
```

Progress persists in the DB across streams and restarts: `goals.<name>.{current, target, label,
progress}`; crossing the target emits `goal.reached {name, current, target}` (hook presets to
it). Actions: `goals.set {name, value}`, `goals.add {name, amount}`, `goals.reset {name}`,
`goals.save {name, fields}` / `goals.delete {name}` (write `project.toml`).

## Chat feed for overlays

`twitch.chat` lines that pass the content filter become `chat.message {id, user, user_id, login,
color, badges, roles, fragments, text, reply_to, bits, platform, ts}` — fragments are
display-safe text, mentions, cheermotes, and emotes with URLs (Twitch, 7TV, BTTV, FFZ).
Removals: `chat.delete {message_id}`, `chat.purge {user_id, user}`, `chat.clear`. The query
`chat.recent` returns the last 100 (already without deleted ones) so a reloaded chat box starts
filled.

## Overlay patches

Web patches (`kind = "web"`, `layer = "overlay"`) in `project-example/patches/`, rendered by the
engine's CEF and placed with `[overlays.<id>]` in `project.toml` (canvases, per-canvas `rect`,
`when`, `z`). They load `/engine.js` and `/web/overlay.js` (DOM-only rendering of viewer text,
emotes, badges, money/number formatting, params). Every param is `patch.<id>.<param>` and can
also be overridden with `?param=value` in the URL.

| patch | shows | notable params |
|---|---|---|
| `alertbox` | the alert queue (`alert.*`), gift recipients, emotes | `accent`, `card_bg`, `position`, `scale`, `style` (pop/slide/fade), `show_icon` |
| `chatbox` | `chat.*` with badges and emotes, deletion-synced | `max_messages`, `fade_after`, `hide_commands`, `hide_users`, `font_size`, `bg` |
| `goals` | `goals.*` bars | `goal` (one name or all), `accent` |
| `labels` | `stats.*` as a row or ticker | `fields` (`path=Label, …`), `layout`, `interval` |
| `eventlist` | recent follows/subs/gifts/cheers/raids/tips (`stats.recent` + live events) | `max_items`, `kinds` |
| `countdown` | starting-soon timer (starts on `mode.enter.preshow` or `patch.countdown.trigger minutes=10` / `target=20:00`) | `minutes`, `target`, `title`, `subtitle`, `show_song` |
| `credits` | end-of-stream roll from the `credits` query (on `patch.credits.trigger` or entering `outro`) | `title`, `speed`, `chatters`, `loop` |

Testing a page in a normal browser: open
`http://127.0.0.1:<http port>/patches/<id>/index.html?token=<API token>` and fire simulator
events (`stream sim gift_bomb count=50`, `stream sim chat message='hi Kappa'`).
Alert sounds are `assets/sounds/alert_*.wav` (synthesized chimes; replace freely).
