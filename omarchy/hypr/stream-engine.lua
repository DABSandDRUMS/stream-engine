-- stream-engine keybinds and window rules for Omarchy's Hyprland Lua config (PLAN §16.3).
--
-- Load it after Omarchy's defaults, e.g. at the end of ~/.config/hypr/bindings.lua:
--   dofile(os.getenv("HOME") .. "/.config/hypr/stream-engine.lua")
-- (`omarchy/install.sh --hypr` copies this file there and appends that line, with a backup.)
--
-- Keys live on SUPER + CTRL + ALT, a layer Omarchy 4.0.4 barely uses (checked against
-- `omarchy menu keybindings --print`). The only clash is SUPER + CTRL + ALT + B ("Show battery
-- remaining", meaningless on a desktop), which is unbound and reused for BRB.
--   SUPER CTRL ALT + S        open / focus the stream-engine UI
--   SUPER CTRL ALT + RETURN   Take (preview → program)
--   SUPER CTRL ALT + ESCAPE   Panic (effects off, safe lights, safe mix)
--   SUPER CTRL ALT + C        Clean (clear chat overrides)
--   SUPER CTRL ALT + B        BRB (toggle live ⇄ brb)

-- Outputs (`hyprctl monitors all`). The TV is matched by EDID description so it keeps working
-- whichever connector it is plugged into; `~/.local/bin/tv` parks it with the same selector.
local main_monitor = "DP-1"
local panel_monitor = "DP-2"
local tv_monitor = "desc:Philips Consumer Electronics Company Philips FTV"

-- A `streamctl` CLI call that reports failure (engine down, command rejected) as a notification.
local function streamctl(args, action)
  return "streamctl " .. args .. " || omarchy-notification-send -u critical stream-engine "
    .. o.shell_quote(action .. " failed: is the engine running?")
end

hl.unbind("SUPER + CTRL + ALT + B")

o.bind("SUPER + CTRL + ALT + S", "stream-engine UI", "stream-engine-launch-or-focus")
o.bind("SUPER + CTRL + ALT + RETURN", "Stream: Take", streamctl("take", "Take"))
o.bind("SUPER + CTRL + ALT + ESCAPE", "Stream: Panic", streamctl("panic", "Panic"))
o.bind("SUPER + CTRL + ALT + C", "Stream: Clean", streamctl("clean", "Clean"))
o.bind("SUPER + CTRL + ALT + B", "Stream: BRB", streamctl("brb", "BRB"))

-- Every stream-engine window shows video: keep it fully opaque (like Omarchy's media windows).
o.window("^stream-engine(\\..+)?$", { tag = "-default-opacity" })
o.window("^stream-engine(\\..+)?$", { opacity = "1 1" })

-- Main window (Show mode) on DP-1.
o.window("^stream-engine$", { monitor = main_monitor })

-- Popped-out panels and the multiview (`stream-engine.<panel>`) on DP-2. The confidence window
-- is excluded here (its app-id is also its initial class) so its own placement below applies
-- no matter how Hyprland orders overlapping rules.
o.window({
  class = "^stream-engine\\..+$",
  initial_class = "negative:^stream-engine\\.program$",
}, { monitor = panel_monitor })

-- Confidence window (`stream-engine.program`): borderless fullscreen on the TV, falling back to
-- DP-2 while the TV is off or parked, and never letting the screen idle while it is open.
o.window("^stream-engine\\.program$", { fullscreen = true, idle_inhibit = "always" })

-- `o.window` does not return the rule, so the two placements use `hl.window_rule` (which it
-- wraps) to keep handles and switch between them as outputs come and go. The UI itself moves an
-- already-open confidence window on hotplug; these rules place it when it opens.
local program_on_tv = hl.window_rule({ match = { class = "^stream-engine\\.program$" }, monitor = tv_monitor })
local program_on_fallback = hl.window_rule({ match = { class = "^stream-engine\\.program$" }, monitor = panel_monitor })

local function place_program()
  local tv_present = hl.get_monitor(tv_monitor) ~= nil
  program_on_tv:set_enabled(tv_present)
  program_on_fallback:set_enabled(not tv_present)
end

place_program()
hl.on("monitor.added", place_program)
hl.on("monitor.removed", place_program)
