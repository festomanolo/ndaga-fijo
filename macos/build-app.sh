#!/bin/sh
# Builds NdagaFijo.app (release) into macos/build/
set -e
cd "$(dirname "$0")"
swift build -c release
APP=build/NdagaFijo.app
rm -rf "$APP" && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp .build/release/NdagaFijo "$APP/Contents/MacOS/NdagaFijo"
cp assets/NdagaFijo.icns "$APP/Contents/Resources/NdagaFijo.icns"

cat > "$APP/Contents/Info.plist" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>Ndaga Fijo</string>
<key>CFBundleDisplayName</key><string>Ndaga Fijo</string>
<key>CFBundleIdentifier</key><string>com.festomanolo.ndagafijo</string>
<key>CFBundleExecutable</key><string>NdagaFijo</string>
<key>CFBundleIconFile</key><string>NdagaFijo</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSRemovableVolumesUsageDescription</key><string>Ndaga Fijo writes bootable images to removable drives.</string>
</dict></plist>
PL
codesign --force --sign - "$APP" 2>/dev/null || true
echo "Built $APP"
