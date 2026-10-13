"""Workspace migration and the generated monitor profile's runtime fallback."""

import copy
import importlib.machinery
import importlib.util
from pathlib import Path
import shutil
import subprocess
import unittest
from unittest.mock import patch


path = Path(__file__).resolve().parents[1] / "bin/stream-engine-drum-screen"
loader = importlib.machinery.SourceFileLoader("drum_screen", str(path))
spec = importlib.util.spec_from_loader(loader.name, loader)
helper = importlib.util.module_from_spec(spec)
loader.exec_module(helper)


class WorkspaceMigration(unittest.TestCase):
    def test_content_and_persistent_workspaces_move_without_chasing_placeholders(self):
        for target in ("tv", "desk"):
            with self.subTest(target=target):
                source = ["DP-1", "DP-2"] if target == "tv" else ["HDMI-A-1", "HDMI-A-1"]
                destinations = ["HDMI-A-1", "HDMI-A-1"] if target == "tv" else ["DP-1", "DP-2"]
                placeholder_monitor = destinations[0]
                workspaces = [
                    {"id": 1, "name": "1", "monitor": source[0], "windows": 2, "ispersistent": False},
                    {"id": 2, "name": "2", "monitor": source[1], "windows": 1, "ispersistent": False},
                    {"id": 3, "name": "3", "monitor": placeholder_monitor, "windows": 0, "ispersistent": False},
                    {"id": 4, "name": "4", "monitor": source[0], "windows": 0, "ispersistent": True},
                ]
                owners = {"1": destinations[0], "2": destinations[1], "3": placeholder_monitor, "4": destinations[1]}

                def query(kind):
                    return copy.deepcopy(workspaces)

                def dispatch(kind, args):
                    workspace = next(w for w in workspaces if w["id"] == args["workspace"])
                    old_monitor = workspace["monitor"]
                    destination = args["monitor"]
                    # An active destination's empty placeholder is reassigned
                    # to the source when its content workspace migrates.
                    for empty in workspaces:
                        if not empty["windows"] and not empty["ispersistent"] and empty["monitor"] == destination:
                            empty["monitor"] = old_monitor
                    workspace["monitor"] = destination

                with patch.object(helper, "query", query), patch.object(helper, "dispatch", dispatch):
                    helper.move_workspaces(owners)
                actual = {str(w["id"]): w["monitor"] for w in workspaces if w["windows"] or w["ispersistent"]}
                self.assertEqual(actual, {"1": destinations[0], "2": destinations[1], "4": destinations[1]})


class MonitorFallback(unittest.TestCase):
    def setUp(self):
        self.desk = [
            {"name": "DP-1", "width": 3440, "height": 1440, "refreshRate": 99.998,
             "x": 0, "y": 0, "scale": 1.25, "transform": 0},
            {"name": "DP-2", "width": 1440, "height": 2560, "refreshRate": 59.999,
             "x": 2752, "y": 0, "scale": 1.5, "transform": 1},
        ]
        self.tv = {"name": "HDMI-A-1", "width": 3840, "height": 2160, "refreshRate": 60.0,
                   "x": 5504, "y": 0, "scale": 1.25, "transform": 0}
        with patch.object(helper.Path, "glob", return_value=[Path("/sys/class/drm/card1-HDMI-A-1/status")]):
            self.profile = helper.tv_override(self.desk, self.tv)

    def run_profile(self, assertions, connected=True):
        interpreter = shutil.which("lua")
        self.assertIsNotNone(interpreter, "Lua is required to exercise the generated Hyprland profile")
        script = """
local active, applied, handlers = {}, {}, {}
local expected_desk = """ + helper.lua([helper.monitor_rule(monitor) for monitor in self.desk]) + """
local profile = """ + helper.lua(self.profile) + """
local connected = """ + helper.lua(connected) + """
hl = {}
function hl.monitor(rule)
  applied[#applied + 1] = rule
  if rule.disabled then active[rule.output] = nil else active[rule.output] = rule end
end
function hl.on(event, callback)
  handlers[event] = handlers[event] or {}
  table.insert(handlers[event], callback)
end
function io.open()
  return { read = function() return connected and "connected" or "disconnected" end,
           close = function() end }
end
local function reload(text)
  -- Hyprland clears config-owned subscriptions before loading replacement rules.
  handlers, applied = {}, {}
  assert(load(text, "drum-screen", "t", _ENV))()
end
local function removed(name)
  active[name] = nil
  for _, callback in ipairs(handlers["monitor.removed"] or {}) do
    callback({ name = name })
  end
end
local function added(ready)
  active["HDMI-A-1"] = {}
  local monitor = { name = "HDMI-A-1", width = ready and 3840 or 0,
                    height = 2160, refresh_rate = 60, dpms_status = true, is_mirror = false }
  for _, callback in ipairs(handlers["monitor.added"] or {}) do callback(monitor) end
end
local function desk_restored()
  for _, expected in ipairs(expected_desk) do
    local actual = assert(active[expected.output], "saved desk output stayed disabled")
    for key, value in pairs(expected) do
      assert(actual[key] == value, "saved monitor field changed: " .. key)
    end
  end
end
local function tv_only()
  assert(active["HDMI-A-1"], "TV was not enabled")
  for _, monitor in ipairs(expected_desk) do
    assert(not active[monitor.output], "desk stayed enabled in TV mode")
  end
end
reload(profile)
""" + assertions
        result = subprocess.run([interpreter, "-"], input=script, text=True,
                                capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr or result.stdout)

    def test_tv_loss_after_completed_handoff_restores_saved_desk_without_reload(self):
        self.run_profile("""
tv_only()
applied = {}
removed("HDMI-A-1")
desk_restored()
assert(#applied == #expected_desk, "fallback changed outputs other than the saved desk")
assert(not active["HDMI-A-1"], "fallback re-enabled the missing TV")
""")

    def test_tv_return_parks_desk_only_after_usable_output_is_added(self):
        self.run_profile("""
removed("HDMI-A-1")
desk_restored()
added(false)
desk_restored()
added(true)
tv_only()
removed("HDMI-A-1")
desk_restored()
""")

    def test_other_output_removal_does_not_change_tv_mode(self):
        self.run_profile("""
applied = {}
removed("DP-1")
removed("DP-2")
removed("FALLBACK")
tv_only()
assert(#applied == 0, "non-TV removal triggered fallback")
""")

    def test_connected_reload_does_not_enable_then_disable_desk(self):
        self.run_profile("""
for attempt = 1, 3 do
  reload(profile)
  tv_only()
  assert(#handlers["monitor.removed"] == 1, "duplicate removal callback after reload")
  assert(#handlers["monitor.added"] == 1, "duplicate addition callback after reload")
  for _, rule in ipairs(applied) do
    if rule.output ~= "HDMI-A-1" then
      assert(rule.disabled, "reload needlessly re-enabled desk output")
    end
  end
end
removed("HDMI-A-1")
desk_restored()
""")

    def test_replacing_tv_profile_removes_fallback_before_intentional_tv_parking(self):
        desk_profile = helper.rules(self.desk) + "hl.monitor({ output = 'HDMI-A-1', disabled = true })\n"
        self.run_profile("""
reload(""" + helper.lua(desk_profile) + """)
desk_restored()
applied = {}
removed("HDMI-A-1")
assert(#applied == 0, "stale TV callback rewrote the intentional desk profile")
added(true)
assert(#applied == 0, "stale TV callback parked the intentional desk profile")
desk_restored()
""")

    def test_disconnected_login_restores_desk_and_later_tv_return_resumes_tv_mode(self):
        self.run_profile("""
desk_restored()
added(true)
tv_only()
""", connected=False)


if __name__ == "__main__":
    unittest.main()
