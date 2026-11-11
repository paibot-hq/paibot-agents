#!/bin/sh
# Paibot Agents.app を target/ に作る。使い方: sh scripts/bundle.sh [コピー先フォルダ]
set -e
cd "$(dirname "$0")/.."
cargo build --release
APP="target/Paibot Agents.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/paibot-agents "$APP/Contents/MacOS/paibot-agents"
VER=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Paibot Agents</string>
  <key>CFBundleDisplayName</key><string>Paibot Agents</string>
  <key>CFBundleIdentifier</key><string>app.paibot.agents</string>
  <key>CFBundleExecutable</key><string>paibot-agents</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VER</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict></plist>
PLIST
if [ -n "$1" ]; then
  rm -rf "$1/Paibot Agents.app" && cp -R "$APP" "$1/" && echo "$1/Paibot Agents.app に入れました"
else
  echo "$APP を作りました"
fi
