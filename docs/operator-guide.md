# Operator guide: running a show

This guide is for the person behind the kit. It walks through a stream from start to finish:
getting ready, going on air, running the show, handling trouble, and wrapping up. Words in bold
are exactly what the app shows. Each section ends with links to the technical notes, in
case you (or a helper) want the details.

## The app at a glance

Stream Engine has two parts. The engine makes the picture, the sound and the lights. It
starts by itself when you log in and keeps running in the background. The app window is how
you control it. Closing the window, or the window crashing, never changes what viewers see or
hear.

The top bar is on every page:

| Part | What it tells you or does |
|---|---|
| **Off air** / **ON AIR 01:02:03** | Whether you're streaming, and for how long. **Engine offline** means the engine isn't reachable. |
| Show button (for example **Starting soon**) | What the show is doing right now. Click it to change it. It appears once the show is doing something. |
| Health pill | **All good**, **Finish setting up**, **2 things to check** (yellow) or **1 problem** (red). Click it for the list; each line has a **Fix** button that opens the right page. |
| **Recording** | The app is recording your selected sources. OBS only streams. |
| **Effects on** / **Effects off** | Click to turn every video effect off at once, or back on. While it says **Effects off** (amber), chat, rewards and automatic moments can't fire effects, effect rewards are paused on Twitch, and anything already running stops. Your own buttons still work. |
| **Lights: auto** / **Lights: normal** | **Lights: auto** lets the lights follow the music by themselves. Click it for **Lights: normal** (amber): automatic light changes stop and the lights go back to your normal look. |
| **Drum screen off** / **Drum screen on** | Moves all desktop workspaces to the Philips TV and disables both desk screens. Click again to restore the desk layout and park the TV. The same toggle is `Super+Ctrl+Alt+V`; it works even when the engine is offline. |
| **Clear chat effects** | Removes every effect viewers started from chat. |
| **Emergency stop** | Press and hold. Stops all effects and puts the lights and sound somewhere safe. |
| Big button | **Open OBS**, then **Go live**, then **End stream**. |

The three **master tabs** keep the jobs separate. **Overview** shows what is on air and what's
next, with scene, light and sound controls. **Edit** is where you set everything up before a
stream; its sidebar contains the detailed pages below. **Clipping** is for recording, past
streams and clips. The sidebar only appears under **Edit**.

| Edit page | What it's for | Tabs |
|---|---|---|
| **Scenes** | Arrange sources in scenes and choose their look. | **Scenes**, **Effects**, **Transitions** |
| **Sources** | Create cameras, media, web and generated visuals; import material. | **Sources**, **Files** |
| **Automation** | Decide what happens when an event or control fires (including viewer alerts), what follows live signals, and which scenes switch by themselves. | **Events**, **Alerts**, **Buttons & pedals**, **Chat commands**, **Actions**, **Modulation**, **Timelines**, **Auto sequence** |
| **Sound** | Levels for everything your viewers hear, and the read-out voice. | **Mix**, **Mixing desk**, **Read-out voice** |
| **Lights** | Set up fixtures and looks, run cue lists, set brightness and blackout. | One console |
| **Community** | Chat bot, song requests, Twitch, goals and giveaways. | **Chat bot**, **Song requests**, **Twitch**, **Goals**, **Giveaways** |
| **Settings** | Devices, accounts, backups, history, health and troubleshooting. | **Get started**, **Devices**, **Accounts & app**, **Backups**, **History**, **Performance**, **Health**, **Troubleshooting** |

A number next to **Edit** means setup still needs attention. A number next to **Clipping**
means clips are waiting for review. Under **Edit**, **Community** also shows items waiting there.

Press `Ctrl+K` anywhere to open the command palette. Type a few letters of what you want (a
scene, an effect, "panic", "skip song") and press `Enter`.

To turn off animated movement, open **Edit → Settings → Accounts & app → Appearance** and
switch on **Reduce motion**. The choice is saved for this computer, not in the stream project.

## Building your stream

New projects start without scenes, assigned controls, looks, alerts or automation. The starter
can include camera source declarations for the installed kit; these are reusable inputs, not
a programmed show. Device discovery and the editors work before you create any show content.
A new item is a draft until you click **Create**; existing edits use **Save** where one appears.

### Files and sources

Start at **Sources → Sources**. A source is one reusable camera, capture card, video, image,
GIF, web page or generated visual. Click **New source**, choose what it produces, fill in its
details and click **Create**. Discovered cameras are offered as choices; discovering hardware
does not put it into a scene. Select a source to see its picture and settings, change its input,
restart it, adjust its color or find the scenes using it. **Add to scene…** puts it in a scene.
Remove its layers from scenes before removing the source.

Use **Sources → Files** to import pictures, videos, sounds, color looks and fonts. You can
drag files onto that page or click **Import files…**. A video, image or GIF in Files can become
a source using **Make a source**; importing the file alone does not put it on air. Other editors
offer a file picker for the material they use.

### Scenes and layers

Open **Scenes → Scenes**. Click **New scene**, name it and choose a blank canvas or a starting
arrangement using your actual camera sources, then click **Create**. The left side lists scenes
and their layers; the center is the Main or Vertical canvas; the right side edits the selected
layer. Click a scene again to edit its own settings. A scene can be empty until you add layers.

- **Add layer** places an existing source or solid color. Drag a layer in the left list to
  change its stacking order; the front of the list draws on top. Select one to place, resize,
  crop, mask, blend, show or hide it, and choose its enter/exit animation. **Edit source**
  opens that source's reusable settings, not a second copy.
- **Effects** in the layer inspector change only that layer; **Scene effects** change the
  whole scene. Add repeated effects, reorder slots, and adjust their settings. **Enabled**
  bypasses a slot; **Chain enabled** bypasses the whole rack without losing settings.
  **Only on trigger** is a separate choice, not what disabling an effect means.
- The **On every scene** group lists sources pinned above all scenes, such as notification
  pages. Select one to choose whether it appears on Main, Vertical or both. Other sources only
  appear where you add their layers.
- Click the scene itself to rename it, change its background or transition, add scene effects,
  specify actions when it starts or ends, or duplicate/delete it. A scene cannot be deleted
  while it is on air.

For reusable looks, open **Scenes → Effects → Target racks / saved chains**. Select a source,
layer, scene, layout, group, master canvas, or final output. Check several targets and use
**Apply to checked targets** to copy a saved chain independently; editing one copy does not
change another. **Replace existing slots** replaces rather than appends. **Save this chain**
captures the selected target's authored settings. Warm stage, Monochrome, VHS tape, and
Digital breakup are available starter chains; none is applied automatically.

Select a layout to create a **Composited group** using its layer IDs and group z. Members are
combined before the group's effects run, unlike putting an effect separately on each layer.
The group acts as one layer in the stack; bypassing its FX keeps it grouped, while
**Delete group** restores individual stacking. Select the group target to edit its rack.
Master canvas/output racks remain across scene changes; final output also includes overlays.
**Effect library / defaults** retains built-in and custom effects and their shared defaults.


For the drum project's **Win 3.1 video window**, open **Scenes → Effects → Effect library / defaults**
and select **Win 3.1 video window** under **Custom effects**. Under **Defaults**, edit the side,
foot, and YouTube title fields (`SIDE CAM`, `FOOT CAM`, and `YOUTUBE` initially).
Changes save and update the picture live. Camera borders, title bars, and captions scale with
the box width: a 480-pixel-wide window has a 20-pixel title bar and 16-pixel text. This keeps
the 16:9 client picture snug against its frame, including reduced-resolution previews.
Camera layers using this frame and the YouTube player preserve their
box proportions when resized, without holding Shift. Changing either Size field also adjusts
the other dimension. The complete video remains fitted inside the frame, never stretched.
The **Window titles (automatic)** layers follow the cameras and should stay full-canvas.
Under **Defaults**, **Title bar color**, **Frame color**, and **Title text color** are shared
by the camera windows, captions, YouTube player, and chatting/alert windows.

The drum project's **Just chatting** webcam window shows a pressed Play button, a smoothly
moving playback cursor and elapsed window time. These are camera-window decorations, not
controls for the song queue; they keep moving while songs are paused. The separate floating
chat layer renders only real chat, including offline, and stays empty until messages arrive.

### Switching scenes

**Scenes → Transitions** shows the built-in ways scenes switch and any custom transitions
you make. **How scenes switch** sets the default or a scene-to-scene exception. A scene can
also pick its own transition in the scene inspector. **Try it** is available off air; switching
on air uses the preview and **Take** controls in Overview.

### Automation and alerts

**Automation → Events** adds a trigger for a stream, music, show or control event. Pick when
it happens, optional conditions and what actions run. **Automation → Actions** stores a named
sequence you can call from a button, event, chat command or timeline. **Buttons & pedals**
assigns controls; **Chat commands** assigns viewer commands. **Modulation** links a continuous
signal (a level, beat, LFO or fader) to a numeric setting. A ∿ next to a setting opens its
signal links; a ⚡ next to a switch shows what changes it and lets you add a trigger.

**Automation → Alerts** edits viewer-facing alerts (follow, sub, cheer, raid, tip), their
sound, voice and actions. Its first row, **Look & timing**, chooses the display source,
placement and queue behavior. **Sound → Read-out voice** sets up speech. Alerts are temporary
and queued, so one at a time shows; they are not scene layers or saved actions. Add a source
that draws alerts to the scene (or pin it above every scene) before expecting them on the
video.

**Automation → Auto sequence** makes the show switch between scenes by itself. Click **New auto
sequence** (the `+`), name it, then under **How it plays** choose **In order** or **Random**, the
**Default time** (for example 45 s), and optionally a **Transition** and its **Switch
length** (**Automatic** uses each scene's own transition settings). Under **Scenes**, use **Add a
scene** (or **Add all scenes**). Each scene has a labeled **Time on scene** field and its own
transition; the arrows change the order. Edited scene times stay independent even if they
equal the default. **Use default** makes that scene follow the default again.
Click **Create** (or **Save** after editing). **Play** starts it right away; you turn it on
and off during the show on the Overview.

### Undo a change, or go back to an earlier version

Stream Engine keeps project versions after changes; recordings and stream history are not part
of these versions. **Undo last change** reverses the latest project edit; **Settings → History**
shows older versions and lets you return to one. Versions from the last week are kept, then
one per day for 90 days; named versions are kept.

Details: [UI model](ui-model.md), [sources](devices-and-sources.md),
[effects and web sources](patches.md), [project versions](api.md#project-versions).

## 1. Before the stream

### Start the app

1. Press `Super+Ctrl+Alt+S`. You can also use the Omarchy menu (**Stream → Open UI**) or click
   the Stream Engine widget in the top bar of your desktop.
2. If the window says **Stream Engine isn't running**, click **Start Stream Engine**. Wait a few
   seconds. From then on it starts by itself when you log in.
3. If Overview shows **Finish setting up**, click **Continue setup**. It opens
   **Settings → Get started**, a short checklist: **Cameras**, **Twitch**, **OBS**, **Song
   requests**, **Text-to-speech voice**, and **Tips and public song queue**. When you're done,
   click **I'm done setting up**.

### Check that everything works

Look at the health pill in the top bar.

- **All good** (green): you're ready.
- **… things to check** (yellow, for example **2 things to check**): something isn't quite
  right, but you can still stream. Read the list and decide.
- **… problems** (red, for example **1 problem**): something is broken. Click the pill, then
  **Fix** next to the line. Fix it before you go on air.

The list uses plain sentences, for example "Camera 3 has no picture. Is the camera on?" or
"Your microphone is silent. Is it muted or unplugged?". **See every check** at the bottom opens
**Settings → Health**.

For the full checklist in a terminal, use the Omarchy menu: **Stream → Preflight**. Each line
starts with `✓` (fine), `!` (check it) or `✗` (broken).

### When something breaks: the alert banner and Health

Whenever a check fails, a red banner appears right under the top bar, on every page. Each line
names what broke (for example **Tips & queue link** or **Public song list page**), what the
engine says about it, and how long it has been like that ("for 3 min"; "for at least 3 min"
when it was already broken when the window connected). While you're on air, problems viewers
would notice (OBS, cameras, overlays, web sources, Twitch, the song player, the queue link and
page) show in yellow even when they're only warnings. Optional extras you never set up (tips,
the YouTube key) never raise the banner, but once they're set up their failures always do. If
the engine itself can't be reached, the banner says so first.

Each banner line has up to three buttons:

- **Recover**: only shown when a fix exists that viewers won't notice. It runs right away.
- **Details**: opens **Settings → Health**.
- **Fix with AI**: opens a diagnosis session (see below).

**Settings → Health** lists every check, worst first, with its detail and how long it has been
in its current state, plus the engine connection. Under each problem are the fixes for it, in
three kinds:

- **Viewers won't notice** (run on click): look for devices again, reconnect the mixing desk,
  reconnect the tips & queue link, sign in to Twitch again (the Twitch code page opens by
  itself), restart the queue service or the tunnel behind the public song list page, start
  Stream Engine when it isn't running.
- **Viewers would notice** (the button ends in "…" and asks first, saying exactly what changes on
  stream): reload the song player (the song playing now stops), reload one overlay, reconnect
  one camera.
- **Stops more than the broken part** (for example rebuilding the whole sound system): only
  offered while you're off air, and it asks first. Signing the web browser in, restarting the
  engine and anything that stops the stream are never offered here.

After you click, the line shows **Recovering…** and the button stays disabled. It says **Working
again** only when the check is seen passing, not when the engine merely accepted the request. If
the check is still failing after 45 seconds (5 minutes for a Twitch sign-in), it says so: try
**Fix with AI**.

**Fix with AI** (on each problem, and one at the top of Health for everything) writes a
diagnostic report to your private runtime folder (`$XDG_RUNTIME_DIR/stream-engine/diagnostics/`,
readable only by you) and opens a terminal with an AI diagnosis session in the Stream Engine
folder. The report has the on-air state, every check, the related queue and Twitch state, and
the last 150 warnings and errors. Tokens, keys, cookies, passwords and other secrets are blanked
out first. Opening the session changes nothing. The session reads first, explains the cause,
and asks you before every change; it never restarts the engine, OBS or sound while you're live.
The button is greyed out when the Stream Engine folder isn't known (start the engine first).

### Cameras

Create a camera or media source at **Sources → Sources → New source**. Discovered devices are
choices, not automatically added sources. The source detail shows its picture and whether it
needs attention. Remove a source's scene layers before removing the source itself.

1. On **Overview**, check the camera pictures in **Cameras & sources**.
2. If one is black or frozen, open **Sources → Sources**, select it and check its input,
   signal and picture settings. **Restart** reopens it.
3. If hardware has just been plugged in, open **Settings → Devices** and click **Look again**.

### Sound

Use **Sound → Mix → Add input** to choose a hardware or virtual audio capture feed, its
device channels and mix bus. **Manage inputs** lets you edit or remove saved inputs.
Nothing is captured merely because it appears in discovery, and removing the final input
leaves no hardware capture configured.

The main mixer and Overview sound bar show selected device/source names and running app audio.
Internal routing names such as `band` and `game` are not inputs; bus controls and bus effects
belong under **Sound → Mix → Advanced**.

1. Play each sound you intend to include (for example backing music, drums and a microphone).
   Confirm the appropriate meters move in Stream Engine for inputs routed through its mixer.
2. Check OBS's selected audio feed and meter for the stream; disable duplicate captures.
   In **Settings → Accounts & app → Recording**, check the **Sound tracks** list (by default
   **Mix** = your band and **Music** = song requests). Make a short recording from **Clipping**
   and listen back to every track. A complete mixed source stays mixed: it does not provide
   separate drum or backing tracks.
3. For the mixing desk, open **Sound → Mixing desk**. Make sure you have a saved mix named
   "Safe" under **Saved mixes** (see [Emergencies](#4-emergencies)).

### Lights

1. Open **Lights**. The pill in the **Master** card should say **Lights working**. **Lights not
   connected** means the lights box is unplugged or off. If the page says **Your lights aren't
   set up yet**, send us your fixture list and we'll set them up.
2. Tap a look under **Looks** and check the rig; tap it again to turn it off. The **Stage**
   picture shows what each light is doing, even if you can't see the rig from your seat.

### Practise off air

- **Rehearsal** runs everything (effects, chat effects, reactions) without going on air.
  Anything that would change something on Twitch, like a poll or a marker, is only pretended.
  Press `Ctrl+K`, type "mode rehearsal", and press `Enter`. To finish, click the show button
  (**Rehearsal**) and pick **Off air**.
- **Settings → Troubleshooting → Test events** pretends something happened (a raid, a big cheer,
  a gift bomb) so you can see your alerts and reactions. Nothing is sent to Twitch.

### A starting-soon screen

New projects do not contain a starting-soon scene or a countdown. To use one, create a scene,
add a web or generated source for its content, and select that scene before going on air.

Details: [devices and sources](devices-and-sources.md), [OBS](obs.md), [sound](audio.md),
[mixing desk](mixer.md), [lights](lights.md), [desktop integration](desktop-integration.md),
[overlays](patches.md).

## 2. Going on air

### Go live

1. If the big button says **Open OBS**, click it. OBS is what sends your stream to Twitch. When
   it's open, the button changes to **Go live**.
2. Click **Go live** and choose to start directly or in **Starting soon**. A new project
   has no starting-soon scene: create one and put it up next before using that option.
3. When you're ready, put your live scene up next and use **Take**.

The top bar now says **ON AIR** with the time since you went live. Show modes such as
**Starting soon**, **Live**, **Be right back**, **Ad break**, **Ending** and **Rehearsal**
control which configured actions are allowed. They do not create a countdown, a BRB scene,
credits or a chat box for you: add and assign those scenes and sources yourself. Rehearsal
runs the show off air without changing Twitch.

While you're on air and **Live**, the top bar has a **Be right back** button. In any other on-air
state it has a **Go live** button that takes you back to **Live**.

Details: [OBS](obs.md), [Twitch](twitch.md), [alerts and the chat bot](bot-and-alerts.md).

## 3. During the stream

### Switching scenes

**Overview** has two pictures side by side:

- **On air** (big, red outline): what your viewers see. **Main**, **Vertical** and **Both** above
  it pick which picture to show.
- **Up next** (smaller, amber outline): the scene you've picked but not shown yet.

To change what's on air:

1. Click a scene under **Scenes**, or press its number key (`1` to `9`; the number is under each
   scene). It appears in **Up next**.
2. Click **Switch to …** under **Up next**, or press `Enter`. It goes on air with a transition.

Shortcuts:

- Double-click a scene, or press `Shift` + its number, to put it on air right away.
- On the Stream Deck, the **TAKE** key does the same as **Switch**.
- `Super+Ctrl+Alt+N` / `Super+Ctrl+Alt+P` put the next / previous scene in **Up next**, even
  when the app isn't in front. `Super+Ctrl+Alt+Enter` switches.

Number keys and `Enter` don't work while you're typing in a box (like the chat box). Click
somewhere else first.

To change a scene, click **Edit scenes** (or open **Scenes**). If you edit the scene that's on
air, a red **This scene is on air** banner warns you that viewers see every change right away.

### Transitions

Under **Up next**:

- **Transition**: **Random (from the scene)** uses what you chose on **Scenes → Transitions**
  (the default, or the scene's or the move's exception). You can also pick one for the next
  switch.
- **Speed**: **Auto**, **Fast**, **Normal** or **Slow**.

To choose which transitions get used where, open **Scenes → Transitions** (see
[Transitions](#transitions) under "Building your stream").

### Auto sequence

The **Auto sequence** bar under **Scenes** switches scenes for you, on a timer. Pick one in its
list and flip the switch on. If the scene on air is part of it, that scene stays and its timer
starts; otherwise the first scene (or a random one) goes on air right away. While it runs, the
bar shows what's on and what comes next, for example "Drum cams → Crowd in 32s". **Skip** goes
to the next scene now. Switching scenes yourself doesn't stop it: the timer starts again from
the scene you picked. Flip the switch off to stop; what's on air stays. **Edit** opens
**Automation → Auto sequence**. Without any, the bar offers **Make one**.

### Buttons on Overview and the Stream Deck

**Buttons** on **Overview** mirror the Stream Deck page you configured. New projects have no
assigned keys or saved actions. Use **Automation → Buttons & pedals** to assign scenes,
saved actions or show controls. Once assigned, click a button or press its deck key to run
it; right-click a running button to stop it. **Running now** lists active actions, light cue
lists and timelines.

The drum project's **Cameras** page has three independent masters:

- **AUTO SCENES** starts/stops the existing Drum camera cycle: nine playing layouts,
  45 seconds each, excluding the three most recently shown scenes while alternatives exist.
- **AUTO LIGHTS** starts/stops the existing song-aware lighting director.
- **AUTO FX** starts/stops the musical video director. It chooses intermittent atmosphere,
  bloom, highlight sheen and edge leaks on camera sources, matched to song mood/energy
  and the lighting scheme. It waits through quiet gaps rather than constantly changing FX.
  Chat/drums can influence it, but a song must be playing. Speech and silence fade it out.

These states survive restarts. Turning **AUTO FX** off cancels pending musical starts and
fades out its owned look over three seconds. Notifications, viewer and manual effects remain
independent. **Effects off** remains the separate all-effects kill switch. Read
`context.fx.*` and `health.context.fx` for activity/selection problems; tuning and library
opt-in are documented in [context.md](context.md).

For individual video effects, open **Scenes → Effects → Effect library / defaults**, select
a built-in or custom picture effect, and expand **Automatic triggers**:

- **Auto-only enabled** is an on/off slider for that effect's automatic starts only. It does
  not disable the effect everywhere, stop a currently running effect, or block manual buttons,
  Twitch/channel-point rewards, notifications, or operator-authored rituals.
- **Minimum automatic interval** sets the minimum time between successful automatic starts,
  with **Seconds** and **Minutes** units. **0** adds no extra wait, preserving the director's
  existing musical timing. A rejected or queued request does not consume the interval.
- These operator-only preferences survive engine restarts and hot reloads. A running session's
  last successful automatic-start time survives library edits and unsuccessful selections.
  The interval is not a periodic timer: there is no guaranteed trigger every N seconds.
  The musical director still needs **AUTO FX**, a playing song, an eligible musical preset,
  suitable mood/energy, and its normal quiet gaps and hourly limits.

Saved action details in **Automation → Actions** reuse these same controls for their built-in
and custom constituent effects; changing one updates its automatic policy everywhere. A
musical candidate containing any disabled or cooling-down effect is excluded *before* the
weighted selection. Autonomous legacy peak/settle/song-peak/fill/mood rules use the same
effect policy for direct triggers, presets, and roulette choices. Notifications such as song
ended, arbitrary context rituals, and viewer events are not musical opportunities; the
existing **AUTO FX** master continues to govern the musical director alone.

For CLI/API edits use `set_base fx.<name>.auto.enabled false` or
`set_base patch.<id>.auto.interval 300` (intervals are always seconds, 0–86,400).
These are saved runtime preferences, not per-scene settings or changes to a patch's global
enabled state. Automatic start timestamps are session-local; after a restart the saved
preference remains, while the director's ordinary startup and quiet-gap safeguards apply.

### Lights

Set up actual fixtures, looks and cue lists under **Lights** before using them. Nothing
is installed for a rig automatically. Once configured:

- **Looks**: tap a look to turn it on, tap it again to turn it off. **Turn all off** takes every
  look off. On air, tapping a look asks first: **Your viewers will see this** → **Turn it on**.
- Under the looks, the look you tapped shows its knobs, for example **Color** and
  **Brightness**. Changes show on your lights right away and are kept for next time; **Reset
  knobs** goes back to how the look was made. To adjust a look without turning it on, click the
  small sliders button on its tile.
- **Cue lists** step through a row of looks: **Go** for the next step, **Back** for the previous
  one, **Stop** to end it. Each shows which step is on and what's next, with its knobs (for
  example a chase's **Speed**) underneath. On air, starting a cue list asks first.
- **Master**: **Brightness** sets how bright all your lights are; **Blackout** turns them all off.
- **Default lights**, at the top of **Master**, restores the project's configured idle room
  lighting and pauses **AUTO LIGHTS** until you explicitly turn it on again. In the drum-stream
  project, the idle room is **Main ON + pink COLORstrips** (`#ff69b4`, intensity `0.30`);
  other projects use their own configured idle look. The Stream Deck default-lights button
  uses the same no-argument `lights.default` action.
  This stops authored lighting playback, pending/outgoing layers, effects and flashes, clears
  programmer/direct manual overrides, releases latched panic, restores brightness to 100% and
  turns blackout off. Output arming, transports, safety caps and rehearsal mode stay unchanged.
  Main follows the existing verified-output idle handoff, not a direct plug toggle; preview
  alone does not prove physical lighting changed. Offline, the connected engine applies it
  immediately; on air, confirm **Your viewers will see this** → **Default lights**.
  The button is unavailable when disconnected, and the **AUTO LIGHTS paused** badge appears
  only after the engine reports that automatic lighting is off.
- The **Overview** has a small **Lights** bar with the same **Default lights** button (including
  on-air confirmation and engine-reported AUTO status), brightness, blackout, looks, and the
  running cue list's **Go** and **Stop**.

Add another look or cue list under **Lights** after your fixtures are configured.

**Configured AUTO SHOW:** the drum-stream project has an **AUTO SHOW** key on the deck's
camera page, using the same `lights.auto` switch as Overview's **Lights: auto / Lights: normal**.
Turning it on starts the current song/drum context; turning it off fades only the show's own
layers, leaving manual and viewer looks alone. It does not start or change songs.

The show combines 28 six-color palettes, 40 independent motions and eight finite accents.
Palette changes follow the song's mood; motion follows energy, sections and the measured beat.
Some motions listen to the backing track, others to your drums. Drops and landed fills get
short accents; raids and hype trains get bounded celebrations. When you talk between songs
it calms down. With no song or drums, it returns to pink overheads and Main on.

Builds wait for a sixteen-beat phrase; other regular changes wait for four beats, and drop
accents for the next beat. Regular palette/motion handoffs fade over two seconds, including
effect depth. **Sound → Mix** reports whether the beat is **locked** or **freewheel**:
freewheel means the clock is continuing without a trusted audio measurement, not that it
has detected a new tempo. Beat lock does not establish which beat is the musical bar's first.

The authored looks retain a pale overhead wash while cans and pixels trade movement.
The scanner normally holds an open white spotlight rather than a colored orbit.
Deeper dimming stays off that supporting wash; shallow breathing and brief transition
dips remain. This is recipe-level support, not a hard output floor:
brightness controls, blackout and panic still work normally. Accents hand their support
back to the running motion when they finish.
The calm **Kit body bloom** and **Track air** looks also retain pale can support:
authored cans at 0.9, bars at 0.85, and shallow 0.25 source/color depths. The ordinary
brightness/energy controls still scale them; quiet passages do not turn every can dark.

Casual can movement includes staggered chases, alternating pairs, gentle roundtables and
cross-meter exchanges. These combine deliberate beat-based movement with selected drum-hit
responses. Regular motion selection prefers reactive looks alongside mood, backing-track
preference and meaningful can choreography, while keeping its repeat history. Detected
kick/snare/hat hits use gated envelopes with 25–50 ms attack and 250–420 ms release;
continuous bass/level looks breathe rather than map raw audio directly to brightness.
Hype increases depth and motion without making every fixture flash on every hit.

Scanner position and wheel changes happen dark, settle for 750 ms, then fade the white
spot in over 750 ms. Only explicitly finite gestures add a small sweep. The neutral
logical aim is not a surveyed safe physical target; commissioning is still required.

**Auto sequence** is separate from AUTO SHOW: start **Drum cycle** to rotate camera scenes;
turning automatic lights on alone does not start scene cycling.

Flash safety is always on: never more than 3 flashes a second. A yellow **Softening flashes
now** badge means some fast flashes are being softened to keep them safe for viewers. That's
normal during strobe effects.

From the deck, the **MIX** page has **LX GO** (next step of the main cue list), **CHASE**,
**WARM** and **SAFE LX**.

### Sound

- Use the sliders in the **Sound** bar at the bottom of Overview. Double-click a slider to
  reset it. Click **Mixer** for the full **Sound → Mix** page.
- While you talk, the music gets lowered automatically. The channel shows **· lowered** while
  that happens. **Sound → Mix → Lowered while you talk** shows which channels it lowers.
- On the be-right-back and ad-break scenes the music stays at full level.
- **Sound → Mixing desk** shows the desk's **Faders** and your **Saved mixes**. Click a saved
  mix to bring it back.
- The deck's **MIX** page has mute keys for **MUSIC**, **BAND**, **SFX** and **TTS**.

### Song requests

Viewers ask for songs by typing `!sr` and a song name in chat. The requested video plays in the
YouTube player on the duo scene, with a now-playing card beside it.

Overview's right-hand column keeps chat above the lower pane. The **Songs**, **Activity**, and
**Mod** tabs select only that lower pane; **Songs** is selected by default. Both panes scroll
independently, and switching tabs does not resize or hide chat:

- The switch at the top turns requests on (**Taking requests**) or off (**Requests closed**).
- **Now playing** shows the song and who asked for it, with pause and skip buttons.
- **Waiting for your OK** lists requests that need your approval. Click **Play it** or
  **No thanks**.
- **Up next** is the queue. Hover a song to move it up or down, or remove it.
- **Few searches left today** means YouTube's daily search allowance is nearly used up.

**Community → Song requests** has more: **Skip**, **Hold to ban this song**, and the request
rules. The deck's **SONGS** page has **SKIP**, **PAUSE**, **RESUME**, **OPEN SR** and
**CLOSE SR**. Saying "next song" into the voice key also skips. Your mods can use chat commands
too (see the song request notes below).

### Alerts and the "Show this alert?" window

Follows, subs, cheers, raids and tips pop up on stream one at a time.

When a viewer's alert includes a message, it waits a few seconds (3 by default) so you or a mod
can stop it. In Overview's right-hand column, **Activity** shows a **Show this alert?**
card with a countdown:

- **Skip**: it never shows on stream.
- **Show now**: it shows right away.
- Do nothing: it shows when the countdown runs out.

Things viewers trigger with their own text (channel points, bits) can wait the same way, with a
**Show this on stream?** card.

**Community → Alerts & goals** shows the alert **On screen now** (with **Skip** and
**Remove**), the alerts **Waiting**, and **Hold alerts** / **Resume alerts** to pause them. Click
any line in **Activity** to see what it set off.

### Chat moderation

Chat stays at the top of Overview's right-hand column. There is no separate Chat tab:
**Songs**, **Activity**, and **Mod** change only the lower pane. Switching these tabs keeps
your unsent message and chat search.

The **Twitch** row above the messages shows **N watching**: Twitch's current stream viewer
count, not the number of people in chat. It updates automatically using the existing Twitch
polling interval (30 seconds by default). **Offline** means Twitch reports no live stream;
**Viewers unavailable** means no count is available yet, the engine is disconnected, or
Twitch needs authorization. An unknown count is never shown
as zero.

The window retains the latest 500 chat messages separately from engine activity. Beats,
scene changes, and other engine events do not push chat out. Twitch message deletion,
timeouts/bans that purge a user's messages, and full chat clears still remove the affected
messages. This is window-local history, not an archive restored after closing the app.

- Hover a message for two quick buttons: time the viewer out for 10 minutes, or delete the
  message.
- Right-click a message for more: **Delete message**, **Time out 1 minute**, **Time out 10
  minutes**, **Time out 1 hour**, and **Hold to ban**.
- Type in **Say something as your bot…** and click **Send** to talk as your bot.

The **Mod** tab lists what needs your OK:

- **Needs your OK**: requests like channel-point redemptions. Click **Approve** or **Reject**.
- **Held back by Twitch**: messages AutoMod is holding. Click **Allow** or **Deny**.
- **All clear** means nothing is waiting. **History** (under the list) shows everything anyone
  did.

**Community → Twitch → Moderation** has the same lists on a full page.

### Polls and predictions

Open **Community → Twitch → Polls & predictions**.

- **Poll**: type a question and at least two choices, pick how long it **Runs for**, then click
  **Start poll**. **End poll now** stops it early.
- **Prediction**: type what might happen and the outcomes, then click **Start prediction**.
  Viewers bet channel points. Click **Close betting** when it's time, then click the
  **"…" happened** button for the outcome that came true. **Hold to cancel & refund** gives
  everyone their points back.

While a poll or prediction runs, a card with the live results appears on the main picture. When
it ends, the result stays up for a few seconds, then the card goes away by itself.

### Ad breaks

Twitch runs ads on a schedule. **Community → Twitch → Stream** has an **Ads** card:

- **Next ad in** counts down to the next ad. It turns yellow in the last two minutes.
- **Snooze 5 min** pushes the ad back five minutes. The button shows how many snoozes you have
  left; when they're used up it says **No snoozes left**.

When the ad starts you don't need to do anything. The show switches to **Ad break**, viewers see
the ad-break scene with a countdown and music, and alerts and chat effects wait. When the ads
end, the show goes back to where it was. While it runs, the **Ads** card says **Ad break
running — chat effects wait until it's over.**

If you click **Go live** or change the show yourself during the break, your choice wins.

### Markers and "clip that"

A marker notes a good moment so it can become a clip after the stream. Stream Engine adds
markers by itself when chat, cheers or your playing get exciting. To add one yourself:

- Press the **MARKER** key on the deck's **MIX** page.
- Hold the microphone key and say "add marker" (you can add a few words, like "add marker great
  fill").
- Press `Ctrl+K` and pick "add session marker".
- Viewers can type `!clip`. When three viewers do, that makes a marker.

Each of these also puts a marker on your Twitch video. **Community → Twitch → Stream → Mark
this moment** (**Add marker**) adds only a Twitch marker.

Details: [Stream Deck, controllers and voice](controllers.md), [lights](lights.md),
[sound](audio.md), [mixing desk](mixer.md), [song requests](song-requests.md),
[alerts and the chat bot](bot-and-alerts.md), [Twitch](twitch.md), [clips](clips.md).

## 4. Emergencies

Stay calm. Most problems fix themselves within a few seconds, and none of the buttons below end
the stream.

### Emergency stop

Press and hold **Emergency stop** in the top bar, or hold `Ctrl+Esc` for one second, or hold
the deck's **PANIC** key for one second. It:

- stops every effect and timeline, and anything waiting to happen,
- stops the auto sequence,
- puts the lights on the safe look,
- puts the sound back to its normal mix and stops sounds,
- brings back your "Safe" mix on the mixing desk,
- pauses alerts.

The pill then says **Emergency stop is on**. When things are calm again, click **Clear chat
effects** (or press `Ctrl+.`). That ends the emergency stop and lets alerts play again.

The lights stay on the safe look until you choose another look on the **Lights** page (or stop
the safe cue list in **Running now**).

### Clear chat effects

**Clear chat effects** (`Ctrl+.`, the deck's **CLEAN** key) removes only what viewers started
from chat. Your own effects keep running. Use it when chat gets out of hand.

### Blackout and the safe look

On the **Lights** page, in the **Master** card (also in the **Lights** bar on the Overview):

- The **Blackout** switch turns every light off at once (**All lights off** shows next to it).
  Switch it off to bring the lights back.
- Hold **Go to safe look** to put the lights on the safe look. It stops light effects and cue
  lists, and nothing else.

The deck's **FX** page also has a **BLACKOUT** effect (tap twice to confirm) that fades the
picture to black and blacks out the lights.

### Safe mix on the mixing desk

On **Sound → Mixing desk**, hold **Go to safe mix** to bring back the mix named "Safe". Save it
before your first show: set the desk the way you want, type "safe" next to
**Save current mix**, and click it. Without it, the page tells you there's no mix named "Safe"
yet.

**Sound → Mix** has **Reset sound** (hold): it stops sounds and effects and puts the channels
back to their normal levels.

### A camera drops

The pill says, for example, "Camera 3 has no picture. Is the camera on?".

1. Switch to a scene that doesn't use that camera.
2. Check the camera's power and cable.
3. Click **Fix** or open **Sources → Sources**, select that camera and click **Restart**.
   After reconnecting the device, use **Settings → Devices → Look again**.

### A web source or the YouTube player drops

A crashed web source reloads itself and keeps its last picture while it does. If the health
pill reports a source problem, click **Fix**; select the source at **Sources → Sources** and
choose **Reload** under **Files** if it needs a manual restart. **Settings → Health** offers
**Reload the song player…** and **Reload <overlay>…** too; they ask first because viewers see
(or hear) that one source restart.

### OBS drops

- **"OBS isn't getting the main video"**: the link between Stream Engine and OBS dropped. OBS
  switches to its **Technical Difficulties** screen by itself, with sound still going, and
  switches back when the picture returns.
- **"OBS isn't open"**: OBS closed, so the stream to Twitch stopped. Open OBS again. Then press
  `Ctrl+K` and pick "start stream (OBS)".

### The app or the engine

- If the app window froze or closed, press `Super+Ctrl+Alt+S` to open it again. The stream
  isn't affected.
- If the top bar says **Engine offline**, the engine is restarting. It restarts by itself and
  comes back with the same scene, show state and song queue within a few seconds. OBS shows the
  last picture or the **Technical Difficulties** screen meanwhile.
- If the window says **Stream Engine isn't running** and it doesn't come back, click
  **Start Stream Engine**.

### From the keyboard or a terminal

These keys work even when the Stream Engine window isn't in front:

| Keys | Does |
|---|---|
| `Super+Ctrl+Alt+S` | Open or bring up the app |
| `Super+Ctrl+Alt+Enter` | Switch (**Up next** goes on air) |
| `Super+Ctrl+Alt+Esc` | Emergency stop |
| `Super+Ctrl+Alt+C` | Clear chat effects |
| `Super+Ctrl+Alt+B` | Be right back / back to Live |
| `Super+Ctrl+Alt+N` / `P` | Next / previous scene to **Up next** |
| `Super+Ctrl+Alt+V` | Drum screen: desk ⇄ Philips TV (replaces the standalone TV on/off shortcut) |

The Omarchy menu (**Stream**) has **Go Live**, **BRB**, **Preflight** and **Panic** too.

If the app won't open at all, open a terminal and type one of these:

| Command | Does |
|---|---|
| `streamctl panic` | Emergency stop |
| `streamctl clean` | Clear chat effects |
| `streamctl brb` | Be right back / back to Live |
| `streamctl take` | Switch (**Up next** goes on air) |
| `streamctl scene duo --cut` | Put the duo scene on air right away |
| `streamctl next` / `streamctl prev` | Next / previous scene to **Up next** |
| `streamctl do autoseq.stop` | Stop the auto sequence (what's on air stays) |
| `streamctl mode live` | Set the show to **Live** (also `offline`, `preshow`, `brb`, `outro`, …) |
| `streamctl marker` | Add a marker |
| `streamctl undo` | Undo the last change |
| `streamctl do mixer.panic` | Bring back the "Safe" mix on the desk |
| `streamctl preflight` | The pre-show checklist |
| `streamctl status` | Is the engine running, and what's the show doing |

If a command says it cannot reach the engine, the engine isn't running.

Details: [desktop integration](desktop-integration.md), [OBS fallback scene](obs.md),
[web pages and overlays](web.md), [lights safety](lights.md), [mixing desk](mixer.md),
[sound](audio.md).

## 5. After the stream

### Going off air

1. Click **End stream**. It asks **End the stream?**
   - **Play the ending first**: the show goes to **Ending** and the credits roll (this stream's
     subs, gifts, cheers, tips, raids, follows, and your top chatters).
   - **Stop now**: OBS stops streaming and the show goes **Off air** straight away.
2. If you played the ending, click **End stream** again when the credits are done, then
   **Stop now**.

When you go off air, Stream Engine stops recording, closes this stream's history, and builds
the show timeline and clips in the background. This can take a while after a long show.

### Review clips

Open **Clipping** to browse the recordings library. Each recorded stream has a thumbnail,
date, duration and clip count.

1. Open a stream to see the clips made from it in a thumbnail grid.
2. Hover a clip for a muted preview. Click it to open focused playback and editing.
3. Review the clip's context, song and requester where known. **Risk of DMCA** is informational;
   it does not block a clip.
4. **Approve** keeps it; **Reject** removes it from the review queue. **Upload** sends an approved clip with your upload command.
   Adjust the start/end times in clip detail and use **Apply trim** to re-cut it.

Use the selected stream's source timeline when you want to find another moment manually.
Pick a time window, click twice for a start/end, adjust the times, then press **Make clip**
(the selected range must fit in one recording and meet the shown length limits).
**Make clips** finds moments automatically instead.

The header above the library shows the recorder (**Recording** / **Recorder ready**, what is
being recorded, its size so far and the free space) and the archive (**Archiving 42%**,
**2 shows to archive**, **Archive paused** or **Archive up to date**; hover it for details).
**Start recording** / **Stop recording** work by hand; **Settings** opens the recording settings.

### Recording and the archive

Every show is recorded automatically. Recording starts when the show goes to **Starting soon**
or **Live** and keeps going until you're off air; OBS only streams. The **Recording** badge in
the top bar confirms it. Set it up in **Settings → Accounts & app**:

- **Recording now** shows what is recording (for example "Recording main + 2 cameras"), how
  big it is so far, the free space, and any camera that had to pause (for example "paused:
  computer busy"). Below it is the archive queue: each show waiting or being archived, its
  step (**Waiting**, **Camera cleanup**, **Shrinking video**, **Checking**, **Keeping full
  quality**, **Packing data**, **Moving**), progress and the reason it waits or failed.
  **Archive now** starts without waiting after the show; **Pause** stops archive work until you
  click **Resume archiving** (it continues where it stopped, also after a restart); **Retry**
  next to a failed show, or **Retry failed**, tries again. These buttons only work while you're
  off air; while live, recording, or streaming they are greyed out and the card says why.
  Archive work also stops by itself within seconds when you go live and carries on afterwards.
- **Record every stream automatically** is on by default. Turning it off shows **Streams won't
  be recorded**: nothing is recorded unless you start it on **Clipping**.
- **Main recording** is the whole show picture with every sound track; clips are made from it.
  **Picture quality** is **High** by default (full quality for clips). **Also record the tall
  picture** keeps the phone-shaped canvas as an extra picture-only file.
- **Cameras** lists your camera sources. Each can be **Off**, **720p** or **1080p**; it is kept
  as its own picture-only file for clips from another angle. One main camera at 1080p and the
  others at 720p is recommended. A camera that is no longer a source is shown in red and must be
  turned off (or added back in Sources) before saving.
- **Sound tracks**: use one **Mix** track when your interface already carries the whole show.
  If Apple Music goes through the mixing desk, it is already included; do not add it again.
  Separate tracks only help when you actually have separate feeds. **Add sound track** / the
  bin button add and remove tracks; a fully mixed input cannot have its music separated later.
- **Recording folder** and **Archive folder**: type a path or click **Browse…**. Each shows the
  free space, warns when the folder doesn't exist yet or is on the system drive (a separate
  drive is safer), and the card warns when there isn't room for a 3-hour show. A blank archive
  folder means **Archive** inside the recording folder.
- **Shrink finished shows into the archive** (on by default): some time after the show (20
  minutes off air by default, never while you're live) the full-quality main recording is made
  smaller and moved, with the show's data and exported clips, to the archive folder. The
  originals are deleted only after the archived copy is checked. **Archive size per 3-hour show**
  sets how big each show gets (3–12 GB, recommended 5.4 GB); the line under it estimates the
  picture quality and how many more shows fit.
- **Delete camera files after their clips are reviewed, or after N days** (14 by default):
  only the separate camera files are deleted, never while you're on air and never for shows
  marked to keep. The main recording is always kept.
- **More options**: which show modes start recording, frames per second (60 by default),
  video encoding, the main recording's exact quality, the space to keep free (camera files stop
  first, then the main recording), a backup recording folder used if the recording folder is
  missing when a show starts, when archiving starts, archive sound quality, exact capture inputs,
  and the show index choices.

Click **Save recording settings**; changes apply to the next recording. **Discard changes**
goes back to what's saved. The health list shows **Recording** and **Archive** problems; their
**Fix** button opens these settings. Details: [recording](clips.md#recording-and-show-data),
[archive](clips.md#after-the-show-archive).

### Backups and disk space

Open **Settings → Backups**.

- **Backup**: Stream Engine saves a copy of your settings and history every day (songs,
  counters, clips and paired phones). It waits while you're on air. Click **Back up now** for an
  extra one. **How to restore a backup** has the steps.
- **Space for recordings**: how full the recordings folder is. Old recordings are only deleted
  when you hold **Clean up old recordings**. Nothing is deleted while you're on air or
  recording.
- **Stream history**: streams older than 90 days are tidied up; the newest ones are always
  kept. Hold **Delete old stream history** to do it now.

Details: [clips](clips.md), [credits, backups and retention](extras.md).

## Keyboard shortcuts

Change any of these in **Settings → Accounts & app → Keyboard shortcuts**.

| Key | Does |
|---|---|
| `Ctrl+K` | Command palette |
| `Tab` | Jump between **Overview** and **Edit** |
| `1` to `9` | Scene to **Up next** |
| `Enter` | Switch (**Up next** goes on air) |
| `Shift+1` to `Shift+9` | Scene straight on air |
| `F1` to `F12` | Saved action pads 1 to 12 |
| `Ctrl+Z` / `Ctrl+Shift+Z` | Undo / redo |
| `Ctrl+.` | Clear chat effects |
| hold `Ctrl+Esc` | Emergency stop |
| `Ctrl+L` | Switch to the next screen layout |
| `Ctrl+F` | Search |
| `Ctrl+=` / `Ctrl+-` | Zoom in / out |
| `Esc` | Cancel or close |
