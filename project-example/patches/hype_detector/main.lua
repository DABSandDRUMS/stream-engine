-- Hype detector, script edition (§18). Same model as the built-in se-clips detector:
-- viewer reactions (chat, emotes, `!clip` votes, bits) are shifted back by the stream delay,
-- everything is scored per 100 ms of *screen time* once `delay + settle` has passed, and an
-- episode above the threshold becomes a marker {start, peak, end, score, reasons} whose window
-- starts 10–30 s before the peak. Time here is this VM's clock (`time`); markers are written
-- with windows relative to "now" (`start_ago`, `peak_ago`, `end_ago`), which the clip job
-- converts to the master clock.

local BUCKET, RING, WARMUP, SETTLE = 0.1, 1200, 30.0, 2.0
local CHAT_WINDOW, BASELINE, TAU, VOTE_WINDOW = 5.0, 300.0, 8.0, 30.0
local HOLD, MERGE_GAP, MAX_LEN, POSTROLL, RELEASE = 2.0, 8.0, 60.0, 5.0, 0.6
local W = { chat = 0.8, emotes = 0.5, copypasta = 0.08, votes = 0.4, bits = 0.6, subs = 0.25, gifts = 0.4,
            raid = 0.5, tip = 0.5, hype_train = 0.5, drop = 0.5, mic = 0.5, laughter = 0.6, novelty = 0.3 }
local IMPULSES = { "bits", "subs", "gifts", "raid", "tip", "hype_train", "drop" }
local SHIFTED = { chat = true, emotes = true, votes = true, bits = true }
local EMOTES = {}
for _, e in ipairs({ "LUL", "LULW", "KEKW", "OMEGALUL", "Pog", "PogU", "PogChamp", "POGGERS", "Kreygasm", "HYPERS",
                     "PepeLaugh", "monkaS", "catJAM", "EZ", "Clap", "WAYTOODANK", "PauseChamp", "KomodoHype",
                     "SeemsGood", "Kappa", "LETSGO", "GIGACHAD" }) do EMOTES[e] = true end

local ring, next_eval, evaluated = {}, nil, 0
local recent, votes = {}, {}
local impulse = {}
for _, k in ipairs(IMPULSES) do impulse[k] = 0 end
local chat_base = { mean = 0, var = 0, n = 0 }
local emote_base = { mean = 0, var = 0, n = 0 }
local mic_base = { mean = 0, var = 0, n = 0 }
local music_short, music_long = 0, { mean = 0, var = 0, n = 0 }
local episode, calm_since = nil, 0
local markers = 0

local function ema(e, x, dt, tau)
  e.n = e.n + 1
  local a = math.min(1, math.max(dt / tau, 1 / e.n))
  local d = x - e.mean
  e.mean = e.mean + a * d
  e.var = (1 - a) * (e.var + a * d * d)
end

local function std(e) return math.sqrt(math.max(0, e.var)) end
local function clamp(x, lo, hi) return math.max(lo, math.min(hi, x)) end

local function delay()
  local ms = get("clips.hype.delay_ms")
  if type(ms) ~= "number" or ms <= 0 then ms = 3000 end
  return ms / 1000
end

local function bucket(t)
  local idx = math.floor(t / BUCKET)
  if next_eval and idx < next_eval then idx = next_eval end
  if next_eval and idx >= next_eval + RING then return nil end
  local slot = idx % RING
  local b = ring[slot]
  if not b or b.idx ~= idx then
    b = { idx = idx, chat = 0, emotes = 0, copy = 0, imp = {}, mic = {}, music = nil }
    ring[slot] = b
  end
  return b
end

local function slot(idx)
  local b = ring[idx % RING]
  if b and b.idx == idx then return b end
end

local function screen(kind) return SHIFTED[kind] and (time - delay()) or time end

-- copypasta key: lowercase words, repeated letters collapsed ("LULLLL" = "lul")
local function normalize(s)
  s = s:lower():gsub("[^%w]+", " ")
  local out, last = {}, ""
  for ch in s:gmatch(".") do
    if ch ~= last or ch == " " then out[#out + 1] = ch end
    last = ch
  end
  return (table.concat(out):gsub("^%s+", ""):gsub("%s+$", ""))
end

local function add_impulse(kind, amount)
  local b = bucket(screen(kind))
  if b then b.imp[kind] = (b.imp[kind] or 0) + amount end
end

on("twitch.chat", function(p, info)
  local text = tostring(p.message or "")
  local user = (info.actor and info.actor.id ~= "" and info.actor.id) or tostring(p.user or "")
  local first = text:match("^%s*(%S+)")
  if first and first:lower() == params.clip_command:lower() then
    votes[user] = screen("votes")
    return
  end
  local t = screen("chat")
  local n_emotes = 0
  if type(p.emotes) == "table" then n_emotes = #p.emotes
  elseif type(p.emote_count) == "number" then n_emotes = p.emote_count
  elseif type(p.fragments) == "table" then
    for _, f in ipairs(p.fragments) do if f.type == "emote" then n_emotes = n_emotes + 1 end end
  else
    for w in text:gmatch("%S+") do if EMOTES[w] then n_emotes = n_emotes + 1 end end
  end
  local norm = normalize(text)
  local repeat_msg = false
  local keep = {}
  for _, r in ipairs(recent) do
    if t - r.t <= CHAT_WINDOW then
      keep[#keep + 1] = r
      if #norm >= 2 and r.norm == norm and r.user ~= user then repeat_msg = true end
    end
  end
  if #keep < 2000 then keep[#keep + 1] = { t = t, norm = norm, user = user } end
  recent = keep
  local b = bucket(t)
  if b then
    b.chat = b.chat + 1
    if repeat_msg then b.copy = b.copy + 1 end
  end
  if n_emotes > 0 then
    local be = bucket(screen("emotes"))
    if be then be.emotes = be.emotes + n_emotes end
  end
end)

on("twitch.cheer", function(p) add_impulse("bits", math.log10(1 + (tonumber(p.bits) or 0) / 100)) end)
local function sub(p)
  if p.is_gift then return end -- counted with the gift event
  local tier = tonumber(p.tier) or 1
  if tier >= 1000 then tier = tier / 1000 end
  add_impulse("subs", ({ 1, 2, 4 })[clamp(math.floor(tier), 1, 3)])
end
on("twitch.sub", sub)
on("twitch.resub", sub)
on("twitch.gift", function(p) add_impulse("gifts", math.log(1 + math.max(1, tonumber(p.count) or 1)) / math.log(2)) end)
on("twitch.raid", function(p) add_impulse("raid", math.log10(1 + (tonumber(p.viewers) or 0) / 10)) end)
on("tip", function(p) add_impulse("tip", math.log10(1 + (tonumber(p.amount) or 0))) end)
on("twitch.hype_train.*", function() add_impulse("hype_train", 1) end)
on("band.drop", function() add_impulse("drop", 1) end)
on("music.drop", function() add_impulse("drop", 1) end)

-- bursty mic energy at 3–8 Hz above the baseline over the last 2 s
local function laughter(idx)
  local xs = {}
  for i = idx - 19, idx do
    local b = slot(i)
    if b then for _, x in ipairs(b.mic) do xs[#xs + 1] = x end end
  end
  if #xs < 20 then return 0 end
  local sum, lo, hi = 0, math.huge, -math.huge
  for _, x in ipairs(xs) do sum = sum + x; lo = math.min(lo, x); hi = math.max(hi, x) end
  local mean, range = sum / #xs, hi - lo
  if not (mean > mic_base.mean + math.max(std(mic_base), 0.02) and range > 0.05) then return 0 end
  local period = 2.0 / #xs * 1000 -- ms per sample over the 2 s window
  local last, regular = nil, 0
  for i = 2, #xs - 1 do
    local x = xs[i]
    if x > xs[i - 1] and x >= xs[i + 1] then
      local m = math.huge
      for j = math.max(1, i - 3), math.min(#xs, i + 3) do m = math.min(m, xs[j]) end
      if x - m >= 0.3 * range then
        if last then
          local gap = (i - last) * period
          if gap >= 120 and gap <= 350 then regular = regular + 1 end
        end
        last = i
      end
    end
  end
  return regular >= 4 and math.min(1.5, regular / 6) or 0
end

local function make_marker(ep, now)
  local s = ep.peak_score
  local start = clamp(ep.rise - 2, ep.peak - params.preroll_max, ep.peak - params.preroll_min)
  local stop = math.max(math.min(math.max(ep.last_above + POSTROLL, ep.peak + POSTROLL), start + MAX_LEN), ep.peak + 1)
  local ranked = {}
  for k, v in pairs(ep.comps) do if v > 0 then ranked[#ranked + 1] = { k, v } end end
  table.sort(ranked, function(a, b) return a[2] > b[2] end)
  local reasons = {}
  for _, r in ipairs(ranked) do
    if r[2] >= math.max(0.05, s * 0.15) then reasons[#reasons + 1] = '"' .. r[1] .. '"' end
  end
  if #reasons == 0 and ranked[1] then reasons[1] = '"' .. ranked[1][1] .. '"' end
  markers = markers + 1
  signal("markers", markers)
  if params.write_markers then
    cmd(string.format("session.marker label=hype kind=hype source=%s score=%.3f start_ago=%.3f peak_ago=%.3f end_ago=%.3f reasons=[%s]",
      patch.id, s, now - start, now - ep.peak, math.max(0, now - stop), table.concat(reasons, ",")))
    local plain = table.concat(reasons, ", "):gsub('"', "")
    cmd(string.format("twitch.marker description='hype %.1f: %s'", s, plain))
  end
  log.info(string.format("hype %.2f (%s)", s, table.concat(reasons, ", ")))
end

local function evaluate(idx, now)
  local dt = BUCKET
  local s = (idx + 1) * BUCKET
  evaluated = evaluated + 1
  local cur = slot(idx) or { chat = 0, emotes = 0, copy = 0, imp = {}, mic = {} }
  local c = {}
  -- chat + emote rates over the window vs slow baselines
  local win = math.floor(CHAT_WINDOW / BUCKET)
  local chat, emotes, copy = 0, 0, 0
  for i = idx - win + 1, idx do
    local b = slot(i)
    if b then chat = chat + b.chat; emotes = emotes + b.emotes; copy = copy + b.copy end
  end
  local chat_rate, emote_rate = chat / CHAT_WINDOW, emotes / CHAT_WINDOW
  if evaluated * dt >= math.max(WARMUP, 3 * CHAT_WINDOW) then
    c.chat = W.chat * clamp(math.log(math.max(chat_rate, 1e-9) / math.max(chat_base.mean, 0.2)), 0, 3)
    c.emotes = W.emotes * clamp(math.log(math.max(emote_rate, 1e-9) / math.max(emote_base.mean, 0.1)), 0, 3)
    c.copypasta = math.min(1.5, W.copypasta * copy)
  end
  ema(chat_base, chat_rate, dt, BASELINE)
  ema(emote_base, emote_rate, dt, BASELINE)
  -- `!clip` votes
  local voters, forced = 0, false
  for u, t in pairs(votes) do
    if s - t > VOTE_WINDOW then votes[u] = nil elseif t <= s then voters = voters + 1 end
  end
  if voters > 0 then
    c.votes = W.votes * voters
    if voters >= params.clip_votes then c.votes = math.max(c.votes, params.threshold * 1.01); forced = true end
  end
  -- decaying event impulses
  local decay = math.exp(-dt / TAU)
  for _, k in ipairs(IMPULSES) do
    impulse[k] = impulse[k] * decay + (cur.imp[k] or 0)
    c[k] = W[k] * impulse[k]
  end
  -- mic spike + laughter
  if #cur.mic > 0 then
    local peak, sum = 0, 0
    for _, x in ipairs(cur.mic) do peak = math.max(peak, x); sum = sum + x end
    if mic_base.n * dt > 5 then
      local z = (peak - mic_base.mean) / math.max(std(mic_base), 0.02)
      c.mic = W.mic * clamp((z - 2.5) / 2, 0, 2)
      c.laughter = W.laughter * laughter(idx)
    end
    ema(mic_base, sum / #cur.mic, dt, math.max(1, BASELINE / 5))
  end
  -- music novelty
  if cur.music then
    music_short = music_short + math.min(1, dt / 0.4) * (cur.music - music_short)
    if music_long.n * dt > 4 and music_long.mean > 0.02 then
      c.novelty = W.novelty * clamp(music_short / music_long.mean - 1.6, 0, 1.5)
    end
    ema(music_long, cur.music, dt, 8)
  end
  local score = 0
  for _, v in pairs(c) do score = score + v end
  signal("score", score)

  -- episodes (same hysteresis as the built-in detector)
  local th = params.threshold
  if not episode then
    if score >= th or forced then
      episode = { rise = calm_since, peak = s, peak_score = score, comps = c, last_above = s, below = nil, closed = nil }
    elseif score < th * 0.35 then
      calm_since = s
    end
    return
  end
  local ep = episode
  local too_long = s - ep.rise >= MAX_LEN
  if ep.closed then
    if (score >= th or forced) and not too_long then
      ep.closed, ep.below, ep.last_above = nil, nil, s
      if score > ep.peak_score then ep.peak, ep.peak_score, ep.comps = s, score, c end
    elseif s - ep.closed >= MERGE_GAP or too_long then
      make_marker(ep, now)
      for u, t in pairs(votes) do if t <= s then votes[u] = nil end end
      episode, calm_since = nil, s
    end
    return
  end
  if score > ep.peak_score then ep.peak, ep.peak_score, ep.comps = s, score, c end
  if score >= th * RELEASE then ep.last_above, ep.below = s, nil elseif not ep.below then ep.below = s end
  if (ep.below and s - ep.below >= HOLD) or too_long then ep.closed = s end
end

function frame(dt, sig)
  local now = time
  local b = bucket(now)
  if b then
    if #b.mic < 8 then b.mic[#b.mic + 1] = math.max(0, sig.mic.level or 0) end
    local m = math.max(sig.music.level or 0, sig.band.level or 0)
    b.music = math.max(b.music or 0, m)
  end
  local ready = now - delay() - SETTLE
  local last = math.floor(ready / BUCKET) - 1
  if last < 0 then return end
  if not next_eval then next_eval = last; calm_since = last * BUCKET end
  if last >= next_eval + RING then next_eval = last + 1 - RING / 2 end
  while next_eval <= last do
    evaluate(next_eval, now)
    next_eval = next_eval + 1
  end
end
