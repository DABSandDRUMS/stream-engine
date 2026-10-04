// stream-engine YouTube player page logic (see player.html and crates/se-songs/src/player.rs).
//
// Desired state from the engine (`song.player`):
//   {rev, required_channel, required_delegate, active: "a"|"b", xfade_ms, a: Slot, b: Slot}
//   Slot = {entry, id, cmd: "stop"|"cue"|"play"|"pause", start, seek, seek_t, auto}
// Reports to the engine (named query `song.player`):
//   hello {page}                               → reply: desired state (+ volume)
//   account {page, verified, channel, delegate, error}  metadata-only iframe identity proof
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
  const ACCOUNT_CHECK_MS = 20000;
  const ACCOUNT_TIMEOUT_MS = 12000;
  const YOUTUBE_ORIGIN = "https://www.youtube.com";
  /** Media time frozen this long while "playing" = an ad or a stall (treated as buffering). */
  const STALL_MS = 1500;

  const se = Engine.connect();
  Win31Theme.bind(se);
  // Project window captions stay editable alongside the camera titles. Keep native
  // chrome at 1x on the main canvas even when CEF renders the larger vertical placement.
  const caption = document.querySelector(".win-cap");
  se.state("patch.win31_video.youtube_title", (_, value) => {
    if (caption) caption.textContent = String(value ?? "");
  });
  let windowWidth = 0;
  function fitWindowChrome() {
    if (windowWidth > 0) {
      document.documentElement.style.setProperty("--pixel", `${innerWidth / windowWidth}px`);
    }
  }
  se.state("scene.drum_cams.node.youtube.rect.wide", (_, rect) => {
    windowWidth = rect[2] * 1920;
    fitWindowChrome();
  });
  addEventListener("resize", fitWindowChrome);

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
      verified: false,
      authorized: false,
      parking: false,
      work: Promise.resolve(),
    };
  }
  const slots = { a: makeSlot("a"), b: makeSlot("b") };
  let active = "a";
  let volume = 1;
  let rev = 0;
  let synced = false; // hello answered: state updates may be applied
  let lastHeartbeat = 0;
  let requiredChannel = "";
  let requiredDelegate = "";
  let accountVerified = false;
  let generation = 0;
  let nonceNumber = 0;
  let pageCheck = null;
  const pendingChecks = new Set();
  let connectionNumber = 0;
  let waitingDesired = null;

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
  // ---- exact embedded-account lock -----------------------------------------------------

  function accountStatus(verified, error) {
    accountVerified = verified;
    document.documentElement.dataset.youtubeAccount = verified ? "verified" : "blocked";
    document.documentElement.dataset.youtubeAccountError = error;
    // Keep the owner's editable caption intact; its tooltip carries the account status.
    if (caption) caption.title = verified
      ? `YouTube account verified: ${requiredChannel}`
      : error;
  }

  function invalidateChecks() {
    generation++;
    for (const cancel of Array.from(pendingChecks)) cancel();
  }

  function stopSlot(s) {
    s.auto = false;
    s.authorized = false;
    s.cueing = false;
    s.parking = false;
    s.entry = 0;
    s.id = "";
    if (s.ready) {
      // One broken iframe must not prevent the other slot from being stopped.
      try { s.yt.mute(); } catch (_) {}
      try { s.yt.stopVideo(); } catch (_) {}
    }
  }

  function blockAccount(error, channel, delegate) {
    invalidateChecks();
    accountStatus(false, error);
    for (const s of Object.values(slots)) {
      s.verified = false;
      stopSlot(s);
    }
    report("account", null, {
      verified: false, channel: channel || "", delegate: delegate || "", error,
    });
  }

  function configuredAccount() {
    return /^UC[A-Za-z0-9_-]{22}$/.test(requiredChannel) && !!requiredDelegate;
  }

  function pageVerified() {
    return configuredAccount() && Object.values(slots).every((s) => s.ready && s.verified);
  }

  function reportVerifiedAccount() {
    accountStatus(true, "");
    report("account", null, {
      verified: true, channel: requiredChannel, delegate: requiredDelegate, error: "",
    });
  }

  function current(epoch) {
    return epoch === generation && synced && se.connected;
  }

  function verifyAccount(s, epoch) {
    if (!current(epoch) || !s.ready) return Promise.resolve(false);
    if (!configuredAccount()) {
      blockAccount("YouTube account blocked: configure the exact Covers channel and delegated session.");
      return Promise.resolve(false);
    }
    let iframe, target, nonce;
    try {
      iframe = s.yt.getIframe();
      const url = new URL(iframe.src);
      if (iframe.tagName !== "IFRAME" || !s.el.contains(iframe) ||
          url.origin !== YOUTUBE_ORIGIN || !url.pathname.startsWith("/embed/")) {
        throw new Error("not the actual YouTube embed");
      }
      target = iframe.contentWindow;
      if (!target) throw new Error("missing iframe window");
      nonce = `${page}-${++nonceNumber}-${crypto.getRandomValues(new Uint32Array(4)).join("-")}`;
    } catch (_) {
      blockAccount("YouTube account blocked: the embedded account bridge is unavailable.");
      return Promise.resolve(false);
    }
    return new Promise((resolve) => {
      let timer;
      const finish = (ok) => {
        clearTimeout(timer);
        removeEventListener("message", receive);
        pendingChecks.delete(cancel);
        resolve(ok);
      };
      const cancel = () => finish(false);
      const fail = (error, channel, delegate) => {
        finish(false);
        if (current(epoch)) blockAccount(error, channel, delegate);
      };
      const receive = (event) => {
        const result = event.data;
        if (event.origin !== YOUTUBE_ORIGIN || event.source !== target ||
            !result || result.type !== "se.youtube.account.result" || result.nonce !== nonce) return;
        if (!current(epoch)) {
          finish(false);
          return;
        }
        if (s.yt.getIframe() !== iframe || iframe.contentWindow !== target) {
          fail("YouTube account blocked: the checked iframe was replaced.");
          return;
        }
        const channel = typeof result.channel === "string" ? result.channel : "";
        const delegate = typeof result.delegate === "string" ? result.delegate : "";
        if (result.verified !== true || result.error) {
          fail(`YouTube account blocked: ${result.error || "embedded identity was not verified."}`, channel, delegate);
        } else if (channel !== requiredChannel) {
          fail(`YouTube account blocked: channel ${channel || "(missing)"} is not the required Covers channel ${requiredChannel}.`, channel, delegate);
        } else if (delegate !== requiredDelegate) {
          fail("YouTube account blocked: the embedded delegated session does not match Covers.", channel, delegate);
        } else {
          s.verified = true;
          finish(true);
        }
      };
      pendingChecks.add(cancel);
      addEventListener("message", receive);
      timer = setTimeout(() => {
        fail("YouTube account blocked: embedded identity check timed out.");
      }, ACCOUNT_TIMEOUT_MS);
      try {
        target.postMessage({
          type: "se.youtube.account.check", nonce,
          required_channel: requiredChannel, required_delegate: requiredDelegate,
        }, YOUTUBE_ORIGIN);
      } catch (_) {
        fail("YouTube account blocked: the embedded account bridge could not be reached.");
      }
    });
  }

  function queueWork(s, job) {
    const epoch = generation;
    s.work = s.work.then(() => {
      if (current(epoch) && s.ready) return job(epoch);
    }).catch(() => {
      if (current(epoch)) blockAccount("YouTube account blocked: an embedded player operation failed.");
    });
  }

  // This is the only entrance to media-changing methods, including muted preloads and seeks.
  async function media(s, epoch, operation) {
    if (!current(epoch) || !accountVerified || !pageVerified()) return false;
    if (!await verifyAccount(s, epoch) || !current(epoch) || !accountVerified) return false;
    reportVerifiedAccount();
    s.authorized = true;
    operation();
    return true;
  }

  function checkPage() {
    if (!synced || !se.connected || (pageCheck && pageCheck.epoch === generation)) return;
    const check = { epoch: generation };
    pageCheck = check;
    const ready = Object.values(slots).filter((s) => s.ready);
    if (!ready.length) {
      pageCheck = null;
      return;
    }
    Promise.all(ready.map((s) => verifyAccount(s, check.epoch))).then((ok) => {
      if (!current(check.epoch) || !ok.every(Boolean)) return;
      if (pageVerified()) {
        reportVerifiedAccount();
        for (const s of Object.values(slots)) reconcile(s);
        applyVolume();
      }
    }).catch(() => {
      if (current(check.epoch)) blockAccount("YouTube account blocked: embedded identity verification failed.");
    }).finally(() => {
      if (pageCheck !== check) return;
      pageCheck = null;
      // The second onReady may have arrived while the first iframe was being checked.
      if (current(check.epoch) && Object.values(slots).some((s) => s.ready && !s.verified)) checkPage();
    });
  }

  accountStatus(false, "YouTube account blocked: embedded Covers identity has not been verified.");

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
      if (accountVerified && s.verified && s.authorized && s.name === active && !s.cueing) {
        s.yt.setVolume(Math.round(Math.max(0, Math.min(1, volume)) * 100));
        s.yt.unMute();
      } else {
        s.yt.mute();
      }
    }
  }

  async function load(s, want, epoch) {
    const opts = { videoId: want.id, startSeconds: want.start || 0 };
    if (!await media(s, epoch, () => {
      s.entry = want.entry;
      s.id = want.id;
      s.seek = want.seek || 0;
      s.lastT = -1;
      s.stalled = false;
      s.cueing = want.cmd === "cue";
      s.cueStart = want.start || 0;
      if (s.cueing) s.yt.mute();
      if (want.cmd === "cue" || want.cmd === "play") {
        // A hidden preload is playback too; it has exactly the same identity gate.
        s.yt.loadVideoById(opts);
      } else {
        s.yt.cueVideoById(opts);
      }
    })) return;
    if (current(epoch)) applyVolume();
  }

  function reconcile(s) {
    queueWork(s, async (epoch) => {
      const want = s.want;
      if (!want || !want.id || want.cmd === "stop" || !want.entry || !accountVerified) return;
      s.auto = !!want.auto;
      if (want.entry !== s.entry || want.id !== s.id) {
        await load(s, want, epoch);
        return;
      }
      // Pausing never needs permission; seek/cue/play always do.
      if (want.cmd === "pause") s.yt.pauseVideo();
      if ((want.seek || 0) > s.seek) {
        if (!await media(s, epoch, () => {
          s.seek = want.seek;
          s.yt.seekTo(want.seek_t || 0, true);
          s.lastT = -1;
        })) return;
      }
      if (!current(epoch)) return;
      if (want.cmd === "play") {
        if (s.cueing) {
          if (!await media(s, epoch, () => {
            s.yt.seekTo(s.cueStart, true);
            s.cueing = false;
          })) return;
        }
        if (!current(epoch)) return;
        if (s.yt.getPlayerState() !== 1) {
          if (!await media(s, epoch, () => s.yt.playVideo())) return;
        }
      } else if (want.cmd === "pause" || (want.cmd === "cue" && !s.cueing)) {
        const st = s.yt.getPlayerState();
        if (st === 1 || st === 3) s.yt.pauseVideo();
      } else if (want.cmd === "cue" && s.cueing && s.yt.getPlayerState() === 2) {
        parkCue(s);
      }
      if (current(epoch)) applyVolume();
    });
  }

  function apply(d, force) {
    if (!d || typeof d !== "object") return;
    if (!force && (d.rev || 0) <= rev) return;
    invalidateChecks();
    const channel = typeof d.required_channel === "string" ? d.required_channel : "";
    const delegate = typeof d.required_delegate === "string" ? d.required_delegate : "";
    if (channel !== requiredChannel || delegate !== requiredDelegate) {
      accountStatus(false, "YouTube account blocked: the configured Covers identity needs verification.");
      for (const s of Object.values(slots)) {
        s.verified = false;
        stopSlot(s);
      }
    }
    requiredChannel = channel;
    requiredDelegate = delegate;
    rev = d.rev || 0;
    const wasActive = active;
    active = d.active === "b" ? "b" : "a";
    for (const s of Object.values(slots)) {
      s.want = d[s.name] || null;
      s.parking = false;
      if (!s.want || !s.want.id || s.want.cmd === "stop" || !s.want.entry) stopSlot(s);
    }
    if (!configuredAccount()) {
      blockAccount("YouTube account blocked: configure the exact Covers channel and delegated session.");
      return;
    }
    if (accountVerified) {
      reconcile(slots.a);
      reconcile(slots.b);
    } else {
      // Backend holds both slots at stop until this metadata-only proof is reported.
      checkPage();
    }
    applyVisual(d.xfade_ms);
    applyVolume();
    if (wasActive !== active) progress(true);
  }

  // Checked handover: the next slot is buffered, but fresh account proof comes before play.
  function handover(from) {
    const next = other(from);
    if (!accountVerified || from.name !== active || !next.ready || !next.entry || !next.auto) return;
    next.auto = false;
    queueWork(next, async (epoch) => {
      if (from.name !== active) return;
      if (next.cueing) {
        if (!await media(next, epoch, () => {
          next.yt.seekTo(next.cueStart, true);
          next.cueing = false;
        })) return;
      }
      await media(next, epoch, () => {
        // The active-slot change cancels all checks for the former slot arrangement.
        invalidateChecks();
        active = next.name;
        applyVisual(null);
        next.yt.playVideo();
        applyVolume();
      });
    });
  }

  // ---- player events -------------------------------------------------------------------

  function onReady(s) {
    invalidateChecks();
    s.ready = true;
    accountStatus(false, "YouTube account blocked: verifying the embedded Covers identity.");
    for (const slot of Object.values(slots)) {
      slot.verified = false;
      stopSlot(slot);
    }
    checkPage();
  }

  function parkCue(s) {
    // Pause immediately; even the parking seek must get a fresh identity proof.
    s.yt.pauseVideo();
    if (s.parking) return;
    s.parking = true;
    queueWork(s, async (epoch) => {
      if (!s.cueing) return;
      if (!await media(s, epoch, () => {
        s.yt.seekTo(s.cueStart, true);
        s.cueing = false;
        s.parking = false;
      })) return;
      if (current(epoch)) {
        report("state", s, { state: "cued", t: s.cueStart, dur: snapshot(s).dur });
      }
    });
  }

  function onState(s, code) {
    // Only playing/buffering is media. `stopVideo()` (every song end, stop, or re-verify)
    // emits "cued" (5) on a slot that was just cleared; blocking on it re-blocked the page
    // after each successful check, so the queue never recovered after a song ended.
    if ((code === 1 || code === 3) &&
        (!accountVerified || !s.verified || !s.authorized || !s.entry)) {
      blockAccount("YouTube account blocked: media started without an approved embedded identity.");
      return;
    }
    if (code === 1 && s.cueing) {
      parkCue(s);
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
      if (!accountVerified || !s.verified || !s.authorized || !s.ready || !s.entry || s.cueing) continue;
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
    const connection = ++connectionNumber;
    waitingDesired = null;
    invalidateChecks();
    accountStatus(false, "YouTube account blocked: verifying the embedded Covers identity.");
    for (const s of Object.values(slots)) {
      s.verified = false;
      s.want = null;
      stopSlot(s);
    }
    se.query("song.player", { ev: "hello", page })
      .then((d) => {
        if (!se.connected || connection !== connectionNumber) return;
        if (d && typeof d.volume === "number") volume = d.volume;
        synced = true;
        apply(d, true);
        if (waitingDesired) apply(waitingDesired, false);
        waitingDesired = null;
      })
      .catch((e) => {
        if (connection !== connectionNumber) return;
        console.warn("hello failed:", e.message);
        setTimeout(() => se.connected && connection === connectionNumber && hello(), 1000);
      });
  }

  se.state("song.player", (_addr, v) => {
    if (synced) apply(v, false);
    else if (!waitingDesired || (v && v.rev > waitingDesired.rev)) waitingDesired = v;
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
    connectionNumber++;
    waitingDesired = null;
    blockAccount("YouTube account blocked: the engine connection was lost.");
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
    setInterval(checkPage, ACCOUNT_CHECK_MS);
  }

  addEventListener("pagehide", () => {
    synced = false;
    invalidateChecks();
    for (const s of Object.values(slots)) stopSlot(s);
  });

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
