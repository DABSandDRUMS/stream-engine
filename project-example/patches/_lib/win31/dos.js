// dos.js: helpers shared by the win31 overlay pieces (patches/terminal_*). Load after
// /engine.js and /web/overlay.js:
//
//   <link rel="stylesheet" href="/patches/_lib/win31/dos.css">
//   <script src="/patches/_lib/win31/dos.js"></script>
//
// Viewer text is always inserted with textContent / DOM nodes, never innerHTML.
(function (global) {
  "use strict";

  const COLS = 80, CELL_W = 12, CELL_H = 24, WIDTH = COLS * CELL_W;

  // Scale a `.dos` screen to the page. `rows` > 0: a fixed block of that many text rows,
  // fitted inside the page and centred across it; 0: 80 columns across the full width with as
  // many rows as the height holds.
  function fit(el, rows = 0) {
    const apply = () => {
      const W = innerWidth, H = innerHeight;
      if (!W || !H) return;
      const s = rows ? Math.min(W / WIDTH, H / (rows * CELL_H)) : W / WIDTH;
      el.style.setProperty("--s", s);
      el.style.height = `${rows ? rows * CELL_H : H / s}px`;
      el.style.left = rows ? `${(W - WIDTH * s) / 2}px` : "0";
    };
    addEventListener("resize", apply);
    apply();
  }

  // A full-width box line: `line("╔", "═", "╗")`.
  const line = (left, fill, right) => left + fill.repeat(COLS - 2) + right;

  const pad = (n, w = 2) => String(n).padStart(w, "0");
  // 04:51, 1:04:51
  function mmss(sec) {
    sec = Math.max(0, Math.floor(sec));
    const hh = Math.floor(sec / 3600), mm = Math.floor((sec % 3600) / 60), ss = sec % 60;
    return (hh ? hh + ":" + pad(mm) : pad(mm)) + ":" + pad(ss);
  }

  // Bright VGA colours for chat names, picked by login so a viewer keeps theirs.
  const NAME = ["#55ffff", "#ff55ff", "#ffff55", "#55ff55", "#ff5555", "#5555ff", "#ffffff", "#00aaaa", "#aa00aa", "#ffaa00"];
  function colorFor(login) {
    let x = 0;
    for (const c of String(login || "")) x = (x * 31 + c.charCodeAt(0)) >>> 0;
    return NAME[x % NAME.length];
  }

  const sleep = ms => new Promise(r => setTimeout(r, ms));

  // Keep a value in engine state (`patch.<id>.<key>`) so a reloaded page continues; writes
  // made while disconnected are sent on reconnect.
  function keeper(se, address) {
    let pending = false, value = null;
    const send = () => {
      if (!se.connected) { pending = true; return; }
      pending = false;
      se.set(address, value).catch(() => {});
    };
    const prev = se.onconnect;
    se.onconnect = m => {
      if (prev) prev(m);
      if (pending) send();
    };
    return v => { value = v; send(); };
  }

  global.Dos = { COLS, CELL_W, CELL_H, WIDTH, fit, line, mmss, colorFor, sleep, keeper };
})(typeof window !== "undefined" ? window : globalThis);
