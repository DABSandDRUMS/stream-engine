#!/usr/bin/env python3
# LLM clip ranker for `[clips] rank_command` (PLAN §18, docs/clips.md "Hooks"): scores each
# candidate 0-10, picks in/out on sentence boundaries, titles it, or drops it.
#
# stdin:  {"session", "min_len", "max_len", "candidates": [{key, score, marker_score, reasons,
#          labels, in, out, peak, transcript_from, transcript_to, transcript, words}]}
# stdout: {"clips": [{"key", "score"?, "in"?, "out"?, "title"?} | {"key", "drop": true}]}
#
# Talks to any OpenAI-compatible chat completions endpoint (Ollama, llama.cpp, vLLM, OpenAI, …).
# Environment:
#   SE_RANK_URL      base URL (http://127.0.0.1:11434/v1) or the full …/chat/completions URL
#   SE_RANK_MODEL    model name
#   SE_RANK_TIMEOUT  total seconds for all requests (default 90; keep below `[clips] rank_timeout`)
#   SE_RANK_BATCH    candidates per request (default 8; smaller fits small context windows)
#   SE_RANK_KEY      API key; else `secret-tool lookup service stream-engine username clips.rank_key`;
#                    none is fine for local servers
# Exit status: 0 = answers on stdout; 1 = any failure (one-line reason on stderr; the engine
# then keeps its deterministic ranking). Python 3 standard library only.

import json
import math
import os
import re
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

KEYRING_SERVICE = "stream-engine"  # se-store's keyring service
KEYRING_NAME = "clips.rank_key"
PAUSE = 0.8  # same sentence split as se-clips: terminal punctuation or a pause >= 0.8 s
SNAP = 1.0  # max distance (s) from a suggested in/out to the sentence boundary it snaps to
LEAD, TAIL = 0.25, 0.4  # padding around the chosen sentences, as the deterministic trim
TITLE_MAX = 140
ANSWER_KEYS = {"key", "score", "in", "out", "title", "drop"}

SYSTEM = (
    "You are the editor picking short highlight clips from a livestream recording. "
    "Judge each candidate from its transcript and hype signals: would a viewer who missed the "
    "stream enjoy it on its own (payoff, humor, surprise, skill, strong reaction)? "
    "Answer with a single JSON object and nothing else."
)


class Fail(Exception):
    pass


def fail(msg, key=None):
    msg = str(msg)
    if key:
        msg = msg.replace(key, "***")
    print("rank-clips-llm: " + " ".join(msg.split()), file=sys.stderr)
    sys.exit(1)


def num(v):
    """A finite JSON number (bools are not numbers)."""
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        return None
    v = float(v)
    return v if math.isfinite(v) else None


# --- transcript --------------------------------------------------------------------------------


def ends_sentence(w):
    return w.rstrip("\"')]").endswith((".", "!", "?"))


def sentences(words):
    """[(t0, t1, text)] of spoken words, split like se-clips `select::sentences`."""
    spoken = [w for w in words if not w.get("annotation")]
    out, cur = [], []
    for i, w in enumerate(spoken):
        cur.append(w)
        nxt = spoken[i + 1] if i + 1 < len(spoken) else None
        if nxt is None or nxt["t0"] - w["t1"] >= PAUSE or ends_sentence(w["w"]):
            out.append((cur[0]["t0"], cur[-1]["t1"], " ".join(x["w"].strip() for x in cur)))
            cur = []
    return out


def clean_words(raw):
    words = []
    for w in raw if isinstance(raw, list) else []:
        if not isinstance(w, dict) or not isinstance(w.get("w"), str):
            continue
        t0, t1 = num(w.get("t0")), num(w.get("t1"))
        if t0 is None or t1 is None or t1 < t0:
            continue
        words.append({"t0": t0, "t1": t1, "w": w["w"], "annotation": w.get("annotation") is True})
    words.sort(key=lambda w: w["t0"])
    return words


class Cand:
    def __init__(self, c):
        self.key = c["key"]
        self.words = clean_words(c.get("words"))
        self.sents = sentences(self.words)
        lo, hi = num(c.get("transcript_from")), num(c.get("transcript_to"))
        if lo is None or hi is None or hi <= lo:
            raise Fail(f"candidate {self.key}: bad transcript window")
        self.lo, self.hi = max(lo, 0.0), hi  # the engine's bounds for in/out
        self.c = c

    def prompt(self):
        c = self.c
        f = lambda v: f"{num(v):.1f}" if num(v) is not None else "?"
        reasons = ", ".join(str(r) for r in c.get("reasons") or []) or "none"
        labels = ", ".join(str(r) for r in c.get("labels") or []) or "none"
        lines = [
            f"### {self.key}",
            f"hype score {f(c.get('marker_score'))} (reasons: {reasons}; labels: {labels}); "
            f"peak {f(c.get('peak'))}; current in/out {f(c.get('in'))}-{f(c.get('out'))}; "
            f"window {self.lo:.1f}-{self.hi:.1f}",
        ]
        if self.sents:
            lines += [f"{t0:.1f}-{t1:.1f} {text}" for t0, t1, text in self.sents]
        else:
            lines.append("(no speech)")
        sounds = [f"{w['w']} {w['t0']:.1f}" for w in self.words if w["annotation"]]
        if sounds:
            lines.append("sounds: " + "; ".join(sounds))
        return "\n".join(lines)

    def _pad(self, t0, t1):
        """Engine-style padding: up to LEAD/TAIL, at most half the gap to the next spoken word."""
        spoken = [w for w in self.words if not w["annotation"]]
        prev = [w["t1"] for w in spoken if w["t1"] <= t0 + 1e-6 and w["t0"] < t0 - 1e-6]
        nxt = [w["t0"] for w in spoken if w["t0"] >= t1 - 1e-6 and w["t1"] > t1 + 1e-6]
        lead = min(LEAD, (t0 - max(prev)) / 2) if prev else LEAD
        tail = min(TAIL, (min(nxt) - t1) / 2) if nxt else TAIL
        return max(t0 - max(lead, 0.0), self.lo), min(t1 + max(tail, 0.0), self.hi)

    def trim(self, t_in, t_out, min_len, max_len):
        """Snap a suggested in/out to sentence boundaries and check it; None when invalid."""
        if t_in is None or t_out is None or not t_in < t_out:
            return None
        if not (self.lo - SNAP <= t_in and t_out <= self.hi + SNAP):
            return None
        if self.sents:
            starts = [s[0] for s in self.sents]
            ends = [s[1] for s in self.sents]
            s_in = min(starts, key=lambda t: abs(t - t_in))
            s_out = min(ends, key=lambda t: abs(t - t_out))
            if abs(s_in - t_in) > SNAP or abs(s_out - t_out) > SNAP or not s_in < s_out:
                return None
            options = [self._pad(s_in, s_out), (s_in, s_out)]
        else:
            options = [(t_in, t_out)]
        for a, b in options:
            if self.lo <= a < b <= self.hi and min_len - 1e-6 <= b - a <= max_len + 1e-6:
                return round(a, 3), round(b, 3)
        return None


# --- model I/O ---------------------------------------------------------------------------------


def build_messages(cands, min_len, max_len):
    rules = (
        f"Candidates below; times are recording seconds. For every candidate return one entry:\n"
        f'- keep: {{"key": "<key>", "score": <0-10>, "in": <sec>, "out": <sec>, "title": "<title>"}}\n'
        f'- drop (dead air, nothing happens, makes no sense without context): {{"key": "<key>", "drop": true}}\n'
        f"Rules: score 10 = must post, 5 = decent, 0 = worthless. \"in\" must be the start time of a listed "
        f"sentence and \"out\" the end time of a listed sentence, inside the candidate's window, with "
        f"out - in between {min_len:g} and {max_len:g} seconds; include the peak and the payoff, cut "
        f"setup that isn't needed. Title: at most 60 characters, catchy, no hashtags or emojis. "
        f"With no speech, omit in/out.\n"
        f'Reply with JSON only: {{"clips": [...]}}'
    )
    body = "\n\n".join([rules] + [c.prompt() for c in cands])
    return [{"role": "system", "content": SYSTEM}, {"role": "user", "content": body}]


def post(url, key, payload, timeout):
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json", "Accept": "application/json", "User-Agent": "stream-engine-rank-clips"},
        method="POST",
    )
    if key:
        req.add_header("Authorization", "Bearer " + key)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8", "replace"))
    except urllib.error.HTTPError as e:
        detail = e.read(300).decode("utf-8", "replace")
        raise Fail(f"HTTP {e.code} from {url}: {detail}") from None
    except urllib.error.URLError as e:
        raise Fail(f"{url}: {e.reason}") from None
    except TimeoutError:
        raise Fail(f"{url}: timed out") from None
    except json.JSONDecodeError:
        raise Fail(f"{url}: reply is not JSON") from None


def reply_text(resp):
    try:
        content = resp["choices"][0]["message"]["content"]
    except (KeyError, IndexError, TypeError):
        raise Fail("reply has no choices[0].message.content") from None
    if isinstance(content, list):  # content parts
        content = "".join(p.get("text", "") for p in content if isinstance(p, dict))
    if not isinstance(content, str) or not content.strip():
        raise Fail("empty reply from the model")
    return content


def extract_json(text):
    """The answer object from a reply that may wrap it in prose, code fences or <think>."""
    text = re.sub(r"<think>.*?</think>", "", text, flags=re.S).strip()
    tries = [text]
    tries += [m.group(1) for m in re.finditer(r"```(?:json)?\s*(.*?)```", text, flags=re.S)]
    for t in tries:
        try:
            return json.loads(t)
        except json.JSONDecodeError:
            pass
    dec = json.JSONDecoder()
    for m in re.finditer(r"[{\[]", text):
        try:
            v, _ = dec.raw_decode(text, m.start())
        except json.JSONDecodeError:
            continue
        if isinstance(v, dict) and "clips" in v or isinstance(v, list):
            return v
    raise Fail("no JSON in the model's reply")


def answers_of(v):
    if isinstance(v, dict) and isinstance(v.get("clips"), list):
        return v["clips"]
    if isinstance(v, list):
        return v
    raise Fail('model reply has no "clips" list')


def validate(ans, by_key, min_len, max_len, done):
    """One model answer → the entry for the engine (known keys only), or None when nothing valid."""
    if not isinstance(ans, dict):
        return None
    ans = {k: v for k, v in ans.items() if k in ANSWER_KEYS}
    key = ans.get("key")
    if not isinstance(key, str) or key not in by_key or key in done:
        return None
    if ans.get("drop") is True:
        return {"key": key, "drop": True}
    out = {"key": key}
    s = num(ans.get("score"))
    if s is not None:
        out["score"] = round(min(max(s, 0.0), 10.0), 2)
    trim = by_key[key].trim(num(ans.get("in")), num(ans.get("out")), min_len, max_len)
    if trim:
        out["in"], out["out"] = trim
    t = ans.get("title")
    if isinstance(t, str):
        t = " ".join(t.split()).strip("\"'“”")
        if t:
            out["title"] = t[:TITLE_MAX]
    return out if len(out) > 1 else None


# --- main --------------------------------------------------------------------------------------


def keyring_key(deadline):
    if os.environ.get("SE_RANK_KEY"):
        return os.environ["SE_RANK_KEY"]
    try:
        r = subprocess.run(
            ["secret-tool", "lookup", "service", KEYRING_SERVICE, "username", KEYRING_NAME],
            capture_output=True, text=True, timeout=max(1.0, min(5.0, deadline - time.monotonic())),
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return (r.stdout.strip() or None) if r.returncode == 0 else None


def env_num(name, default, cast):
    v = os.environ.get(name, "").strip()
    if not v:
        return default
    try:
        n = cast(v)
    except ValueError:
        n = None
    if n is None or not math.isfinite(n) or n <= 0:
        fail(f"{name}={v!r} is not a positive number")
    return n


def main():
    base = os.environ.get("SE_RANK_URL", "").strip()
    model = os.environ.get("SE_RANK_MODEL", "").strip()
    if not base or not model:
        fail("set SE_RANK_URL and SE_RANK_MODEL")
    url = base if base.rstrip("/").endswith("/chat/completions") else base.rstrip("/") + "/chat/completions"
    timeout = env_num("SE_RANK_TIMEOUT", 90.0, float)
    batch = env_num("SE_RANK_BATCH", 8, int)
    deadline = time.monotonic() + timeout

    try:
        inp = json.load(sys.stdin)
        min_len, max_len = num(inp["min_len"]), num(inp["max_len"])
        raw = inp["candidates"]
        if min_len is None or max_len is None or not isinstance(raw, list):
            raise ValueError
    except (ValueError, KeyError, TypeError):
        fail("stdin is not the rank_command input")
    try:
        cands = [Cand(c) for c in raw if isinstance(c, dict) and isinstance(c.get("key"), str)]
    except Fail as e:
        fail(e)
    if not cands:
        print(json.dumps({"clips": []}))
        return

    key = keyring_key(deadline)

    # Hard stop for the whole run, whatever the server does (trickling bytes, slow DNS, …).
    def on_alarm(*_):
        raise Fail(f"no answer within SE_RANK_TIMEOUT={timeout:g} s")

    signal.signal(signal.SIGALRM, on_alarm)
    signal.setitimer(signal.ITIMER_REAL, max(deadline - time.monotonic(), 0.01))

    by_key = {c.key: c for c in cands}
    clips, done = [], set()
    json_mode = True
    try:
        for i in range(0, len(cands), batch):
            part = cands[i : i + batch]
            payload = {"model": model, "messages": build_messages(part, min_len, max_len), "stream": False}
            while True:
                if json_mode:
                    payload["response_format"] = {"type": "json_object"}
                else:
                    payload.pop("response_format", None)
                try:
                    resp = post(url, key, payload, max(deadline - time.monotonic(), 0.01))
                    break
                except Fail as e:
                    # servers without JSON mode reject response_format: retry once without it
                    if json_mode and str(e).startswith("HTTP 400"):
                        json_mode = False
                        continue
                    raise
            for ans in answers_of(extract_json(reply_text(resp))):
                entry = validate(ans, by_key, min_len, max_len, done)
                if entry:
                    clips.append(entry)
                    done.add(entry["key"])
    except Fail as e:
        signal.setitimer(signal.ITIMER_REAL, 0)
        fail(e, key)
    signal.setitimer(signal.ITIMER_REAL, 0)

    if not clips:
        fail("the model gave no usable answer")
    print(json.dumps({"clips": clips}, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # anything unexpected is still a one-line failure
        fail(f"{type(e).__name__}: {e}", os.environ.get("SE_RANK_KEY"))
