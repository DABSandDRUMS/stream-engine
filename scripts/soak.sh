#!/usr/bin/env bash
# Engine soak test (PLAN §23: an 8 h run with the simulator firing events at high rate; memory,
# frame drops, xruns, and restore-after-kill checked).
#
#   scripts/soak.sh --start                                     # own dev engine, 8 h
#   scripts/soak.sh --start --duration 10m --interval 15s --kill-at 5m
#   scripts/soak.sh --duration 8h --allow-kill-service          # the installed service, off air only
#
# Fires random simulator presets (`streamctl sim`, the engine's own `sim.presets` list with
# randomized sizes) at --rate per second for the whole run, samples the engine every --interval
# (RSS from /proc/<pid>/status, perf.vram_mb, perf.fps, perf.dropped, perf.late,
# perf.audio.xruns; "n/a" when the subsystem is off), and once — at --kill-at — sets runtime state
# that must survive (mode, program and preview scene, a toggle preset, a disabled rule), kill -9s
# the engine, waits for it to come back, and compares.
#
# Engine: --start launches a private dev engine from a copy of --project (default project-example)
# in a fresh dir under $TMPDIR (own socket, SE_RUNTIME_DIR, data dir, ports) with every hardware
# and network subsystem off (SE_DISABLE; audio, devices, video-in, input, mixer are never started,
# nor twitch/songs/tiktok/extras/tts/clips/obs/lights/web); the core, alerts, bot, patches and
# timelines run. The renderer stays off because a second compositor on the GPU steals frame time
# from the live engine, so vram/fps/dropped/late read n/a; --render turns it on and is refused
# while the stream-engine service is active. The script restarts its own engine after the kill.
# Without --start it attaches to --socket (default: the service's socket). It kills the systemd
# service (and waits for systemd to restart it) only with --allow-kill-service; any other attached
# engine skips the kill. Attached runs refuse to start while show.mode is `live`: the simulated
# events reach alerts, the bot and the stream.
#
# Options: --duration D (8h)  --interval D (60s)  --rate N events/s (5)  --kill-at D|off (half
#   the run)  --port N (--start http port, osc = N+1; default random)  --keep (keep the work dir)
#   --warmup D (min(10m, duration/10))  --max-rss-slope MB/h (10)  --max-mem-factor F (1.5)
#   --max-dropped N (0)  --max-restart S (3)  --render  --project DIR  --socket PATH
#   Durations: 90, 90s, 15m, 8h. BIN_DIR overrides the binaries (default target/release, else
#   target/debug; streamctl from PATH if not built there).
#
# Fails (exit 1) when, in any engine run (before/after the kill, measured after --warmup):
#   - RSS (or VRAM) at the end exceeds the post-warm-up value × --max-mem-factor
#   - the RSS trend exceeds --max-rss-slope MB/h (judged only on runs of ≥ 30 min after warm-up)
#   - perf.audio.xruns grows at all, or perf.dropped grows by more than --max-dropped
#   - the engine dies or restarts outside the planned kill, or a simulator command is refused
#   - the restored state differs from the state before the kill, or the engine takes longer than
#     --max-restart seconds from kill -9 to ready (PLAN §17.3: output back within ~3 s)
# Exit 2 = bad usage or the engine could not be started/reached.
set -euo pipefail

duration=8h interval=60 rate=5 socket="" start=0 project="" port="" keep=0 kill_at="" warmup=""
max_slope=10 max_factor=1.5 max_dropped=0 max_restart=3 render=0 allow_service=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --duration) duration="$2"; shift 2 ;;
    --interval) interval="$2"; shift 2 ;;
    --rate) rate="$2"; shift 2 ;;
    --socket) socket="$2"; shift 2 ;;
    --start) start=1; shift ;;
    --project) project="$2"; shift 2 ;;
    --port) port="$2"; shift 2 ;;
    --render) render=1; shift ;;
    --kill-at) kill_at="$2"; shift 2 ;;
    --allow-kill-service) allow_service=1; shift ;;
    --warmup) warmup="$2"; shift 2 ;;
    --max-rss-slope) max_slope="$2"; shift 2 ;;
    --max-mem-factor) max_factor="$2"; shift 2 ;;
    --max-dropped) max_dropped="$2"; shift 2 ;;
    --max-restart) max_restart="$2"; shift 2 ;;
    --keep) keep=1; shift ;;
    -h|--help) sed -n '2,/^set -euo pipefail/{/^set -euo/d;s/^# \{0,1\}//;p}' "$0"; exit 0 ;;
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
total=$(secs "$duration") interval=$(secs "$interval")
case "$kill_at" in
  off) kill_at=-1 ;;
  "") kill_at=$(( total / 2 )) ;;
  *) kill_at=$(secs "$kill_at") ;;
esac
if [[ -n "$warmup" ]]; then warmup=$(secs "$warmup"); else warmup=$(( total / 10 < 600 ? total / 10 : 600 )); fi
[[ $total -gt 0 && $interval -gt 0 && $rate -ge 1 ]] || { echo "--duration, --interval and --rate must be positive" >&2; exit 2; }
[[ $kill_at -lt $total ]] || { echo "--kill-at must be before the end of the run" >&2; exit 2; }
[[ $start -eq 1 && -n "$socket" ]] && { echo "--start runs its own engine; drop --socket" >&2; exit 2; }
[[ $start -eq 0 && $render -eq 1 ]] && { echo "--render only applies to --start" >&2; exit 2; }

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
repo="$(cd "$(dirname "$0")/.." && pwd)"
bin_dir="${BIN_DIR:-$repo/target/release}"
[[ -x "$bin_dir/stream-engine" ]] || bin_dir="$repo/target/debug"
engine="$bin_dir/stream-engine"
cli="$bin_dir/streamctl"
[[ -x "$cli" ]] || cli="$(command -v streamctl || true)"
[[ -n "$cli" ]] || { echo "streamctl not found (cargo build -p se-cli)" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }

work="$(mktemp -d "${TMPDIR:-/tmp}/se-soak.XXXXXX")"
pid="" sim_pid="" own_pid=""
cleanup() {
  if [[ -n "$sim_pid" ]]; then kill "$sim_pid" 2>/dev/null || true; wait "$sim_pid" 2>/dev/null || true; fi
  if [[ -n "$own_pid" ]]; then kill "$own_pid" 2>/dev/null || true; wait "$own_pid" 2>/dev/null || true; fi
  if [[ $keep -eq 0 ]]; then rm -rf "$work"; else echo "kept $work"; fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

sock_args=()
ctl() { "$cli" "${sock_args[@]}" "$@"; }
get() { ctl --json get "$1" | jq -r --arg a "$1" 'select(.address == $a) | .value | tostring'; }
engine_pid() { ctl status 2>/dev/null | awk '$1 == "engine" { for (i = 2; i < NF; i++) if ($i == "pid") print $(i + 1) }'; }
service_pid() { systemctl --user show -p MainPID --value stream-engine 2>/dev/null || echo 0; }
ready_count() { grep -c 'engine ready' "$work/engine.log" || true; }
fdiff() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", a - b }'; }
per_sec() { awk -v n="$1" -v t="$2" 'BEGIN { printf "%.1f", (t > 0 ? n / t : 0) }'; }

# --start: a private engine. `omarchy` is shadowed by a stub so a mode change in the dev engine
# never flips the desktop's idle setting (se-app/src/omarchy.rs).
disable="devices,video-in,render,extras,tts,tiktok,clips,obs,audio,web,twitch,input,mixer,songs,lights"
launch() {
  SE_DISABLE="$disable" SE_RUNTIME_DIR="$work/run" PATH="$work/bin:$PATH" \
    "$engine" daemon --project "$work/project" --socket "$socket" --http "127.0.0.1:$port" \
    --osc "127.0.0.1:$(( port + 1 ))" --data-dir "$work/data" >>"$work/engine.log" 2>&1 &
  own_pid=$! pid=$!
}
# Waits until the log shows the n-th "engine ready"; fails if the engine exits or $2 s pass.
wait_ready() {
  local n="$1" deadline=$(( SECONDS + $2 ))
  while [[ $(ready_count) -lt $n ]]; do
    kill -0 "$own_pid" 2>/dev/null || return 1
    [[ $SECONDS -lt $deadline ]] || return 1
    sleep 0.05
  done
}

mode_kind=attached
if [[ $start -eq 1 ]]; then
  if [[ $render -eq 1 ]]; then
    if systemctl --user is-active --quiet stream-engine 2>/dev/null; then
      echo "--render: the stream-engine service is running; a second renderer would take GPU time from it. Stop it first." >&2
      exit 2
    fi
    disable="${disable/render,/}"
  fi
  [[ -x "$engine" ]] || { echo "no engine binary at $engine (cargo build -p se-app)" >&2; exit 2; }
  mkdir -m 700 "$work/run" "$work/bin"
  printf '#!/bin/sh\nexit 1\n' >"$work/bin/omarchy" && chmod +x "$work/bin/omarchy"
  cp -r "${project:-$repo/project-example}" "$work/project"
  socket="$work/run/engine.sock"
  port="${port:-$(( 20000 + RANDOM % 20000 ))}"
  launch
  wait_ready 1 120 || { echo "engine did not become ready; log:" >&2; tail -20 "$work/engine.log" >&2; exit 2; }
  mode_kind=own
  echo "engine pid $pid ($engine), log $work/engine.log, SE_DISABLE=$disable"
fi
[[ -n "$socket" ]] && sock_args=(--socket "$socket")

if [[ $start -eq 0 ]]; then
  pid=$(engine_pid)
  [[ -n "$pid" ]] || { echo "cannot reach the engine${socket:+ at $socket}" >&2; exit 2; }
  if [[ "$pid" == "$(service_pid)" ]]; then mode_kind=service; fi
  if [[ "$(get show.mode)" == live ]]; then
    echo "the engine is on air (show.mode = live); soak it off air — simulated events reach the stream" >&2
    exit 2
  fi
  echo "attached to engine pid $pid ($mode_kind)"
fi
kill_note=""
if [[ $kill_at -ge 0 && $mode_kind == service && $allow_service -eq 0 ]]; then
  kill_at=-1 kill_note="skipped: attached to the stream-engine service (pass --allow-kill-service to kill it)"
elif [[ $kill_at -ge 0 && $mode_kind == attached ]]; then
  kill_at=-1 kill_note="skipped: attached to an engine this script did not start and systemd does not manage"
elif [[ $kill_at -lt 0 ]]; then
  kill_note="skipped: --kill-at off"
fi

# ---- simulator ------------------------------------------------------------------------------
mapfile -t presets < <(ctl --json query sim.presets | jq -r '.[].name')
[[ ${#presets[@]} -gt 0 ]] || { echo "the engine lists no simulator presets" >&2; exit 2; }
sim_loop() {
  local n=0 errs=0 p ms out
  local -a args
  while :; do
    if [[ -e "$work/pause" ]]; then : >"$work/paused"; sleep 0.1; continue; fi
    rm -f "$work/paused"
    p="${presets[RANDOM % ${#presets[@]}]}"
    case "$p" in
      cheer) args=("bits=$(( 1 + RANDOM % 5000 ))") ;;
      gift_bomb) args=("count=$(( 1 + RANDOM % 100 ))") ;;
      raid) args=("viewers=$(( 1 + RANDOM % 2000 ))") ;;
      resub) args=("months=$(( 2 + RANDOM % 48 ))") ;;
      tip) args=("amount=$(( 1 + RANDOM % 100 ))") ;;
      *) args=() ;;
    esac
    if out=$(ctl sim "$p" "${args[@]}" 2>&1); then
      n=$(( n + 1 ))
    else
      errs=$(( errs + 1 ))
      printf '%s\n' "$out" >>"$work/sim-errors.log"
    fi
    printf '%s %s\n' "$n" "$errs" >"$work/sim.count.new" && mv -f "$work/sim.count.new" "$work/sim.count"
    ms=$(( RANDOM * 2000 / rate / 32768 ))
    sleep "$(printf '%d.%03d' $(( ms / 1000 )) $(( ms % 1000 )))"
  done
}
sim_counts() { cat "$work/sim.count" 2>/dev/null || echo "0 0"; }
pause_sim() {
  rm -f "$work/paused"; : >"$work/pause"
  local deadline=$(( SECONDS + 15 ))
  while [[ ! -e "$work/paused" && $SECONDS -lt $deadline ]]; do sleep 0.05; done
}
resume_sim() { rm -f "$work/pause"; }

# ---- sampling -------------------------------------------------------------------------------
# perf counters → "vram fps dropped late xruns" ("n/a" for missing signals)
perf() {
  ctl --json get 'perf.**' | jq -rs 'map({(.address): .value}) | add // {}
    | [.["perf.vram_mb"], .["perf.fps"], .["perf.dropped"], .["perf.late"], .["perf.audio.xruns"]]
    | map(if . == null then "n/a" else tostring end) | join(" ")'
}
rss_mb() { awk '/^VmRSS:/ { printf "%.1f", $2 / 1024 }' "/proc/$1/status" 2>/dev/null || true; }
delta() { if [[ -z "$1" || -z "$2" || "$1" == n/a || "$2" == n/a ]]; then echo n/a; else echo $(( $1 - $2 )); fi; }

failures=() seg=0 seg_open=0 seg_lines=()
# Starts an engine run ("segment"): counters restart with the engine.
open_segment() {
  seg=$(( seg + 1 )) seg_t0=$1 seg_pid=$pid seg_open=1
  read -r _ _ seg_d0 seg_l0 seg_x0 <<<"$(perf)"
  : >"$work/seg$seg.samples"
}
# Judges the finished run from its samples ("elapsed rss vram dropped late xruns").
close_segment() {
  local f="$work/seg$seg.samples" t1=$1 bad detail last xr dr lt
  seg_open=0
  IFS='|' read -r bad detail < <(awk -v warm=$(( seg_t0 + warmup )) -v slope_max="$max_slope" -v fmax="$max_factor" '
    $1 >= warm && $2 != "n/a" {
      n++; x = $1 / 3600; sx += x; sy += $2; sxx += x * x; sxy += x * $2
      if (n == 1) { t0 = $1; r0 = $2; v0 = $3 }
      t1 = $1; r1 = $2; v1 = $3
    }
    END {
      if (n < 2) { print "|rss n/a (fewer than 2 samples after warm-up)"; exit }
      f = r1 / r0; bad = ""
      if (f > fmax) bad = bad sprintf("; rss grew ×%.2f (max ×%s)", f, fmax)
      d = n * sxx - sx * sx; s = d > 0 ? (n * sxy - sx * sy) / d : 0
      judged = (t1 - t0 >= 1800)
      if (judged && s > slope_max) bad = bad sprintf("; rss trend %+.1f MB/h (max %s)", s, slope_max)
      vram = "vram n/a"
      if (v0 != "n/a" && v0 > 0) {
        vram = sprintf("vram %s → %s MB", v0, v1)
        if (v1 / v0 > fmax) bad = bad sprintf("; vram grew ×%.2f (max ×%s)", v1 / v0, fmax)
      }
      printf "%s|rss %.1f → %.1f MB (×%.2f, %+.1f MB/h%s) · %s\n", substr(bad, 3), r0, r1, f, s,
        (judged ? "" : ", trend not judged under 30 min"), vram
    }' "$f")
  last=$(awk 'END { print (NR ? $5 " " $6 " " $4 : "n/a n/a n/a") }' "$f")
  read -r lt xr dr <<<"$last"
  seg_lines+=("run $seg (pid $seg_pid, ${seg_t0}–${t1}s): $detail · xruns $xr · dropped $dr · late $lt")
  [[ -z "$bad" ]] || failures+=("run $seg: $bad")
  [[ "$xr" == n/a || "$xr" -eq 0 ]] || failures+=("run $seg: $xr audio xruns")
  [[ "$dr" == n/a || "$dr" -le $max_dropped ]] || failures+=("run $seg: $dr dropped frames (max $max_dropped)")
}
alive() {
  if [[ -n "$own_pid" ]]; then kill -0 "$own_pid" 2>/dev/null; else [[ "$(engine_pid)" == "$pid" ]]; fi
}
dead=0
sample() {
  local el=$1 rss vram fps d l x n e
  if ! alive; then
    dead=1 failures+=("engine (pid $pid) died at ${el}s outside the planned kill")
    [[ -n "$own_pid" ]] && { echo "engine log tail:"; tail -20 "$work/engine.log"; }
    return 0
  fi
  rss=$(rss_mb "$pid")
  read -r vram fps d l x <<<"$(perf)"
  read -r n e <<<"$(sim_counts)"
  d=$(delta "$d" "$seg_d0") l=$(delta "$l" "$seg_l0") x=$(delta "$x" "$seg_x0")
  printf '%s %s %s %s %s %s\n' "$el" "${rss:-n/a}" "$vram" "$d" "$l" "$x" >>"$work/seg$seg.samples"
  printf '%-8s %-9s %-8s %-6s %-8s %-6s %-6s %-8s %-7s %s\n' "${el}s" "${rss:-n/a}" "$vram" "$fps" "$d" "$l" "$x" "$n" "$(per_sec "$n" "$el")" "$e"
}

# ---- restore after kill ---------------------------------------------------------------------
restore_line=""
pick() { local -a a=("$@"); [[ ${#a[@]} -gt 0 ]] && echo "${a[RANDOM % ${#a[@]}]}"; }
apply() {
  local out
  out=$(ctl "$@" 2>&1) || failures+=("restore check: \`streamctl $*\` was refused: $out")
}
rule_enabled() { ctl --json query rules | jq -r --arg n "$1" '.[] | select(.name == $n) | .enabled'; }
snapshot() {
  local s
  s="mode=$(get show.mode) program=$(get show.scene.program) preview=$(get show.scene.preview)"
  [[ -n "$k_preset" ]] && s+=" preset.$k_preset.active=$(get "preset.$k_preset.active")"
  [[ -n "$k_rule" ]] && s+=" rule '$k_rule' enabled=$(rule_enabled "$k_rule")"
  echo "$s"
}
kill_check() {
  local el o_mode o_prog o_prev want before after t_kill old new deadline restart m
  local -a modes scenes toggles rules
  echo "--- restore-after-kill at ${1}s"
  pause_sim
  sleep 3 # let rule chains and transitions from the last events settle
  o_mode=$(get show.mode) o_prog=$(get show.scene.program) o_prev=$(get show.scene.preview)
  mapfile -t modes < <(ctl --json query modes | jq -r --arg c "$o_mode" \
    '.[] | select(. != $c and (. as $m | ["offline", "live", "brb", "ad_break", "outro"] | index($m) | not))')
  m=""
  for c in rehearsal preshow; do [[ " ${modes[*]} " == *" $c "* ]] && { m=$c; break; }; done
  [[ -n "$m" ]] || m=$(pick "${modes[@]}") || true
  mapfile -t scenes < <(ctl --json query scenes | jq -r --arg p "$o_prog" '.[].name | select(. != $p and . != "brb")')
  k_prog=$(pick "${scenes[@]}") || k_prog=""
  mapfile -t scenes < <(printf '%s\n' "${scenes[@]}" | awk -v p="$k_prog" 'NF && $0 != p')
  k_prev=$(pick "${scenes[@]}") || k_prev=""
  mapfile -t toggles < <(ctl --json query presets | jq -r '.[] | select(.toggle and (.confirm | not) and (.active | not)) | .name')
  k_preset=$(pick "${toggles[@]}") || k_preset=""
  mapfile -t rules < <(ctl --json query rules | jq -r '.[] | select(.enabled and (.name | contains("'"'"'") | not)) | .name')
  k_rule=$(pick "${rules[@]}") || k_rule=""
  [[ -z "$m" ]] || apply mode "$m"
  [[ -z "$k_prog" ]] || apply scene "$k_prog" --cut
  [[ -z "$k_prev" ]] || apply scene "$k_prev"
  [[ -z "$k_preset" ]] || apply preset "$k_preset"
  [[ -z "$k_rule" ]] || apply "do" "rule.disable name='$k_rule'"
  sleep 1
  want="mode=${m:-$o_mode} program=${k_prog:-$o_prog} preview=${k_prev:-$o_prev}"
  [[ -n "$k_preset" ]] && want+=" preset.$k_preset.active=true"
  [[ -n "$k_rule" ]] && want+=" rule '$k_rule' enabled=false"
  before=$(snapshot)
  echo "state before: $before"
  if [[ "$before" != "$want" ]]; then
    failures+=("restore check: the state did not take before the kill (wanted: $want)")
  fi
  el=$(( SECONDS - t0 ))
  sample "$el"
  [[ $dead -eq 0 ]] || return 0
  close_segment "$el"
  old=$pid t_kill=$EPOCHREALTIME
  kill -9 "$old"
  if [[ $mode_kind == own ]]; then
    wait "$old" 2>/dev/null || true
    local n=$(( $(ready_count) + 1 ))
    launch
    if ! wait_ready "$n" 60; then
      failures+=("restore check: the engine did not come back within 60 s"); dead=1
      echo "engine log tail:"; tail -20 "$work/engine.log"; return 0
    fi
  else
    deadline=$(( SECONDS + 60 ))
    while :; do
      new=$(systemctl --user show -p MainPID -p ActiveState stream-engine | awk -F= '{ v[$1] = $2 } END { if (v["ActiveState"] == "active") print v["MainPID"] }')
      [[ -n "$new" && "$new" != 0 && "$new" != "$old" ]] && break
      if [[ $SECONDS -ge $deadline ]]; then
        failures+=("restore check: systemd did not restart the engine within 60 s"); dead=1; return 0
      fi
      sleep 0.05
    done
    pid=$new
  fi
  restart=$(fdiff "$EPOCHREALTIME" "$t_kill")
  after=$(snapshot)
  echo "state after:  $after (pid $old → $pid, ready ${restart} s after kill -9)"
  if [[ "$after" == "$before" ]]; then
    restore_line="ok, ready in ${restart} s ($after)"
  else
    restore_line="MISMATCH, ready in ${restart} s"
    failures+=("restore check: state before the kill [$before] != after the restart [$after]")
  fi
  if awk -v r="$restart" -v m="$max_restart" 'BEGIN { exit !(r > m) }'; then
    failures+=("restore check: ready ${restart} s after kill -9 (max ${max_restart} s)")
  fi
  if [[ $mode_kind == service ]]; then # put the service back the way it was
    ctl mode "$o_mode" >/dev/null || true
    ctl scene "$o_prog" --cut >/dev/null || true
    ctl scene "$o_prev" >/dev/null || true
    [[ -n "$k_preset" ]] && { ctl release "$k_preset" >/dev/null || true; }
    [[ -n "$k_rule" ]] && { ctl "do" "rule.enable name='$k_rule'" >/dev/null || true; }
  fi
  open_segment "$(( SECONDS - t0 ))"
  resume_sim
}

# ---- run ------------------------------------------------------------------------------------
t0=$SECONDS
open_segment 0
sim_loop &
sim_pid=$!
echo "soak: ${total}s, sample every ${interval}s, ~${rate} sim events/s from: ${presets[*]}"
echo "kill -9 restore check: $([[ $kill_at -ge 0 ]] && echo "at ${kill_at}s" || echo "$kill_note"); warm-up ${warmup}s per engine run"
printf '%-8s %-9s %-8s %-6s %-8s %-6s %-6s %-8s %-7s %s\n' elapsed rss_mb vram_mb fps dropped late xruns events ev/s sim_err
next=$interval killed=0
while [[ $dead -eq 0 ]]; do
  el=$(( SECONDS - t0 ))
  [[ $el -lt $total ]] || break
  if [[ $kill_at -ge 0 && $killed -eq 0 && $el -ge $kill_at ]]; then
    killed=1
    kill_check "$el"
    continue
  fi
  if [[ $el -ge $next ]]; then
    sample "$el"
    while [[ $next -le $el ]]; do next=$(( next + interval )); done
    continue
  fi
  wake=$(( next < total ? next : total ))
  [[ $kill_at -ge 0 && $killed -eq 0 && $kill_at -lt $wake ]] && wake=$kill_at
  sleep $(( wake - el ))
done
kill "$sim_pid" 2>/dev/null || true
wait "$sim_pid" 2>/dev/null || true
sim_pid=""
el=$(( SECONDS - t0 ))
[[ $dead -eq 1 ]] || sample "$el"
[[ $seg_open -eq 0 ]] || close_segment "$el"
read -r n e <<<"$(sim_counts)"
[[ $e -eq 0 ]] || failures+=("$e simulator commands refused (first: $(head -1 "$work/sim-errors.log" 2>/dev/null))")

echo "---"
echo "duration ${el}s  sim events fired $n ($(per_sec "$n" "$el")/s)  refused $e"
for l in "${seg_lines[@]}"; do echo "$l"; done
echo "restore after kill -9: ${restore_line:-${kill_note:-not reached (the run ended first)}}"
if [[ ${#failures[@]} -eq 0 ]]; then
  echo "result: PASS"
else
  echo "result: FAIL"
  for f in "${failures[@]}"; do echo "  - $f"; done
  exit 1
fi
