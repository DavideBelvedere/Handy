#!/usr/bin/env bash
# Build Handy from this checkout and install it to /Applications (macOS, local build).
# Usage: scripts/install-macos.sh [--no-reset]   (--no-reset keeps the Accessibility grant; see BUILD.md)
#
# Signs with the code-signing certificate "Handy Local" (or $HANDY_SIGNING_IDENTITY) when the
# keychain has it: the Accessibility grant then survives rebuilds and is never reset. Without it
# the build is ad-hoc signed, and each build needs Accessibility granted again.
set -euo pipefail
cd "$(dirname "$0")/.."

# Tauri runs `xattr -crs` on the bundle; a pip-installed xattr (e.g. a pyenv shim) has no -r, so use the system one.
export PATH="/usr/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

# Not `-v`: a self-signed certificate is "not trusted" yet still signs fine.
identity="${HANDY_SIGNING_IDENTITY:-Handy Local}"
if security find-identity -p codesigning | grep -qF "\"$identity\""; then
  export APPLE_SIGNING_IDENTITY="$identity"
  echo "Signing with \"$identity\": the Accessibility grant is kept."
else
  echo "No \"$identity\" code-signing certificate: ad-hoc build, Accessibility must be granted again." >&2
fi

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
if [ -z "${APPLE_SIGNING_IDENTITY:-}" ] && [ "${1:-}" != "--no-reset" ]; then
  tccutil reset Accessibility com.pais.handy
  echo "Accessibility reset: grant it again when Handy asks."
fi
open /Applications/Handy.app
echo "Installed /Applications/Handy.app ($(git rev-parse --short HEAD))"
