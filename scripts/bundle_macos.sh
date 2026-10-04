#!/bin/sh
# Builds "Coport.app", a menu bar app (no Dock icon), into target/.
set -eu

cd "$(dirname "$0")/.."
command -v python3 >/dev/null
cargo build --locked --release -p coport-gui

output="target/Coport.app"
stage=$(mktemp -d target/.bundle-macos.XXXXXX)
trap 'rm -rf "$stage"' EXIT
trap 'exit 1' HUP INT TERM
app="$stage/Coport.app"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' gui/Cargo.toml | head -n 1)
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/coport-gui "$app/Contents/MacOS/"
cp target/release/coport-daemon "$app/Contents/MacOS/"

# iconutil can reject valid PNGs in a sandbox. Write the standard ICNS PNG
# chunks directly with Python's standard library; no GUI services are needed.
iconset="$stage/AppIcon.iconset"
mkdir -p "$iconset"
target/release/coport-gui --export-icon "$iconset/icon_512x512@2x.png" 1024
for size in 16 32 128 256 512; do
    sips -z $size $size "$iconset/icon_512x512@2x.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z $double $double "$iconset/icon_512x512@2x.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
python3 - "$iconset" "$app/Contents/Resources/AppIcon.icns" <<'PY'
from pathlib import Path
import struct
import sys

iconset = Path(sys.argv[1])
entries = [
    (b"icp4", "16x16"), (b"icp5", "32x32"), (b"icp6", "32x32@2x"),
    (b"ic07", "128x128"), (b"ic08", "256x256"), (b"ic09", "512x512"),
    (b"ic10", "512x512@2x"), (b"ic11", "16x16@2x"),
    (b"ic12", "32x32@2x"), (b"ic13", "128x128@2x"), (b"ic14", "256x256@2x"),
]
chunks = []
for kind, name in entries:
    png = (iconset / f"icon_{name}.png").read_bytes()
    chunks.append(kind + struct.pack(">I", len(png) + 8) + png)
payload = b"".join(chunks)
Path(sys.argv[2]).write_bytes(b"icns" + struct.pack(">I", len(payload) + 8) + payload)
PY

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
