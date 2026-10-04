#!/usr/bin/python
"""Chromium native host for explicit Stream Engine Apple Music controls.

Only native-message frames are written to stdout. Chromium owns this process;
closing its native connection stops the watch and all outstanding CLI children.
"""

import asyncio
from collections import deque
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import signal
import struct
import sys
import tempfile
import time
import uuid


ACTIONS = frozenset({"play", "pause", "next", "previous", "volume_up", "volume_down"})
ORIGINS = frozenset({"deck", "cli", "ui", "api", "midi", "voice", "osc", "system"})
MAX_MESSAGE = 1024 * 1024
MAX_WATCH_LINE = 256 * 1024
MAX_CONTROLS = 32
MAX_REPORTS = 128
REQUEST_TIMEOUT = 8.0
STATUS_INTERVAL = 5.0
CLI_TIMEOUT = 2.0
STATE_PATH = Path.home() / ".local/state/stream-engine/apple-music.json"


def log(message):
    # Chromium can outlive the terminal/launcher that supplied its stderr.
    # Health and the status file are authoritative; a closed diagnostic sink
    # must never terminate the native dispatcher or its shutdown callback.
    try:
        print(f"apple-music: {message}", file=sys.stderr, flush=True)
    except OSError:
        pass


def brief(value):
    return str(value)[:2048]


def command_string(value):
    # streamctl do joins argv and uses its own quote-aware command parser, not
    # a shell. Its parser has no backslash quote escaping; a single-quoted JSON
    # string with Unicode-escaped apostrophes preserves arbitrary error text.
    return "'" + json.dumps(str(value), ensure_ascii=True).replace("'", "\\u0027") + "'"


async def terminate(process):
    async def drain_stdout():
        while await process.stdout.read(65536):
            pass

    # A paused PIPE transport can keep Process.wait() blocked even after the
    # child exits. Drain and discard watch output while terminating; never
    # accumulate it or feed it back into the control queue.
    drainer = asyncio.create_task(drain_stdout()) if process.stdout is not None else None
    try:
        if process.returncode is None:
            try:
                process.terminate()
            except ProcessLookupError:
                pass
        try:
            await asyncio.wait_for(process.wait(), 1.0)
        except asyncio.TimeoutError:
            try:
                process.kill()
            except ProcessLookupError:
                pass
            await asyncio.wait_for(process.wait(), 1.0)
    finally:
        if drainer is not None:
            drainer.cancel()
            await asyncio.gather(drainer, return_exceptions=True)


class Host:
    def __init__(self):
        self.loop = asyncio.get_running_loop()
        self.stop = asyncio.Event()
        self.wake_reporter = asyncio.Event()
        self.controls = asyncio.Queue(maxsize=MAX_CONTROLS)
        self.reports = deque()
        self.pending = {}
        self.active_request = None
        self.shutdown_action = None
        self.generation = 0
        self.status = None
        self.verified = False
        self.browser_error = None
        self.engine_error = None
        self.persistence_error = None
        self.last_action = None
        self.shutdown_error = None
        self.shutdown_is_failure = False
        self.published = {}
        self.last_status_result = None

    def health(self):
        if self.shutdown_error:
            return {"status": "fail" if self.shutdown_is_failure else "warn", "detail": self.shutdown_error}
        error = self.persistence_error or self.browser_error or self.engine_error
        if error:
            return {"status": "fail", "detail": error}
        if not self.verified:
            return {"status": "warn", "detail": "Waiting for verified Apple Music browser status"}
        return {"status": "pass", "detail": "Apple Music browser connected; playback and volume verified"}

    def save_state(self):
        temporary = None
        previous_error = self.persistence_error
        try:
            STATE_PATH.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            fd, temporary = tempfile.mkstemp(prefix=".apple-music-", dir=STATE_PATH.parent)
            with os.fdopen(fd, "w", encoding="utf-8") as stream:
                json.dump({
                    "connected": self.verified,
                    "status": self.status,
                    "error": self.shutdown_error or self.browser_error or self.engine_error,
                    "time": datetime.now(timezone.utc).isoformat(timespec="milliseconds"),
                    "last_action": self.last_action,
                }, stream, ensure_ascii=True, allow_nan=False)
                stream.write("\n")
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, STATE_PATH)
            temporary = None
            self.persistence_error = None
        except OSError as error:
            self.persistence_error = f"Cannot write Apple Music status file {STATE_PATH}: {brief(error)}"
            log(self.persistence_error)
            if self.persistence_error != previous_error:
                self.report("status", False, self.persistence_error)
        finally:
            if temporary is not None:
                try:
                    os.unlink(temporary)
                except OSError:
                    pass
        self.wake_reporter.set()

    def report(self, action, ok, error=None, request_id=None, notify=True):
        item = {"action": brief(action), "ok": ok}
        if error:
            item["error"] = brief(error)
        if request_id:
            item["id"] = request_id
        if len(self.reports) >= MAX_REPORTS:
            log("Result reporting queue full; oldest result discarded (controls are never replayed)")
            self.reports.popleft()
        self.reports.append((item, notify))
        self.wake_reporter.set()

    async def cli(self, *arguments):
        process = None
        try:
            process = await asyncio.create_subprocess_exec(
                "streamctl", *arguments,
                stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=None,
            )
            return await asyncio.wait_for(process.wait(), CLI_TIMEOUT) == 0
        except (OSError, asyncio.TimeoutError) as error:
            log(f"streamctl {' '.join(arguments[:2])} failed: {brief(error)}")
            return False
        finally:
            if process is not None and process.returncode is None:
                await terminate(process)

    async def publish_state(self):
        values = {"health.apple_music": self.health()}
        if self.status is not None:
            values["apple_music.playing"] = self.status["playing"]
            values["apple_music.volume"] = self.status["volume"]
        for address, value in values.items():
            encoded = json.dumps(value, ensure_ascii=True, allow_nan=False, separators=(",", ":"))
            if self.published.get(address) == encoded:
                continue
            if await self.cli("set", address, encoded):
                self.published[address] = encoded

    async def publish_result(self, item, notify):
        arguments = [f"{key}={json.dumps(value, ensure_ascii=True)}" for key, value in item.items()]
        await self.cli("fire", "apple_music.result", *arguments)
        if not item["ok"] and notify:
            await self.cli(
                "do", "notify.send",
                "title=" + command_string("Apple Music control failed"),
                "body=" + command_string(item.get("error", "Unknown Apple Music error")),
                "key=" + command_string("apple_music.control"),
                "urgency=normal",
            )

    async def reporter(self):
        while not self.stop.is_set():
            await self.wake_reporter.wait()
            self.wake_reporter.clear()
            await self.publish_state()
            # Each result is attempted once; reporting never retries a music
            # control. A bounded snapshot permits newer health changes between
            # reports even during an engine outage.
            count = len(self.reports)
            for _ in range(count):
                if not self.reports:
                    break
                item, notify = self.reports.popleft()
                await self.publish_result(item, notify)
                if self.wake_reporter.is_set():
                    self.wake_reporter.clear()
                    await self.publish_state()
            if self.reports:
                self.wake_reporter.set()

    async def native_reader(self, reader):
        reading_body = False
        try:
            while not self.stop.is_set():
                header = await reader.readexactly(4)
                length = struct.unpack("<I", header)[0]
                if not 0 < length <= MAX_MESSAGE:
                    raise ValueError(f"Native message length {length} is outside 1..{MAX_MESSAGE}")
                reading_body = True
                message = json.loads((await reader.readexactly(length)).decode("utf-8"))
                reading_body = False
                if not isinstance(message, dict) or not isinstance(message.get("id"), str):
                    raise ValueError("Native response must be an object with a string id")
                future = self.pending.get(message["id"])
                if future is not None and not future.done():
                    future.set_result(message)
                else:
                    log(f"Ignoring stale or unsolicited native response {brief(message['id'])}")
        except asyncio.IncompleteReadError as error:
            if error.partial or reading_body:
                self.shutdown_error = "Apple Music native connection closed with a truncated message"
                self.shutdown_is_failure = True
            else:
                self.shutdown_error = "Apple Music browser closed; open Apple Music to reconnect"
        except (OSError, UnicodeError, ValueError) as error:
            self.shutdown_error = "Apple Music native protocol error: " + brief(error)
            self.shutdown_is_failure = True
            log(self.shutdown_error)
        finally:
            self.stop.set()

    async def native_write(self, message):
        payload = json.dumps(message, ensure_ascii=True, allow_nan=False, separators=(",", ":")).encode("utf-8")
        if not 0 < len(payload) <= MAX_MESSAGE:
            raise ValueError("Outgoing native message exceeds the size bound")
        framed = memoryview(struct.pack("<I", len(payload)) + payload)
        fd = sys.stdout.fileno()
        while framed:
            try:
                written = os.write(fd, framed)
                if written == 0:
                    raise BrokenPipeError("Native output closed")
                framed = framed[written:]
            except BlockingIOError:
                writable = self.loop.create_future()

                def ready():
                    if not writable.done():
                        writable.set_result(None)

                self.loop.add_writer(fd, ready)
                try:
                    await writable
                finally:
                    self.loop.remove_writer(fd)

    async def request(self, action):
        request_id = uuid.uuid4().hex
        future = self.loop.create_future()
        self.pending[request_id] = future
        self.active_request = (request_id, action)

        async def exchange():
            await self.native_write({"id": request_id, "action": action})
            return await future

        try:
            response = await asyncio.wait_for(exchange(), REQUEST_TIMEOUT)
        finally:
            self.active_request = None
            self.pending.pop(request_id, None)
            if not future.done():
                future.cancel()
        if not isinstance(response.get("ok"), bool):
            raise ValueError("Native response is missing boolean ok")
        error = None if response["ok"] else brief(response.get("error") or "Apple Music extension rejected the request")
        status = response.get("status")
        if status is None and not response["ok"]:
            return request_id, None, error
        if not isinstance(status, dict) or not isinstance(status.get("playing"), bool):
            raise ValueError("Native response is missing actual playback status")
        volume = status.get("volume")
        if isinstance(volume, bool) or not isinstance(volume, (float, int)) or not math.isfinite(volume) or not 0 <= volume <= 1:
            raise ValueError("Native response has an invalid volume (expected 0..1)")
        verified = {"playing": status["playing"], "volume": float(volume)}
        if "title" in status:
            if not isinstance(status["title"], str):
                raise ValueError("Native status title must be a string")
            verified["title"] = status["title"][:4096]
        return request_id, verified, error

    async def execute(self, action):
        if action != "status":
            self.last_action = action
        status = None
        request_id = None
        try:
            request_id, status, failure = await self.request(action)
            if status is not None:
                self.status = status
            if failure:
                raise ValueError(failure)
        except asyncio.TimeoutError:
            error = f"Apple Music {action} timed out after 8 seconds; open Apple Music and try again (not retried)"
        except BrokenPipeError:
            self.shutdown_error = "Apple Music browser closed; open Apple Music to reconnect"
            self.shutdown_action = action
            self.stop.set()
            return
        except (OSError, ValueError) as failure:
            error = f"Apple Music {action} failed: {brief(failure)}"
        else:
            self.status = status
            self.verified = True
            self.browser_error = None
            self.save_state()
            signature = (True, json.dumps(status, sort_keys=True))
            if action != "status" or signature != self.last_status_result:
                self.report(action, True, request_id=request_id, notify=False)
            if action == "status":
                self.last_status_result = signature
            return
        self.verified = status is not None
        self.browser_error = error
        self.save_state()
        signature = (False, error)
        if action != "status" or signature != self.last_status_result:
            self.report(action, False, error, request_id=request_id)
        if action == "status":
            self.last_status_result = signature
        log(error)

    def reject(self, event, error):
        action = event.get("payload", {}).get("action", "unknown") if isinstance(event.get("payload"), dict) else "unknown"
        self.browser_error = error
        self.save_state()
        self.report(action, False, error)
        log(error)

    def discard_controls(self, error):
        while not self.controls.empty():
            _, _, event = self.controls.get_nowait()
            self.reject(event, error)

    async def dispatcher(self):
        # Native startup is read-only, including when a button was pressed as
        # Chromium was opening the connection.
        await self.execute("status")
        while not self.stop.is_set():
            try:
                generation, received, event = await asyncio.wait_for(self.controls.get(), STATUS_INTERVAL)
            except asyncio.TimeoutError:
                await self.execute("status")
                continue
            if generation != self.generation or self.engine_error is not None:
                self.reject(event, "Apple Music control discarded after engine disconnect; press the button again")
                continue
            if time.monotonic() - received >= REQUEST_TIMEOUT:
                self.reject(event, "Stale Apple Music control discarded; press the button again")
                continue
            origin = event.get("origin")
            if not isinstance(origin, str) or origin not in ORIGINS or event.get("actor") is not None:
                self.reject(event, "Apple Music controls require an operator origin without a viewer actor")
                continue
            payload = event.get("payload")
            action = payload.get("action") if isinstance(payload, dict) else None
            if not isinstance(action, str) or action not in ACTIONS:
                self.reject(event, "Apple Music action must be play, pause, next, previous, volume_up, or volume_down")
                continue
            await self.execute(action)

    async def watch(self):
        backoff = 1.0
        previous_error = None
        while not self.stop.is_set():
            process = None
            started = time.monotonic()
            try:
                process = await asyncio.create_subprocess_exec(
                    "streamctl", "--json", "watch", "--events", "apple_music.control", "--state=",
                    stdin=asyncio.subprocess.DEVNULL,
                    stdout=asyncio.subprocess.PIPE,
                    stderr=None,
                    limit=MAX_WATCH_LINE,
                )
                self.generation += 1
                # streamctl has no subscription acknowledgement on stdout. Let
                # immediate connection failures exit before marking a reconnect.
                await asyncio.sleep(0.25)
                if process.returncode is not None:
                    raise OSError(f"streamctl watch exited with status {process.returncode}")
                self.engine_error = None
                self.published.clear()
                self.save_state()
                while not self.stop.is_set():
                    line = await process.stdout.readline()
                    if not line or process.returncode is not None:
                        raise OSError("Stream Engine control watch disconnected")
                    try:
                        message = json.loads(line)
                    except (ValueError, UnicodeError) as error:
                        log(f"Ignoring malformed watch line: {brief(error)}")
                        continue
                    event = message.get("event") if isinstance(message, dict) else None
                    if not isinstance(event, dict) or event.get("type") != "apple_music.control":
                        continue
                    try:
                        self.controls.put_nowait((self.generation, time.monotonic(), event))
                    except asyncio.QueueFull:
                        self.reject(event, "Apple Music control backlog full; press the button again when idle")
            except (OSError, ValueError) as error:
                self.engine_error = "Stream Engine unavailable; start the engine to receive Apple Music controls: " + brief(error)
                self.generation += 1
                self.discard_controls("Apple Music control discarded after engine disconnect; press the button again")
                self.save_state()
                if self.engine_error != previous_error:
                    self.report("status", False, self.engine_error)
                    log(self.engine_error)
                    previous_error = self.engine_error
            finally:
                if process is not None:
                    await terminate(process)
            if time.monotonic() - started >= 30.0:
                backoff = 1.0
                previous_error = None
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2.0, 30.0)

    async def run(self):
        reader = asyncio.StreamReader(limit=MAX_MESSAGE)
        protocol = asyncio.StreamReaderProtocol(reader)
        input_transport, _ = await self.loop.connect_read_pipe(lambda: protocol, sys.stdin.buffer)
        os.set_blocking(sys.stdout.fileno(), False)
        for signum in (signal.SIGTERM, signal.SIGINT):
            self.loop.add_signal_handler(signum, self.stop.set)
        # Startup health is warn rather than a false healthy cached connection.
        self.save_state()
        tasks = [
            asyncio.create_task(self.native_reader(reader)),
            asyncio.create_task(self.watch()),
            asyncio.create_task(self.dispatcher()),
            asyncio.create_task(self.reporter()),
        ]

        def task_done(task):
            if task.cancelled():
                return
            failure = task.exception()
            if failure is not None:
                self.shutdown_error = "Apple Music host failed: " + brief(failure)
                self.shutdown_is_failure = True
                log(self.shutdown_error)
                self.stop.set()

        for task in tasks:
            task.add_done_callback(task_done)
        try:
            await self.stop.wait()
        finally:
            active_request = self.active_request
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            input_transport.close()
            self.verified = False
            if not self.shutdown_error:
                self.shutdown_error = "Apple Music browser closed; open Apple Music to reconnect"
            self.save_state()
            # Persisted engine overrides must not remain falsely green when the
            # browser closes. This attempt is bounded and never restarts engine.
            await self.cli("set", "health.apple_music", json.dumps(self.health(), ensure_ascii=True))
            failed_action = active_request[1] if active_request else self.shutdown_action
            if self.shutdown_is_failure or (failed_action and failed_action != "status"):
                item = {"action": failed_action or "status", "ok": False, "error": self.shutdown_error}
                if active_request:
                    item["id"] = active_request[0]
                await self.publish_result(item, notify=True)
        return 1 if self.shutdown_is_failure else 0


async def main():
    return await Host().run()


if __name__ == "__main__":
    try:
        raise SystemExit(asyncio.run(main()))
    except (OSError, ValueError) as error:
        log(f"Cannot start native host: {brief(error)}")
        raise SystemExit(1)
