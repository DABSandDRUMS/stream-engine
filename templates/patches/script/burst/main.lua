-- {{label}}: every trigger adds a burst; `amount` in the payload scales it.
local sparks = {}
local colors = { "accent", "yellow", "cyan", "magenta" }

on("trigger", function(e)
  local n = math.min(4000 - #sparks, math.floor(params.count * (e.scale or 1)))
  for _ = 1, n do
    local a = math.random() * math.pi * 2
    local v = params.speed * (0.3 + math.random())
    sparks[#sparks + 1] = {
      x = 0.5, y = 0.5,
      vx = math.cos(a) * v / patch.aspect, vy = math.sin(a) * v,
      life = 1 + math.random() * 1.5,
      c = colors[math.random(#colors)],
    }
  end
end)

function frame(dt, s)
  draw.clear()
  for i = #sparks, 1, -1 do
    local p = sparks[i]
    p.life = p.life - dt
    if p.life <= 0 then
      sparks[i] = sparks[#sparks]
      sparks[#sparks] = nil
    else
      p.vy = p.vy + params.gravity * dt
      p.x, p.y = p.x + p.vx * dt, p.y + p.vy * dt
      draw.circle(p.x, p.y, 0.004 + p.life * 0.002, palette[p.c], math.min(1, p.life))
    end
  end
end
