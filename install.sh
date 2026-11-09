#!/usr/bin/env bash
# Build and install muesli for the current user (~/.local), then start the daemon.
#   ./install.sh            build + install + enable the systemd user service
#   ./install.sh --no-start install only
set -euo pipefail
cd "$(dirname "$0")"

features=""
if ! pkg-config --exists 'webrtc-audio-processing-2 >= 2.1'; then
  echo "webrtc-audio-processing-2 not found; building it from source (needs meson + ninja)."
  echo "  (or: sudo pacman -S webrtc-audio-processing)"
  features="--features aec-bundled"
fi

cargo build --release -p muesli-daemon $features
cargo build --release -p muesli-cli -p muesli-ui

bin="$HOME/.local/bin"
mkdir -p "$bin" "$HOME/.local/share/applications" \
  "$HOME/.local/share/icons/hicolor/scalable/apps" "$HOME/.config/systemd/user"
install -m755 target/release/muesli-daemon target/release/muesli-cli target/release/muesli "$bin/"
install -m644 packaging/dev.muesli.Muesli.desktop "$HOME/.local/share/applications/"
install -m644 packaging/dev.muesli.Muesli.svg "$HOME/.local/share/icons/hicolor/scalable/apps/"
install -m644 packaging/muesli-daemon.service "$HOME/.config/systemd/user/"
# Bundled hook scripts (opt in by linking them from ~/.config/muesli/hooks/<event>.d/).
install -d "$HOME/.local/share/muesli/hooks"
install -m755 hooks/*.py "$HOME/.local/share/muesli/hooks/"

# Omarchy bar plugin (if Omarchy shell is present).
if [ -d "$HOME/.config/omarchy" ] && [ -d omarchy-plugin/ssobhani.muesli ]; then
  mkdir -p "$HOME/.config/omarchy/plugins"
  ln -sfn "$PWD/omarchy-plugin/ssobhani.muesli" "$HOME/.config/omarchy/plugins/ssobhani.muesli"
  grep -q '"ssobhani.muesli"' "$HOME/.config/omarchy/shell.json" 2>/dev/null ||
    echo "Omarchy plugin linked. Add {\"id\": \"ssobhani.muesli\"} to bar.layout in ~/.config/omarchy/shell.json."
fi

systemctl --user daemon-reload
if [ "${1:-}" != "--no-start" ]; then
  systemctl --user enable --now muesli-daemon.service
  systemctl --user restart muesli-daemon.service
fi

if [ -z "${OPENROUTER_API_KEY:-}" ] && [ ! -f "$HOME/.config/muesli/openrouter.key" ]; then
  echo "Set your OpenRouter key:  muesli-cli set-key"
fi
for _ in $(seq 30); do "$bin/muesli-cli" status >/dev/null 2>&1 && break; sleep 0.1; done
echo "Installed. Status: $("$bin/muesli-cli" status 2>&1 | head -1)"
