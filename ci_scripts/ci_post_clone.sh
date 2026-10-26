#!/bin/sh
# Xcode Cloud: stamp the version from the build/tag and install the pinned Rust toolchain.
set -eu
: "${CI_PRIMARY_REPOSITORY_PATH:?}"
here="$(cd "$(dirname "$0")" && pwd)"
# Fails fast on a bad build number or tag, before the toolchain download.
sh "$here/stamp-version.sh"
cd "$CI_PRIMARY_REPOSITORY_PATH"

toolchain="$(cat ci_scripts/rust-toolchain)"
if ! command -v rustup >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain none --no-modify-path
fi
export PATH="$HOME/.cargo/bin:$PATH"
rustup toolchain install "$toolchain" --profile minimal --target aarch64-apple-darwin
rustup default "$toolchain"
rustc --version
# Fetch crates now so a network hiccup fails here, not halfway through the archive.
cargo fetch --locked

# gpui compiles its Metal shaders at build time; newer Xcode ships the compiler separately.
xcrun -f metal >/dev/null 2>&1 || xcodebuild -downloadComponent MetalToolchain
