// Public song-queue page (`/queue`): now playing, upcoming, requester, position. Live over
// `/queue/ws`; shows "offline" while the engine isn't connected. No third-party resources
// except YouTube thumbnails.

import type { Env } from "./env";
import { escapeHtml } from "./util";

const CSP =
  "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; img-src https://i.ytimg.com; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const STYLE = `
:root{color-scheme:dark;--bg:#0f1115;--card:#171a21;--fg:#e8eaf0;--dim:#8b93a7;--accent:#7aa2f7;--ok:#9ece6a;--bad:#f7768e;font-family:system-ui,-apple-system,"Segoe UI",sans-serif}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg)}
main{max-width:760px;margin:0 auto;padding:24px 16px 48px}
header{display:flex;align-items:center;gap:12px;margin-bottom:20px}
h1{font-size:1.4rem;margin:0;flex:1}
.pill{font-size:.8rem;padding:3px 10px;border-radius:999px;background:var(--card);color:var(--dim)}
.pill.on{color:var(--ok)}.pill.off{color:var(--bad)}
.now{display:flex;gap:16px;background:var(--card);border-radius:14px;padding:14px;margin-bottom:22px}
.now img{width:192px;height:108px;object-fit:cover;border-radius:10px;background:#000;flex:none}
.now .meta{min-width:0;flex:1;display:flex;flex-direction:column;gap:4px}
.now .title{font-weight:600;font-size:1.05rem;overflow-wrap:anywhere}
.dim{color:var(--dim);font-size:.9rem}
.bar{height:6px;background:#262b36;border-radius:3px;margin-top:auto;overflow:hidden}
.bar i{display:block;height:100%;width:0;background:var(--accent)}
.time{display:flex;justify-content:space-between;font-size:.8rem;color:var(--dim);font-variant-numeric:tabular-nums}
h2{font-size:1rem;color:var(--dim);font-weight:500;margin:0 0 8px}
ol{list-style:none;margin:0;padding:0}
li{display:grid;grid-template-columns:2.2em 1fr auto;gap:10px;align-items:baseline;padding:9px 10px;border-radius:10px}
li:nth-child(odd){background:var(--card)}
li .pos{color:var(--dim);text-align:right;font-variant-numeric:tabular-nums}
li .t{overflow-wrap:anywhere}li .d{color:var(--dim);font-variant-numeric:tabular-nums}
.empty{color:var(--dim);padding:10px}
li.empty{display:block}
@media (max-width:520px){.now{flex-direction:column}.now img{width:100%;height:auto;aspect-ratio:16/9}}
`;

export function queuePage(env: Env): Response {
  const title = escapeHtml(env.QUEUE_TITLE || "Song queue");
  const html = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>${title}</title><style>${STYLE}</style></head>
<body><main>
<header><h1>${title}</h1><span id="flags" class="pill" hidden></span><span id="status" class="pill">connecting…</span></header>
<section class="now" id="now" hidden>
  <img id="thumb" alt="">
  <div class="meta">
    <div class="title" id="now-title"></div>
    <div class="dim" id="now-channel"></div>
    <div class="dim" id="now-user"></div>
    <div class="bar"><i id="bar"></i></div>
    <div class="time"><span id="pos">0:00</span><span id="dur">0:00</span></div>
  </div>
</section>
<p class="empty" id="idle">Nothing playing right now.</p>
<h2 id="up-head">Up next</h2>
<ol id="upcoming"></ol>
</main><script src="/queue.js"></script></body></html>`;
  return new Response(html, {
    headers: {
      "content-type": "text/html; charset=utf-8",
      "content-security-policy": CSP,
      "x-content-type-options": "nosniff",
      "referrer-policy": "no-referrer",
      "cache-control": "public, max-age=60",
    },
  });
}

const SCRIPT = `"use strict";
(() => {
  const $ = (id) => document.getElementById(id);
  let state = { online: false, snapshot: null };
  let skew = 0; // relay clock − local clock (ms)
  const fmt = (s) => {
    s = Math.max(0, Math.floor(s || 0));
    const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, x = String(s % 60).padStart(2, "0");
    return h ? h + ":" + String(m).padStart(2, "0") + ":" + x : m + ":" + x;
  };
  const position = () => {
    const n = state.snapshot && state.snapshot.now;
    if (!n) return 0;
    let p = n.position || 0;
    if (n.playing && state.online && n.at) p += (Date.now() + skew - n.at) / 1000;
    return Math.min(Math.max(0, p), n.duration || p);
  };
  function render() {
    const s = state.snapshot || {};
    const st = $("status");
    st.textContent = state.online ? "live" : "offline";
    st.className = "pill " + (state.online ? "on" : "off");
    const flags = [];
    if (s.open === false) flags.push("requests closed");
    if (s.paused) flags.push("paused");
    $("flags").hidden = flags.length === 0;
    $("flags").textContent = flags.join(" · ");
    const n = s.now;
    $("now").hidden = !n;
    $("idle").hidden = !!n;
    if (n) {
      $("now-title").textContent = n.title || "";
      $("now-channel").textContent = n.channel || "";
      $("now-user").textContent = n.user ? "requested by " + n.user : "";
      const src = /^[A-Za-z0-9_-]{11}$/.test(n.video || "") ? "https://i.ytimg.com/vi/" + n.video + "/mqdefault.jpg" : "";
      if ($("thumb").getAttribute("src") !== src) $("thumb").setAttribute("src", src);
      $("dur").textContent = fmt(n.duration);
    }
    const ol = $("upcoming");
    ol.replaceChildren();
    const up = Array.isArray(s.upcoming) ? s.upcoming : [];
    for (const e of up) {
      const li = document.createElement("li");
      const pos = document.createElement("span"); pos.className = "pos"; pos.textContent = e.pos;
      const t = document.createElement("span"); t.className = "t";
      const title = document.createElement("div"); title.textContent = e.title || "";
      const who = document.createElement("div"); who.className = "dim"; who.textContent = e.user ? "requested by " + e.user : "";
      t.append(title, who);
      const d = document.createElement("span"); d.className = "d"; d.textContent = fmt(e.duration);
      li.append(pos, t, d);
      ol.append(li);
    }
    if (!up.length) {
      const li = document.createElement("li"); li.className = "empty"; li.textContent = "The queue is empty.";
      ol.append(li);
    }
    const more = (s.length || 0) - up.length;
    $("up-head").textContent = "Up next" + (s.length ? " (" + s.length + ")" : "") + (more > 0 ? " — showing " + up.length : "");
    tick();
  }
  function tick() {
    const n = state.snapshot && state.snapshot.now;
    if (!n) return;
    const p = position();
    $("pos").textContent = fmt(p);
    $("bar").style.width = n.duration ? Math.min(100, (p / n.duration) * 100) + "%" : "0";
  }
  let attempt = 0;
  function connect() {
    const ws = new WebSocket((location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/queue/ws");
    let ping = null;
    ws.onopen = () => { attempt = 0; ping = setInterval(() => ws.readyState === 1 && ws.send("ping"), 30000); };
    ws.onmessage = (ev) => {
      if (ev.data === "pong") return;
      let m; try { m = JSON.parse(ev.data); } catch { return; }
      if (typeof m.now === "number") skew = m.now - Date.now();
      if (m.t === "queue") state = { online: !!m.online, snapshot: m.snapshot || null };
      else if (m.t === "status") state.online = !!m.online;
      render();
    };
    ws.onclose = () => {
      clearInterval(ping);
      $("status").textContent = "reconnecting…";
      $("status").className = "pill off";
      setTimeout(connect, Math.min(30000, 1000 * 2 ** attempt++));
    };
  }
  setInterval(tick, 500);
  connect();
})();
`;

export function queueScript(): Response {
  return new Response(SCRIPT, {
    headers: { "content-type": "text/javascript; charset=utf-8", "x-content-type-options": "nosniff", "cache-control": "public, max-age=300" },
  });
}
