"""Desktop migration must not chase Hyprland's replacement empty workspaces."""

import copy
import importlib.machinery
import importlib.util
from pathlib import Path
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


if __name__ == "__main__":
    unittest.main()
