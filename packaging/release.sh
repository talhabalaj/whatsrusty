#!/usr/bin/env bash
# Local Developer ID release of WhatsRusty, outside the Xcode Cloud pipeline (see CI.md):
# xcodebuild archive -> developer-id export -> verify -> notarize + staple app -> DMG -> notarize + staple DMG.
#
#   packaging/release.sh                 # full pipeline -> dist/WhatsRusty-<version>.dmg (+ .zip)
#   packaging/release.sh --no-notarize   # sign, verify and package only (local testing)
#
# Env:
#   SIGN_IDENTITY     codesign identity (default: SHA-1 of the first
#                     "Developer ID Application ... (W7TT249CUU)" identity in the keychain)
#   NOTARY_PROFILE    notarytool keychain profile (default: WhatsRusty)
#   BUILD_NUMBER      CFBundleVersion (default: the project's CURRENT_PROJECT_VERSION)
#   CARGO_TARGET_DIR  passed through to the cargo build phase (default: ./target)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

APP_NAME="WhatsRusty"
TEAM_ID="W7TT249CUU"
NOTARY_PROFILE="${NOTARY_PROFILE:-WhatsRusty}"
BUILD_DIR="$ROOT/build/release"
ARCHIVE="$BUILD_DIR/$APP_NAME.xcarchive"
EXPORT="$BUILD_DIR/export"
APP="$EXPORT/$APP_NAME.app"

NOTARIZE=1
for arg in "$@"; do
  case "$arg" in
    --no-notarize) NOTARIZE=0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

step() { printf '\n==> %s\n' "$*"; }

# Fail before a long build if notarization can't happen.
if [[ $NOTARIZE == 1 ]] && ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
  cat >&2 <<EOF
error: no notarytool keychain profile named "$NOTARY_PROFILE".

Store an App Store Connect API key once (Users and Access > Integrations > Team Keys), then re-run:

  xcrun notarytool store-credentials "$NOTARY_PROFILE" \\
    --key ~/path/AuthKey_<KEYID>.p8 --key-id "<KEYID>" --issuer "<issuer UUID>"

Or run with --no-notarize to only sign and package.
EOF
  exit 1
fi

if [[ -z ${SIGN_IDENTITY:-} ]]; then
  SIGN_IDENTITY="$(security find-identity -v -p codesigning \
    | awk '/"Developer ID Application: .*\('"$TEAM_ID"'\)"/ {print $2; exit}')"
  [[ -n $SIGN_IDENTITY ]] || { echo "error: no Developer ID Application identity for $TEAM_ID" >&2; exit 1; }
fi

if [[ ! -d WhatsRusty.xcodeproj ]] || [[ project.yml -nt WhatsRusty.xcodeproj/project.pbxproj ]]; then
  step "Generating WhatsRusty.xcodeproj"
  xcodegen generate
fi

rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"
overrides=(CODE_SIGN_STYLE=Manual "CODE_SIGN_IDENTITY=$SIGN_IDENTITY" "DEVELOPMENT_TEAM=$TEAM_ID")
[[ -z ${BUILD_NUMBER:-} ]] || overrides+=("CURRENT_PROJECT_VERSION=$BUILD_NUMBER")

step "Archiving (Release, arm64)"
xcodebuild -project WhatsRusty.xcodeproj -scheme WhatsRusty -configuration Release \
  -destination 'generic/platform=macOS' -derivedDataPath "$BUILD_DIR/derived" \
  -archivePath "$ARCHIVE" "${overrides[@]}" archive

step "Exporting with Developer ID"
plutil -create xml1 "$BUILD_DIR/export-options.plist"
plutil -insert method -string developer-id "$BUILD_DIR/export-options.plist"
plutil -insert signingStyle -string manual "$BUILD_DIR/export-options.plist"
plutil -insert teamID -string "$TEAM_ID" "$BUILD_DIR/export-options.plist"
plutil -insert signingCertificate -string "$SIGN_IDENTITY" "$BUILD_DIR/export-options.plist"
xcodebuild -exportArchive -archivePath "$ARCHIVE" -exportPath "$EXPORT" \
  -exportOptionsPlist "$BUILD_DIR/export-options.plist"
python3 -I "$ROOT/scripts/ci/cloud-app.py" "$EXPORT" >/dev/null

step "Verifying signatures"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign --display --verbose=4 "$APP" 2>&1 | grep -E '^(Identifier|Authority|TeamIdentifier|Timestamp|Runtime Version)'
for nested in Contents/Frameworks/Sparkle.framework/Versions/B/Autoupdate \
    Contents/Frameworks/Sparkle.framework/Versions/B/Updater.app \
    Contents/Frameworks/Sparkle.framework/Versions/B/XPCServices/Downloader.xpc \
    Contents/Frameworks/Sparkle.framework/Versions/B/XPCServices/Installer.xpc; do
  details="$(codesign --display --verbose=4 "$APP/$nested" 2>&1)"
  grep -q "^TeamIdentifier=$TEAM_ID\$" <<< "$details" && grep -q 'flags=.*runtime' <<< "$details" \
    || { echo "error: $nested lacks the team signature or hardened runtime" >&2; exit 1; }
done
codesign --display --entitlements - --xml "$APP" > "$BUILD_DIR/entitlements.plist"
[[ "$(/usr/libexec/PlistBuddy -c 'Print :com.apple.security.device.audio-input' "$BUILD_DIR/entitlements.plist")" == true ]]
[[ "$(lipo -archs "$APP/Contents/MacOS/$APP_NAME")" == arm64 ]]
# Before notarization spctl reports "Unnotarized Developer ID"; that's expected.
spctl -a -vv -t exec "$APP" || true

notarize() {
  step "Notarizing $(basename "$1")"
  xcrun notarytool submit "$1" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json \
    | tee "$BUILD_DIR/notary.json"
  local id status
  id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$BUILD_DIR/notary.json")"
  status="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"])' "$BUILD_DIR/notary.json")"
  if [[ $status != Accepted ]]; then
    xcrun notarytool log "$id" --keychain-profile "$NOTARY_PROFILE" >&2 || true
    echo "error: notarization $status" >&2
    exit 1
  fi
}

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")"
BUILD="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$APP/Contents/Info.plist")"
mkdir -p "$ROOT/dist"
ZIP="$ROOT/dist/$APP_NAME-$VERSION-$BUILD.zip"
DMG="$ROOT/dist/$APP_NAME-$VERSION.dmg"

if [[ $NOTARIZE == 1 ]]; then
  # Notarize + staple the .app first, so the copy inside the DMG carries its
  # own ticket and launches offline after being dragged to /Applications.
  ditto -c -k --keepParent "$APP" "$BUILD_DIR/notarize.zip"
  notarize "$BUILD_DIR/notarize.zip"
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
  spctl -a -vv -t exec "$APP"
fi
# Same layout as the Sparkle archive CI publishes.
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

step "Creating DMG"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
ditto "$APP" "$STAGE/$APP_NAME.app"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$DMG" >/dev/null
codesign --force --sign "$SIGN_IDENTITY" --timestamp "$DMG"
codesign --verify --strict --verbose=2 "$DMG"

if [[ $NOTARIZE == 1 ]]; then
  notarize "$DMG"
  xcrun stapler staple "$DMG"
  xcrun stapler validate "$DMG"
  spctl -a -vv -t open --context context:primary-signature "$DMG"
fi

step "Done"
echo "app: $APP"
echo "zip: $ZIP"
echo "dmg: $DMG"
