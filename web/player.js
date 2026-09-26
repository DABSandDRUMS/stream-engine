// stream-engine YouTube player page logic (see player.html and crates/se-songs/src/player.rs).
//
// Desired state from the engine (`song.player`):
//   {rev, active: "a"|"b", xfade_ms, a: Slot, b: Slot}
//   Slot = {entry, id, cmd: "stop"|"cue"|"play"|"pause", start, seek, seek_t, auto}
// Reports to the engine (named query `song.player`):
//   hello {page}                               → reply: desired state (+ volume)
//   state {slot, entry, id, state, t, dur}      on every player state change
//   progress {slot, entry, id, state, t, dur, stalled}   4 Hz for the active slot
//   ended {slot, entry, id} · error {slot, entry, id, code} · heartbeat {page}
(function () {
  "use strict";

  // YouTube refuses embeds (error 150) on pages whose origin is an IP literal such as
  // http://127.0.0.1:7870, while http://localhost:7870 works (verified 2026-09-25). Reload
  // under `localhost` (same port, path and token) before creating any player, without a
  // referrer: the embed reports the embedding page's referrer to YouTube (`gporigin`).
  if (location.hostname === "127.0.0.1" || location.hostname === "[::1]") {
    const meta = document.createElement("meta");
    meta.name = "referrer";
    meta.content = "no-referrer";
    document.head.appendChild(meta);
    location.replace(location.href.replace(location.hostname, "localhost"));
    return;
  }

  const STATES = { "-1": "unstarted", 0: "ended", 1: "playing", 2: "paused", 3: "buffering", 5: "cued" };
  const PROGRESS_MS = 250;
  const HEARTBEAT_MS = 2000;
  /** Media time frozen this long while "playing" = an ad or a stall (treated as buffering). */
  const STALL_MS = 1500;

  const se = Engine.connect();
  const page = Math.random().toString(36).slice(2, 10);

  function makeSlot(name) {
    return {
      name,
      el: document.getElementById("slot-" + name),
      yt: null,
      ready: false,
      entry: 0,
      id: "",
      seek: 0,
      auto: false,
      cueing: false,
      cueStart: 0,
      want: null,
      lastT: -1,
      lastMoved: 0,
      stalled: false,
    };
  }
  const slots = { a: makeSlot("a"), b: makeSlot("b") };
  let active = "a";
  let volume = 1;
  let rev = 0;
  let synced = false; // hello answered: state updates may be applied
  let lastHeartbeat = 0;

  const other = (s) => (s.name === "a" ? slots.b : slots.a);

  function report(ev, s, extra) {
    if (!se.connected) return;
    const msg = Object.assign({ ev, page }, s ? { slot: s.name, entry: s.entry, id: s.id } : {}, extra || {});
    se.query("song.player", msg).catch((e) => console.warn("report", ev, e.message));
  }

  function snapshot(s) {
    const y = s.yt;
    const st = y.getPlayerState ? y.getPlayerState() : -1;
    return {
      state: STATES[st] || "unstarted",
      t: Math.max(0, Number(y.getCurrentTime ? y.getCurrentTime() : 0) || 0),
      dur: Math.max(0, Number(y.getDuration ? y.getDuration() : 0) || 0),
    };
  }

  // ---- applying the desired state ------------------------------------------------------

  function applyVisual(xfade) {
    for (const s of Object.values(slots)) {
      if (xfade != null) s.el.style.setProperty("--xfade", xfade + "ms");
      s.el.classList.toggle("active", s.name === active);
    }
  }

  function applyVolume() {
    for (const s of Object.values(slots)) {
      if (!s.ready) continue;
      if (s.name === active && !s.cueing) {
        s.yt.setVolume(Math.round(Math.max(0, Math.min(1, volume)) * 100));
        s.yt.unMute();
      } else {
        s.yt.mute();
      }
    }
  }

  function load(s, want) {
    s.entry = want.entry;
    s.id = want.id;
    s.seek = want.seek || 0;
    s.lastT = -1;
    s.stalled = false;
    const opts = { videoId: want.id, startSeconds: want.start || 0 };
    if (want.cmd === "cue") {
      // Play muted until it actually plays (buffered), then pause at the start: gapless later.
      s.cueing = true;
      s.cueStart = want.start || 0;
      s.yt.mute();
      s.yt.loadVideoById(opts);
    } else if (want.cmd === "play") {
      s.cueing = false;
      s.yt.loadVideoById(opts);
    } else {
      s.cueing = false;
      s.yt.cueVideoById(opts);
    }
  }

  function reconcile(s, want) {
    if (!want) return;
    if (!s.ready) {
      s.want = want;
      return;
    }
    s.auto = !!want.auto;
    if (!want.id || want.cmd === "stop" || !want.entry) {
      if (s.entry) {
        s.yt.stopVideo();
        s.entry = 0;
        s.id = "";
        s.cueing = false;
      }
      return;
    }
    if (want.entry !== s.entry || want.id !== s.id) {
      load(s, want);
      return;
    }
    if ((want.seek || 0) > s.seek) {
      s.seek = want.seek;
      s.yt.seekTo(want.seek_t || 0, true);
      s.lastT = -1;
    }
    const st = s.yt.getPlayerState();
    if (want.cmd === "play") {
      if (s.cueing) {
        s.cueing = false;
        s.yt.seekTo(s.cueStart, true);
      }
      if (st !== 1) s.yt.playVideo();
    } else if (want.cmd === "pause" || (want.cmd === "cue" && !s.cueing)) {
      if (st === 1 || st === 3) s.yt.pauseVideo();
    }
  }

  function apply(d, force) {
    if (!d || typeof d !== "object") return;
    if (!force && (d.rev || 0) <= rev) return;
    rev = d.rev || 0;
    const wasActive = active;
    active = d.active === "b" ? "b" : "a";
    reconcile(slots.a, d.a);
    reconcile(slots.b, d.b);
    applyVisual(d.xfade_ms);
    applyVolume();
    if (wasActive !== active) progress(true);
  }

  // Gapless: the active song ended and the next one is buffered in the other slot.
  function handover(from) {
    const next = other(from);
    if (!next.ready || !next.entry || !next.auto) return;
    active = next.name;
    if (next.cueing) {
      next.cueing = false;
      next.yt.seekTo(next.cueStart, true);
    }
    applyVisual(null);
    applyVolume();
    next.yt.playVideo();
  }

  // ---- player events -------------------------------------------------------------------

  function onReady(s) {
    s.ready = true;
    s.yt.mute();
    if (s.want) {
      const w = s.want;
      s.want = null;
      reconcile(s, w);
      applyVolume();
    }
  }

  function onState(s, code) {
    if (code === 1 && s.cueing) {
      // preload reached playback: park it paused at the start, still muted
      s.yt.pauseVideo();
      s.yt.seekTo(s.cueStart, true);
      s.cueing = false;
      report("state", s, { state: "cued", t: s.cueStart, dur: snapshot(s).dur });
      return;
    }
    if (!s.entry) return;
    const snap = snapshot(s);
    if (code === 0) {
      report("ended", s);
      if (s.name === active) handover(s);
    }
    if (code === 1) {
      s.lastT = snap.t;
      s.lastMoved = performance.now();
      s.stalled = false;
    }
    report("state", s, snap);
  }

  function onError(s, code) {
    if (!s.entry) return;
    s.cueing = false;
    report("error", s, { code: Number(code) || 0 });
  }

  function progress(forceReport) {
    const now = performance.now();
    for (const s of Object.values(slots)) {
      if (!s.ready || !s.entry || s.cueing) continue;
      const snap = snapshot(s);
      if (snap.state === "playing") {
        if (s.lastT < 0 || Math.abs(snap.t - s.lastT) > 0.05) {
          s.lastT = snap.t;
          s.lastMoved = now;
          s.stalled = false;
        } else if (now - s.lastMoved > STALL_MS) {
          s.stalled = true;
        }
      } else {
        s.stalled = false;
      }
      if (s.name === active && (forceReport || snap.state === "playing" || snap.state === "buffering")) {
        report("progress", s, Object.assign(snap, { stalled: s.stalled }));
      }
    }
    if (now - lastHeartbeat > HEARTBEAT_MS) {
      lastHeartbeat = now;
      report("heartbeat", null, { active });
    }
  }

  // ---- engine connection ---------------------------------------------------------------

  function hello() {
    synced = false;
    se.query("song.player", { ev: "hello", page })
      .then((d) => {
        if (d && typeof d.volume === "number") volume = d.volume;
        synced = true;
        apply(d, true);
      })
      .catch((e) => {
        console.warn("hello failed:", e.message);
        setTimeout(() => se.connected && hello(), 1000);
      });
  }

  se.state("song.player", (_addr, v) => {
    if (synced) apply(v, false);
  });
  se.state("song.volume", (_addr, v) => {
    if (typeof v === "number") {
      volume = v;
      applyVolume();
    }
  });
  se.onconnect = hello;
  se.ondisconnect = () => {
    synced = false;
  };

  // ---- YouTube IFrame API --------------------------------------------------------------

  function createPlayers() {
    for (const s of Object.values(slots)) {
      s.yt = new YT.Player("player-" + s.name, {
        width: "100%",
        height: "100%",
        playerVars: {
          autoplay: 0,
          controls: 0,
          disablekb: 1,
          fs: 0,
          iv_load_policy: 3,
          playsinline: 1,
          rel: 0,
          enablejsapi: 1,
          origin: location.origin,
        },
        events: {
          onReady: () => onReady(s),
          onStateChange: (e) => onState(s, e.data),
          onError: (e) => onError(s, e.data),
        },
      });
    }
    setInterval(() => progress(false), PROGRESS_MS);
  }

  window.onYouTubeIframeAPIReady = createPlayers;
  if (window.YT && window.YT.Player) {
    createPlayers();
  } else {
    const tag = document.createElement("script");
    tag.src = "https://www.youtube.com/iframe_api";
    tag.onerror = () => console.error("YouTube IFrame API failed to load");
    document.head.appendChild(tag);
  }
})();
