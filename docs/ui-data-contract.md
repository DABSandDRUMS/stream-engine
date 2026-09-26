# UI data contract

The UI (`crates/se-ui`) is a pure API client. Everything it shows comes from state, signals,
events, and named queries; everything it does is a command or an action. This page lists what
the built-in views read and send, so a subsystem shows up correctly by publishing these names.
Missing data never breaks a view: it shows an explanatory empty state.

## Top bar, Performance

| Read | Meaning |
|---|---|
| `show.mode`, `show.live_since` (unix ms, 0 = off air; engine) | live state + uptime |
| `obs.link`, `obs.output.<id>.{active,kbps,dropped,total,label,canvas,kind}` (`kind = stream` = one pill per output), fallback `obs.stream.{active,kbps,dropped}`; `obs.record.active`; `obs.stale.{wide,tall}`; `obs.fallback.active`; `obs.fps`, `obs.render.{ms,lagged}`, `obs.encode.skipped`, `obs.stream.{congestion,lag_ms}` | per-output health, encoder health |
| `perf.gpu_ms`, `perf.frame_ms`, `perf.fps`, `perf.dropped`, `perf.late`, `perf.vram_mb`, `perf.vram_budget_mb`, `perf.pass.<name>_ms` | render timings (GPU ms > 8 lowers UI preview rates) |
| `perf.audio.{xruns,load,dsp_ms,quantum,rate,latency_ms}` | audio |
| `health.<check>` = `{status: pass\|warn\|fail, detail}`, `project.errors`, `patch.<id>.error` | warnings list |

## Live page

| Area | Reads | Sends |
|---|---|---|
| Monitors, multiview | frames.sock canvases (docs/frames-protocol.md); `render.atlas.layout` = `[{source, rect:[x,y,w,h]}]` | — |
| Scenes | query `scenes`, `config.scenes`; `show.scene.{preview,program}`, `show.transition.*`, `show.direct` | `scene.go`, `scene.cut`, `scene.take` |
| Pads | query `controllers.page` → `{page,label,keys:[{key,kind,preset?,label,icon,color,action,active,cooldown_ms?,cooldown_total_ms?}]}`, state `controllers.page`; else query `presets` | `deck.press {key,page}` / `deck.release`, else `preset.fire` |
| Active | query `active`; `lights.cuelist.<cl>.{playing,cue,master}`; `timeline.<n>.{active,status,time,length,timecode}` | `preset.release`, `release`, `lights.release {cuelist}`, `timeline.stop {name}` |
| Events | events; policy veto window `policy.pending` entries with `kind = veto` (`{id,type,user,text,reward,amount,created_ns,expires_ns}`, master-clock ns); alert veto `alerts.veto` = `[{id,kind,user,text,remaining_ms,window_ms}]` | `mod.reject {id}` / `mod.approve {id}`; `alerts.veto {id}` (kill) / `alerts.approve {id}` (pass now) |
| Chat | events `twitch.chat` (payload `message_id, message, user, user_id, color`, actor roles), removals `twitch.chat.delete {message_id}`, `twitch.user.purge {user_id}`, `twitch.chat.clear`; `twitch.eventsub.connected` | `mod.delete {message_id}`, `mod.timeout {user_id,user,duration_s}`, `mod.ban {user_id,user}`, `bot.say {args:[text]}` |
| Queue | query `queue` (re-queried when `queue.*`/`song.*` change), signal `song.position` | `queue.skip|pause|resume|open|close`, `queue.approve|reject|remove {id}`, `queue.reorder {id,to}` |
| Mod | `policy.pending` entries with `kind = approval`, `twitch.automod.queue` = `[{message_id,user,text,category,level,reason}]` (+ count `twitch.automod.held`), query `audit` | `mod.approve|reject {id}`, `mod.automod.approve|deny {message_id}` |
| Mix | `audio.bus.<b>.{gain (dB, Meta range),mute,ducked}`, signals `audio.<b>.level|peak`; `mixer.16r.{connected,channels}`, `mixer.16r.ch.<n>.{fader,mute,name,db}`, signal `mixer.16r.meter.ch.<n>` | `set` on gain/fader/mute |
| Lights bar (`lights::overview_strip`) | queries `lights.rig` (fixtures → "not set up yet"), `lights.palettes` (`look_active`), `lights.cuelists`; `lights.master`, `lights.blackout`, `lights.cuelist.<cl>.{playing,cue}`, on air (`status::on_air`) | `set lights.master` / `lights.blackout`, `lights.cue {look}` / `lights.release {look}`, `lights.go {cuelist}` / `lights.release {cuelist}` (starting on air asks first) |

## Edit pages: Scenes, Sources, Notifications, Automation, Sound, Lights

| Area | Reads | Sends |
|---|---|---|
| Scenes → Scenes | queries `scenes`, `config.scenes`, `sources`, `patches`, `transitions`; `project.read {path: scenes/<s>.toml}`; scene node state and on-air state | `project.write` for create, rename, duplicate, delete and file edits (layers, scene effects, actions, transition, background); `set` while dragging a property, `set_base` on release |
| Scenes → Effects | built-in effect catalog and `patches` for custom picture/sound effects; config scenes for usage; current `fx.*` values | `set`/`set_base`/`release` on effect defaults, `trigger fx.<id>`, `patch.new|remove|reload|enable|disable` for custom effects |
| Sources → Sources / Files | `project.sources`, `sources`, `devices`, `patches`, `patch.templates`, `scenes`, `project.read project.toml`, `project.assets` | `project.source.save|remove`, `source.assign|reopen|save_controls`, `patch.new|remove|reload`, `project.import`, `project.write` for scene placement and on-every-scene settings |
| Transitions | queries `transitions`, `config.transitions` (every file as parsed: `label`, `kind`, `shader`, `ms`, `ease`, `enter`, `exit`, settings), `config.scenes` (pools → "where it's used"), `patches` (transition-layer shader patches as starting points), `scenes`; `project.read {path: transitions/<id>.toml}`; looks and slider ranges from `se_core::transitions`; `show.scene.program`, `show.direct`, `show.live_since` / OBS output state (on air) | `project.write {path, text}` (new, edits saved 0.6 s after the last change, duplicate), `project.write {path, delete}`, `project.write {path: scenes/<s>.toml, set: {"transitions.pool": […], "transitions.name": null, "transitions.from.<s>.pool": …}}` (use in a scene / take out on delete); Try it (off air): `scene.cut {scene: from, transition: cut}` when needed, then `scene.go {to}` + `scene.take {transition, ms}` (direct mode: `scene.cut {to, transition}`) |
| Canvas editor | `scene.<s>.node.<id>.{rect,crop,radius,z}.<canvas>`, `.visible` | `set` while dragging (live preview), `set_base` on release (→ `scenes/<s>.toml`, comments kept), `release` |
| Lights | `lights.rig`, `lights.palettes`, `lights.cuelists`, `lights.output`, `lights.master`, `health.dmx` | `lights.cue|release|go|back|goto|knob|panic`, `set lights.master`, project writes for fixture and look edits |
| Notifications → Alerts / Look & timing / Read-out voice | `alerts`, `alerts.config`, `patches`, `project.assets`, `tts`, project settings | `alerts.pause|resume|veto|approve`, alert file edits via `project.write`, patch settings, `tts.skip|clear` |
| Automation → Events / Actions / Buttons / Chat / Modulation / Timelines | `rules`, `config.rules`, `presets`, `config.presets`, `controllers.deck`, `controllers.midi`, `bot.commands`, `bindings`, `config.bindings`, `timelines`, live signals | `project.write` for rules, saved actions and bindings; `deck.assign`, `midi.learn`, `bot.command.save`, timeline edits |
| Console | logs, query `errors`, `patch.<id>.error` | opens `file:line` in `$EDITOR` |

## Settings

| Reads | Sends |
|---|---|
| `twitch.auth.{status (none\|pending\|authorized\|expired\|error), login, user_code, verification_uri, expires_in_s, error, missing_scopes}` and the same under `twitch.auth.bot.*` | `twitch.auth.start|cancel|logout {account: broadcaster\|bot}` |
| query `secrets.status` → `[{name,label,set}]` | `secrets.set {name,value}`, `secrets.delete {name}` (keyring; event `secrets.changed {name}`) |
| query `api.info` → `{socket,http,ws,osc,token_set,devices:[{name,scope}]}` (the token itself is never served; the UI copies it via `streamctl token`) | `api.token.rotate`, `api.device.add {name,token,scope}`, `api.device.remove {name}` |
| layouts (`layouts/*.toml`, via `project.read`) | Screens: switch layout; "Show the program on the TV" writes `[confidence] enabled` with `project.write {path: layouts/<name>.toml, text}` |
| Devices: query `devices`, `sources` (only to show which discovered camera is assigned); `health.devices`, `mixer.16r.connected`; temporary preview leases | `devices.rescan`, `devices.rename|expect|forget`, `source.assign` for an unassigned camera |
| Backups: query `retention`, `health.{backup,recordings}`, `retention.recordings.{used_gb,budget_gb}` | `retention.backup_now|scan|prune_sessions|prune_recordings` |
| History: query `project.versions` (re-asked when `project.versions.latest` changes), `health.versions`; header "Undo last change" / "Redo" on the Scenes and Lights pages: `project.versions.{undo,redo}` | `project.version.save {label}`, `project.version.restore {id}`, `project.undo`, `project.redo` |
| Accounts & app, mods helping: `remote_mod.{enabled,active}` | `project.write {path: project.toml, set: {remote_mod.enabled}}` |
| Recordings: queries `sessions`, `clips {status: ready}`, `clips.session {session}` (one reply per stream); `clips.pending`, `clips.job.{state,stage,progress,session,queue}` | `clips.process {session}`, `clips.approve|reject|upload {id}`, `clips.retrim {id,in,out}` |

## Engine actions/queries added for the UI (se-app)

- `project.write {path, text}` / `{path, set:{dotted.key: value|null}}` / `{path, table, match|index|append, set}` / `{…, delete:true}` — config files only (`.toml`, relative, no hidden dirs, not `sessions/`/`assets/`), toml_edit round-trip, the project reloads after the write.
- `project.read {path}` → `{path, exists, text}`; `project.files {kind?}` → `[{kind,name,path}]`.
- `project.versions` → newest first `[{id, at_ms, label, auto, kind, changed, first, restored_from}]`; `project.version.save {label}`, `project.version.restore {id}`, `project.undo`, `project.redo` (see [api.md](api.md#project-versions)).

## Driving the UI from outside

Events the running UI follows (e.g. `streamctl fire ui.layout name=build`): `ui.layout {name}`,
`ui.mode {mode: show|build}`, `ui.open {view}` (panel/view id: `performance`, `settings`,
`console`, …), `omarchy.font_set {font}`.
