-- Confetti: every trigger adds a burst; pieces tumble, flutter, and fade out on their own.
local pieces = {}
local colors = { "accent", "red", "yellow", "green", "cyan", "magenta" }
local MAX = 6000

local function spawn(x, y, vx, vy)
  if #pieces >= MAX then return end
  pieces[#pieces + 1] = {
    x = x, y = y, vx = vx, vy = vy,
    rot = math.random() * math.pi * 2,
    spin = (math.random() - 0.5) * 14,
    flutter = math.random() * math.pi * 2,
    color = colors[math.random(#colors)],
    w = 0.6 + math.random() * 0.8,
    life = 3.5 + math.random() * 2.5,
  }
end

on("trigger", function(e)
  local n = e.count or params.count
  if e.amount or e.bits then
    -- scale with the size of the cheer/tip: 100 bits → 1×, 1000 → ~2×
    local amount = e.amount or e.bits
    n = math.floor(n * math.min(4, 0.5 + math.log10(math.max(10, amount)) / 2))
  end
  n = math.max(1, math.min(n, 1500))
  local top = params.cannons and math.floor(n * 0.5) or n
  for _ = 1, top do
    spawn(math.random(), -0.05 - math.random() * 0.3, (math.random() - 0.5) * 0.2, math.random() * 0.2)
  end
  if params.cannons then
    local side = n - top
    for i = 1, side do
      local left = i % 2 == 0
      local speed = 0.9 + math.random() * 0.8
      local angle = math.rad(55 + math.random() * 25)
      local vx = math.cos(angle) * speed * (left and 1 or -1)
      spawn(left and 0.0 or 1.0, 1.02, vx, -math.sin(angle) * speed)
    end
  end
end)

function frame(dt, s)
  draw.clear()
  if #pieces == 0 then return end
  local g, size = params.gravity, params.size
  local drag = math.exp(-1.6 * dt)
  for i = #pieces, 1, -1 do
    local p = pieces[i]
    p.life = p.life - dt
    p.vy = p.vy + g * dt
    p.vx = p.vx * drag
    p.vy = math.min(p.vy * drag, 0.35)
    p.flutter = p.flutter + dt * 6
    p.x = p.x + (p.vx + math.sin(p.flutter) * 0.05) * dt
    p.y = p.y + p.vy * dt
    p.rot = p.rot + p.spin * dt
    if p.life <= 0 or p.y > 1.1 then
      pieces[i] = pieces[#pieces]
      pieces[#pieces] = nil
    else
      local alpha = math.min(1, p.life / 0.8)
      -- the width shrinks and grows as the piece turns over
      local sx = math.abs(math.cos(p.flutter * 0.7)) * 0.8 + 0.2
      draw.push({ x = p.x, y = p.y, rotate = p.rot, scale = { sx, 1 } })
      draw.rect(-size * p.w * 0.5, -size * 0.3, size * p.w, size * 0.6, palette[p.color], alpha)
      draw.pop()
    end
  end
end
