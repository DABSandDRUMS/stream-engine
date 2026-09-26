.pragma library

// Pure state reduction for the stream-engine bar widget: turns `stream --json` output into the
// widget model. No QML types here, so the logic is testable with plain JS.

// State addresses the widget subscribes to (`stream --json watch --state …`).
var WATCH_STATE = [
  "show.mode",
  "show.scene.program",
  "show.scene.preview",
  "show.panic",
  "obs.link",
  "obs.stream.active",
  "obs.record.active",
  "health.**",
  "queue.pending",
  "alerts.veto_pending",
  "twitch.automod.held",
  "policy.pending.count"
]

function emptyState() {
  return {
    mode: "",
    program: "",
    preview: "",
    panic: false,
    obsLink: false,
    streaming: false,
    recording: false,
    health: ({}),
    queuePending: 0,
    alertsVeto: 0,
    automodHeld: 0,
    policyPending: 0
  }
}

function copyState(state) {
  var next = ({})
  for (var k in state) next[k] = state[k]
  var health = ({})
  for (var h in state.health) health[h] = state.health[h]
  next.health = health
  return next
}

function toInt(value) {
  var n = Number(value)
  return isFinite(n) ? Math.max(0, Math.round(n)) : 0
}

function toText(value) {
  if (value === null || value === undefined) return ""
  return typeof value === "string" ? value : JSON.stringify(value)
}

function healthEntry(value) {
  if (value && typeof value === "object") {
    return { status: String(value.status || "warn"), detail: toText(value.detail) }
  }
  return { status: "warn", detail: toText(value) }
}

// Apply one `stream --json watch` line. Returns the next state, or null when the line carries
// no state change (events, signals, logs, or unparsable output).
function applyWatchLine(state, line) {
  var msg
  try {
    msg = JSON.parse(line)
  } catch (e) {
    return null
  }
  if (!msg || typeof msg !== "object" || !msg.state || typeof msg.state !== "object") return null

  var next = copyState(state)
  for (var address in msg.state) {
    var value = msg.state[address]
    if (address.indexOf("health.") === 0) {
      next.health[address.slice(7)] = healthEntry(value)
      continue
    }
    switch (address) {
    case "show.mode": next.mode = toText(value); break
    case "show.scene.program": next.program = toText(value); break
    case "show.scene.preview": next.preview = toText(value); break
    case "show.panic": next.panic = value === true; break
    case "obs.link": next.obsLink = value === true; break
    case "obs.stream.active": next.streaming = value === true; break
    case "obs.record.active": next.recording = value === true; break
    case "queue.pending": next.queuePending = toInt(value); break
    case "alerts.veto_pending": next.alertsVeto = toInt(value); break
    case "twitch.automod.held": next.automodHeld = toInt(value); break
    case "policy.pending.count": next.policyPending = toInt(value); break
    }
  }
  return next
}

// `stream --json preflight` → the full checklist (health.* plus the engine's disk/GPU/idle checks).
// Returns the next state, or null when the output is not a checklist.
function applyPreflight(state, text) {
  var list
  try {
    list = JSON.parse(text)
  } catch (e) {
    return null
  }
  if (!Array.isArray(list)) return null
  var next = copyState(state)
  next.health = ({})
  for (var i = 0; i < list.length; i++) {
    var item = list[i]
    if (!item || typeof item !== "object" || !item.name) continue
    next.health[String(item.name)] = healthEntry(item)
  }
  return next
}

// `stream --json query engine.info` → {version, session, project, pid, started} or null.
function parseInfo(text) {
  try {
    var info = JSON.parse(text)
    return info && typeof info === "object" && !Array.isArray(info) ? info : null
  } catch (e) {
    return null
  }
}

var STATUS_RANK = { fail: 0, warn: 1, pass: 2 }

function healthSummary(health) {
  var summary = { pass: 0, warn: 0, fail: 0, problems: [] }
  for (var name in health) {
    var entry = health[name]
    var status = STATUS_RANK[entry.status] === undefined ? "warn" : entry.status
    summary[status]++
    if (status !== "pass") summary.problems.push({ name: name, status: status, detail: entry.detail })
  }
  summary.problems.sort(function(a, b) {
    return STATUS_RANK[a.status] - STATUS_RANK[b.status] || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0)
  })
  return summary
}

function pendingTotal(state) {
  return state.queuePending + state.alertsVeto + state.automodHeld + state.policyPending
}

// On air: the show is in `live` mode or OBS is sending the stream.
function isLive(state) {
  return state.mode === "live" || state.streaming
}

function modeLabel(mode) {
  return mode ? String(mode).replace(/_/g, " ").toUpperCase() : "—"
}

// Seconds → "1:02:03" / "2:03".
function formatDuration(seconds) {
  var s = Math.max(0, Math.floor(seconds))
  var h = Math.floor(s / 3600)
  var m = Math.floor((s % 3600) / 60)
  var sec = s % 60
  var pad = function(n) { return n < 10 ? "0" + n : String(n) }
  return h > 0 ? h + ":" + pad(m) + ":" + pad(sec) : m + ":" + pad(sec)
}

// Omarchy theme colors.toml → { key: "#rrggbb" }.
function parseColors(text) {
  var colors = ({})
  var lines = String(text || "").split("\n")
  for (var i = 0; i < lines.length; i++) {
    var match = lines[i].match(/^\s*([A-Za-z0-9_-]+)\s*=\s*["']?(#[0-9A-Fa-f]{6})/)
    if (match) colors[match[1]] = match[2]
  }
  return colors
}
