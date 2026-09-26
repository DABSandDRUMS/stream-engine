// engine.js — browser client for the stream-engine WebSocket API.
// Used by web patches (overlays, alert boxes, chat box), the YouTube player page, and tools.
//
//   <script src="/engine.js"></script>
//   const se = Engine.connect();              // token from ?token=…, server from the page origin
//   se.on("twitch.cheer", e => …);            // event patterns: `*` one segment, `**` any
//   se.state("patch.confetti.**", (addr, v) => …);
//   se.signals("band.*", values => …, 30);    // {name: value} at 30 Hz
//   se.cmd("preset.fire hype");               // any one-line command (subject to the token scope)
//   se.set("patch.confetti.count", 40);
//   await se.get("show.*");                   // [{address, value}]
//   await se.query("presets");
//   se.patch                                   // {id, params, env, trigger} for pages under /patches/<id>/
//
// Reconnects automatically; subscriptions are restored.
(function (global) {
  "use strict";

  function matches(pattern, s) {
    if (pattern === "**" || pattern === s) return true;
    const p = pattern.split("."), a = s.split(".");
    const seg = (x, y) => {
      if (x === "*") return true;
      if (!x.includes("*")) return x === y;
      const re = new RegExp("^" + x.split("*").map(t => t.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join(".*") + "$");
      return re.test(y);
    };
    const rec = (i, j) => {
      if (i === p.length) return j === a.length;
      if (p[i] === "**") {
        for (let k = j; k <= a.length; k++) if (rec(i + 1, k)) return true;
        return false;
      }
      return j < a.length && seg(p[i], a[j]) && rec(i + 1, j + 1);
    };
    return rec(0, 0);
  }

  class Engine {
    static connect(opts = {}) {
      return new Engine(opts);
    }

    constructor(opts) {
      const q = new URLSearchParams(location.search);
      this.token = opts.token || q.get("token") || "";
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      this.url = opts.url || `${proto}//${location.host || "127.0.0.1:7870"}/ws`;
      this.req = 1;
      this.pending = new Map();
      this.eventSubs = [];
      this.stateSubs = [];
      this.signalSubs = [];
      this.values = new Map();
      this.connected = false;
      this.onconnect = null;
      this.ondisconnect = null;
      const m = location.pathname.match(/\/patches\/([^/]+)\//);
      this.patch = m ? { id: m[1], params: {}, env: 0, trigger: null } : null;
      if (this.patch) {
        const id = this.patch.id;
        this.state(`patch.${id}.**`, (addr, v) => {
          const rest = addr.slice(`patch.${id}.`.length);
          if (rest === "env") this.patch.env = v;
          else if (!rest.includes(".")) this.patch.params[rest] = v;
        });
        this.on(`patch.${id}.trigger`, e => {
          this.patch.trigger = e.payload;
        });
      }
      this._open();
    }

    _open() {
      this.ws = new WebSocket(this.url);
      this.ws.onopen = () => {
        this._send({ t: "hello", client: "web", token: this.token, version: 1 });
      };
      this.ws.onmessage = m => this._recv(JSON.parse(m.data));
      this.ws.onclose = () => {
        const was = this.connected;
        this.connected = false;
        for (const [, p] of this.pending) p.reject(new Error("disconnected"));
        this.pending.clear();
        if (was && this.ondisconnect) this.ondisconnect();
        setTimeout(() => this._open(), 1000);
      };
    }

    _send(msg) {
      if (this.ws && this.ws.readyState === 1) this.ws.send(JSON.stringify(msg));
    }

    _resubscribe() {
      const hz = Math.max(1, ...this.signalSubs.map(s => s.hz), 1);
      this._send({
        t: "subscribe",
        sub: {
          events: this.eventSubs.map(s => s.pattern),
          state: this.stateSubs.map(s => s.pattern),
          signals: this.signalSubs.map(s => s.pattern),
          signal_hz: this.signalSubs.length ? hz : null,
          logs: false,
          trace: false,
        },
      });
    }

    _recv(m) {
      switch (m.t) {
        case "welcome":
          this.connected = true;
          this._resubscribe();
          if (this.onconnect) this.onconnect(m);
          break;
        case "event":
          for (const s of this.eventSubs) if (matches(s.pattern, m.event.type)) s.fn(m.event);
          break;
        case "state":
          for (const [a, v] of m.changes) {
            this.values.set(a, v);
            for (const s of this.stateSubs) if (matches(s.pattern, a)) s.fn(a, v);
          }
          break;
        case "signals": {
          const obj = {};
          for (const [n, v] of m.values) obj[n] = v;
          for (const s of this.signalSubs) {
            const sel = {};
            for (const n in obj) if (matches(s.pattern, n)) sel[n] = obj[n];
            s.fn(sel);
          }
          break;
        }
        case "values":
        case "reply":
        case "explain": {
          const p = this.pending.get(m.req);
          if (p) {
            this.pending.delete(m.req);
            if (m.error) p.reject(new Error(m.error));
            else p.resolve(m.t === "values" ? m.entries : m.t === "explain" ? m.provenance : m.value);
          }
          break;
        }
        case "ack": {
          const p = m.req != null && this.pending.get(m.req);
          if (p) {
            this.pending.delete(m.req);
            if (m.ok) p.resolve(m.id);
            else p.reject(new Error(m.error || "failed"));
          }
          break;
        }
        case "error":
          console.warn("engine:", m.msg);
          break;
      }
    }

    _ask(msg) {
      const req = this.req++;
      msg.req = req;
      return new Promise((resolve, reject) => {
        this.pending.set(req, { resolve, reject });
        this._send(msg);
      });
    }

    on(pattern, fn) {
      this.eventSubs.push({ pattern, fn });
      if (this.connected) this._resubscribe();
      return this;
    }

    state(pattern, fn) {
      this.stateSubs.push({ pattern, fn });
      if (this.connected) this._resubscribe();
      return this;
    }

    signals(pattern, fn, hz = 30) {
      this.signalSubs.push({ pattern, fn, hz });
      if (this.connected) this._resubscribe();
      return this;
    }

    value(address) {
      return this.values.get(address);
    }

    cmd(text) {
      return this._ask({ t: "cmd_text", text });
    }

    set(address, value) {
      return this._ask({ t: "cmd", cmd: { origin: "patch", op: { kind: "set", address, value } } });
    }

    emit(type, payload) {
      return this._ask({ t: "cmd", cmd: { origin: "patch", op: { kind: "emit", type, payload: payload ?? null } } });
    }

    get(pattern, meta = false) {
      return this._ask({ t: "get", pattern, meta });
    }

    explain(address) {
      return this._ask({ t: "explain", address });
    }

    query(name, args = null) {
      return this._ask({ t: "query", name, args });
    }
  }

  global.Engine = Engine;
  global.Engine.matches = matches;
})(typeof window !== "undefined" ? window : globalThis);
