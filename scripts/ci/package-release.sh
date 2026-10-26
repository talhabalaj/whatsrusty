#!/bin/bash
set -euo pipefail

artifact_path="${1:?Usage: package-release.sh NOTARIZED_ARTIFACT CLOUD_RESULT OUTPUT_DIRECTORY}"
cloud_result="${2:?Xcode Cloud result is required}"
output_dir="${3:?Output directory is required}"
repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
: "${SPARKLE_PRIVATE_KEY:?SPARKLE_PRIVATE_KEY is required}"
: "${SPARKLE_PUBLIC_KEY:?SPARKLE_PUBLIC_KEY is required}"
mkdir -p "$output_dir"
extract_dir="$(mktemp -d "${TMPDIR:-/tmp}/whatsrusty-release.XXXXXX")"
trap 'rm -rf "$extract_dir"' EXIT

# This is a signed and stapled Xcode Cloud artifact, never a local rebuild.
node - "$cloud_result" <<'JS'
const result = JSON.parse(require('node:fs').readFileSync(process.argv[2], 'utf8'));
if (result.artifactType !== 'STAPLED_NOTARIZED_ARCHIVE' || !result.artifactId || !result.commitSha) throw new Error('A verified Xcode Cloud notarized artifact is required.');
JS
tar -xf "$artifact_path" -C "$extract_dir"
# Require the native product name; never repair an incorrectly named export.
app_path="$(python3 "$repo_root/scripts/ci/cloud-app.py" "$extract_dir")"
plist="$app_path/Contents/Info.plist"
plist_value() { /usr/libexec/PlistBuddy -c "Print :$1" "$plist"; }
[[ "$(plist_value CFBundleIdentifier)" == me.balaj.WhatsRusty ]]
[[ "$(plist_value SUPublicEDKey)" == "$SPARKLE_PUBLIC_KEY" ]]
[[ "$(plist_value SUFeedURL)" == https://whatsrusty.apps.balaj.me/appcast.xml ]]
[[ "$(plist_value SURequireSignedFeed)" == true ]]
[[ "$(plist_value SUVerifyUpdateBeforeExtraction)" == true ]]
[[ -d "$app_path/Contents/Frameworks/Sparkle.framework" ]]
version="$(plist_value CFBundleShortVersionString)"
build="$(plist_value CFBundleVersion)"
minimum_macos="$(plist_value LSMinimumSystemVersion)"
[[ "$version" =~ ^[0-9]+(\.[0-9]+){0,2}$ && "$build" =~ ^[0-9]+(\.[0-9]+){0,2}$ ]]
executable="$(plist_value CFBundleExecutable)"
[[ "$executable" == WhatsRusty ]]
[[ "$(lipo -archs "$app_path/Contents/MacOS/$executable")" == arm64 ]]
node - "$cloud_result" "$version" "$build" <<'JS'
const fs = require('node:fs');
const [resultPath, version, build] = process.argv.slice(2);
const cloud = JSON.parse(fs.readFileSync(resultPath, 'utf8'));
if (!Number.isSafeInteger(cloud.buildNumber) || cloud.buildNumber < 1 || build !== String(cloud.buildNumber)) {
  throw new Error('The notarized app must use the Xcode Cloud build counter, starting at 1.');
}
if (cloud.ref !== `refs/tags/v${version}`) throw new Error('The notarized app version must match its release tag.');
JS

codesign --verify --deep --strict --verbose=2 "$app_path"
codesign --display --verbose=4 "$app_path" 2> "$output_dir/signature.txt"
grep -q '^Authority=Developer ID Application:' "$output_dir/signature.txt"
grep -q '^TeamIdentifier=W7TT249CUU$' "$output_dir/signature.txt"
grep -q 'flags=.*runtime' "$output_dir/signature.txt"
grep -q '^Timestamp=' "$output_dir/signature.txt"
# Sparkle's nested helpers must carry the same team signature and hardened runtime.
for nested in Contents/Frameworks/Sparkle.framework/Versions/B/Autoupdate \
    Contents/Frameworks/Sparkle.framework/Versions/B/Updater.app \
    Contents/Frameworks/Sparkle.framework/Versions/B/XPCServices/Downloader.xpc \
    Contents/Frameworks/Sparkle.framework/Versions/B/XPCServices/Installer.xpc; do
  details="$(codesign --display --verbose=4 "$app_path/$nested" 2>&1)"
  grep -q '^TeamIdentifier=W7TT249CUU$' <<< "$details" || { printf 'Unexpected signature on %s\n' "$nested" >&2; exit 1; }
  grep -q 'flags=.*runtime' <<< "$details" || { printf 'No hardened runtime on %s\n' "$nested" >&2; exit 1; }
done
codesign --display --entitlements - --xml "$app_path" > "$output_dir/entitlements.plist"
[[ "$(/usr/libexec/PlistBuddy -c 'Print :com.apple.security.device.audio-input' "$output_dir/entitlements.plist")" == true ]]
for entitlement in com.apple.security.app-sandbox com.apple.security.get-task-allow; do
  value="$(/usr/libexec/PlistBuddy -c "Print :$entitlement" "$output_dir/entitlements.plist" 2>/dev/null || true)"
  [[ "$value" != true ]] || { printf 'Unexpected release entitlement: %s\n' "$entitlement" >&2; exit 1; }
done
xcrun stapler validate "$app_path"
spctl --assess --type execute --verbose=4 "$app_path"
zip_path="$output_dir/WhatsRusty-$version-$build.zip"
ditto -c -k --sequesterRsrc --keepParent "$app_path" "$zip_path"

# Pin both the tools and their checksum independently of the runner environment.
tools_dir="$extract_dir/sparkle"
mkdir "$tools_dir"
curl --fail --silent --show-error --location --retry 3 \
  https://github.com/sparkle-project/Sparkle/releases/download/2.10.0/Sparkle-2.10.0.tar.xz \
  --output "$tools_dir/Sparkle.tar.xz"
printf 'c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c  %s\n' "$tools_dir/Sparkle.tar.xz" | shasum -a 256 -c -
tar -xf "$tools_dir/Sparkle.tar.xz" -C "$tools_dir"
signature="$(printf '%s' "$SPARKLE_PRIVATE_KEY" | "$tools_dir/bin/sign_update" --ed-key-file - -p "$zip_path")"
sha256="$(shasum -a 256 "$zip_path" | awk '{print $1}')"
bytes="$(stat -f '%z' "$zip_path")"
node - "$cloud_result" "$output_dir/metadata.json" "$version" "$build" "$minimum_macos" "$signature" "$sha256" "$bytes" <<'JS'
const fs = require('node:fs');
const [resultPath, output, version, build, minimumMacOS, signature, sha256, bytes] = process.argv.slice(2);
const cloud = JSON.parse(fs.readFileSync(resultPath, 'utf8'));
fs.writeFileSync(output, JSON.stringify({ version, build, minimumMacOS, signature, sha256, bytes: Number(bytes), architecture: 'arm64', commitSha: cloud.commitSha, cloudBuildId: cloud.buildId, artifactId: cloud.artifactId }, null, 2) + '\n');
JS
node "$repo_root/scripts/ci/release-files.mjs" "$output_dir/metadata.json" "$output_dir"
printf '%s' "$SPARKLE_PRIVATE_KEY" | "$tools_dir/bin/sign_update" --ed-key-file - "$output_dir/appcast.xml"
cp "$cloud_result" "$output_dir/xcode-cloud-result.json"
printf 'Verified and packaged WhatsRusty %s (%s).\n' "$version" "$build"
