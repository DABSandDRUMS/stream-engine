# API reference

Everything the UI, `streamctl`, web patches, and controllers do goes through one message
protocol (PLAN §2.2, §19). This page is for programmers integrating with the engine: the
transports, every message and field, the command grammar, queries, auth, OSC, and the CLI.
Per-subsystem addresses, events, and actions live in the subsystem docs (see
[Where addresses are documented](#where-addresses-are-documented)).

Sources: `crates/se-proto/src/{wire,command,types,value,address}.rs`, `crates/se-api/src/`,
`crates/se-client/src/lib.rs`, `crates/se-cli/src/main.rs`, `web/engine.js`.

## Transports

| Transport | Where | Encoding | Auth | Default origin |
|---|---|---|---|---|
| Unix socket | `$XDG_RUNTIME_DIR/stream-engine/engine.sock` (`/run/user/<uid>/…` without `XDG_RUNTIME_DIR`) | `u32` big-endian length + MessagePack body (named fields) | none: file permissions (socket 0600, directory 0700) give full access | `ui` (`cmd_text`); `cmd` keeps the command's `origin` |
| WebSocket | `ws://127.0.0.1:7870/ws` | one JSON object per text frame (binary frames holding JSON are accepted too) | token in `hello`, or `?token=` on the URL | `api` |
| OSC | `udp://127.0.0.1:7871` | OSC 1.0 messages and bundles | `/auth <token>` per source address | `osc` |

- Both stream transports carry the same `ClientMsg` / `ServerMsg` types. Frames above 16 MiB
  (Unix socket) or 8 MiB (WebSocket) are rejected.
- The engine refuses to start if another engine is already answering on the socket path; a
  stale socket file is removed.
- Bind addresses: `stream-engine daemon --socket <path> --http <addr:port> --osc <addr:port>`, or
  `http = "…"` / `osc = "…"` in `~/.config/stream-engine/engine.toml`. Defaults are
  `127.0.0.1:7870` and `127.0.0.1:7871`.
- **LAN exposure is opt-in:** bind `--http` / `--osc` to a LAN address (e.g. `0.0.0.0:7870`) and
  give each phone/tablet its own paired-device token ([Auth](#auth)). The engine speaks plain
  HTTP/WS and UDP; there is no TLS listener. `engine.js` switches to `wss:` when its page is
  served over `https:`, so TLS means a reverse proxy in front of the engine.
- `query api.info` returns the endpoints actually in use:
  `{socket, http, ws, osc, token_set, devices: [{name, scope}]}`.
- The frame sockets (`frames.sock`, `obs.sock`) are a separate protocol: see
  [frames-protocol.md](frames-protocol.md).

### HTTP routes (same port as `/ws`)

| Route | Serves |
|---|---|
| `GET /ws` | WebSocket upgrade for the JSON API (`?token=<token>` authorizes at once) |
| `GET /health` | `{"ok": true}` |
| `GET /engine.js` | the browser client (below) from `<share>/web/engine.js` |
| `/web/*` | static files from `<share>/web/` (`player.html`, `player.js`, `overlay.js`, …) |
| `/patches/<id>/…` | the project's `patches/` folder (web patch pages, [patches.md](patches.md)) |
| `/assets/…` | the project's `assets/` folder |

Static routes need no token; the API behind `/ws` does. Web sources are opened by the engine as
`http://localhost:<port>/patches/<id>/<entry>?token=…` ([web.md](web.md)).

## Session flow

1. Connect; send `hello` (optional on the Unix socket, which is already authorized).
2. The engine answers `welcome`, or `error {msg: "unauthorized"}` and closes.
   Any other first message on the WebSocket gets `error {msg: "send hello with a token first"}`
   and the connection closes. With `?token=` the WebSocket is authorized on connect and a
   `welcome` is sent before your `hello`; a later `hello` just gets another `welcome`.
3. Send `subscribe` to receive pushes; send `cmd` / `cmd_text` / `get` / `explain` / `trace` /
   `query` / `ping` at any time. Replies carry your `req` number.
4. If the client falls behind the engine's bus, missed state is re-sent as one `state` message
   with the current values of every subscribed state pattern (events in the gap are lost).

```sh
websocat "ws://127.0.0.1:7870/ws?token=$(streamctl token)"
{"t":"subscribe","sub":{"events":["twitch.*"],"state":["show.*"]}}
{"t":"cmd_text","req":1,"text":"preset.fire hype"}
```

Unix socket from Python (`msgpack`):

```python
import os, socket, struct, msgpack
s = socket.socket(socket.AF_UNIX)
s.connect(os.path.join(os.environ["XDG_RUNTIME_DIR"], "stream-engine/engine.sock"))
def send(m):
    b = msgpack.packb(m); s.sendall(struct.pack(">I", len(b)) + b)
def recv():
    n, = struct.unpack(">I", s.recv(4, socket.MSG_WAITALL))
    return msgpack.unpackb(s.recv(n, socket.MSG_WAITALL))
send({"t": "hello", "client": "script"}); print(recv())   # {'t': 'welcome', …}
send({"t": "get", "req": 1, "pattern": "show.*"}); print(recv())
```

## Messages

Every message is an object whose `t` field names its type (snake_case). Values (`value`, `to`,
`payload`, `args`) are plain JSON/MessagePack values: null, bool, integer, float, string, list,
map. Semantic types (colour, vec2, enum) come from the address metadata, not the value.
Timestamps (`ts`, `now`) are master-clock nanoseconds (CLOCK_MONOTONIC); ids are `u64`.

### Client → engine

| `t` | Fields | Reply |
|---|---|---|
| `hello` | `client` (free text: `cli`, `ui`, `web`, …), `token`?, `version`? (protocol version, currently `1`) | `welcome` |
| `cmd` | `req`?, `cmd` ([Command](#commands)) | `ack` |
| `cmd_text` | `req`?, `text` (one-line command, [grammar](#one-line-command-grammar)) | `ack` |
| `subscribe` | `sub` ([Subscription](#subscriptions)); replaces the previous one | `state` with current values if `sub.state` is non-empty |
| `get` | `req`, `pattern` (address or pattern), `meta`? (default false) | `values` |
| `explain` | `req`, `address` | `explain` |
| `trace` | `req`, `id` (trace id: a command, event, or record id) | `trace` |
| `query` | `req`, `name`, `args`? (default null) | `reply` |
| `ping` | `stamp` (any `u64`) | `pong` |

### Engine → client

| `t` | Fields | When |
|---|---|---|
| `welcome` | `version` (protocol, `1`), `engine` (engine version), `session` (session id), `now`, `pid` | after `hello` |
| `ack` | `req` (your `req` or null), `id` (command id), `ok`, `error` (string or null) | one per `cmd`/`cmd_text` |
| `state` | `changes`: `[[address, value], …]` | subscribed state changed (also right after `subscribe` and after a resync) |
| `values` | `req`, `entries`: `[{address, value, meta?}]` | reply to `get` (`meta` only when requested) |
| `event` | `event` ([Event](#events)) | subscribed event |
| `signals` | `ts`, `values`: `[[name, number], …]` | subscribed signals, at `signal_hz` |
| `explain` | `req`, `provenance`: `{address, value, layers: [{kind, source, priority?, value, active}]}` or null (unknown address) | reply to `explain` |
| `trace` | `req` (null for live pushes), `records`: `[{id, parent, ts, kind, label}]` | reply to `trace`, or pushed with `sub.trace` |
| `reply` | `req`, `value`, `error` (string or null) | reply to `query` |
| `log` | `level`, `target`, `msg`, `ts` | engine log lines with `sub.logs` |
| `pong` | `stamp` (echoed), `now` | reply to `ping` |
| `error` | `msg` | auth failures (the connection then closes) |

```json
{"t":"hello","client":"my-tool","token":"<token>","version":1}
{"t":"welcome","version":1,"engine":"0.1.0","session":"20260925-200000","now":81234567890123,"pid":4242}
{"t":"get","req":2,"pattern":"show.scene.*"}
{"t":"values","req":2,"entries":[{"address":"show.scene.preview","value":"wide"},{"address":"show.scene.program","value":"duo"}]}
{"t":"query","req":3,"name":"presets"}
{"t":"reply","req":3,"value":[{"name":"hype","label":"HYPE","active":false,"remaining_ms":null,"toggle":false,"confirm":false,"chat":true,"color":null}],"error":null}
```

**`get` metadata** (`meta: true`): `{type, range, default, unit, description, options, readonly,
merge, owner}`. `type` is one of `any, float, int, bool, color, enum, string, vec2, vec4,
texture_ref, trigger, list, map`; `merge` is `ltp` or `htp`.

**`explain` layers**, in resolution order: `base` (source `project`), `scene`, `binding`
(source = binding), `override` / `animation` (source = override key, with its `priority`), and
`clamp` (source `meta` or `safety`). `active` marks the layers that shaped the final value.
The resolver builds explanation layers only for `explain`, not on normal ticks. Exact `get`
uses the address index; wildcard matching iterates segments without allocating temporary
paths. Animation, expiry, HTP/LTP precedence and wildcard ordering are unchanged.
Binding updates merge ordered target IDs with the state tree instead of hashing every parameter ID
on each tick; binding declaration order, change publication order, and clearing disabled or
removed bindings are preserved. Snapshot publication caches which IDs belong to lighting and
rebuilds that membership when addresses change. Override priorities are still recomputed for
every snapshot, so release, expiry, and priority changes remain visible without changing the
snapshot cadence or mutating a snapshot already held by a reader.

**`trace` record kinds:** `event`, `rule`, `command`, `change`, `preset`, `error`, `policy`.

### Acks

- `ack.ok` is the core's verdict after executing the command (`id` = the command's id).
- Parse errors in `cmd_text` are acked at once with `id: 0`, `ok: false`.
- Scope refusals are acked at once: `error: "not permitted: <command>"`.
- Subsystem actions (`lights.cue`, `queue.skip`, …) are acked `ok` once the core has routed
  them; the subsystem's own failure shows up as an `error` log line (subscribe with `logs`).
- Commands without `req` are still acked, with `req: null`.

## Commands

`cmd.cmd` is a Command:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `op` | object | required | the operation, tagged by `kind` (table below) |
| `origin` | string | `system` | `ui rule binding patch cli api deck midi voice chat mixer timeline osc twitch relay sim audio obs system` |
| `id` | u64 | fresh id | trace id; `0` is replaced |
| `ts` | u64 | now | master-clock ns |
| `actor` | `{platform, id, name, roles}` | none | viewer the command acts for; `roles` ⊂ `everyone follower sub vip mod owner` |
| `causal` | u64 | none | id of the event/command that caused it (trace chain) |
| `priority` | u16 | from `origin` | override priority |
| `key` | string | from origin/actor | override layer key (e.g. `cuelist:<name>`, `timeline:<name>`); overrides with the same key replace each other and are released together; at chat priority, an explicit key is scoped as `chat:<actor>:<key>` and cannot escape chat caps or TTL |

Only full-scope clients may choose `origin` and `priority`; every other scope is forced to
origin `api` with its default priority. Full-scope WebSocket clients that omit `origin`
(`system`) become `api`. On the Unix socket the command is taken as sent — set `origin`
(`streamctl` sends `cli`, the UI `ui`).

| `kind` | Fields | Effect |
|---|---|---|
| `set` | `address`, `value` | runtime override at the command's priority |
| `set_base` | `address`, `value` | edit the project (base) value; written to the project file, undoable |
| `animate` | `address`, `to`, `ms`, `ease`? | animated override from the current value (`ease`: `linear` (default), `in_quad`, `out_quad`, `in_out_quad`, `in_cubic`, `out_cubic`, `in_out_cubic`, `smoothstep`, `out_back`, `step`) |
| `trigger` | `address`, `payload`? | fire a triggerable address; `payload` keys `attack`, `hold`, `release` (ms or `"2s"`) shape the envelope; emits `<address>.trigger` |
| `release` | `address` | release this origin's override or trigger on `address` |
| `scene_go` | `scene` | scene to preview (straight to program in direct mode) |
| `scene_cut` | `scene`, `transition`? | scene to program |
| `scene_take` | `transition`?, `ms`? | preview → program |
| `preset_fire` | `name`, `payload`? | fire a preset |
| `preset_release` | `name` | release an active preset |
| `mode_set` | `mode` | show mode (`offline`, `preshow`, `live`, `brb`, …: query `modes`) |
| `emit` | `type`, `payload`? | inject an event |
| `panic` | – | release every preset, override below manual priority, trigger, and scheduled command; sends `lights.panic`, `mixer.panic`, `audio.panic` |
| `clean` | – | remove chat-priority overrides, triggers, and presets |
| `undo` / `redo` | – | undo/redo `set_base` edits |
| `wait` | `ms` | delay inside command lists (rules, presets, timelines); a no-op on its own |
| `action` | `name`, `args`? | subsystem action routed by name prefix (`lights.cue`, `queue.skip`, `bot.say`, `obs.stream.start`, …) |

Authored preset, scene, and transition `lights` references use `lights.layer.select` on the
musical **base** layer, not the manual cue-console API. `look` names a palette. `cue` alone
names a cuelist; with `cuelist`, it names a cue within that list. `lights.hold` supplies the
layer's duration; temporary presets also retire their lighting when released or expired.
Each preset runtime instance carries its own owner token, so an older expiration cannot
release a newer preset, scene, or UI selection. Scene selections share the ordered `scene`
owner: entering a scene with no lighting releases only that owner, and a failed replacement
leaves the previous successful scene lighting available for the next scene change to retire.
Selection preserves the originating actor and trace cause; chat-triggered lighting remains
chat-priority and bounded by chat TTL. Layers own their playback duration timers.
`lights.layer.pick {layer, tags, any?, kind?, avoid_repeat?, …select fields}` lets rules ask
for tagged content (palette/cue list `tags`) instead of a name, then selects it with the same
ownership rules; no matching content is a logged no-op, not an error.
`lights.cue`, Go/Back/Locate, and programmer actions remain advanced manual-console APIs;
explicit authored command lists can still invoke them deliberately. See [lights.md](lights.md)
for layer controls, ownership, clock, and output safety.

```json
{"t":"cmd","req":7,"cmd":{"origin":"api","op":{"kind":"animate","address":"lights.par_left.dimmer","to":0.5,"ms":800,"ease":"out_cubic"}}}
{"t":"ack","req":7,"id":1758900000000123,"ok":true,"error":null}
{"t":"cmd","req":8,"cmd":{"op":{"kind":"emit","type":"patch.confetti.clicked","payload":{"x":0.5}}}}
{"t":"cmd","req":9,"cmd":{"op":{"kind":"action","name":"queue.skip","args":null}}}
```

### Priorities and origins

| Priority | Origins | Behaviour |
|---|---|---|
| 300 manual | `ui cli api deck midi voice osc mixer` | wins over presets and chat; override key `manual`; `release` without an own override clears every override on the address |
| 200 preset | `rule binding patch timeline sim audio obs system` | override key = origin name (presets, scenes, and timelines use their own keys) |
| 100 chat | `chat twitch relay` | values clamped by the chat caps, overrides and triggers auto-expire, never `mixer.*`, no scene/mode changes, no `mixer.*`/`obs.*`/`rule.*` actions, paused outside the policy's effect modes; key `chat:<actor id>` |

- Highest priority wins; among equal priorities the latest override wins (`ltp`). Addresses with
  `merge = htp` (light intensities) take the numeric maximum of all overrides instead.
- `panic` drops everything below 300; `clean` drops everything at or below 100.
- Read-only addresses (metadata `readonly`) reject `set` from any origin but `system`.

### Wildcard set / animate / release

`set`, `animate`, and `release` accept an address pattern (`*` = one segment, `**` = any number,
`cam_*` = glob inside a segment). The command runs once per matching existing address;
`release` also matches active trigger addresses. It succeeds if at least one address succeeded
(otherwise the first error is returned) and fails with `` `<pattern>` matches nothing `` when
nothing matches.

```sh
streamctl set 'scene.*.node.cam_face.offset_y' 5
streamctl do "release lights.**"
```

`trigger` takes a single address.

### One-line command grammar

Used by `cmd_text`, `streamctl do`, OSC `/cmd`, `engine.js cmd()`, and the command lists in
rules, presets, timelines, and chat commands (`Op::parse`).

```text
set <addr> <value>              set_base <addr> <value>
animate <addr> <to> <dur> [ease]
trigger <addr> [k=v …]          <addr>.trigger [k=v …]        release <addr>   <addr>.release
scene.go <name>                 scene.cut <name> [transition]
scene.take [transition] [dur]   preset.fire <name> [k=v …]    preset.release <name>
mode.set <mode>                 emit <type> [k=v …]  (alias: fire)                wait <dur>
toggle <addr>  (= action toggle address=<addr>: switch flips; number 0 ⇄ its max, or 1; like `set` from the same origin; wildcards flip each match)
panic | clean | undo | redo     <any.action> [positional …] [k=v …]
autoseq.play [name]   autoseq.toggle [name]   autoseq.select <name>   autoseq.stop   autoseq.next   (auto-sequence.md)
fx.on | fx.off | fx.toggle   fx.auto.on | fx.auto.off | fx.auto.toggle   lights.auto.on | lights.auto.off | lights.auto.toggle   context.spend   (context.md)
```

- Tokens split on whitespace; `'…'` and `"…"` quote (quotes are removed), and `[…]` / `{…}`
  keep their contents together (`set a.b [1, 2]`).
- Values: `true`/`false`, `null`/`none`, integers, floats, TOML inline arrays/tables, quoted
  strings, else the bare text.
- Durations: `100ms`, `8s`, `5m`, `1h`, `1:12.400` (m:ss.mmm), `1:02:03`, or a plain number of ms.
- `k=v` pairs become the `payload` map. For actions, `k=v` pairs (valid address keys) become
  map entries and every other token goes into `args.args` as a list:
  `lights.cue blackout fade=2` → `{"args": ["blackout"], "fade": 2}`.
- `<addr>.trigger` is a trigger only when every following token is `k=v`; `<addr>.release` only
  without arguments. Anything else that is a valid address is an action; other words are
  `unknown command`.
- Templating (`{event.user}`, `{bits}`, any state address) is applied per token in rules,
  presets, timelines, and chat commands only; API `cmd_text` is parsed literally.

```sh
streamctl do "emit twitch.cheer bits=1000 message='hello there'"
streamctl do "patch.confetti.trigger count=40"
streamctl do "animate fx.vhs.amount 0.8 2s in_out_cubic"
streamctl do "scene.take fade 400ms"
```

## Subscriptions

`subscribe.sub` (all fields optional):

| Field | Type | Meaning |
|---|---|---|
| `events` | patterns | event types to push as `event` |
| `state` | patterns | addresses whose changes are pushed as `state` |
| `signals` | patterns | signals pushed as `signals` |
| `signal_hz` | number | signal push rate, 1–120 (default 20) |
| `logs` | bool | push engine log lines as `log` |
| `trace` | bool | push trace records as they happen (`trace` with `req: null`) |

Patterns everywhere (`get`, subscriptions, queries `addresses`/`signals`, OSC `/subscribe`):
`*` matches exactly one segment, `**` zero or more segments, and `*` inside a segment is a glob
(`cam_*`). Addresses are dot-separated segments of `[A-Za-z0-9_-]`.

```json
{"t":"subscribe","sub":{"events":["twitch.*","mode.enter.*"],"state":["show.**","patch.confetti.**"],"signals":["band.*"],"signal_hz":30}}
{"t":"state","changes":[["show.mode","live"],["show.scene.program","duo"]]}
{"t":"signals","ts":81234599990000,"values":[["band.low",0.42],["band.mid",0.18]]}
```

## Events

An event is `{id, ts, type, origin, actor?, payload, causal?}` (`actor` =
`{platform, id, name, roles}`). Types are dotted names:

| Type | Source |
|---|---|
| `<address>.trigger` | a trigger fired (payload = the trigger payload, e.g. `patch.confetti.trigger`) |
| `<address>.ended` | a trigger's envelope finished |
| `preset.<name>.fired`, `preset.<name>.released` | presets (state `preset.<name>.active`) |
| `mode.changed`, `mode.exit.<from>`, `mode.enter.<to>` | `mode.set` (payload `{from, to}`; state `show.mode`) |
| `scene.take` (`{from, to, transition, ms}`), `scene.changed` (`{scene, from, transition}`) | transitions (state `show.scene.{preview,program}`, `show.transition.*`) |
| `panic`, `clean` | those commands (state `show.panic`) |
| `autoseq.started {preset}`, `autoseq.stopped {preset, reason}`, `autoseq.step {preset, scene, step, transition}` | auto sequence ([auto-sequence.md](auto-sequence.md); state `show.autoseq.*`) |
| `fx.changed {enabled}`, `fx.auto.changed {enabled}`, `lights.auto.changed {enabled}` | operator switches `fx.on/off/toggle`, `fx.auto.on/off/toggle`, `lights.auto.on/off/toggle` (persistent read-only state `fx.enabled`, `fx.auto` default `false`, `lights.auto`; operator `set fx.auto` / `set lights.auto` route through switch actions; [context.md](context.md)) |
| `context.peak {energy, mood}`, `context.settle {mood}`, `context.song_peak {strength, mood}`, `context.fill_landed {strength}`, `context.mood {mood, previous}` | context layer (signals `context.*`, state `context.mood`; [context.md](context.md)) |
| `context.musical_fx {preset, mood, energy, level, attack, release}` | intermittent musical video director; `fx.auto` gates only this musical provenance. Read-only `context.fx.preset/reason/next_at` and `health.context.fx`; notifications/manual/viewer effects remain independent ([context.md](context.md)) |
| `policy.accepted`, `policy.rejected`, `policy.pending`, `policy.approved` | chat policy decisions ([twitch.md](twitch.md)) |
| `twitch.*`, `tip`, … | platform adapters, relay, simulator ([twitch.md](twitch.md), [relay.md](relay.md), [tiktok.md](tiktok.md)) |
| `osc.<path>` | OSC input ([below](#osc)) |
| `project.imported`, `project.import.failed`, `project.asset.renamed`, `project.asset.deleted`, `project.asset.failed` | media library actions ([below](#project-media)) |
| anything | `emit` from rules, patches, clients |

Events with origin `twitch`, `chat`, `relay`, or `sim` and an actor pass the chat policy before
rules see them, like real platform events.

### Simulator

`streamctl sim <preset> [k=v …]` sends the action `sim.<preset>`; the core emits events with
origin `sim` and a random viewer (`user=<name>` picks the name). Query `sim.presets` lists
them.

| Preset | Default args | Emits |
|---|---|---|
| `cheer` | `bits=1000 message='…'` | `twitch.cheer {bits, message, user}` |
| `sub` | `tier=1 months=1` | `twitch.sub {tier, months, is_gift, message, user}` |
| `resub` | `tier=1 months=12` | `twitch.resub` (same payload) |
| `gift_bomb` | `count=50 tier=1` | `twitch.gift {count, tier, total, gift_id, user}` + `count` × `twitch.sub {is_gift: true, gifter, gift_id, …}` |
| `follow` | – | `twitch.follow {user}` |
| `raid` | `viewers=300` | `twitch.raid {viewers, from, user}` |
| `redeem` | `reward=HYPE cost=2000 input=''` | `twitch.redeem {reward, reward_id, redemption_id, cost, input, user}` |
| `chat` | `message='hello'` (+ `role=`) | `twitch.chat {message, message_id, user}` |
| `tip` | `amount=5 currency=USD message=''` | `tip {amount, currency, message, is_public, provider: "kofi", user}` |
| `ad_break` | `duration=90` | `twitch.ad_break {duration, automatic: false}` (the project's ad-break scene reacts to it) |
| `hype_train` | `level=2` | `twitch.hype_train.begin {level}` |

## Queries

`query {name, args}` is answered by the subsystem registered for the longest matching prefix,
else by the core. Unknown names reply with ``error: "unknown query `<name>`"``.
`streamctl query <name> ['<json args>']` prints the value.

**Core**

| Name | Args | Returns |
|---|---|---|
| `presets` | – | `[{name, label, color, active, remaining_ms, toggle, confirm, chat, held, knobs}]` — `held`: it stays on (a `hold`, `toggle`, or `set`), so it can be stopped; `knobs`: its [knobs](#quick-effect-knobs), each with its current `value` |
| `rules` | – | `[{name, when, if, do, enabled, file, last_fired_ms_ago}]` |
| `bindings` | – | `[{name, target, signal, enabled, active, output, targets, file, mode, scope}]` |
| `scenes` | – | `[{name, label, key, nodes, sources}]` |
| `scene` | `{name}` | one scene definition |
| `transitions`, `modes` | – | names |
| `signals` | `{pattern}` | `[{name, value}]` |
| `signal.history` | `{name}` | recent values of one signal |
| `active` | – | active presets and triggers `[{kind, name, …, remaining_ms}]` |
| `addresses` | `{pattern}` | matching addresses |
| `errors` | – | project load errors `[{file, msg}]` |
| `trace.recent` | `{n}` (default 200) | `[{id, parent, kind, label, ts}]` |
| `timelines`, `timeline` | –, `{name}` | timeline definitions and state ([timelines.md](timelines.md)) |
| `autoseq` | – | auto sequence presets `[{name, label, order, dwell_ms, transition, ms, steps}]` ([auto-sequence.md](auto-sequence.md)) |
| `sim.presets` | – | `[{name, args}]` |
| `undo` | – | `{undo, redo}` stack depths |
| `config.scenes`, `config.presets`, `config.transitions`, `config.rules`, `config.bindings`, `config.project` | – | parsed project config (`config.transitions`: every `transitions/*.toml` by name, with `label`, `kind`, `shader`, `ms`, `ease`, `enter`, `exit` and its settings) |
| `config.sources` | – | normalized source-name → source table map |
| `config.render` | – | direct `[render]` settings table from `project.toml` |
| `config.fx_chains` | – | chain-name → saved chain definition map |
| `fx.chains` | – | `[{name, label, fx}]`, including stable slot IDs and authored parameters |

**Engine**

| Name | Args | Returns |
|---|---|---|
| `engine.info` | – | `{version, session, project, share, pid, started}` (`share`: absolute share/repo root serving `web/`, `""` if missing) |
| `preflight` | – | `[{name, status: pass\|warn\|fail, detail}]` (every `health.*` plus disk, idle inhibitor, night light, GPU) |
| `sessions` | `{n}` (default 50) | recent sessions |
| `audit` | `{n}` (default 200) | recent audit rows (commands with origin, actor, result) |
| `clock` | – | master-clock mappings (wall, OBS, audio device, Twitch delay) |
| `api.info` | – | endpoints and paired devices (never the token) |
| `secrets.status` | – | `[{name, label, set}]` |
| `project.read` | `{path}` | `{path, exists, text}` |
| `project.files` | `{kind}`? | `[{kind, name, path}]` |
| `project.assets` | – | the media library: `[{path, kind, name, bytes, used_by, abs, modified_ms, sound?}]` ([below](#project-media)) |
| `project.versions` | – | every project version, newest first: `[{id, at_ms, label, auto, kind, changed, first, restored_from}]` ([below](#project-versions)) |

**Subsystems** (details in their docs)

| Names | Doc |
|---|---|
| `alerts`, `alerts.config`, `credits {chatters}`, `chat.recent`, `stats`, `stats.recent {n}`, `goals` | [bot-and-alerts.md](bot-and-alerts.md) |
| `bot.commands`, `bot.timers`, `bot.files`, `bot.counters`, `bot.quotes` | [bot-and-alerts.md](bot-and-alerts.md) |
| `queue`, `queue.policy`, `queue.library {q, limit}`, `queue.history {n}`, `youtube.status`, `relay.status`, `song.player` | [song-requests.md](song-requests.md) |
| `twitch.follow_age {user_id}`, `twitch.scopes`, `emotes` | [twitch.md](twitch.md) |
| `lights.rig`, `lights.cuelists`, `lights.palettes`, `lights.effects`, `lights.tags`, `lights.programmer`, `lights.output`, `lights.rdm` | [lights.md](lights.md) |
| `mixer` / `mixer.status`, `mixer.coverage`, `mixer.snapshots`, `mixer.discover {seconds}` | [mixer.md](mixer.md) |
| `audio.mix`, `audio.devices`, `project.audio.inputs`, `analysis.grid {path, force} \| {media}` | [audio.md](audio.md) |
| `controllers.page {deck, page}`, `controllers.pages`, `controllers.deck`, `controllers.deck.preview {deck, since}`, `controllers.midi`, `controllers.midi.ports`, `controllers.midi.monitor {n, device}`, `controllers.voice`, `controllers.learn` | [controllers.md](controllers.md) |
| `patches {id}`, `patch.templates` | [patches.md](patches.md) |
| `clips {session, status, limit}`, `clips.session {session}`, `clips.jobs`, `clips.feedback {session}`, `recording.status`, `recording.sources`, `recording.timeline {session, from?, to?, limit?}` | [clips.md](clips.md) |
| `recording.finalize` | Stops app capture and waits for finalized files plus persisted metadata; full-access clients only. Internal session rotation/shutdown barrier. |
| `session.persist {session, values}` | Internal durable metadata barrier; rejects a closed session ID. Full-access clients only. |
| `timeline.grid {name}`, `timeline.ports` | [timelines.md](timelines.md) |
| `devices`, `sources`, `project.sources` | [devices-and-sources.md](devices-and-sources.md) |
| `render` | [render.md](render.md) |
| `web` | [web.md](web.md) |
| `obs` | [obs.md](obs.md) |
| `tts` | [tts.md](tts.md) |
| `tiktok` | [tiktok.md](tiktok.md) |
| `giveaway`, `retention`, `remote_mod.request` (used by the relay link) | [extras.md](extras.md) |

The app recorder is independent of OBS: actions `recording.start` / `recording.stop` control
capture of the video/audio sources selected in app Settings and persisted under `[recording]`.
State `recording.active`, `recording.starting`, `recording.stopping`, `recording.path` and
`health.recording` describe app capture, not OBS outputs. `recording.sources` offers source
choices without claiming complete external-device discovery; manual FFmpeg format/source
pairs are supported. Per-file `recordings` entries in session metadata retain
`{canvas, path, start_ns, end_ns, tracks}` on the app master-clock timebase for clipping.

### Project media

Media files live in the project's `assets/` folder, one folder per kind: `images` (png, jpg,
jpeg, webp, gif, svg), `video` (mp4, mov, webm, mkv), `sounds` (wav, mp3, ogg, flac, opus),
`luts` (cube) and `fonts` (ttf, otf). The UI's Scenes → Media tab and its file picker use:

| Field (`project.assets`) | Meaning |
|---|---|
| `path` | project-relative path (`assets/sounds/horn.wav`) — what scenes, presets and patches store |
| `kind` | `images`, `video`, `sounds`, `luts`, `fonts` (from the extension) |
| `name` | file name without the extension |
| `bytes`, `modified_ms` | size, last change (unix ms) |
| `used_by` | project files that refer to it: its path (`assets/…` anywhere; the part after `assets/` inside TOML strings, e.g. alert images), and for a sound the sampler plays by name, `sound = "<name>"` / `audio.play <name>` in TOML files. `assets/`, `sessions/`, hidden folders and files over 2 MiB are not scanned |
| `abs` | absolute path (the UI runs on the engine's machine and reads thumbnails directly) |
| `sound` | the name `audio.play` knows it by (files directly in `assets/sounds/` of a type the sampler loads) |

| Action | Args | Effect |
|---|---|---|
| `project.import` | `{src, kind?, name?}` | copy the local file `src` (absolute path) to `assets/<kind>/<slug>.<ext>` (`slug` of `name`, else of the file name; lowercase, `_` for anything else). Never overwrites: a taken name gets `-2`, `-3`, …. Refuses unknown types, folders, files over 2 GB, a `kind` that doesn't match the file, and files already in `assets/`. The copy runs in the background |
| `project.asset.rename` | `{path, name}` | move the file to `<slug of name>.<ext>` in the same folder and rewrite every reference `used_by` found (a sound's name references follow too, unless a `[sounds.<name>]` entry defines that name). Refuses a name that's taken |
| `project.asset.delete` | `{path, force?}` | delete the file; refused while something uses it unless `force` |

Events: `project.imported {src, path, kind, name}`, `project.import.failed {src, error}`
(`error` is a plain sentence for the user), `project.asset.renamed {path, to, updated}`
(`updated` = the project files rewritten), `project.asset.deleted {path}`,
`project.asset.failed {path, error, used_by?}`. Changes under `assets/sounds/` also send
`audio.reload` so the sampler picks them up. Paths are checked like `project.write` paths
(relative, no `..`, nothing hidden) and must be under `assets/` with a known extension.

### Project versions

The engine keeps the project's history in `<data_dir>/versions/` (never inside the project):
a version is saved at startup, 5 s after each burst of changes to the project (from any
writer: `project.write`, `set_base`, imports, a text editor; a steady stream of writes still
gets one every 60 s), and before anything is put back. Everything in the project is included
except `sessions/`, hidden files and folders, `node_modules/`, `target/`, symlinks, and editor
scratch files. File contents are stored once by blake3 hash (text zstd-compressed, media as
is), so an unchanged video costs nothing per version.

| Field | Meaning |
|---|---|
| `id` | version id (a string; unix ms, unique and increasing) |
| `at_ms` | when it was saved (unix ms) |
| `kind` | `edit` (saved automatically), `named` (saved by `project.version.save`), `restore`, `undo`, `redo` |
| `auto` | `kind != "named"` |
| `label` | the name of a named version |
| `changed` | project paths added, changed, or removed since the version before it |
| `first` | the first version (nothing before it) |
| `restored_from` | restore/undo/redo: `{id, at_ms, label, kind}` of the version put back, else `null` |

| Action | Args | Effect |
|---|---|---|
| `project.version.save` | `{label}` | save the project as it is now as a named version (kept forever) |
| `project.version.restore` | `{id}` | save the current state, then put version `id` back: changed files are rewritten atomically, files the version didn't have are removed (never `sessions/` or anything untracked), the project reloads |
| `project.undo` | – | put back the state before the latest change; again steps further back (an undo stands for the state it put back); undoing a `restore` brings back what was there before it |
| `project.redo` | – | right after an undo (nothing changed since): put back what the undo took away |

State: `project.versions.count`, `project.versions.latest` (id), `project.versions.undo` /
`project.versions.redo` (`{kind, at_ms, changed}` of what they would change, or `null`),
`project.versions.bytes` (disk used). Events: `project.version.saved {id, kind, label,
changed}`, `project.version.restored {id, kind, restored_from, changed}`. `health.versions`
fails while versions can't be saved (disk full, permissions); a failed save is retried every
minute.

Retention: every version from the last 7 days, then the newest per day for 90 days; named
versions and the newest version are kept forever. Stored files no kept version uses are
deleted.

### Quick effect knobs

Quick effects (`presets/<id>.toml`) are written by developers; the operator only turns the few
premade knobs each one declares. A knob maps to one engine address and a value range:

```toml
set = { "lights.effect.strobe.rate" = 2.5 }   # where the knob's value lives

[[knob]]
label = "Strobe speed"             # plain words shown in the UI
target = "lights.effect.strobe.rate"
min = 0.5
max = 3.0
step = 0.25                        # optional
unit = "flashes a second"          # optional, plain words; "%" shows value × 100 ("60%", "+30%")
default = 2.5                      # optional: what "Reset knobs" goes back to
# kind = "color" (value "#rrggbb", no min/max), or kind = "choice" with
# options = [{ label = "Slow", value = 1.0 }, { label = "Fast", value = 4.0 }]
```

The target must be something the quick effect already changes, and its value lives where
firing picks it up:

- a key of its `set` map (held while it runs), or
- `<trigger address>.<key>` of one of its `fx` entries — `fx.shake.strength` is the `strength`
  key of `{ name = "shake" }` (a built-in effect setting, held while the effect runs),
  `patch.confetti.count` the `count` key of `{ name = "patch.confetti" }` (sent with the trigger
  when it fires). Timings (`hold`, `attack`, `release`) aren't knobs.

A file whose knobs are malformed (no label, `min` not below `max`, a `default` outside the range
or not `#rrggbb`, a choice without options, two knobs on one address, a target the quick effect
doesn't change, or a value of the wrong kind at the target) is a config error for that file
(query `errors`), with a message naming the knob. Shared type: `se_core::knob::Knob`.

| Action | Args | Effect |
|---|---|---|
| `preset.knob` | `{name, target, value, save?}` | turn quick effect `name`'s knob on `target`: the value is fitted to the knob (clamped, snapped to `step`, colour normalised), the next firing uses it, and a running instance's held value follows at once (`set` entries and built-in effect settings; an overlay's burst size and trigger strengths apply on the next firing). With `save` (default true) the value is also written into `presets/<name>.toml` with `toml_edit`, comments kept. Not from chat. |

The Quick effects tab sends `save: false` while a slider is held and `save: true` when it's let
go or has rested for 0.6 s.

### Video FX authoring

These actions persist comment-preserving project edits and hot-reload them. They require
full-access, actor-less authoring commands; chat, viewer-derived commands, and patches cannot
author chains/groups. Runtime `set`/`animate`/bindings on slot addresses remain separate.
As with other subsystem actions, an ack confirms routing; an authoring failure is an error log.

`Target` is one of:

```text
{kind:"source", name:"camera"}
{kind:"node", scene:"main", node:"kit"}       # all matching canvas placements
{kind:"scene", scene:"main"}
{kind:"layout", scene:"main", canvas:"wide"}
{kind:"group", scene:"main", canvas:"wide", group:"cameras"}
{kind:"canvas", canvas:"wide"}               # master, before ordinary overlays
{kind:"output", canvas:"wide"}               # master, after overlays
```

| Action | Args | Behavior |
|---|---|---|
| `fx.slot.add` | `{target, name}` | append an independent slot; repeated effect kinds get unique IDs |
| `fx.slot.remove` | `{target, id}` | remove that slot |
| `fx.slot.move` | `{target, id, forward}` | move one position; `forward: true` means later in the chain |
| `fx.slot.set` | `{target, id, key, value}` | author a slot setting, `enabled`, or `triggered`; null removes an explicit setting; identity cannot be changed |
| `fx.chain.set` | `{target, enabled}` | bypass/enable the entire chain without removing slots |
| `fx.chain.apply` | `{name, targets:[Target,…], replace?}` | append independent copies of a saved chain, or replace existing slots; conflicting IDs are regenerated |
| `fx.chain.save` | `{name, label?, target}` | capture the authored chain into `fx_chains/<name>.toml`; rejects node chains that differ across canvases |
| `fx.group.set` | `{scene, canvas, id, nodes:[id,…], z?, enabled?, delete?}` | create/update group membership and stacking; `enabled` controls group FX, not membership; `delete` dissolves the group |

Apply validates every target and resulting document before writing; multi-file write failures
attempt rollback. A source/scene must exist. Master targets must name a configured canvas.
Saving captures authored settings, not temporary live overrides.

CLI arguments use TOML inline tables/arrays (not JSON object syntax):

```sh
streamctl query fx.chains
streamctl do 'fx.chain.apply name=warm_stage targets=[{kind="node",scene="main",node="kit"},{kind="node",scene="main",node="room"}]'
streamctl do 'fx.chain.set target={kind="output",canvas="wide"} enabled=false'
streamctl do 'fx.slot.set target={kind="node",scene="main",node="kit"} id=warmth key=saturation value=0.0'
```

Every attachment host and its live address prefix are listed in [render.md](render.md#slots-targets-and-saved-chains).


## Auth

| Credential | Where | Scope |
|---|---|---|
| Unix socket | file permissions | full |
| API token | keyring, service `stream-engine`, entry `api.token` (64 hex chars, created on first start); `streamctl token` prints it | full |
| Paired device token | keyring `api.device.<name>`; name → scope in the runtime DB | `full`, `read`, or `mod` |
| Web patch token | random per page, passed as `?token=`, registered as `web.<id>` while the source is open | patch |

If the keyring is unavailable at startup the WebSocket/OSC endpoints accept no full token.
Tokens are compared in constant time.

| Scope | May | Queries |
|---|---|---|
| full | everything; may set `origin`/`priority` | all |
| read | `get`, `explain`, `trace`, `subscribe`, `ping`; every command is refused | all |
| patch `<id>` | commands whose target is `patch.<id>` or below, plus targets covered by the manifest `grants` | all |
| mod | `clean`, actions `queue.*`, `mod.*`, `alerts.*`, `giveaway.*`, `bot.say`, `tts.skip`, `tts.clear` | `queue`, `mod`, `alerts`, `presets`, `chat`, `tts`, `giveaway` (and their dotted children) |

**Patch targets:** the address for `set`/`animate`/`trigger`/`release`, the event type for
`emit`, the action name for `action`, and the command word for `scene.go`, `scene.cut`,
`scene.take`, `preset.fire`, `preset.release`, `mode.set`. A grant (an address pattern) covers
every target it matches and everything beneath it. Never allowed for patches, whatever the
grants: `api.*`, `secrets.*`, `project.*`, actions `fx.chain.*`, `fx.slot.*`, `fx.group.*`,
`patch.new`, `patch.open`, `patch.remove`, secret-carrying actions
(`*.key.set`, `*.secret.set`, `*.token.set`, `secrets.set`, `api.device.add`), `set_base`,
`panic`, `clean`, `undo`, `redo`. Grant changes apply to the page's existing token immediately
([patches.md](patches.md)).

**Token and device actions** (full scope; the args of `api.device.add` are redacted from logs,
traces, sessions, and the audit table):

| Action | Args | Effect |
|---|---|---|
| `api.token.rotate` | – | new full token in the keyring; reconnect WebSocket/OSC clients with it |
| `api.device.add` | `name`, `token` (≥ 24 chars, you generate it), `scope` (`full`, `read`/`readonly`, `mod`; default `read`) | pair (or re-pair) a device |
| `api.device.remove` | `name` (or first positional) | revoke and delete its token |

```sh
tok=$(openssl rand -hex 24)
streamctl do api.device.add name=tablet token=$tok scope=read
# on the tablet: ws://<engine LAN address>:7870/ws?token=<tok>
streamctl do api.device.remove tablet
```

**Remote moderators** use the relay's `/mod` page, not this API: the relay forwards their
requests over the engine link, and the engine checks them against `[remote_mod]` and the same
mod scope ([extras.md](extras.md)).

## OSC

UDP, default `127.0.0.1:7871`. Authorize once per source address and port with `/auth`; the
engine replies `/auth/result <bool>` and keeps the source authorized until it has been silent
for an hour. Messages from unauthorized sources are ignored. Bundles are unpacked in order, so
`/auth` and a command can share one bundle. Tools that open a new socket per message (like
`oscsend`) change source port every time; use one long-lived socket.

| Address | Args | Effect |
|---|---|---|
| `/auth` | token (string) | authorize this source → `/auth/result T\|F` |
| `/cmd` | text | any [one-line command](#one-line-command-grammar) |
| `/set` | address, value | `set` |
| `/preset` | name | `preset.fire` |
| `/trigger` | address | `trigger` |
| `/release` | address | `release` |
| `/scene` | name | `scene.go` |
| `/take` | – | `scene.take` |
| `/panic`, `/clean` | – | `panic`, `clean` |
| `/event` | type, then key, value, key, value… | `emit` with those pairs as payload |
| `/subscribe` | pattern | state changes sent back as `/<address with / for .> <value…>` |
| `/subscribe/events` | pattern | events sent back as `/event/<type with / for .> key value key value…` (the shape `/event` takes) |
| `/unsubscribe` | [pattern] | drop one state/event pattern, or all |
| anything else | one number (int, float, or bool) | sets signal `osc.<path>` (`/fader/1` → `osc.fader.1`) and emits event `osc.<path> {value}` when it rises to ≥ 0.5 |
| anything else | other args | emits event `osc.<path>` with the args as a list payload |

- Commands run with origin `osc` (manual priority) and are checked against the token's scope;
  refusals are logged (`osc: not permitted from <addr>`). Read-only tokens cannot send
  signals or `osc.*` events either.
- Argument types: int/long → integer, float/double → float, bool → bool, string → parsed like
  command text (`"0.5"` is a number). Outgoing: numbers as `i`/`f`, lists flattened, maps and
  nested payload values as one JSON string, null as no argument.

```python
import socket
from pythonosc.osc_message_builder import OscMessageBuilder
from pythonosc.osc_message import OscMessage

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
def send(addr, *args):
    b = OscMessageBuilder(address=addr)
    for a in args:
        b.add_arg(a)
    sock.sendto(b.build().dgram, ("127.0.0.1", 7871))

send("/auth", TOKEN)                      # TOKEN = output of `streamctl token`
send("/preset", "hype")
send("/set", "lights.par_left.dimmer", 0.5)
send("/event", "twitch.cheer", "bits", 100, "user", "osc_fan")
send("/subscribe", "show.*")
send("/subscribe/events", "twitch.*")
while True:
    m = OscMessage(sock.recvfrom(65536)[0])
    print(m.address, m.params)             # /auth/result [True], /show/mode ['live'], /event/twitch/cheer ['bits', 100, 'user', 'osc_fan']
```

## engine.js (browser client)

`<script src="/engine.js"></script>` — used by web patches, the player page, and tools. It takes
the token from `?token=` and the server from the page origin (`ws://127.0.0.1:7870/ws` when
opened from a file), reconnects after 1 s, and restores subscriptions.

| Call | Protocol |
|---|---|
| `Engine.connect({token?, url?})` | `hello {client: "web"}` |
| `se.on(pattern, fn)` / `se.state(pattern, fn)` / `se.signals(pattern, fn, hz)` | `subscribe` (union of all registrations) |
| `se.value(address)` | last value received for a subscribed address |
| `await se.cmd(text)` | `cmd_text` → resolves with the command id, rejects with the ack error |
| `await se.set(address, value)` / `await se.emit(type, payload)` | `cmd` with `set` / `emit` |
| `await se.get(pattern, meta)` / `se.explain(address)` / `se.query(name, args)` | `get` / `explain` / `query` |
| `se.patch` | `{id, params, env, trigger}` on pages under `/patches/<id>/` |
| `se.onconnect`, `se.ondisconnect`, `Engine.matches(pattern, s)` | hooks and the pattern matcher |

## streamctl

`streamctl` talks MessagePack over the Unix socket (`hello {client: "cli"}`); commands are sent
as `cmd` with origin `cli` and printed as `ok: <command>` or an error (exit status 1).

| Global flag | Meaning |
|---|---|
| `--socket <path>` | engine socket (default `$XDG_RUNTIME_DIR/stream-engine/engine.sock`) |
| `--trace` | after a command, print its cause chain (`trace` of the command id) |
| `--json` | machine-readable output (`get`, `query`, `preflight`, `watch`) |

| Verb | Sends |
|---|---|
| `fire <type> [k=v …] [--as <user>]` | `emit` (with `--as`: a Twitch viewer actor) |
| `sim <preset> [k=v …]` | `action sim.<preset>` |
| `do <command text…>` | the parsed [one-line command](#one-line-command-grammar) as `cmd` |
| `scene <name> [--cut]` | `scene_go` / `scene_cut` |
| `take [transition]` | `scene_take` |
| `preset <name>`, `release <name>` | `preset_fire`, `preset_release` |
| `mode <mode>` | `mode_set` |
| `set <address> <value>` | `set` (value parsed like command text) |
| `panic`, `clean`, `undo`, `redo` | the same ops |
| `brb` | `get show.mode`, then `mode_set` `brb` ⇄ `live` |
| `next`, `prev` | `action scene.next` / `scene.prev` |
| `marker [label]` | `action session.marker {label}` (default `clip`) |
| `get <pattern> [--meta]` | `get` |
| `explain <address>` | `explain` (active layer marked `▸`) |
| `trace <id\|last>` | `trace` (`last` = newest root from `query trace.recent`) |
| `query <name> ['<json args>']` | `query` |
| `watch [--events P]… [--state P]… [--signals P]… [--logs]` | `subscribe` (events default `**`; `--events=` for none; signals at 10 Hz); with `--json` one object per line: `{"event":…}`, `{"state":{addr: value}}`, `{"signals":{name: x}}`, `{"log":{level, target, msg, ts}}` |
| `preflight` | `query preflight` (exit 1 if any check fails) |
| `status` | `query engine.info` + `get show.*` |
| `token` | reads the API token from the keyring (no engine connection) |

## Where addresses are documented

| Area | Doc |
|---|---|
| Twitch events, actions, policy, rewards | [twitch.md](twitch.md) |
| Chatbot, alerts, goals, overlays | [bot-and-alerts.md](bot-and-alerts.md) |
| Song queue, YouTube player, song metadata (`song.current.{title,artist,genres,year}`, entry `artist`/`genres`/`year`) | [song-requests.md](song-requests.md) |
| Lights | [lights.md](lights.md) |
| Audio graph, effects, mixer | [audio.md](audio.md), [audio-effects.md](audio-effects.md), [mixer.md](mixer.md) |
| Stream Deck, MIDI, voice | [controllers.md](controllers.md) |
| Cameras and media sources | [devices-and-sources.md](devices-and-sources.md) |
| Patches, web sources | [patches.md](patches.md), [web.md](web.md) |
| Rendering, frames, OBS | [render.md](render.md), [frames-protocol.md](frames-protocol.md), [obs.md](obs.md) |
| Timelines, auto sequence, clips, TTS, TikTok, extras, relay | [timelines.md](timelines.md), [auto-sequence.md](auto-sequence.md), [clips.md](clips.md), [tts.md](tts.md), [tiktok.md](tiktok.md), [extras.md](extras.md), [relay.md](relay.md) |
| Effects/lights operator switches, context signals and moments | [context.md](context.md) |
| What each UI page reads and sends | [ui-data-contract.md](ui-data-contract.md) |
