#!/bin/sh
# Builds "Coport.app", a menu bar app (no Dock icon), into target/.
set -eu

cd "$(dirname "$0")/.."
cargo build --locked --release -p coport-gui

output="target/Coport.app"
stage=$(mktemp -d target/.bundle-macos.XXXXXX)
trap 'rm -rf "$stage"' EXIT
trap 'exit 1' HUP INT TERM
app="$stage/Coport.app"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' gui/Cargo.toml | head -n 1)
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/coport-gui "$app/Contents/MacOS/"
cp target/release/coportd "$app/Contents/MacOS/"

# Assemble standard ICNS PNG chunks with the Rust packer.
iconset="$stage/AppIcon.iconset"
mkdir -p "$iconset"
target/release/coport-gui --export-icon "$iconset/icon_512x512@2x.png" 1024
for size in 16 32 128 256 512; do
    sips -z $size $size "$iconset/icon_512x512@2x.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z $double $double "$iconset/icon_512x512@2x.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
cargo run --locked --example pack_icns -- "$iconset" "$app/Contents/Resources/AppIcon.icns"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Coport</string>
  <key>CFBundleDisplayName</key><string>Coport</string>
  <key>CFBundleIdentifier</key><string>io.github.coport.gui</string>
  <key>CFBundleExecutable</key><string>coport-gui</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Coport connects to the local-network proxies and devices you configure to test connections and retrieve their status.</string>
</dict>
</plist>
EOF

# Do not publish an incomplete bundle or hide signing failures.
plutil -lint "$app/Contents/Info.plist"
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"
rm -rf "$output"
mv "$app" "$output"
echo "Built $output"
