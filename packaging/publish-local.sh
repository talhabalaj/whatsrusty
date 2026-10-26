#!/usr/bin/env bash
# Notarized WhatsRusty release from this Mac, without Xcode Cloud (see CI.md, "Local release").
# Builds the exact tagged commit in a clean worktree, notarizes it with a notarytool profile,
# signs the update with the Sparkle key in the login Keychain, and publishes through the same
# verified GitHub Release + website path as release.yml.
#
#   packaging/publish-local.sh v0.1.0 --dry-run   # build, notarize, sign; publish nothing
#   packaging/publish-local.sh v0.1.0             # ...then publish (the tag must be pushed)
#
# Env:
#   NOTARY_PROFILE  notarytool keychain profile (default: WhatsRusty)
#   BUILD_NUMBER    CFBundleVersion (default: the published build + 1, or 1)
set -euo pipefail

TAG="${1:?usage: publish-local.sh vX.Y.Z [--dry-run]}"
DRY_RUN=0
[[ "${2:-}" == --dry-run ]] && DRY_RUN=1
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO=talhabalaj/whatsrusty
ORIGIN=https://whatsrusty.apps.balaj.me
SPARKLE_ACCOUNT=me.balaj.WhatsRusty
SPARKLE_PUBLIC_KEY=9q0PzQIIxhsgZT877jGgyo11cQVAjhI1KsygiZExj4I=
VERSION="${TAG#v}"
[[ "$TAG" == v* && "$VERSION" =~ ^[0-9]+(\.[0-9]+){0,2}$ ]] || { echo "error: tag must look like v1.2.3" >&2; exit 2; }

step() { printf '\n==> %s\n' "$*"; }

# A dry run may precede tagging: it then builds the committed HEAD.
if ! COMMIT="$(git -C "$ROOT" rev-parse --verify --quiet "$TAG^{commit}")"; then
  [[ $DRY_RUN == 1 ]] || { echo "error: tag $TAG doesn't exist" >&2; exit 1; }
  COMMIT="$(git -C "$ROOT" rev-parse HEAD)"
fi
if [[ $DRY_RUN == 0 ]]; then
  remote="$(gh api "repos/$REPO/commits/$TAG" --jq .sha 2>/dev/null || true)"
  [[ "$remote" == "$COMMIT" ]] || { echo "error: push $TAG to origin first (remote: ${remote:-missing})" >&2; exit 1; }
fi

if [[ -z "${BUILD_NUMBER:-}" ]]; then
  current="$(curl -sf "$ORIGIN/downloads/manifest.json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["build"])' 2>/dev/null || true)"
  BUILD_NUMBER=$(( ${current:-0} + 1 ))
fi
[[ "$BUILD_NUMBER" =~ ^[0-9]+$ ]]

step "Building $TAG ($COMMIT), build $BUILD_NUMBER, in a clean worktree"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/whatsrusty-release.XXXXXX")"
cleanup() { git -C "$ROOT" worktree remove --force "$WORK/src" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT
git -C "$ROOT" worktree add --detach "$WORK/src" "$COMMIT" >/dev/null
(cd "$WORK/src" && CARGO_TARGET_DIR="$ROOT/target-release" BUILD_NUMBER="$BUILD_NUMBER" \
  NOTARY_PROFILE="${NOTARY_PROFILE:-WhatsRusty}" packaging/release.sh)

APP="$WORK/src/build/release/export/WhatsRusty.app"
plist() { /usr/libexec/PlistBuddy -c "Print :$1" "$APP/Contents/Info.plist"; }
[[ "$(plist CFBundleShortVersionString)" == "$VERSION" ]] || { echo "error: MARKETING_VERSION is $(plist CFBundleShortVersionString), tag is $VERSION" >&2; exit 1; }
[[ "$(plist CFBundleVersion)" == "$BUILD_NUMBER" ]]
[[ "$(plist SUPublicEDKey)" == "$SPARKLE_PUBLIC_KEY" ]]
[[ "$(plist SUFeedURL)" == "$ORIGIN/appcast.xml" ]]

OUT="$ROOT/build/local-release/$TAG"
rm -rf "$OUT"
mkdir -p "$OUT"
ZIP_NAME="WhatsRusty-$VERSION-$BUILD_NUMBER.zip"
cp "$WORK/src/dist/$ZIP_NAME" "$OUT/$ZIP_NAME"
cp "$WORK/src/dist/WhatsRusty-$VERSION.dmg" "$OUT/"

step "Signing the update with the Keychain Sparkle key"
tools="$WORK/sparkle"
mkdir "$tools"
curl --fail --silent --show-error --location --retry 3 \
  https://github.com/sparkle-project/Sparkle/releases/download/2.10.0/Sparkle-2.10.0.tar.xz --output "$tools/Sparkle.tar.xz"
printf 'c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c  %s\n' "$tools/Sparkle.tar.xz" | shasum -a 256 -c - >/dev/null
tar -xf "$tools/Sparkle.tar.xz" -C "$tools"
signature="$("$tools/bin/sign_update" --account "$SPARKLE_ACCOUNT" -p "$OUT/$ZIP_NAME")"
notary_id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$WORK/src/build/release/notary.json")"
node - "$OUT/metadata.json" <<JS
require('node:fs').writeFileSync(process.argv[2], JSON.stringify({
  version: '$VERSION', build: '$BUILD_NUMBER', minimumMacOS: '$(plist LSMinimumSystemVersion)',
  signature: '$signature', sha256: '$(shasum -a 256 "$OUT/$ZIP_NAME" | awk '{print $1}')',
  bytes: $(stat -f '%z' "$OUT/$ZIP_NAME"), architecture: 'arm64', commitSha: '$COMMIT',
  cloudBuildId: 'local', artifactId: 'notary:$notary_id',
}, null, 2) + '\n');
JS
node "$ROOT/scripts/ci/release-files.mjs" "$OUT/metadata.json" "$OUT"
"$tools/bin/sign_update" --account "$SPARKLE_ACCOUNT" "$OUT/appcast.xml"

if [[ $DRY_RUN == 1 ]]; then
  step "Dry run: built, notarized and signed into $OUT; nothing published"
  exit 0
fi

step "Publishing $TAG"
SPARKLE_PUBLIC_KEY="$SPARKLE_PUBLIC_KEY" node "$ROOT/scripts/ci/publish-release.mjs" "$OUT"
gh release upload "$TAG" "$OUT/WhatsRusty-$VERSION.dmg" --repo "$REPO"
cat "$OUT/publication.json"
