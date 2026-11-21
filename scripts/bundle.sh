#!/bin/bash
# Builds dist/YesHarness.app (macOS). Usage: scripts/bundle.sh [--install]
set -euo pipefail
cd "$(dirname "$0")/.."

# The version comes from the git tags (v1.5.0 …): YH_VERSION, else the latest tag, else what's in Cargo.toml.
VERSION="${YH_VERSION:-$(git describe --tags --abbrev=0 --match 'v[0-9]*' 2>/dev/null | sed 's/^v//')}"
if [ -n "$VERSION" ]; then
  echo "› Version $VERSION"
  BACKUP=$(mktemp -d)
  cp Cargo.toml Cargo.lock "$BACKUP/" && cp macos/Info.plist "$BACKUP/Info.plist"
  python3 scripts/bump_version.py --set "$VERSION" >/dev/null
  # Put the tracked files back afterwards: the version is only for this build.
  trap 'cp "$BACKUP/Cargo.toml" "$BACKUP/Cargo.lock" . && cp "$BACKUP/Info.plist" macos/Info.plist' EXIT
fi

echo "› Building app (release)…"
# Don't bake your home folder path into the binary (it shows up in panic locations).
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~"
cargo build --locked --release

echo "› Building voice helper…"
swift build -c release --package-path helper
cp helper/.build/release/yh-dictate target/release/yh-dictate

APP=dist/YesHarness.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp macos/Info.plist "$APP/Contents/Info.plist"
cp target/release/yesharness target/release/yh-dictate "$APP/Contents/MacOS/"
[ -f macos/AppIcon.icns ] && cp macos/AppIcon.icns "$APP/Contents/Resources/"

# Ad-hoc signature so macOS can remember the mic / speech permissions.
codesign --force --sign - --identifier io.yesharness.dictate "$APP/Contents/MacOS/yh-dictate"
codesign --force --sign - "$APP"

echo "✓ Built $APP"
if [ "${1:-}" = "--install" ]; then
  rm -rf /Applications/YesHarness.app
  cp -R "$APP" /Applications/
  echo "✓ Installed to /Applications/YesHarness.app"
fi
