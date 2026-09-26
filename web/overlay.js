// overlay.js — helpers shared by the overlay patches (alertbox, chatbox, goals, labels,
// eventlist, countdown, credits). Load after engine.js:
//
//   <script src="/engine.js"></script>
//   <script src="/web/overlay.js"></script>
//
// Everything viewer-supplied is inserted with textContent / DOM nodes, never innerHTML.
(function (global) {
  "use strict";

  const h = (tag, cls, text) => {
    const el = document.createElement(tag);
    if (cls) el.className = cls;
    if (text != null) el.textContent = String(text);
    return el;
  };

  // Only http(s) and same-origin engine paths may become image sources.
  const safeUrl = u => {
    const s = String(u || "");
    if (/^https:\/\//i.test(s) || /^http:\/\/(127\.0\.0\.1|localhost)[:/]/i.test(s)) return s;
    if (/^\/(assets|patches|web)\/[^\s"'<>]*$/.test(s)) return s;
    return "";
  };

  const img = (url, cls, alt) => {
    const u = safeUrl(url);
    if (!u) return null;
    const el = document.createElement("img");
    el.className = cls || "";
    el.alt = alt || "";
    el.decoding = "async";
    el.referrerPolicy = "no-referrer";
    el.src = u;
    return el;
  };

  // Third-party + Twitch emote/badge sets from the `emotes` query (refreshed every 10 min).
  class Emotes {
    constructor(se) {
      this.se = se;
      this.map = new Map();
      this.badges = {};
      this.ready = this.load();
      setInterval(() => this.load(), 10 * 60 * 1000);
      se.onconnect = (prev => m => { this.load(); if (prev) prev(m); })(se.onconnect);
    }
    async load() {
      try {
        const r = await this.se.query("emotes");
        if (r && r.emotes) this.map = new Map(Object.entries(r.emotes));
        if (r && r.badges) this.badges = r.badges;
      } catch (_) {
        // no emote provider (Twitch not connected): plain text
      }
    }
    get(code) {
      return this.map.get(code);
    }
    badgeUrl(b) {
      return (b && (b.url || this.badges[`${b.set_id}/${b.id}`])) || "";
    }
  }

  const emoteNode = (e, text) => {
    const el = img(e.url, "emote" + (e.zero_width ? " zw" : ""), text);
    if (el) el.title = text;
    return el || document.createTextNode(text);
  };

  // Plain text → text + emote nodes (words matching the emote map).
  function renderText(text, emotes) {
    const frag = document.createDocumentFragment();
    const words = String(text || "").split(/(\s+)/);
    let buf = "";
    const flush = () => {
      if (buf) frag.appendChild(document.createTextNode(buf));
      buf = "";
    };
    for (const w of words) {
      const e = emotes && w.trim() && emotes.get(w);
      if (e && e.url) {
        flush();
        frag.appendChild(emoteNode(e, w));
      } else buf += w;
    }
    flush();
    return frag;
  }

  // Chat fragments (from `chat.message`) → nodes. Text fragments also get third-party
  // emote matching (the simulator and non-Twitch sources send plain text).
  function renderFragments(frags, emotes) {
    const out = document.createDocumentFragment();
    for (const f of frags || []) {
      if (f.type === "emote" && f.url) out.appendChild(emoteNode(f, f.text || ""));
      else if (f.type === "mention") out.appendChild(h("span", "mention", f.text));
      else if (f.type === "cheermote") out.appendChild(h("span", "cheermote", f.text));
      else out.appendChild(renderText(f.text, emotes));
    }
    return out;
  }

  function badgeNodes(badges, emotes) {
    const out = document.createDocumentFragment();
    for (const b of badges || []) {
      const el = img(emotes ? emotes.badgeUrl(b) : b.url, "badge", b.set_id || "");
      if (el) out.appendChild(el);
    }
    return out;
  }

  const fmtNum = n => {
    const x = Number(n || 0);
    return Number.isInteger(x) ? x.toLocaleString("en-US") : x.toLocaleString("en-US", { maximumFractionDigits: 2 });
  };

  const SYMBOLS = { USD: "$", EUR: "€", GBP: "£", JPY: "¥", CAD: "CA$", AUD: "A$", BRL: "R$" };
  const fmtMoney = (amount, currency) => {
    const c = String(currency || "").toUpperCase();
    const n = Number(amount || 0).toFixed(2);
    return SYMBOLS[c] ? SYMBOLS[c] + n : c ? `${n} ${c}` : n;
  };

  // Patch params: engine state `patch.<id>.<param>`, overridable with `?param=value` in the URL
  // (handy when testing a page in a normal browser), falling back to `defaults`.
  function params(se, defaults, onChange) {
    const q = new URLSearchParams(location.search);
    const current = {};
    const coerce = (v, d) => (typeof d === "number" ? Number(v) : typeof d === "boolean" ? v === true || v === "true" || v === 1 : v);
    for (const k in defaults) current[k] = q.has(k) ? coerce(q.get(k), defaults[k]) : defaults[k];
    if (se.patch) {
      se.state(`patch.${se.patch.id}.*`, (addr, v) => {
        const k = addr.slice(`patch.${se.patch.id}.`.length);
        if (!(k in defaults) || q.has(k) || v == null) return;
        current[k] = coerce(v, defaults[k]);
        if (onChange) onChange(current, k);
      });
    }
    return current;
  }

  // Apply a color param (hex `#rrggbb[aa]` or `[r,g,b,a]` 0–1) to a CSS variable.
  function cssColor(v) {
    if (Array.isArray(v)) {
      const [r, g, b, a] = v.map(Number);
      return `rgba(${Math.round(r * 255)},${Math.round(g * 255)},${Math.round(b * 255)},${a == null ? 1 : a})`;
    }
    return /^#[0-9a-f]{3,8}$/i.test(String(v)) ? String(v) : "";
  }

  global.Overlay = { h, img, safeUrl, Emotes, renderText, renderFragments, badgeNodes, fmtNum, fmtMoney, params, cssColor };
})(typeof window !== "undefined" ? window : globalThis);
