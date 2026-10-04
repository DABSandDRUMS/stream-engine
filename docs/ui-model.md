# UI model: what things are called and how they fit together

This is the vocabulary the app uses everywhere (labels, docs, empty states) and the hierarchy the
Edit pages follow. It borrows established broadcast terms (OBS: scenes, sources, filters →
effects, transitions; Streamer.bot: triggers → actions) instead of inventing new ones.

## The pieces

```text
Files ──► Source ──► Layer (in a Scene) ──► Canvas (Main / Vertical) ──► OBS
                        │  transform, crop, look, visibility
                        └─ Effects ─ params
                                        ▲
Trigger ──► Actions ────────────────────┤  (discrete: turn on/off, set, fire)
Signal  ──► Modulation ─────────────────┘  (continuous: follow a level, beat, LFO, fader)

Event ──► Alert (queued, temporary) ──► shown by a source that draws alerts
```

| Term | What it is | Where it lives |
|---|---|---|
| **File** | A picture, GIF, video, sound or font in the project (`assets/`). Just material. | Sources → Files |
| **Source** | Anything that produces picture or sound: a **camera/capture** device, a **media** file (video, image, GIF), a **web** page (HTML/CSS/JS), a **generative** visual (shader, particles, script), a solid **color**. A source exists once and can be placed in any number of scenes. | Sources → Sources |
| **Scene** | An ordered stack of layers, laid out for the Main (16:9) and Vertical (9:16) canvases. | Scenes → Scenes |
| **Layer** | One source placed in one scene: position/size, scale, rotation, offset, crop, corner radius, opacity, blend, when it shows, enter/exit animation, and its effects. Top of the list draws on top. | Scene inspector |
| **On every scene** | Layers that draw above every scene (the engine's overlay layer: chat box, notification box, labels). Not a separate kind of thing: just sources pinned above all scenes. | Top group of the Layers list |
| **Effect** | Processing on a layer (per scene) or on a whole scene: blur, grade, chroma key, pixelate, RGB split, VHS, glitch, shake, vignette, zoom pulse, LUT, custom shader effects. Each has an on/off switch and settings. | Layer / scene inspector; library in Scenes → Effects |
| **Transition** | How program changes from one scene to the next. | Scene inspector; library in Scenes → Transitions |
| **Trigger** | Something that happens: a button or pedal, a chat command, a stream event (follow, cheer, raid…), an audio event (kick, drop, beat), a show event (scene changed, mode), a timeline cue. | Automation |
| **Action** | What to do when a trigger fires: switch scene, turn an effect on/off, set or animate a setting, play a sound, run a light cue, show a notification, say something in chat… A **saved action** is a named list of actions any trigger can run; it can last a while, toggle, or stay on until released. | Automation → Actions |
| **Modulation** | A live link from a signal (audio level, bass, beat, LFO, MIDI fader, viewer count) to any numeric setting: layer offset follows the kick, effect strength follows the bass. | ∿ next to any setting; all links in Automation → Modulation |
| **Alert** | A temporary, queued presentation for a viewer event (follow, sub, cheer…): text, image, sound, voice, duration, priority, veto. Drawn by a source that shows alerts, placed in a scene or on every scene. | Automation → Alerts |
| **Auto sequence** | A list of scenes the show switches between by itself on a timer, in order or at random, each with its own time and transition. One runs at a time. | Automation → Auto sequence; on/off in Overview |

Rules of thumb:

- A camera is **hardware** until it becomes a **source**; hardware detection and health live in
  Settings → Devices, sources live in Sources.
- "Overlay" is not a kind of thing. Anything placed above other layers is an overlay; anything
  pinned above every scene is in the **On every scene** group.
- Effects belong to where they apply. Turning one on from a button is a **trigger → action**
  ("turn layer effect on"), not a different kind of effect.
- Discrete changes are **actions**; continuous following is **modulation**. Both target the same
  settings.
- An alert is an event-driven reaction, so it lives in Automation, but it has its own queue and
  look (one at a time, veto window). A trigger can also *show* one.

## Pages

```text
Overview   Edit   Clipping
           ├─ SHOW
           │   ├─ Scenes          Scenes · Effects · Transitions
           │   ├─ Sources         Sources · Files
           │   └─ Automation      Events · Alerts · Buttons & pedals · Chat commands · Actions · Modulation · Timelines · Auto sequence
           ├─ PRODUCTION
           │   ├─ Sound           Mix · Mixing desk · Read-out voice
           │   └─ Lights
           ├─ CHANNEL
           │   └─ Community       Chat bot · Song requests · Twitch · Goals · Giveaways
           └─ Settings            Get started · Devices · Accounts & app · Backups · History · Performance · Health · Troubleshooting
```

## Layout grammar

Every editing surface uses the same shape so nothing has to be learned twice:

- **List pane** (left, fixed width): heading, count, one `+` to create. Items grouped with small
  uppercase group labels. The list never shows its own empty-state button; the detail pane does.
- **Detail pane** (right): `detail_header` (kind icon, name, one-line description, actions), then
  **inspector sections** (collapsible, hairline-separated) of **property rows** (fixed label
  column, control, optional ∿ modulation / ⚡ trigger badges on the right).
- **Create** always starts with a kind chooser (`kind_tile`s) when there is more than one kind,
  then a draft that is not saved until **Create**.
- **Empty detail**: one sentence of what this is, one primary action.
- The Scene editor is the one three-pane surface: **Scenes + Layers** (left) · **Canvas**
  (center) · **Inspector** (right, the selected layer or the scene).

Widgets: `se_ui_kit::widgets::{split, pane_header, group_label, detail_header,
inspector_section, prop_row, kind_tile, list_row, empty_state}`.
