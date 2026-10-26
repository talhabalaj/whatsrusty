#!/usr/bin/env bash
# Build, notarize, Sparkle-sign and publish a WhatsRusty release (see CI.md).
# Runs the same way in .github/workflows/release.yml and on a Mac:
# builds the exact tagged commit in a clean worktree, versioned from the tag, and publishes
# through the verified GitHub Release + website path.
#
#   packaging/publish.sh v0.1.0 --dry-run   # build, notarize, sign; publish nothing
#   packaging/publish.sh v0.1.0             # ...then publish (the tag must be pushed)
#
# Env:
#   NOTARY_PROFILE                 notarytool keychain profile (default: WhatsRusty), or
#   NOTARY_KEY_PATH/_KEY_ID/_ISSUER an App Store Connect key file (CI)
#   SPARKLE_PRIVATE_KEY            Sparkle EdDSA key (CI); otherwise the login Keychain item
#   BUILD_NUMBER                   CFBundleVersion (default: the published build + 1, or 1)
#   GH_TOKEN                       for publishing (CI); otherwise the gh CLI login
set -euo pipefail

TAG="${1:?usage: publish.sh vX.Y.Z [--dry-run]}"
DRY_RUN=0
[[ "${2:-}" == --dry-run ]] && DRY_RUN=1
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO=talhabalaj/whatsrusty
ORIGIN=https://whatsrusty.apps.balaj.me
SPARKLE_ACCOUNT=me.balaj.WhatsRusty
SPARKLE_PUBLIC_KEY=9q0PzQIIxhsgZT877jGgyo11cQVAjhI1KsygiZExj4I=
VERSION="${TAG#v}"
[[ "$TAG" == v* && "$VERSION" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || { echo "error: tag must look like v1.2.3" >&2; exit 2; }
if [[ -n ${GITHUB_RUN_ID:-} ]]; then BUILDER="github-actions run $GITHUB_RUN_ID"; else BUILDER="local $(hostname -s)"; fi

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
(cd "$WORK/src" && CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target-release}" BUILD_NUMBER="$BUILD_NUMBER" \
  MARKETING_VERSION="$VERSION" packaging/release.sh)

APP="$WORK/src/build/release/export/WhatsRusty.app"
plist() { /usr/libexec/PlistBuddy -c "Print :$1" "$APP/Contents/Info.plist"; }
[[ "$(plist CFBundleShortVersionString)" == "$VERSION" && "$(plist CFBundleVersion)" == "$BUILD_NUMBER" ]]
[[ "$(plist SUPublicEDKey)" == "$SPARKLE_PUBLIC_KEY" && "$(plist SUFeedURL)" == "$ORIGIN/appcast.xml" ]]

OUT="$ROOT/build/release-out/$TAG"
rm -rf "$OUT"
mkdir -p "$OUT"
ZIP_NAME="WhatsRusty-$VERSION-$BUILD_NUMBER.zip"
cp "$WORK/src/dist/$ZIP_NAME" "$WORK/src/dist/WhatsRusty-$VERSION.dmg" "$OUT/"

step "Signing the update with the Sparkle key"
tools="$WORK/sparkle"
mkdir "$tools"
curl --fail --silent --show-error --location --retry 3 \
  https://github.com/sparkle-project/Sparkle/releases/download/2.10.0/Sparkle-2.10.0.tar.xz --output "$tools/Sparkle.tar.xz"
printf 'c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c  %s\n' "$tools/Sparkle.tar.xz" | shasum -a 256 -c - >/dev/null
tar -xf "$tools/Sparkle.tar.xz" -C "$tools"
sparkle_sign() {
  if [[ -n ${SPARKLE_PRIVATE_KEY:-} ]]; then
    printf '%s' "$SPARKLE_PRIVATE_KEY" | "$tools/bin/sign_update" --ed-key-file - "$@"
  else
    "$tools/bin/sign_update" --account "$SPARKLE_ACCOUNT" "$@"
  fi
}
signature="$(sparkle_sign -p "$OUT/$ZIP_NAME")"
notary_id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$WORK/src/build/release/notary-app.json")"
node - "$OUT/metadata.json" "$VERSION" "$BUILD_NUMBER" "$(plist LSMinimumSystemVersion)" "$signature" \
  "$(shasum -a 256 "$OUT/$ZIP_NAME" | awk '{print $1}')" "$(stat -f '%z' "$OUT/$ZIP_NAME")" "$COMMIT" "$notary_id" "$BUILDER" <<'JS'
const [output, version, build, minimumMacOS, signature, sha256, bytes, commitSha, notaryId, builder] = process.argv.slice(2);
require('node:fs').writeFileSync(output, JSON.stringify({ version, build, minimumMacOS, signature, sha256, bytes: Number(bytes),
  architecture: 'arm64', commitSha, notaryId, builder }, null, 2) + '\n');
JS
node "$ROOT/scripts/ci/release-files.mjs" "$OUT/metadata.json" "$OUT"
sparkle_sign "$OUT/appcast.xml"

if [[ $DRY_RUN == 1 ]]; then
  step "Dry run: built, notarized and signed into $OUT; nothing published"
  exit 0
fi

step "Publishing $TAG"
SPARKLE_PUBLIC_KEY="$SPARKLE_PUBLIC_KEY" node "$ROOT/scripts/ci/publish-release.mjs" "$OUT"
gh release upload "$TAG" "$OUT/WhatsRusty-$VERSION.dmg" --repo "$REPO"
cat "$OUT/publication.json"
