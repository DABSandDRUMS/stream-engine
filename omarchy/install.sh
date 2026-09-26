#!/usr/bin/env bash
# Opt-in installer for stream-engine's Omarchy extras (PLAN §16.2–16.3).
#
#   omarchy/install.sh [--bar] [--hypr] [--no-plugin] [--no-menu] [--no-hook] [--dry-run]
#
# By default it installs, into ~/.config/omarchy:
#   plugins/stream-engine.status/         bar widget plugin (installed, not yet placed on the bar)
#   extensions/omarchy-menu.jsonc         "Stream" menu entries, merged; existing ids are kept
#   hooks/font-set.d/stream-engine        font-set hook → `streamctl fire omarchy.font_set`
# Opt-in:
#   --bar    place the widget in shell.json's bar layout (right section)
#   --hypr   copy hypr/stream-engine.lua to ~/.config/hypr/ and require it from bindings.lua
# Every file it changes is backed up first (<file>.bak.<timestamp>). --dry-run only prints the
# plan. The running Omarchy shell and Hyprland pick the changes up by themselves (they watch
# these files); nothing is restarted.
set -euo pipefail

src="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
omarchy_dir="$HOME/.config/omarchy"
plugin_id="stream-engine.status"
stamp="$(date +%Y%m%d-%H%M%S)"

do_plugin=1 do_menu=1 do_hook=1 do_bar=0 do_hypr=0 dry=0
usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "${BASH_SOURCE[0]}"; }
while (($#)); do
  case $1 in
  --bar) do_bar=1 ;;
  --hypr) do_hypr=1 ;;
  --no-plugin) do_plugin=0 ;;
  --no-menu) do_menu=0 ;;
  --no-hook) do_hook=0 ;;
  --dry-run | -n) dry=1 ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "unknown option: $1" >&2
    usage >&2
    exit 2
    ;;
  esac
  shift
done

say() { printf '%s\n' "$*"; }
act() { if ((dry)); then say "  would: $*"; else say "  $*"; fi; }
need() { command -v "$1" >/dev/null || { echo "$1 is required for $2" >&2; exit 1; }; }

# Back up an existing file before it is modified or replaced.
backup() {
  [[ -e $1 ]] || return 0
  act "back up $1 → $1.bak.$stamp"
  ((dry)) || cp -a "$1" "$1.bak.$stamp"
}

# Replace $2 with the contents of $1 (atomic rename in the same directory), keeping $2's mode.
replace_file() {
  local tmp
  tmp="$(mktemp "$(dirname "$2")/.$(basename "$2").XXXXXX")"
  cat "$1" >"$tmp"
  chmod --reference="$2" "$tmp" 2>/dev/null || chmod 644 "$tmp"
  mv -f "$tmp" "$2"
}

# The JSONC dialect Omarchy's menu parser accepts (plugins/menu/MenuModel.js stripJsonc): whole-line
# // comments and trailing commas before } or ]. Prints plain JSON.
jsonc_to_json() {
  perl -0777 -pe 's#^\s*//[^\n]*(\n|$)##gm; s/,(\s*[}\]])/$1/g' "$1"
}

install_plugin() {
  local from="$src/plugins/$plugin_id" dest="$omarchy_dir/plugins/$plugin_id"
  say "plugin: $dest"
  if command -v omarchy-plugin-validate >/dev/null; then
    omarchy-plugin-validate "$from"
  fi
  if [[ -d $dest ]] && diff -rq "$from" "$dest" >/dev/null; then
    say "  up to date"
    return
  fi
  if [[ -d $dest ]]; then
    # Hidden names are ignored by Omarchy's plugin scanner and file watcher.
    act "back up $dest → $omarchy_dir/plugins/.$plugin_id.bak.$stamp"
    ((dry)) || cp -a "$dest" "$omarchy_dir/plugins/.$plugin_id.bak.$stamp"
  fi
  act "copy $from → $dest"
  if ((!dry)); then
    local stage="$omarchy_dir/plugins/.$plugin_id.new.$$"
    mkdir -p "$omarchy_dir/plugins"
    rm -rf "$stage"
    cp -r "$from" "$stage"
    rm -rf "$dest"
    mv "$stage" "$dest"
  fi
}

place_on_bar() {
  local file="$omarchy_dir/shell.json" base
  need jq "--bar"
  say "bar: $file"
  if [[ -f $file ]]; then
    base="$file"
  else
    # No user file: the shell runs on Omarchy's defaults, so start from those.
    base="${OMARCHY_PATH:-/usr/share/omarchy}/config/omarchy/shell.json"
    [[ -f $base ]] || { echo "no $file and no Omarchy default shell.json at $base" >&2; exit 1; }
    act "create $file from $base"
  fi
  jq -e '.version == 1' "$base" >/dev/null || { echo "$base: unsupported shell.json (expected version 1)" >&2; exit 1; }
  if jq -e --arg id "$plugin_id" \
    '[.bar.layout // {} | .[]? | .[]? | if type == "object" then .id else . end] | index($id) != null' "$base" >/dev/null; then
    say "  $plugin_id is already on the bar"
    return
  fi
  local out
  out="$(mktemp)"
  jq --arg id "$plugin_id" --indent 2 \
    '.bar //= {} | .bar.layout //= {} | .bar.layout.right = ((.bar.layout.right // []) + [{id: $id}])' \
    "$base" >"$out"
  act "add {\"id\": \"$plugin_id\"} to bar.layout.right"
  if ((dry)); then
    diff -u "$base" "$out" | sed 's/^/    /' || true
  else
    backup "$file"
    mkdir -p "$omarchy_dir"
    replace_file "$out" "$file"
  fi
  rm -f "$out"
}

merge_menu() {
  local file="$omarchy_dir/extensions/omarchy-menu.jsonc" snippet="$src/omarchy-menu.jsonc"
  need jq "the menu merge"
  need perl "the menu merge"
  say "menu: $file"
  jsonc_to_json "$snippet" | jq -e 'type == "object"' >/dev/null
  if [[ ! -f $file ]]; then
    act "create $file with $(jsonc_to_json "$snippet" | jq 'length') stream-engine entries"
    if ((!dry)); then
      mkdir -p "$(dirname "$file")"
      cp "$snippet" "$file"
    fi
    return
  fi
  local existing
  existing="$(jsonc_to_json "$file" | jq -r 'if type == "object" then (.items // .) | keys[] else empty end')" || {
    echo "$file does not parse as Omarchy menu JSONC; fix it first (nothing changed)" >&2
    exit 1
  }
  local id line lines=() kept=()
  while IFS= read -r id; do
    if grep -qxF -- "$id" <<<"$existing"; then
      kept+=("$id")
      continue
    fi
    line="$(grep -E "^[[:space:]]*\"$(sed 's/[.[\*^$]/\\&/g' <<<"$id")\":" "$snippet")"
    lines+=("$line")
  done < <(jsonc_to_json "$snippet" | jq -r 'keys_unsorted[]')
  ((${#kept[@]} == 0)) || say "  keeping existing entries: ${kept[*]}"
  if ((${#lines[@]} == 0)); then
    say "  up to date"
    return
  fi
  if jsonc_to_json "$file" | jq -e 'has("items")' >/dev/null; then
    echo "$file uses the {\"items\": {…}} form; add the entries from $snippet by hand" >&2
    exit 1
  fi
  # Insert before the file's final closing brace; make sure the previous entry ends with a comma.
  local out
  out="$(mktemp)"
  {
    printf '  // stream-engine (added by stream-engine omarchy/install.sh, %s)\n' "$stamp"
    printf '%s\n' "${lines[@]}"
  } >"$out.block"
  awk -v block="$out.block" '
    { buf[NR] = $0 }
    END {
      close_at = 0
      for (i = NR; i >= 1; i--) if (buf[i] ~ /^[[:space:]]*}[[:space:]]*$/) { close_at = i; break }
      if (!close_at) exit 3
      last = 0
      for (i = close_at - 1; i >= 1; i--) if (buf[i] !~ /^[[:space:]]*(\/\/.*)?$/) { last = i; break }
      for (i = 1; i < close_at; i++) {
        line = buf[i]
        if (i == last && line !~ /[,{][[:space:]]*$/) sub(/[[:space:]]*$/, ",", line)
        print line
      }
      while ((getline l < block) > 0) print l
      for (i = close_at; i <= NR; i++) print buf[i]
    }' "$file" >"$out" || { echo "$file: no closing brace found; nothing changed" >&2; rm -f "$out" "$out.block"; exit 1; }
  jsonc_to_json "$out" | jq -e 'type == "object"' >/dev/null || {
    echo "merged menu would not parse; nothing changed (result kept at $out)" >&2
    exit 1
  }
  act "add ${#lines[@]} entries"
  if ((dry)); then
    diff -u "$file" "$out" | sed 's/^/    /' || true
  else
    backup "$file"
    replace_file "$out" "$file"
  fi
  rm -f "$out" "$out.block"
}

install_hook() {
  local from="$src/hooks/font-set.d/stream-engine" dest="$omarchy_dir/hooks/font-set.d/stream-engine"
  say "hook: $dest"
  if [[ -f $dest ]] && cmp -s "$from" "$dest"; then
    say "  up to date"
    return
  fi
  backup "$dest"
  act "install $from → $dest"
  ((dry)) || install -Dm755 "$from" "$dest"
}

install_hypr() {
  local from="$src/hypr/stream-engine.lua" hypr="$HOME/.config/hypr"
  local dest="$hypr/stream-engine.lua" bindings="$hypr/bindings.lua"
  local require_line='require("hypr.stream-engine")'
  say "hypr: $dest"
  [[ -f $bindings ]] || { echo "$bindings not found (is this an Omarchy Hyprland Lua config?)" >&2; exit 1; }
  if command -v luac >/dev/null; then
    luac -p "$from"
  fi
  if [[ -f $dest ]] && cmp -s "$from" "$dest"; then
    say "  up to date"
  else
    backup "$dest"
    act "copy $from → $dest"
    ((dry)) || install -Dm644 "$from" "$dest"
  fi
  if grep -qE '^[[:space:]]*(require|dofile)\(.*stream-engine' "$bindings"; then
    say "  $bindings already loads it"
    return
  fi
  backup "$bindings"
  act "append $require_line to $bindings"
  if ((!dry)); then
    printf '\n-- stream-engine keybinds and window rules (added by stream-engine omarchy/install.sh)\n%s\n' \
      "$require_line" >>"$bindings"
  fi
}

((dry)) && say "dry run: nothing is written"
((do_plugin)) && install_plugin
((do_bar)) && place_on_bar
((do_menu)) && merge_menu
((do_hook)) && install_hook
((do_hypr)) && install_hypr

if ((do_plugin && !do_bar && !dry)); then
  say "widget installed; show it with: omarchy plugin enable $plugin_id   (or re-run with --bar)"
fi
command -v stream-engine-launch-or-focus >/dev/null ||
  say "note: stream-engine-launch-or-focus is not on PATH (install the package or run packaging/dev-install.sh)"
exit 0
