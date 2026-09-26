#!/usr/bin/env bash
# Audio soak test (PLAN §21: zero xruns in a 4 h soak at the chosen quantum).
#
#   scripts/audio-soak.sh --duration 4h                      # attach to the running engine
#   scripts/audio-soak.sh --duration 10m --start --project project-example --quantum 256
#
# Samples the engine's own counters (perf.audio.xruns/load/dsp_ms/quantum, health.audio.rt)
# and PipeWire's per-node error counters (pw-top ERR for se-engine and the driver) once per
# interval, prints a line per sample, and a summary at the end. Exit status 0 = no xruns.
set -euo pipefail

duration=4h interval=60 socket="" start=0 project="" quantum="" keep=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --duration) duration="$2"; shift 2 ;;
    --interval) interval="$2"; shift 2 ;;
    --socket) socket="$2"; shift 2 ;;
    --start) start=1; shift ;;
    --project) project="$2"; shift 2 ;;
    --quantum) quantum="$2"; shift 2 ;;
    --keep) keep=1; shift ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

secs() {
  local d="$1"
  case "$d" in
    *h) echo $(( ${d%h} * 3600 )) ;;
    *m) echo $(( ${d%m} * 60 )) ;;
    *s) echo "${d%s}" ;;
    *) echo "$d" ;;
  esac
}
total=$(secs "$duration")
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
repo="$(cd "$(dirname "$0")/.." && pwd)"
bin_dir="${BIN_DIR:-$repo/target/release}"
[[ -x "$bin_dir/stream-engine" ]] || bin_dir="$repo/target/debug"
engine="$bin_dir/stream-engine"
cli="$bin_dir/streamctl"
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }

pid=""
cleanup() {
  if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  if [[ $keep -eq 0 && -n "${tmp:-}" ]]; then rm -rf "$tmp"; fi
}
trap cleanup EXIT

if [[ $start -eq 1 ]]; then
  tmp="$(mktemp -d /tmp/se-audio-soak.XXXXXX)"
  cp -r "${project:-$repo/project-example}" "$tmp/project"
  if [[ -n "$quantum" ]]; then
    printf '\nquantum = %s\n' "$quantum" > "$tmp/project/audio/zz-soak.toml"
  fi
  socket="$tmp/engine.sock"
  port=$(( 20000 + RANDOM % 20000 ))
  "$engine" daemon --project "$tmp/project" --socket "$socket" --http "127.0.0.1:$port" --osc "127.0.0.1:$(( port + 1 ))" --data-dir "$tmp/data" >"$tmp/engine.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 100); do [[ -S "$socket" ]] && break; sleep 0.1; done
  sleep 3
  echo "engine pid $pid, log $tmp/engine.log"
fi
sock_args=()
[[ -n "$socket" ]] && sock_args=(--socket "$socket")

get() { "$cli" "${sock_args[@]}" --json get "$1" | jq -r --arg a "$1" 'select(.address == $a) | .value | tostring'; }
pw_err() { timeout 5 pw-top -b -n 2 2>/dev/null | awk -v n="$1" '$NF == n { e = $9 } END { print (e == "" ? "-" : e) }'; }

x0=$(get perf.audio.xruns); e0=$(pw_err se-engine)
q=$(get perf.audio.quantum); rate=$(get perf.audio.rate)
echo "soak: ${total}s, quantum $q @ ${rate} Hz, rt: $("$cli" "${sock_args[@]}" --json get health.audio.rt | jq -r '.value.status + " — " + .value.detail')"
printf '%-8s %-8s %-8s %-8s %-10s %s\n' elapsed xruns load dsp_ms quantum pw_err
t=0 max_load=0
while (( t < total )); do
  step=$(( interval < total - t ? interval : total - t ))
  sleep "$step"; t=$(( t + step ))
  x=$(get perf.audio.xruns); l=$(get perf.audio.load); d=$(get perf.audio.dsp_ms); qq=$(get perf.audio.quantum)
  e=$(pw_err se-engine)
  max_load=$(awk -v a="$max_load" -v b="$l" 'BEGIN { print (b > a ? b : a) }')
  printf '%-8s %-8s %-8s %-8s %-10s %s\n' "${t}s" "$(( x - x0 ))" "$l" "$d" "$qq" "$e"
done
x1=$(get perf.audio.xruns); e1=$(pw_err se-engine)
xr=$(( x1 - x0 ))
echo "---"
echo "duration ${total}s  quantum $(get perf.audio.quantum)  engine xruns $xr  max load $max_load  pw-top ERR se-engine ${e0} → ${e1}  allocs in callback $(get perf.audio.allocs)"
[[ $xr -eq 0 ]]
