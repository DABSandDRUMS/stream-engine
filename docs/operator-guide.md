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
| **Scenes** | Arrange your stream and choose what happens between scenes. | **Scenes**, **Quick effects**, **Overlays**, **Transitions**, **Media** |
| **Inputs** | Check cameras and set up buttons and pedals. | **Cameras & devices**, **Buttons & pedals** |
| **Lights** | Pick a look, run cue lists, set brightness and blackout. | One console |
| **Sound** | Levels for everything your viewers hear. | **Mix**, **Mixing desk**, **Text to speech** |
| **Automation** | Things that happen on their own. | **Reactions**, **Chat commands**, **Timelines** |
| **Community** | Alerts, goals, song requests, Twitch, giveaways. | **Alerts & goals**, **Song requests**, **Twitch**, **Giveaways** |
| **Settings** | Accounts, backups, history and troubleshooting. | **Get started**, **Accounts & app**, **Backups**, **History**, **Performance**, **Troubleshooting** |

A number next to **Edit** means setup still needs attention. A number next to **Clipping**
means clips are waiting for review. Under **Edit**, **Community** also shows items waiting there.

Press `Ctrl+K` anywhere to open the command palette. Type a few letters of what you want (a
scene, an effect, "panic", "skip song") and press `Enter`.

To turn off animated movement, open **Edit → Settings → Accounts & app → Appearance** and
switch on **Reduce motion**. The choice is saved for this computer, not in the stream project.

## Building your stream

You set up scenes, transitions and the rest ahead of time, in the app. Everything you change
saves by itself.

### Scenes

Open **Scenes → Scenes**. Your scenes are on the left; the one on air has a red dot.

- **Make a scene**: click **New scene**, type a name, and pick what to **Start from**: **One
  camera**, **Side by side**, **Picture in picture**, **Three across**, **2×2 grid**, **Big + two
  small**, or **Blank**. Click **Create**. The boxes show your first cameras (cameras with a
  picture first); with fewer cameras than boxes, cameras repeat. The main and vertical videos
  each get the layout, with the cameras cropped so they aren't squashed.
- **Swap a camera**: click its layer, then pick another one under **Camera**. The box keeps its
  place, size and effects.
- **Move and resize** layers on the picture, or use **Place it** (**Full screen**, **Left half**,
  …). **Add layer** puts in another camera or an overlay.
- **An overlay's own settings**: click an overlay layer (the starting-soon title, for example).
  Its settings are right there under **Overlay settings**: the title text, the countdown, its
  colors. They're the same settings as on the **Overlays** tab, show on the video straight away,
  and count everywhere that overlay is shown.
- **When a layer shows**: under **Show this layer**, pick **Always** or **Only when…** and say
  when: **The show mode** is (or isn't) a mode, **A song request** is (or isn't) playing, or **An
  overlay** is (or isn't) playing. The starting-soon scene uses that last one: the title and chat
  wait while the boot overlay plays. **Add another check** adds more; all of them must be true.
  A layer that's waiting says **hidden right now** in the Layers list, and when you click it,
  **Hidden right now** says which check isn't true yet. **Details** shows the rule the way
  Stream Engine reads it; you can change it there and click **Use this**. A rule the simple
  choices can't show (**It has its own rule**) stays editable there, and one with a mistake
  says so, because the layer then never shows.
- **Scene settings** (top right): rename it, pick the transitions into this scene and their
  speed, add effects to the whole scene, **Duplicate** it, or hold **Delete scene**.
- **When this scene comes on** and **When it goes off** (in **Scene settings**): steps Stream
  Engine does every time you switch to (or away from) the scene, top to bottom: **Play an
  overlay**, **Fire a quick effect**, **Turn on a light look**, **Run a light cue**, **Play a
  sound**, **Change the show mode**, **Wait a moment** and more. **Add a step**, pick what it
  does, and it's saved; a step you haven't finished (nothing picked yet) says **isn't saved
  yet**. The same steps are in **Automation → Reactions**.
- **Background** (in **Scene settings**): the color wherever no layer covers the video, on both
  videos. **Use black** goes back to the default.

### Overlays

Open **Scenes → Overlays**: every overlay and effect as a card, with an on/off switch, whether
it's working, and its **Settings**. Overlays that play once (the ad break card, the countdown,
confetti) have **Try it**; on air it says **Try it on air** and asks first, because your
viewers will see it.

### Quick effects

Open **Scenes → Quick effects** to operate definitions from your project's `presets/`
folder. The list starts empty; nothing is installed, run, or assigned automatically.
Definitions and exposed knobs are configured in those project files.

Your quick effects are on the left with their pad color, what they do in one line and how many
places fire them (a search box appears when there are more than eight). Click one to open it:

- **Try it** runs it. Ones that stay on for a while (or until pressed again) show **Stop** while
  they run. On air the button says **Try it on air** and asks first, because your viewers see it.
- **Knobs** are the few things you can tune on that effect, in plain words: **Strobe speed**
  (flashes a second), **Intensity**, **How much confetti**, **How far it shakes**, **Warmth**…
  Move a knob and it's saved by itself; from then on the effect fires that way from every pad,
  pedal, reaction and chat command. While it's running you see (and hear) the change right away,
  so fire it and tune it live. **Reset knobs** puts every knob back where it started.
- **What it does** says, in words, what happens on screen, with the lights and with the sound,
  and how long it lasts (one burst, a few seconds, or until you press it again).
- **Where you can fire it** lists its pad on **Overview**, Stream Deck keys, pedals, reactions,
  chat commands, timelines and rewards. Set up physical controls in **Inputs → Buttons &
  pedals** (the button next to the list takes you there).
- If a quick effect's file has a mistake (for example a knob whose lowest setting is above its
  highest), it says **Needs a fix** in the list and explains what's wrong. Correct its
  project file, or go back to an earlier version in **Settings → History**.

### Transitions

Open **Scenes → Transitions** to choose which transitions get used where. The transitions
themselves are made for you (**Glide**, **Crossfade** and **Cut** are built in); if you want a
new look, ask for it.

- **Your transitions**: every way of switching scenes, each with a little moving sketch, one
  sentence about what it does, and where it's used.
- **Try it** (off air): pick the two scenes under **Try it** (it starts with the one on air and
  another), then click **Try it** on a transition. The picture below shows what your viewers
  would see; click again to play it back the other way. While you're on air the buttons are
  off, because they would switch scenes for your viewers.
- **How scenes switch**: the way every scene switches unless it has an exception. **Pick at
  random** from the transitions you tick (turn on **Don't repeat the last one** so the same one
  doesn't come twice in a row), or **Always use one**. With nothing picked, every switch is a
  **Crossfade**.
- **Exceptions**: click **Add an exception** for a scene (**Switching to Duo**) or for a move
  between two scenes (**From Kit to Drums**), then pick **Random** from some transitions or
  **Always** one. A scene's exception can also go back to **Use the default**. The bin icon
  removes an exception. A move between two scenes wins over the scene's own choice.
- A transition chosen on Overview for the next switch, a scene's **Always**, and a move's
  **Always** win over chat's `!transition` vote; the vote wins over random picks and over the
  default.

**Scenes → Scenes → Scene settings** shows the same choice for the scene you're editing, under
**Switching to this scene**.

### Your media: pictures, videos, sounds, color looks and fonts

**Scenes → Media** holds the files your scenes, overlays and quick effects use: pictures (logos,
backgrounds), videos (clips and loops), sounds (for quick effects, alerts and buttons), color
looks (`.cube` files that change how a camera's colors look) and fonts.

- **Add files**: drag them from your file manager onto the window while **Media** is open, or
  click **Import files…** and pick them. A card says **Adding 2 files…** while they're copied
  and **Added 2 files** when they're ready. Stream Engine keeps its own copy, so you can move or
  delete the originals. A file with a name you already have gets a number (`horn`, `horn-2`).
  Files it can't use (a `.txt`, a folder, anything over 2 GB) are refused with a note saying why.
- **See what you have**: the chips at the top (**All**, **Images**, **Videos**, **Sounds**,
  **Color looks**, **Fonts**) filter the grid; **Search your media** appears once you have more
  than eight files. Each file shows a picture (videos show a frame, sounds a waveform), its size
  and length, and what uses it ("Used by scene “Duo”" or "Not used yet").
- **Listen**: the ▶ button on a sound plays it through Stream Engine; press it again to stop.
  On air it first asks **Play it on air?**, because your viewers will hear it too.
- **Rename**: click a file, type a new name under **Name** and click **Rename**. Every scene,
  quick effect, overlay, alert or button that uses the file is updated to the new name, so
  nothing breaks.
- **Delete**: click a file, then **Delete…**. If something still uses it, the app says what
  (for example **Scene “Duo” uses it**) and offers **Delete anyway** or **Keep it**; deleting
  anyway leaves that spot empty until you pick another file.

Other editors choose files with the same small picker: the current file with its picture (or ▶
for a sound), and **Choose…**, which lists your media of the right kind with a search box and
**Import a sound…** (or picture, video…); the new file is picked for you once it's added.

### Undo a change, or go back to an earlier version

Stream Engine keeps a version of your project a few seconds after every change, whether you
made it in the app or by editing a file. Recordings and stream history aren't part of it and
never change.

- **Undo last change** (top right of the **Scenes** and **Lights** pages, or "undo last
  change" in the command palette) takes back the latest change. Press it again to step further
  back. Point at it to see what it will undo. Right after an undo, **Redo** next to it brings
  the change back.
- **Settings → History** lists every version, newest first, in plain words ("Changed 2
  things: Hype (quick effect) and Duo (scene)"). Click one to see everything that changed, then
  **Go back to this**. How things are right then is saved first, so going back can be undone
  too.
- **Save a version** gives the way things are now a name, like "Before Friday's show".

Versions from the last week are all kept, then one a day for 90 days. Named versions are kept
forever. Big media files are stored once, however many versions use them.

Details: [project versions](api.md#project-versions).

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

1. On **Overview**, look at **Cameras & sources** at the bottom. Every camera should show a
   moving picture.
2. If one is black or frozen, open **Edit → Inputs → Cameras & devices**. Each camera has a badge:
   **Working**, **Needs a look**, **Not connected** or **Off** (off just means no scene on screen
   uses it right now).
3. Just plugged something in? Click **Look again**. A camera that won't start? Click
   **Restart** on its card.

### Sound

1. At the bottom of **Overview**, the **Sound** bar has one strip per channel (**Mic**,
   **Band**, **Music**, **Sound effects**, **Read-out voice**, **Everything**, …). Each has a
   slider, a level meter and a mute button.
2. Play a few hits and talk. The meters for **Band** and **Mic** should move.
3. If the pill says OBS can't hear Stream Engine, open **Sound → Mix** and click
   **Add our sound to OBS**.
4. For the mixing desk, open **Sound → Mixing desk**. Make sure you have a saved mix named
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

### The starting-soon countdown

When you start with the starting-soon screen (next section), a **Countdown** card with
"Starting soon" and the song that's playing covers the main and vertical pictures. It runs for 5
minutes unless you change it. To change the length, or count to a clock time instead, open
**Scenes → Overlays**, find the **Countdown** card, and open **Settings**. When it
reaches zero it says "Here we go!" and waits for you.

Details: [devices and sources](devices-and-sources.md), [OBS](obs.md), [sound](audio.md),
[mixing desk](mixer.md), [lights](lights.md), [desktop integration](desktop-integration.md),
[overlays](patches.md).

## 2. Going on air

### Go live

1. If the big button says **Open OBS**, click it. OBS is what sends your stream to Twitch. When
   it's open, the button changes to **Go live**.
2. Click **Go live**. It asks **How do you want to start?**
   - **Starting-soon screen first**: OBS starts streaming and the show goes to **Starting
     soon** with the countdown.
   - **Go live right now**: OBS starts streaming and the show goes straight to **Live**.
3. If you started with the starting-soon screen, click **Go live** again in the top bar when
   you're ready. The show switches to **Live**.

The top bar now says **ON AIR** with the time since you went live.

### What the show is doing

The show button in the top bar always shows one of these. You can change it by clicking the
button, but most changes happen by themselves.

| Show button | When | What happens by itself |
|---|---|---|
| **Off air** | Not streaming. | Nothing reaches viewers. |
| **Starting soon** | After **Starting-soon screen first**. | The countdown covers the picture. |
| **Live** | The show proper. | Alerts, chat effects, reactions and the chat box all run. |
| **Be right back** | You click **Be right back** in the top bar (or press your BRB key). | The picture switches to the be-right-back scene. Music stays at full level. Alerts wait and play when you're back. Chat effects are off until you're back. Click **Go live** to come back: the picture returns to the duo scene. |
| **Ad break** | Twitch starts an ad. You don't press anything. | The picture switches to the ad-break scene: a background, "Back in a moment", a countdown of the ad length, and the song that's playing. Music isn't lowered. Alerts wait, and chat effects wait until it's over. When the ads end, the show goes back to what it was doing and the scene you had on. |
| **Ending** | You pick **Play the ending first** under **End stream**. | The credits roll over the picture. |
| **Rehearsal** | You choose it (see [Practise off air](#practise-off-air)). | Everything runs, nothing on Twitch really changes, and you're not on air. |

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

### Quick effects and the Stream Deck

**Quick effects** on **Overview** mirror the page your Stream Deck is showing, so screen and
deck always match. Scroll below the lights bar if the effect pads are below the visible area.

- Click a pad (or press `F1` to `F12`) to start it. Right-click to stop it.
- Some pads need two taps to confirm, like strobe. The deck shows **CONFIRM?** after the first
  press.
- **Running now** lists everything that's active (effects, light cue lists, timelines). Click
  the cross next to one to stop it.

In the starter setup, the deck's **SHOW** page has your scenes on the top row, effects (**HYPE**,
**CONFETTI**, **CHILL**, **CHORUS**) and **TAKE** on the middle row, and on the bottom row: page
keys (**MIX**, **FX**), the microphone key (hold it and speak a voice command), **CLEAN** and
**PANIC** (hold for one second). Your X-TOUCH and FBV footswitch can fire the same effects;
set that up in **Inputs → Buttons & pedals**.

### Lights

Your looks and cue lists are made for you together with your lights. The **Lights** page is
where you use them:

- **Looks**: tap a look to turn it on, tap it again to turn it off. **Turn all off** takes every
  look off. On air, tapping a look asks first: **Your viewers will see this** → **Turn it on**.
- Under the looks, the look you tapped shows its knobs, for example **Color** and
  **Brightness**. Changes show on your lights right away and are kept for next time; **Reset
  knobs** goes back to how the look was made. To adjust a look without turning it on, click the
  small sliders button on its tile.
- **Make a quick effect** puts that look on a pad: the new quick effect opens in **Scenes →
  Quick effects**, where you can put it on a Stream Deck key.
- **Cue lists** step through a row of looks: **Go** for the next step, **Back** for the previous
  one, **Stop** to end it. Each shows which step is on and what's next, with its knobs (for
  example a chase's **Speed**) underneath. On air, starting a cue list asks first.
- **Master**: **Brightness** sets how bright all your lights are; **Blackout** turns them all off.
- The **Overview** has a small **Lights** bar with the same brightness, blackout, looks, and the
  running cue list's **Go** and **Stop**.

Want another look, cue list or knob? Ask us and we'll add it.

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
3. Click **Fix** (or open **Settings → Devices**). Click **Restart** on the camera's card, or
   **Look again** if you re-plugged it.

### An overlay or the YouTube player drops

A crashed overlay page reloads by itself, and it keeps its last picture while it does. If the
pill says an overlay has a problem, click **Fix**. On the overlay's card in
**Scenes → Overlays**, open **Details** and click **Reload**.

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

Open **Clipping** (a number on it means clips are waiting). Choose **Recording**, **Past streams**
or **Clips**:

- **Recording**: choose the folder where OBS saves each stream's video. Recording starts in
  Starting soon or Live and stops when you go off air. Check recording health, free space and
  sound tracks before going live; Start/Stop recording is there if you need it manually.
- **Clips**: each clip shows why it was picked, the song and requester where known, and an
  informational **Risk of DMCA** badge for requested-song clips. The badge does not block clips.
  - **Keep** approves it. **Skip** rejects it.
  - **Upload** sends a kept clip with your upload command.
  - **Trim** changes where it starts and ends. Click **Cut it again** to re-cut it.
- **Past streams**: each stream has its recordings, clips, and a timeline of songs, talk, scenes,
  lights, effects, chat, hype and markers. Pick a time window, click twice for a start/end,
  adjust the times, then press **Make clip** (the selected range must fit in one recording and
  meet the shown length limits). **Make clips** finds moments automatically instead.

The **Recording** badge in the top bar confirms OBS is recording; Clipping → Recording also
shows the real destination and lets you start/stop manually. If the badge is off unexpectedly,
check the recording health message and OBS before continuing.

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
| `F1` to `F12` | Quick effect pads 1 to 12 |
| `Ctrl+Z` / `Ctrl+Shift+Z` | Undo / redo |
| `Ctrl+.` | Clear chat effects |
| hold `Ctrl+Esc` | Emergency stop |
| `Ctrl+L` | Switch to the next screen layout |
| `Ctrl+F` | Search |
| `Ctrl+=` / `Ctrl+-` | Zoom in / out |
| `Esc` | Cancel or close |
