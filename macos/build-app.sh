#!/bin/sh
# Builds Rufus.app (release) into macos/build/
set -e
cd "$(dirname "$0")"
swift build -c release
APP=build/Rufus.app
rm -rf "$APP" && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp .build/release/Rufus "$APP/Contents/MacOS/Rufus"

# Icon from the existing Rufus artwork
ICONS=build/rufus.iconset
mkdir -p "$ICONS"
SRC=../res/icons
cp $SRC/rufus-16.png  "$ICONS/icon_16x16.png"
cp $SRC/rufus-32.png  "$ICONS/icon_16x16@2x.png"
cp $SRC/rufus-32.png  "$ICONS/icon_32x32.png"
cp $SRC/rufus-256.png "$ICONS/icon_128x128@2x.png"
cp $SRC/rufus-128.png "$ICONS/icon_128x128.png"
cp $SRC/rufus-256.png "$ICONS/icon_256x256.png"
iconutil -c icns "$ICONS" -o "$APP/Contents/Resources/Rufus.icns" 2>/dev/null || true
rm -rf "$ICONS"

cat > "$APP/Contents/Info.plist" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>Rufus</string>
<key>CFBundleDisplayName</key><string>Rufus</string>
<key>CFBundleIdentifier</key><string>ie.akeo.rufus.macos</string>
<key>CFBundleExecutable</key><string>Rufus</string>
<key>CFBundleIconFile</key><string>Rufus</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSRemovableVolumesUsageDescription</key><string>Rufus writes bootable images to removable drives.</string>
</dict></plist>
PL
codesign --force --sign - "$APP" 2>/dev/null || true
echo "Built $APP"
