#!/bin/sh
# Builds the telemetry plugin and installs it into every ETS2/ATS found in the
# user's Steam libraries:
#   * Windows game under Proton → bin/win_x64/plugins/aigakira_telemetry_ets2_udp.dll
#   * native Linux game         → bin/linux_x64/plugins/aigakira_telemetry_ets2_udp.so
#
#   tools/install-game-plugin.sh            # build + install
#   tools/install-game-plugin.sh --dry-run  # only show what would be done
#
# Restart the game afterwards; it loads plugins only at startup.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DRY=0
[ "${1:-}" = "--dry-run" ] && DRY=1
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"
cd "$ROOT"

if [ "$DRY" = 0 ]; then
  cargo build --release --target x86_64-pc-windows-gnu
  cargo build --release
fi
DLL="target/x86_64-pc-windows-gnu/release/aigakira_telemetry_ets2_udp.dll"
SO="target/release/libaigakira_telemetry_ets2_udp.so"

# Steam library roots (native and Flatpak Steam), plus extra ones from libraryfolders.vdf.
LIBS=""
for steam in "$HOME/.local/share/Steam" "$HOME/.steam/steam" "$HOME/.var/app/com.valvesoftware.Steam/.local/share/Steam"; do
  [ -d "$steam/steamapps" ] && LIBS="$LIBS
$steam"
  vdf="$steam/steamapps/libraryfolders.vdf"
  [ -f "$vdf" ] && LIBS="$LIBS
$(sed -n 's/^[[:space:]]*"path"[[:space:]]*"\(.*\)".*/\1/p' "$vdf")"
done

seen=""
echo "$LIBS" | sort -u | while IFS= read -r lib; do
  [ -n "$lib" ] || continue
  for game in "Euro Truck Simulator 2" "American Truck Simulator"; do
    dir="$lib/steamapps/common/$game/bin"
    [ -d "$dir" ] || continue
    real="$(cd "$dir" && pwd -P)"
    case "$seen" in *"|$real|"*) continue ;; esac
    seen="$seen|$real|"
    for kind in win_x64 linux_x64; do
      [ -d "$dir/$kind" ] || continue
      if [ "$kind" = win_x64 ]; then src="$DLL"; name=aigakira_telemetry_ets2_udp.dll; else src="$SO"; name=aigakira_telemetry_ets2_udp.so; fi
      dest="$dir/$kind/plugins/$name"
      echo "$game ($kind): $dest"
      if [ "$DRY" = 0 ]; then
        mkdir -p "$dir/$kind/plugins"
        # install = new inode, safe even if the old file is still mapped by a running game
        install -m755 "$src" "$dest"
      fi
    done
  done
done
echo "done. Restart the game; on first start accept the \"Advanced SDK features\" dialog."
