#!/bin/bash
# Xcode "Build Rust executable" phase: compiles the cargo workspace for arm64 and
# installs the binary as the bundle executable (Release: WhatsRusty, Debug: WhatsRusty Dev).
set -euo pipefail

export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
: "${TARGET_BUILD_DIR:?}" "${EXECUTABLE_PATH:?}" "${CONFIGURATION:?}"
command -v cargo >/dev/null || { echo "error: cargo not found; install rustup (see CI.md)" >&2; exit 1; }

# Xcode exports variables that leak into cc/build scripts of dependencies.
unset LD LDPLUSPLUS LIBRARY_PATH CPATH

target=aarch64-apple-darwin
args=(build --locked -p whatsapp-gpui --target "$target")
profile=debug
if [[ $CONFIGURATION == Release ]]; then
  args+=(--release)
  profile=release
fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}"

cd "$PROJECT_DIR"
cargo "${args[@]}"

binary="$CARGO_TARGET_DIR/$target/$profile/whatsapp-gpui"
destination="$TARGET_BUILD_DIR/$EXECUTABLE_PATH"
mkdir -p "$(dirname "$destination")"
# Copy (not hard link) so Xcode's signature never lands on cargo's output.
rm -f "$destination"
cp "$binary" "$destination"
[[ $profile == debug ]] || strip -S -x "$destination"
