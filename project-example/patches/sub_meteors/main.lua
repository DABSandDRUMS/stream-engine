-- patches/sub_meteors/main.lua (PLAN §6.4)
local rocks = {}
on("trigger", function(e)
  for _ = 1, params.count * (e.tier or 1) do
    rocks[#rocks + 1] = { x = math.random(), y = -0.1, v = 0.3 + math.random() * params.speed }
  end
  emit("lights.flash", { color = palette.accent, ms = 400 })
end)
function frame(dt, s)
  draw.clear()
  for i = #rocks, 1, -1 do
    local r = rocks[i]
    r.y = r.y + r.v * dt * (1 + s.music.bass)
    if r.y > 1.1 then table.remove(rocks, i)
    else draw.circle(r.x, r.y, 0.006, palette.accent, env) end
  end
end
