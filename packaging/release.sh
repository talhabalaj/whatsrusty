#!/usr/bin/env bash
# Build, sign (Developer ID + hardened runtime), notarize, staple, and package
# "WhatsApp GPUI" as a DMG in dist/.
#
#   packaging/release.sh                 # build + sign + verify + dmg + notarize + staple
#   packaging/release.sh --no-notarize   # build + sign + verify + dmg (local testing)
#   packaging/release.sh --skip-build    # reuse the existing dist/*.app (re-signs it)
#   packaging/release.sh --universal     # passed through to build-app.sh
#
# Env:
#   SIGN_IDENTITY   codesign identity (default: SHA-1 of the first
#                   "Developer ID Application" identity in the keychain)
#   NOTARY_PROFILE  notarytool keychain profile (default: whatsapp-gpui-notary)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

APP_NAME="WhatsApp GPUI"
APP="$ROOT/dist/$APP_NAME.app"
ENTITLEMENTS="$ROOT/packaging/entitlements.plist"
NOTARY_PROFILE="${NOTARY_PROFILE:-whatsapp-gpui-notary}"
TEAM_ID="W7TT249CUU"

NOTARIZE=1
BUILD=1
BUILD_ARGS=()
for arg in "$@"; do
  case "$arg" in
    --no-notarize) NOTARIZE=0 ;;
    --skip-build) BUILD=0 ;;
    --universal) BUILD_ARGS+=(--universal) ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

step() { printf '\n==> %s\n' "$*"; }

# Fail before a long build if notarization can't happen.
if [[ $NOTARIZE == 1 ]] && ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
  cat >&2 <<EOF
error: no notarytool keychain profile named "$NOTARY_PROFILE".

Store credentials once (pick one), then re-run:

  # Apple ID + app-specific password (create at https://account.apple.com > Sign-In and Security)
  xcrun notarytool store-credentials "$NOTARY_PROFILE" \\
    --apple-id "<your Apple ID email>" --team-id "$TEAM_ID" --password "<app-specific password>"

  # or an App Store Connect API key (Users and Access > Integrations > Team Keys)
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

if [[ $BUILD == 1 ]]; then
  step "Building app bundle"
  "$ROOT/packaging/build-app.sh" "${BUILD_ARGS[@]+"${BUILD_ARGS[@]}"}"
fi
[[ -d $APP ]] || { echo "error: $APP not found" >&2; exit 1; }

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")"
DMG="$ROOT/dist/WhatsApp-GPUI-$VERSION.dmg"

step "Signing app with $SIGN_IDENTITY"
# Single-binary bundle, so no nested code to sign first.
codesign --force --sign "$SIGN_IDENTITY" --options runtime --timestamp \
  --entitlements "$ENTITLEMENTS" "$APP"

step "Verifying signature"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -dvv "$APP" 2>&1 | grep -E '^(Identifier|Authority|TeamIdentifier|Timestamp|Runtime Version|CodeDirectory)'
# Before notarization spctl reports "Unnotarized Developer ID"; that's expected.
spctl -a -vv -t exec "$APP" || true

notarize() {
  step "Notarizing $(basename "$1")"
  xcrun notarytool submit "$1" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json \
    | tee /tmp/whatsapp-gpui-notary.json
  local id status
  id="$(python3 -c 'import json; print(json.load(open("/tmp/whatsapp-gpui-notary.json"))["id"])')"
  status="$(python3 -c 'import json; print(json.load(open("/tmp/whatsapp-gpui-notary.json"))["status"])')"
  if [[ $status != Accepted ]]; then
    xcrun notarytool log "$id" --keychain-profile "$NOTARY_PROFILE" >&2 || true
    echo "error: notarization $status" >&2
    exit 1
  fi
}

if [[ $NOTARIZE == 1 ]]; then
  # Notarize + staple the .app first, so the copy inside the DMG carries its
  # own ticket and launches offline after being dragged to /Applications.
  ZIP="$ROOT/dist/$APP_NAME.zip"
  ditto -c -k --keepParent "$APP" "$ZIP"
  notarize "$ZIP"
  rm -f "$ZIP"
  xcrun stapler staple "$APP"
  spctl -a -vv -t exec "$APP"
fi

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
echo "dmg: $DMG"
