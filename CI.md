# WhatsRusty builds and releases

Pushing a `vX.Y.Z` tag releases the app. A GitHub-hosted macOS runner builds the tagged commit with Xcode, signs it with the Developer ID certificate, notarizes and staples it, Sparkle-signs the update archive and feed, and publishes them to GitHub Releases. The website at `https://whatsrusty.apps.balaj.me` serves the feed and downloads from there. The same script runs on a Mac, so a release never depends on CI being available.

## Layout

- Public source repository: `talhabalaj/whatsrusty`.
- Shared Xcode project/scheme: `WhatsRusty.xcodeproj` / `WhatsRusty` (XcodeGen, `project.yml`; the generated project is committed). The target has no compiled sources. Its "Build Rust executable" phase (`packaging/xcode-cargo-build.sh`) runs `cargo build --locked --release --target aarch64-apple-darwin` and installs the binary as `Contents/MacOS/WhatsRusty`. Xcode adds Info.plist, the Icon Composer icon, entitlements, the embedded Sparkle.framework (SPM binary package pinned by checksum in `packaging/Sparkle`) and signing.
- Developer team: `W7TT249CUU`; production identity `me.balaj.WhatsRusty`, Debug `me.balaj.WhatsRusty.dev` (`WhatsRusty Dev.app`).
- Rust toolchain pinned in `packaging/rust-toolchain`.
- `packaging/release.sh`: archive → Developer ID export → verify → notarize + staple app → ZIP + DMG → notarize + staple DMG. Verification covers the Developer ID authority, team, hardened runtime and timestamp (including Sparkle's helpers), arm64, bundle id, Sparkle feed settings, `audio-input` present and no sandbox or `get-task-allow`.
- `packaging/publish.sh vX.Y.Z [--dry-run]`: builds the tagged commit in a clean worktree with `MARKETING_VERSION` from the tag and the next build number, runs `release.sh`, Sparkle-signs the ZIP and `appcast.xml`, writes `manifest.json` (with the notarization submission ID and who built it) and publishes.
- Worker `whatsrusty-site`, custom domain `whatsrusty.apps.balaj.me`, backed by GitHub release assets (no R2).
- `.github/workflows/ci.yml`: on macOS, `cargo build`, `cargo clippy -D warnings` and `cargo test`. On Linux, the release-tooling tests, the website tests/types/build and script syntax checks.
- `.github/workflows/release.yml`: on a `v*` tag, `cargo test`, then `publish.sh` with the signing certificate in a temporary keychain. `workflow_dispatch` runs it as a dry run by default (tag name + ref to build), which builds, notarizes and signs but publishes nothing; the signed files are kept as a run artifact.
- `.github/workflows/site-deploy.yml`: deploys `site/` on pushes to `main` once `CLOUDFLARE_CI_ENABLED=true`.

## Repository secrets and variables

See SIGNING.md for where each comes from.

| Name | Kind | Used for |
| --- | --- | --- |
| `DEVELOPER_ID_P12`, `DEVELOPER_ID_P12_PASSWORD` | secrets | Developer ID Application certificate + key (base64 .p12) for codesign |
| `ASC_KEY_ID`, `ASC_ISSUER_ID`, `ASC_PRIVATE_KEY` | secrets | notarytool (App Store Connect API key) |
| `SPARKLE_PRIVATE_KEY` | secret | Signing the update archive and feed |
| `SPARKLE_PUBLIC_KEY` | variable | Verifying them before publishing |
| `CLOUDFLARE_API_TOKEN` | secret | Website deploys |
| `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_CI_ENABLED` | variables | Website deploys |

## Publish

1. Push `main` and wait for CI to pass.
2. Optionally dry-run first: Actions → Release macOS app → Run workflow, with the tag you are about to create (it need not exist yet). Or locally: `packaging/publish.sh v0.1.0 --dry-run`.
3. Tag the exact commit with a new, unused version and push it:
   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   ```
   The version comes from the tag; there is nothing to bump in the project. Build numbers count up from the published one.
4. `release.yml` runs the tests, builds, notarizes and signs. `publish-release.mjs` then creates a draft release for the tag and uploads the assets without overwriting (matching retries are allowed). It publishes with `--latest=false`, verifies anonymous downloads and the website's feed, rechecks against downgrades, then marks the release latest as the final promotion. The DMG is attached afterwards for manual downloads.

To release from a Mac instead: `packaging/publish.sh vX.Y.Z` after pushing the tag. It uses the login Keychain's Developer ID identity, the notarytool profile `WhatsRusty`, the Sparkle key in Keychain and the `gh` login.

Publication is serialized (`concurrency: whatsrusty-app-production`). A failed run can be re-run; identical assets already uploaded are accepted, different ones are refused.

Only describe notarization as complete after a successful Apple submission and the stapler/Gatekeeper checks. Only describe automatic updating as tested after an actual Sparkle upgrade between two signed releases.

## Local verification

- Unsigned build: `xcodebuild -project WhatsRusty.xcodeproj -scheme WhatsRusty -configuration Release -destination 'generic/platform=macOS' CODE_SIGNING_ALLOWED=NO archive -archivePath /tmp/whatsrusty.xcarchive`.
- Signed, not notarized: `packaging/release.sh --no-notarize`.
- Full pipeline without publishing: `packaging/publish.sh v0.1.0 --dry-run`. On 2026-10-27 (Xcode 27.1, Rust 1.99.1) this archived, exported with Developer ID, passed the verification above, was notarized (`Accepted`) and stapled for both the app and the DMG, and passed Gatekeeper as "Notarized Developer ID". It Sparkle-signed the 22 MB ZIP and the feed.
- Tooling tests: `node --test scripts/ci/*.test.mjs`, `python3 -m unittest discover -s scripts/ci -p '*_test.py'`, `node --experimental-strip-types --test site/tests/*.test.mjs`.

## First-run notes for the existing install

The first Release launch renames `~/Library/Application Support/WhatsApp GPUI` and `~/Library/Logs/WhatsApp GPUI` to `WhatsRusty`, keeping the linked session. Quit the old `WhatsApp GPUI.app` first and delete it afterwards. The phone most likely keeps showing the device as "WhatsApp GPUI" until it is relinked: the device name is sent when pairing.
