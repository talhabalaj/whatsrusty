#!/bin/sh
set -eu

if [ "${CI_XCODE_CLOUD:-}" != "TRUE" ]; then
  echo "This hook is only intended for Xcode Cloud." >&2
  exit 1
fi

xcode_version=$(xcodebuild -version | sed -n '1p')
case "$xcode_version" in
  "Xcode 27"|"Xcode 27."*) ;;
  *) echo "WhatsRusty requires Xcode 27; configure the cloud environment accordingly." >&2; exit 1 ;;
esac
sdk_version=$(xcrun --sdk macosx --show-sdk-version)
case "$sdk_version" in
  27|27.*) ;;
  *) echo "WhatsRusty requires the macOS 27 SDK." >&2; exit 1 ;;
esac
export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "cargo is missing; ci_post_clone.sh did not run." >&2; exit 1; }
xcrun -f metal >/dev/null || { echo "The Metal toolchain is missing." >&2; exit 1; }

echo "Building WhatsRusty with $xcode_version, $(rustc --version), macOS SDK $sdk_version."
