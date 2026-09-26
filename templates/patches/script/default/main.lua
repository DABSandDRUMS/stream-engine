-- {{label}}: fire it with `stream do patch.{{id}}.trigger` or from a rule/preset.
--
-- Globals: params, palette, env (trigger envelope 0–1), time, trigger (last payload),
-- signals, patch = { id, frame, resolution, aspect }.
-- API: on(pattern, fn), get/set/animate, emit, trigger, cmd, signal, log.info/warn,
-- draw.clear/rect/circle/line/path/text/image/push/pop. Coordinates are 0–1 of the layer.

local last = "—"

on("trigger", function(e, ev)
  -- e = the trigger payload (user, amount, tier, message, ...); ev = { type, origin, actor, ts }
  last = e.user or (ev.actor and ev.actor.name) or "someone"
  log.info("triggered by " .. last)
end)

function frame(dt, s)
  draw.clear()
  if env <= 0 then return end
  local r = params.size * (1 + s.band.kick * 0.5)
  draw.circle(0.5, 0.5, r * (0.5 + env * 0.5), params.color, env)
  draw.text(last, 0.5, 0.5 + r + 0.05, 0.04, palette.foreground, env, { align = "center" })
end
