#!/bin/bash
# Builds dist/YesHarness.app (macOS). Usage: scripts/bundle.sh [--install]
set -euo pipefail
cd "$(dirname "$0")/.."

echo "› Building app (release)…"
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
