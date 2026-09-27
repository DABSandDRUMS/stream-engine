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
| **Recording** | OBS is recording. |
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
| **Automation** | Decide what happens when an event or control fires (including viewer alerts), and what follows live signals. | **Events**, **Alerts**, **Buttons & pedals**, **Chat commands**, **Actions**, **Modulation**, **Timelines** |
| **Sound** | Levels for everything your viewers hear, and the read-out voice. | **Mix**, **Mixing desk**, **Read-out voice** |
| **Lights** | Set up fixtures and looks, run cue lists, set brightness and blackout. | One console |
| **Community** | Chat bot, song requests, Twitch, goals and giveaways. | **Chat bot**, **Song requests**, **Twitch**, **Goals**, **Giveaways** |
| **Settings** | Devices, accounts, backups, history and troubleshooting. | **Get started**, **Devices**, **Accounts & app**, **Backups**, **History**, **Performance**, **Troubleshooting** |

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
  whole scene. Add an effect there, then adjust its strength and settings. **Scenes → Effects**
  is the library for built-in and custom effects and their shared defaults.
- The **On every scene** group lists sources pinned above all scenes, such as notification
  pages. Select one to choose whether it appears on Main, Vertical or both. Other sources only
  appear where you add their layers.
- Click the scene itself to rename it, change its background or transition, add scene effects,
  specify actions when it starts or ends, or duplicate/delete it. A scene cannot be deleted
  while it is on air.

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
"Your microphone is silent. Is it muted or unplugged?".

For the full checklist in a terminal, use the Omarchy menu: **Stream → Preflight**. Each line
starts with `✓` (fine), `!` (check it) or `✗` (broken).

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
   In **Settings → Accounts & app → Recording**, choose the feeds the app should record.
   Make a short recording from **Clipping** and listen back to every selected feed.
   A complete mixed source stays mixed: it does not provide separate drum or backing tracks.
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

### Buttons on Overview and the Stream Deck

**Buttons** on **Overview** mirror the Stream Deck page you configured. New projects have no
assigned keys or saved actions. Use **Automation → Buttons & pedals** to assign scenes,
saved actions or show controls. Once assigned, click a button or press its deck key to run
it; right-click a running button to stop it. **Running now** lists active actions, light cue
lists and timelines.

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
- The **Overview** has a small **Lights** bar with the same brightness, blackout, looks, and the
  running cue list's **Go** and **Stop**.

Add another look or cue list under **Lights** after your fixtures are configured.

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

In Overview's right-hand column, open **Songs**:

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

Open **Chat** in Overview's right-hand column.

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
choose **Reload** under **Files** if it needs a manual restart.

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

The compact recording controls stay above the library. Configure sources, destination and
automatic capture in **Settings → Accounts & app → Recording**. With automation enabled,
capture starts in Starting soon or Live and stops when you go off air. OBS only streams.

The **Recording** badge in the top bar confirms the app is recording; **Clipping** also
lets you start/stop manually. If the badge is off unexpectedly,
check recording health and your selected sources in Settings before continuing. Source choices
are not an exhaustive list: use the manual input format/source fields for other supported feeds.

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
