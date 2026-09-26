#!/usr/bin/env bash
# Build release binaries and install them for the current user (development setup):
#   ~/.local/bin/{stream-engine,stream}
#   ~/.config/systemd/user/stream-engine.service  (ExecStart → ~/.local/bin)
#   ~/.config/stream-engine/engine.toml            (project path, if missing)
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
project="${1:-$HOME/stream-project}"
cd "$repo"
cargo build --release -p se-app -p se-cli
mkdir -p "$HOME/.local/bin" "$HOME/.config/systemd/user" "$HOME/.config/stream-engine"
install -m755 target/release/stream-engine "$HOME/.local/bin/stream-engine"
install -m755 target/release/stream "$HOME/.local/bin/stream"
sed -e "s|ExecStart=/usr/bin/stream-engine|ExecStart=$HOME/.local/bin/stream-engine|" \
    -e "s|^Environment=RUST_BACKTRACE=1|Environment=RUST_BACKTRACE=1\nEnvironment=STREAM_ENGINE_SHARE=$repo|" \
    packaging/stream-engine.service > "$HOME/.config/systemd/user/stream-engine.service"
if [ ! -f "$project/project.toml" ]; then
  STREAM_ENGINE_SHARE="$repo" "$HOME/.local/bin/stream-engine" new "$project"
fi
if [ ! -f "$HOME/.config/stream-engine/engine.toml" ]; then
  printf 'project = "%s"\n' "$project" > "$HOME/.config/stream-engine/engine.toml"
fi
systemctl --user daemon-reload
echo "installed. start with: systemctl --user enable --now stream-engine"
