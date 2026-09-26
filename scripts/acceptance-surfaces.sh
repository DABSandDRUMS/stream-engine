#!/usr/bin/env bash
# Guided hardware acceptance for M7 (cue list from a deck key and the X-TOUCH fader) and M8
# (the same preset from deck, MIDI, footswitch, voice, keybind and chat; X-TOUCH LED rings).
#
#   scripts/acceptance-surfaces.sh            run every step
#   scripts/acceptance-surfaces.sh deck fbv   run only these steps
#
# Steps: deck xtouch fbv voice keybind chat rings cuelist. Each step tells you what to press and
# passes when the engine reports the matching event with the right origin (a physical press
# can't be faked by the script). Uses the running engine and the example project's mappings
# (controllers/deck.toml, xtouch.toml, fbv.toml). Needs jq; the voice step also needs espeak-ng.
set -uo pipefail

CTL=${STREAMCTL:-streamctl}
TIMEOUT=${TIMEOUT:-45}
declare -A RESULT
ORDER=()

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
note() { printf '  %s\n' "$*"; }

# wait_jq <jq program over `inputs`> [watch args…]: true when the program prints something
# before TIMEOUT. The watcher is stopped as soon as jq has its answer.
wait_jq() {
  local prog=$1
  shift
  local fifo ok=1
  fifo=$(mktemp -u)
  mkfifo "$fifo"
  timeout "$TIMEOUT" "$CTL" --json watch "$@" 2>/dev/null >"$fifo" &
  local wpid=$!
  if jq --unbuffered -c -n "$prog" <"$fifo" | grep -q .; then ok=0; fi
  kill "$wpid" 2>/dev/null
  wait "$wpid" 2>/dev/null
  rm -f "$fifo"
  return $ok
}

# wait_for <jq filter over one watch line> [watch args…]
wait_for() {
  local filter=$1
  shift
  wait_jq "first(inputs | select($filter))" "$@"
}

record() {
  ORDER+=("$1")
  RESULT[$1]=$2
  if [[ $2 == pass ]]; then printf '  \033[32m✓ %s\033[0m\n' "$1"; else printf '  \033[31m✗ %s (%s)\033[0m\n' "$1" "$2"; fi
}

release_hype() { "$CTL" release hype >/dev/null 2>&1 || true; }

fired_from() { # preset origin
  printf '.event.type == "preset.%s.fired" and .event.origin == "%s"' "$1" "$2"
}

step_deck() {
  say "Stream Deck: press the HYPE key (page SHOW, second row, first key)."
  "$CTL" do "deck.page show" >/dev/null 2>&1
  if wait_for "$(fired_from hype deck)" --events 'preset.**' --state=; then record deck pass; else record deck "no deck press within ${TIMEOUT}s"; fi
  release_hype
}

step_xtouch() {
  say "X-TOUCH MINI: press the first button of the top row (HYPE)."
  if wait_for "$(fired_from hype midi)" --events 'preset.**' --state=; then record xtouch pass; else record xtouch "no X-TOUCH press within ${TIMEOUT}s"; fi
  release_hype
}

step_fbv() {
  say "FBV Express: step on footswitch A (HYPE)."
  # the X-TOUCH also has origin midi: require the footswitch's own control event first
  local ok=fail
  if wait_jq 'first(foreach inputs as $l (false; . or (($l.event.type // "") | startswith("midi.fbv."));
        if . and $l.event.type == "preset.hype.fired" and $l.event.origin == "midi" then $l else empty end))' \
    --events 'midi.**' --events 'preset.**' --state=; then
    ok=pass
  fi
  if [[ $ok == pass ]]; then record fbv pass; else record fbv "no footswitch press within ${TIMEOUT}s"; fi
  release_hype
}

step_voice() {
  say "Voice: press Enter and the script speaks \"fire confetti\" through the voice path"
  note "(espeak-ng → voice.file), or type m + Enter to do it yourself: hold the microphone key"
  note "(deck page MIX, bottom row, middle) and say \"fire confetti\"."
  local a wav
  read -r a
  wait_for "$(fired_from confetti voice)" --events 'preset.**' --state= &
  local w=$!
  if [[ $a != [mM]* ]]; then
    wav=$(mktemp --suffix .wav)
    sleep 1
    espeak-ng -s 130 -w "$wav" "fire confetti" && "$CTL" do "voice.file $wav" >/dev/null
  fi
  if wait "$w"; then record voice pass; else record voice "no voice command within ${TIMEOUT}s"; fi
  rm -f "${wav:-}"
}

step_keybind() {
  say "Keybind: click into the Stream Engine window and press F6 (HYPE pad),"
  note "or press a Hyprland bind that runs \`streamctl preset hype\`."
  if wait_for '(.event.type == "preset.hype.fired") and (.event.origin == "ui" or .event.origin == "cli")' --events 'preset.**' --state=; then
    record keybind pass
  else
    record keybind "no key press within ${TIMEOUT}s"
  fi
  release_hype
}

step_chat() {
  say "Chat: the script sends a VIP \`!hype\` through the chat path (simulated viewer)."
  note "Chat effects only run on air, so the show mode goes to live for a moment (OBS isn't touched)."
  local mode
  mode=$("$CTL" get show.mode 2>/dev/null | awk '{print $3}')
  wait_for "$(fired_from hype chat)" --events 'preset.**' --state= &
  local w=$!
  sleep 1
  "$CTL" mode live >/dev/null
  "$CTL" sim chat message='!hype' role=vip >/dev/null
  if wait "$w"; then record chat pass; else record chat "the bot didn't fire the preset (!hype has a 2-minute cooldown: wait and retry)"; fi
  "$CTL" mode "${mode:-offline}" >/dev/null 2>&1
  release_hype
}

step_rings() {
  say "X-TOUCH LED rings: watch encoder 1's ring. It sweeps up, then back down."
  local target
  target=$(awk -F'"' '/^control *= *"enc\.1"/ { m = 1; next } m && /^target *=/ { print $2; exit }' "${PROJECT:-$HOME/stream-project}/controllers/xtouch.toml")
  if [[ -z $target ]]; then
    record rings "no ring mapping in controllers/xtouch.toml"
    return
  fi
  note "(ring follows \`$target\`)"
  for v in 0 0.25 0.5 0.75 1 0.75 0.5 0.25 0; do
    "$CTL" set "$target" "$v" >/dev/null 2>&1
    sleep 0.4
  done
  "$CTL" release "$target" >/dev/null 2>&1
  read -r -p "  Did the ring sweep up and down? [y/n] " a
  if [[ $a == [yY]* ]]; then record rings pass; else record rings "ring didn't follow"; fi
}

step_cuelist() {
  say "Cue list from the deck: press LX GO (deck page MIX, top row, last key)."
  "$CTL" do "lights.release main" >/dev/null 2>&1
  "$CTL" do "deck.page mix" >/dev/null 2>&1
  if wait_for '.event.type == "lights.cue.go" and .event.origin == "deck" and .event.payload.cuelist == "main"' --events 'lights.**' --state=; then
    record cuelist.deck pass
  else
    record cuelist.deck "no LX GO press within ${TIMEOUT}s"
  fi
  "$CTL" do "lights.release main" >/dev/null 2>&1
  say "Cue list from the X-TOUCH fader: pull the fader all the way down, then push it up."
  note "(fader start: raising it from zero starts the list; the master follows the fader)"
  if wait_for '.state["lights.cuelist.main.playing"] == true' --events= --state 'lights.cuelist.main.**'; then
    record cuelist.fader pass
  else
    record cuelist.fader "the list didn't start from the fader"
  fi
  "$CTL" do "lights.release main" >/dev/null 2>&1
  "$CTL" release lights.cuelist.main.master >/dev/null 2>&1
}

command -v jq >/dev/null || { echo "needs jq" >&2; exit 2; }
"$CTL" status >/dev/null 2>&1 || { echo "the engine isn't running (systemctl --user start stream-engine)" >&2; exit 2; }

steps=("$@")
[[ ${#steps[@]} -eq 0 ]] && steps=(deck xtouch fbv voice keybind chat rings cuelist)
for s in "${steps[@]}"; do
  if declare -F "step_$s" >/dev/null; then "step_$s"; else echo "unknown step $s" >&2; exit 2; fi
done

say "Results"
fail=0
for k in "${ORDER[@]}"; do
  printf '  %-14s %s\n' "$k" "${RESULT[$k]}"
  [[ ${RESULT[$k]} == pass ]] || fail=1
done
exit $fail
