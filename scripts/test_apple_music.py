#!/usr/bin/python
"""Regression coverage for native control failure isolation; no live engine/browser."""
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

# Chromium refuses extension directories containing __pycache__.
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("apple_music_host", Path(__file__).parent / "apple-music/host.py")
host = importlib.util.module_from_spec(spec)
spec.loader.exec_module(host)


class DispatcherRecovery(unittest.IsolatedAsyncioTestCase):
    async def test_closed_launcher_stderr_does_not_disable_later_controls(self):
        for failing_action in ("invalid", "play"):
            with self.subTest(failing_action=failing_action), tempfile.TemporaryDirectory() as directory:
                class BrowserHost(host.Host):
                    async def request(self, action):
                        if action == "play":
                            raise ValueError("Browser rejected playback")
                        if action == "pause":
                            self.stop.set()
                        return action, {"playing": action == "status", "volume": 0.65}, None

                instance = BrowserHost()
                for action in (failing_action, "pause"):
                    event = {"origin": "deck", "payload": {"action": action}}
                    instance.controls.put_nowait((instance.generation, host.time.monotonic(), event))
                read_fd, write_fd = os.pipe()
                os.close(read_fd)
                state = Path(directory) / "music.json"
                closed_sink = os.fdopen(write_fd, "w")
                try:
                    with patch.object(host, "STATE_PATH", state), patch.object(sys, "stderr", closed_sink):
                        await asyncio.wait_for(instance.dispatcher(), 1)
                finally:
                    # TextIO retries its failed flush on close; only cleanup's
                    # failure is ignored, never a dispatcher exception.
                    try:
                        closed_sink.close()
                    except BrokenPipeError:
                        pass
                saved = json.loads(state.read_text())
                self.assertEqual(saved["last_action"], "pause")
                self.assertFalse(saved["status"]["playing"])
                self.assertIsNone(saved["error"])
                self.assertEqual(instance.health()["status"], "pass")
                results = [(item["action"], item["ok"]) for item, _ in instance.reports]
                self.assertEqual(results, [("status", True), (failing_action, False), ("pause", True)])


if __name__ == "__main__":
    unittest.main()
