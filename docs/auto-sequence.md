# Auto sequence

An auto sequence cycles the program scene through a list of scenes on a timer, in order or at
random, with its own time and transition per scene ("pick a handful of scenes, hit play, and it
switches between them every 45 seconds"). A preset is one file in `autoseq/<id>.toml`. It
hot-reloads and runs inside the deterministic core, so session replays reproduce every switch.
One preset runs at a time: program is single.

## Editor

In **Automation → Auto sequence**, add the scenes under **Scenes** and set **Time on scene**
on each scene's card. Each field is editable independently; **Create** or **Save** writes
the scene's `dwell` into its `[[step]]`.

**How it plays → Default time** supplies the time for scenes without an override.
Changing it does not overwrite explicitly edited scene times, even when one equals the
default. **Use default** removes a scene's override so it follows future default changes.
The sequence summary shows **per-scene times** when its effective times differ.


## File format

```toml
# autoseq/drums.toml
label = "Drum cycle"      # display name; default = the id (file stem)
order = "random"          # "in_order" (default) | "random"
avoid_repeat = 3          # random mode excludes this many recent scenes; default 1, maximum 32
dwell = "45s"             # default time on each scene; default 30s, at least 1s
transition = "fade"       # optional default transition into each scene (cut | morph | fade | transitions/<name>)
ms = "800ms"              # optional default transition duration

[[step]]
scene = "drum_cams"       # scene id (file stem in scenes/), required
dwell = "60s"             # optional per-step override
transition = "morph"      # optional per-step override (transition into this scene)
ms = "1200ms"             # optional per-step override
```

- At least one `[[step]]`; a scene may appear only once; every `dwell` is at least 1 s;
  `avoid_repeat` must be at most 32. A file breaking these rules (or not parsing) is reported
  in the `errors` query and its last good version stays live.
- Unknown scenes and transitions are reported in `errors` but don't reject the file. At run
  time steps whose scene doesn't exist are skipped.
- No `transition`/`ms` anywhere: the take picks them the normal way (scene/pair pool, chat vote,
  project default; [render.md](render.md)).

## Behaviour

- **Play:** `autoseq.play [name]` selects the preset (default: the selected one) and starts it.
  Starting a stopped sequence switches immediately: from a scene already in the list, to
  the following playable step (`in_order`) or an eligible different step (`random`); from
  outside the list, to the first playable step or an eligible random step. The new scene's
  dwell starts after that switch; no initial timer wait. A single available scene already
  on air stays. Repeating Play on the same running preset keeps its current scene and
  restarts its dwell. Fails when there is no preset or none of its scenes exist.
- **Timer:** the dwell restarts whenever `show.scene.program` changes — a sequence switch or a
  manual cut. On a step: that step's dwell; the next scene is the following step (`in_order`,
  wrapping) or a random eligible step (`random`). Random selection always excludes the scene
  on air while alternatives exist and excludes the most recent `avoid_repeat` scenes.
  If that exhausts the pool, the oldest exclusion is relaxed first. A single step stays.
  On a scene outside the list: the preset's dwell; next is the step after the last one shown
  (`in_order`) or an eligible random step.
- The next scene is picked when the dwell starts (so it can be shown) and again when the
  program changes.
- **Switch:** preview ← scene, then a take like `scene.cut`, with the step's transition and
  duration, else the preset's. The switch waits while a transition is still running.
- **Stop:** `autoseq.stop`, panic, or deleting the selected preset's file. Program stays where
  it is. Editing the file while it runs keeps it running with the new definition.
  A scene can stop cycling as soon as it takes program with `on_enter = ["autoseq.stop"]`.
  The drum project's `scenes/chatting.toml` uses this hook: selecting **Just chatting**
  from the UI, deck, or another scene control stops AUTO SCENES, including during the
  entry transition. Starting AUTO SCENES explicitly leaves chatting immediately.
- **Chat:** chat-priority origins can't start, stop, or switch it (as with scene commands).
- **Restart:** the selected preset, whether it runs, the last step, and recent scene history
  are part of runtime state; after a restart a running sequence resumes with a fresh dwell
  while preserving recent-scene exclusions.

## Actions

| Action | Args |
|---|---|
| `autoseq.play [name]` | select and start (`{name}` or first positional) |
| `autoseq.stop` | stop |
| `autoseq.toggle [name]` | play when stopped, else stop |
| `autoseq.select <name>` | choose the preset; while running, it takes over as `autoseq.play <name>` |
| `autoseq.next` | while running: switch to the pending next scene now |

Create, edit, and delete presets with `project.write` (`{path: "autoseq/<id>.toml", text}` /
`{path, delete: true}`).

## State, events, query

State (read-only, owner `autoseq`):

| Address | Type | Meaning |
|---|---|---|
| `show.autoseq.preset` | string | selected preset id (`""` = none) |
| `show.autoseq.active` | bool | running |
| `show.autoseq.step` | int | index of the program scene in the steps; -1 when it isn't a step or stopped |
| `show.autoseq.next` | string | scene that comes next (`""` when stopped) |
| `show.autoseq.next_at` | int | master-clock ns of the next switch (same clock as `show.transition.start`); 0 when stopped |
| `show.autoseq.dwell_ms` | int | length of the current dwell |

Events (origin `system`): `autoseq.started {preset}`, `autoseq.stopped {preset, reason}`
(`stop` | `panic` | `removed`), `autoseq.step {preset, scene, step, transition}` on every switch
(followed by the take's `scene.take`/`scene.changed`). Each switch is also a trace record of kind
`autoseq`, the parent of the take.

Query `autoseq`: every preset, sorted by id: `[{name, label, order: "in_order"|"random",
avoid_repeat, dwell_ms, transition, ms, steps: [{scene, dwell_ms, transition, ms}]}]`
(unset values `null`).

## Replay

The timer runs on core time and random picks draw from the core RNG, so `stream-engine replay`
regenerates the same switches from the logged `autoseq.*` and scene commands.
