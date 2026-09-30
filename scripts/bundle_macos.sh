#!/bin/sh
# Builds "Coding Agent Proxy.app", a menu bar app (no Dock icon), into target/.
set -eu

cd "$(dirname "$0")/.."
cargo build --locked --release -p coding-agent-proxy-gui

app="target/Coding Agent Proxy.app"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' gui/Cargo.toml | head -n 1)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/coding-agent-proxy-gui "$app/Contents/MacOS/"

# The icon is drawn by the app itself; iconutil packs the standard sizes.
iconset=$(mktemp -d)/AppIcon.iconset
mkdir -p "$iconset"
target/release/coding-agent-proxy-gui --export-icon "$iconset/icon_512x512@2x.png" 1024
for size in 16 32 128 256 512; do
    sips -z $size $size "$iconset/icon_512x512@2x.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z $double $double "$iconset/icon_512x512@2x.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Coding Agent Proxy</string>
  <key>CFBundleDisplayName</key><string>Coding Agent Proxy</string>
  <key>CFBundleIdentifier</key><string>io.github.coding-agent-proxy.gui</string>
  <key>CFBundleExecutable</key><string>coding-agent-proxy-gui</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

# Ad-hoc signature so Gatekeeper accepts the locally built bundle.
codesign --force --deep --sign - "$app" >/dev/null 2>&1 || true
echo "Built $app"
