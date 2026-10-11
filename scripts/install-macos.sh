#!/usr/bin/env bash
# Build Handy from this checkout and install it to /Applications (macOS, local ad-hoc signed build).
# Usage: scripts/install-macos.sh [--no-reset]   (--no-reset keeps the Accessibility grant; see BUILD.md)
set -euo pipefail
cd "$(dirname "$0")/.."

# Tauri runs `xattr -crs` on the bundle; a pip-installed xattr (e.g. a pyenv shim) has no -r, so use the system one.
export PATH="/usr/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

# Updater artifacts need the maintainers' signing key: skip them.
bun run tauri build --bundles app --config '{"bundle":{"createUpdaterArtifacts":false}}'

if pgrep -f "target/debug/handy" >/dev/null; then
  echo "Built src-tauri/target/release/bundle/macos/Handy.app, NOT installed: a dev Handy (bun run tauri dev) is running." >&2
  echo "Stop it, then run this script again (the second build is fast)." >&2
  exit 1
fi

osascript -e 'tell application id "com.pais.handy" to quit' 2>/dev/null || true
sleep 1
rm -rf /Applications/Handy.app
cp -R src-tauri/target/release/bundle/macos/Handy.app /Applications/

# Each ad-hoc build has a new code identity: the old Accessibility grant stays visibly on but no longer applies.
if [ "${1:-}" != "--no-reset" ]; then
  tccutil reset Accessibility com.pais.handy
  echo "Accessibility reset: grant it again when Handy asks."
fi
open /Applications/Handy.app
echo "Installed /Applications/Handy.app ($(git rev-parse --short HEAD))"
