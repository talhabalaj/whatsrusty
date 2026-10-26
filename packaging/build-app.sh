#!/usr/bin/env bash
# Build an ad-hoc signed app bundle through the Xcode project, for running locally from a bundle
# (system notifications, Launch at Login and Sparkle need one).
#
#   packaging/build-app.sh            # Debug: build/app/Build/Products/Debug/WhatsRusty Dev.app
#   packaging/build-app.sh --release  # Release: .../Release/WhatsRusty.app (Sparkle enabled)
#
# Release bundles move legacy "WhatsApp GPUI" data on first launch; try them with a temp HOME:
#   HOME=$(mktemp -d) WA_MOCK=1 build/app/Build/Products/Release/WhatsRusty.app/Contents/MacOS/WhatsRusty
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

configuration=Debug
for arg in "$@"; do
  case "$arg" in
    --release) configuration=Release ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

if [[ ! -d WhatsRusty.xcodeproj ]] || [[ project.yml -nt WhatsRusty.xcodeproj/project.pbxproj ]]; then
  xcodegen generate
fi
xcodebuild -project WhatsRusty.xcodeproj -scheme WhatsRusty -configuration "$configuration" \
  -destination 'generic/platform=macOS' -derivedDataPath "$ROOT/build/app" \
  CODE_SIGN_IDENTITY=- CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM= \
  `# Library validation would refuse the ad-hoc signed Sparkle.framework.` \
  ENABLE_HARDENED_RUNTIME=NO build
name="$([[ $configuration == Debug ]] && echo "WhatsRusty Dev" || echo WhatsRusty)"
echo "$ROOT/build/app/Build/Products/$configuration/$name.app"
