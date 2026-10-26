#!/usr/bin/env bash
# Build a release binary and assemble "dist/WhatsApp GPUI.app" (unsigned).
#
#   packaging/build-app.sh               # arm64 (host)
#   packaging/build-app.sh --universal   # arm64 + x86_64 via lipo
#
# Env: BUILD_NUMBER (CFBundleVersion, default: the Cargo version).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

APP_NAME="WhatsApp GPUI"
BUNDLE_ID="com.talhabalaj.whatsapp-gpui"
BIN="whatsapp-gpui"
APP="$ROOT/dist/$APP_NAME.app"
ICNS="$ROOT/packaging/icon/AppIcon.icns"            # flat fallback (render.sh)
ICON_DOC="$ROOT/packaging/icon/AppIcon.icon"        # Icon Composer doc (make-icon-composer.py)

UNIVERSAL=0
for arg in "$@"; do
  case "$arg" in
    --universal) UNIVERSAL=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

VERSION="$(cargo metadata --no-deps --format-version 1 \
  | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="whatsapp-gpui"))')"
BUILD_NUMBER="${BUILD_NUMBER:-$VERSION}"

if [[ $UNIVERSAL == 1 ]]; then
  cargo build --release -p whatsapp-gpui --target aarch64-apple-darwin
  cargo build --release -p whatsapp-gpui --target x86_64-apple-darwin
  BIN_PATH="$ROOT/target/universal/$BIN"
  mkdir -p "$(dirname "$BIN_PATH")"
  lipo -create -output "$BIN_PATH" \
    "target/aarch64-apple-darwin/release/$BIN" "target/x86_64-apple-darwin/release/$BIN"
else
  cargo build --release -p whatsapp-gpui
  BIN_PATH="$ROOT/target/release/$BIN"
fi

[[ -f $ICNS ]] || "$ROOT/packaging/icon/render.sh"
[[ -d $ICON_DOC ]] || python3 "$ROOT/packaging/icon/make-icon-composer.py" >/dev/null

# LSMinimumSystemVersion must not be lower than what the binary was linked
# for, so read it from the Mach-O instead of hard-coding it.
MIN_OS="$(vtool -show-build "$BIN_PATH" 2>/dev/null | awk '/minos/ {print $2; exit}')"
MIN_OS="${MIN_OS:-11.0}"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN_PATH" "$APP/Contents/MacOS/$BIN"

# Prefer the Icon Composer document: actool compiles it into Assets.car (the
# Liquid Glass icon with light/dark/tinted/clear variants, used via
# CFBundleIconName) plus an AppIcon.icns for older macOS. Without Xcode, ship
# the flat rendered icns only.
ICON_NAME_KEY=""
if [[ -d $ICON_DOC ]] && xcrun --find actool >/dev/null 2>&1; then
  xcrun actool "$ICON_DOC" --compile "$APP/Contents/Resources" \
    --platform macosx --target-device mac --minimum-deployment-target "$MIN_OS" \
    --app-icon AppIcon --output-partial-info-plist "$ROOT/target/actool-partial.plist" \
    --output-format human-readable-text >/dev/null
  ICON_NAME_KEY=$'    <key>CFBundleIconName</key>\n    <string>AppIcon</string>'
else
  cp "$ICNS" "$APP/Contents/Resources/AppIcon.icns"
fi

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>$BIN</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$BUILD_NUMBER</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
$ICON_NAME_KEY
    <key>LSMinimumSystemVersion</key>
    <string>$MIN_OS</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.social-networking</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>WhatsApp GPUI uses the microphone to record voice messages.</string>
    <key>NSUserNotificationAlertStyle</key>
    <string>alert</string>
    <key>NSHumanReadableCopyright</key>
    <string>Copyright © $(date +%Y) Talha Balaj</string>
</dict>
</plist>
EOF
plutil -lint "$APP/Contents/Info.plist" >/dev/null
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "built $APP (version $VERSION, build $BUILD_NUMBER, min macOS $MIN_OS, $(lipo -archs "$APP/Contents/MacOS/$BIN"))"
