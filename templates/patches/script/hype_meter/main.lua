-- Hype meter: reads signals every frame (the `s` table), smooths them, draws a bar.
local score, shown = 0, 0
local armed = true

local function clamp01(x) return math.max(0, math.min(1, x)) end

function frame(dt, s)
  local chat = clamp01(s.twitch.chat_rate / params.chat_full)
  local band = clamp01(s.band.level * 1.4)
  local target = clamp01(chat * 0.6 + band * 0.4 + env * 0.3)
  local k = 1 - math.exp(-dt / params.smoothing)
  score = score + (target - score) * k
  shown = shown + (score - shown) * math.min(1, dt * 12)

  -- fire once per crossing, re-arm below 80% of the threshold
  if armed and score >= params.threshold then
    armed = false
    emit("hype.peak", { level = score, chat_rate = s.twitch.chat_rate, viewers = s.twitch.viewers })
  elseif not armed and score < params.threshold * 0.8 then
    armed = true
  end

  -- the score as a signal (`patch.hype_meter.level`) for bindings, rules, and the UI
  signal("level", score)

  local x, y, w, h = params.x, params.y, params.width, 0.018
  draw.clear()
  draw.rect(x, y, w, h, palette.background, 0.65, { radius = h / 2 })
  local fill = shown >= params.threshold and palette.red or palette.accent
  if shown > 0.005 then
    draw.rect(x, y, w * shown, h, fill, 0.95, { radius = h / 2 })
  end
  draw.rect(x + w * params.threshold - 0.001, y - 0.004, 0.002, h + 0.008, palette.foreground, 0.7)
  draw.text("HYPE " .. math.floor(shown * 100 + 0.5) .. "%", x, y - 0.012, 0.022, palette.foreground, 0.9)
end
