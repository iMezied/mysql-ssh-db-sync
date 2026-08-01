#!/usr/bin/env bash
#
# Build DBSync Studio and replace the copy in /Applications.
#
# Why this exists: the app keys its store off the bundle identifier, so every
# build — dev or installed — shares one database at
# ~/Library/Application Support/com.dbsync-studio.app/dbsync.db. The moment a
# newer build applies a migration the installed copy has never heard of, sqlx
# refuses to start it:
#
#   migration N was previously applied but is missing in the resolved migrations
#
# There is no recovery from inside the old binary. The installed copy has to
# move forward with the source tree, which is what this script is for: run it
# after every change worth keeping, and /Applications never falls behind.
#
# Usage:
#   npm run install:mac              # build, then install
#   npm run install:mac -- --no-build  # install whatever is already built

set -euo pipefail

APP_NAME="DBSync Studio.app"
BUNDLE_ID="com.dbsync-studio.app"
DESKTOP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE_DIR="$(cd "$DESKTOP_DIR/../.." && pwd)"
BUILT_APP="$WORKSPACE_DIR/target/release/bundle/macos/$APP_NAME"
DEST_APP="/Applications/$APP_NAME"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "install-macos.sh: macOS only (found $(uname -s))" >&2
  exit 1
fi

do_build=1
for arg in "$@"; do
  case "$arg" in
    --no-build) do_build=0 ;;
    *) echo "install-macos.sh: unknown argument '$arg'" >&2; exit 2 ;;
  esac
done

# 1. Build. `npm run bundle` stages the `dbsync` sidecar and then runs
#    `tauri build` with the config that declares it as an external binary.
if [[ $do_build -eq 1 ]]; then
  echo "==> building"
  (cd "$DESKTOP_DIR" && npm run bundle)
fi

if [[ ! -d "$BUILT_APP" ]]; then
  echo "install-macos.sh: no bundle at $BUILT_APP — run without --no-build" >&2
  exit 1
fi

version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$BUILT_APP/Contents/Info.plist")"

# 2. Quit anything already running, installed copy or dev copy. A running
#    instance holds the store open and would also be the thing you keep
#    testing by accident after the swap.
echo "==> stopping running instances"
pkill -f "$APP_NAME/Contents/MacOS/db-sync-desktop" 2>/dev/null || true
sleep 1

# 3. Replace. The identifier check is the guard: this script deletes a
#    directory under /Applications, so it refuses to touch anything that is
#    not the bundle it thinks it is.
if [[ -e "$DEST_APP" ]]; then
  existing_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$DEST_APP/Contents/Info.plist" 2>/dev/null || echo "")"
  if [[ "$existing_id" != "$BUNDLE_ID" ]]; then
    echo "install-macos.sh: $DEST_APP has identifier '$existing_id', expected '$BUNDLE_ID' — refusing to replace it" >&2
    exit 1
  fi
  echo "==> removing previous install ($(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$DEST_APP/Contents/Info.plist" 2>/dev/null || echo "unknown"))"
  rm -rf "$DEST_APP"
fi

echo "==> installing $version to /Applications"
cp -R "$BUILT_APP" "$DEST_APP"

# 4. Sign. Tauri only signs when a Developer ID identity is configured, so an
#    unsigned local bundle arrives with a linker signature on the main binary
#    and no _CodeSignature seal at all — `codesign --verify` and `spctl` both
#    reject it. Ad-hoc signing here costs nothing and keeps the bundle
#    well-formed. Nested code first: the outer seal covers it.
#
#    Caveat worth knowing: an ad-hoc signature's identity is its cdhash, which
#    changes on every build. Keychain items are bound to the signature that
#    created them, so saved database passwords re-prompt after an install.
#    That is macOS behaving correctly. A real Developer ID identity is the only
#    thing that makes it stop.
echo "==> ad-hoc signing"
codesign --force --sign - "$DEST_APP/Contents/MacOS/dbsync" 2>/dev/null || true
codesign --force --sign - --entitlements "$DESKTOP_DIR/src-tauri/entitlements.plist" "$DEST_APP"

# 5. Register. Two bundles share this identifier while a dev build sits in
#    target/, and LaunchServices resolves the identifier to whichever it saw
#    most recently — which is how `open -b com.dbsync-studio.app` ends up
#    launching a build from the source tree. Registering last makes the
#    installed copy win.
if [[ -x "$LSREGISTER" ]]; then
  echo "==> registering with LaunchServices"
  "$LSREGISTER" -f "$DEST_APP"
fi

echo "==> verifying"
codesign --verify --strict "$DEST_APP" && echo "    signature ok"
echo "    installed: $DEST_APP ($version)"
echo
echo "Open it with:  open -a \"DBSync Studio\""
