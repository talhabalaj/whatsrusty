# WhatsRusty builds and releases

Same release architecture as Pufflet and Said. Xcode Cloud builds a **STAPLED_NOTARIZED_ARCHIVE** from a `v` tag, GitHub Actions verifies it on macOS, Sparkle signs the archive and feed, and GitHub Releases stores versioned downloads served through `https://whatsrusty.apps.balaj.me`.

## Layout

- Public source repository: `talhabalaj/whatsrusty`.
- Shared Xcode project/scheme: `WhatsRusty.xcodeproj` / `WhatsRusty` (XcodeGen, `project.yml`). The target has no compiled sources. Its "Build Rust executable" phase (`packaging/xcode-cargo-build.sh`) runs `cargo build --locked --release --target aarch64-apple-darwin` and installs the binary as `Contents/MacOS/WhatsRusty`. Xcode adds Info.plist, the Icon Composer icon, entitlements, the embedded Sparkle.framework (SPM binary package pinned by checksum in `packaging/Sparkle`) and signing.
- Developer team: `W7TT249CUU`; production identity `me.balaj.WhatsRusty`, Debug `me.balaj.WhatsRusty.dev` (`WhatsRusty Dev.app`).
- `ci_scripts/ci_post_clone.sh` stamps `CURRENT_PROJECT_VERSION` from the Cloud build number and `MARKETING_VERSION` from the `vX.Y.Z` tag (`stamp-version.sh`). It installs rustup with the toolchain pinned in `ci_scripts/rust-toolchain`, runs `cargo fetch --locked`, and installs the Metal toolchain if missing; gpui compiles its shaders at build time. `ci_pre_xcodebuild.sh` requires Xcode 27, cargo and Metal.
- Worker `whatsrusty-site`, custom domain `whatsrusty.apps.balaj.me`, backed by GitHub release assets (no R2).
- The Sparkle public key is committed in `packaging/Info.plist`; the private key lives in Keychain, 1Password and an Actions secret (SIGNING.md).
- `.github/workflows/ci.yml`: on macOS, `cargo build`, `cargo clippy -D warnings` and `cargo test` for the workspace. On Linux, the release-tooling tests, the website tests/types/build and script syntax checks.
- `.github/workflows/release.yml`: `cargo test` on macOS → Xcode Cloud build → verify, sign and publish.
- `.github/workflows/site-deploy.yml`: deploys `site/` on pushes to `main` once `CLOUDFLARE_CI_ENABLED=true`.

The Xcode Cloud workflow has only an Archive action. The app has no XCTest target, so the Rust tests gate the release in GitHub Actions (`release.yml` job `test`) before Xcode Cloud starts. `scripts/ci/xcode-cloud.mjs` requires an enabled, clean, locked workflow on `WhatsRusty.xcodeproj` for product `me.balaj.WhatsRusty`, with a required macOS ARCHIVE action on the `WhatsRusty` scheme and Xcode 27.

## Local verification

Unsigned archive and export, like Xcode Cloud's archive step (no signing, notarization or launch):

```sh
xcodegen generate   # only after editing project.yml
xcodebuild -project WhatsRusty.xcodeproj -scheme WhatsRusty \
  -configuration Release -destination 'generic/platform=macOS' \
  -derivedDataPath /tmp/whatsrusty-derived \
  -archivePath /tmp/whatsrusty.xcarchive CODE_SIGNING_ALLOWED=NO archive
plutil -create xml1 /tmp/whatsrusty-export-options.plist
plutil -insert method -string mac-application /tmp/whatsrusty-export-options.plist
xcodebuild -exportArchive -archivePath /tmp/whatsrusty.xcarchive \
  -exportPath /tmp/whatsrusty-export \
  -exportOptionsPlist /tmp/whatsrusty-export-options.plist
python3 scripts/ci/cloud-app.py /tmp/whatsrusty-export   # → /tmp/whatsrusty-export/WhatsRusty.app
```

On 2026-10-26 (Xcode 27.1, Rust 1.99.1) this produced `WhatsRusty.app` with `CFBundleIdentifier=me.balaj.WhatsRusty`, name/display name/executable `WhatsRusty`, an arm64-only executable (`minos 14.0`, SDK 27.0), `Contents/Frameworks/Sparkle.framework`, `Assets.car` + `AppIcon.icns` and the `SU*` keys. `cloud-app.py` accepted it.

Developer ID signed build (no notarization): `packaging/release.sh --no-notarize`. On the same date it archived with Developer ID, exported (`developer-id`), and passed `codesign --verify --deep --strict`. Checks also covered the Developer ID authority, team `W7TT249CUU`, hardened runtime and secure timestamp, plus the team signature and hardened runtime on Sparkle's Autoupdate, Updater.app, Downloader.xpc and Installer.xpc. The entitlements were `audio-input` and `network.client`, and the executable is arm64. As expected before notarization, `spctl` rejected it as "Unnotarized Developer ID". The signed bundle was launched with a temporary `HOME` in mock mode: Sparkle started and the app menu showed Check for Updates… and Launch at Login. Notarization, stapling and a Sparkle upgrade between two signed releases have **not** been exercised yet.

Tooling tests: `node --test scripts/ci/*.test.mjs`, `python3 -m unittest discover -s scripts/ci -p '*_test.py'`, `node --experimental-strip-types --test site/tests/*.test.mjs`.

## One-time setup

Nothing below has been done yet. Order matters: the release job verifies the published files through the website, so the site must be live before the first tag.

1. **GitHub repository.** Review the tree before the first push to a public repo. `reference/`, `target*/`, `build/`, `dist/`, `Local.xcconfig` and databases are gitignored; check that no screenshots or logs with real chats are committed. Add a `LICENSE` file matching `license = "MIT"` in `Cargo.toml` (or change both). Then:
   ```sh
   gh repo create talhabalaj/whatsrusty --public --source ~/personal/whatsapp-gpui --push
   ```
   Under Settings → Actions → General, keep the default read-only `GITHUB_TOKEN`; the publish job asks for `contents: write` itself.
2. **Sparkle key backup and secrets.** The key pair already exists in the login Keychain (SIGNING.md). With `generate_keys` from the Sparkle 2.10.0 tools:
   ```sh
   generate_keys --account me.balaj.WhatsRusty -x /tmp/whatsrusty-sparkle.key
   # 1Password: Personal → new item "WhatsRusty - Sparkle update signing" with the file contents
   gh secret set SPARKLE_PRIVATE_KEY --repo talhabalaj/whatsrusty < /tmp/whatsrusty-sparkle.key
   rm -P /tmp/whatsrusty-sparkle.key
   gh variable set SPARKLE_PUBLIC_KEY --repo talhabalaj/whatsrusty --body '9q0PzQIIxhsgZT877jGgyo11cQVAjhI1KsygiZExj4I='
   ```
3. **App Store Connect API key.** In App Store Connect → Users and Access → Integrations → Team Keys, reuse the Pufflet CI key (same team) or create one with the same role; it must start Xcode Cloud builds and read their artifacts. Save it in 1Password as **WhatsRusty - App Store Connect CI**, then:
   ```sh
   gh secret set ASC_KEY_ID --repo talhabalaj/whatsrusty --body '<key id>'
   gh secret set ASC_ISSUER_ID --repo talhabalaj/whatsrusty --body '<issuer id>'
   gh secret set ASC_PRIVATE_KEY --repo talhabalaj/whatsrusty < AuthKey_<key id>.p8   # real newlines
   xcrun notarytool store-credentials WhatsRusty --key AuthKey_<key id>.p8 --key-id '<key id>' --issuer '<issuer id>'   # for packaging/release.sh
   ```
4. **Xcode Cloud workflow.** Open `WhatsRusty.xcodeproj` in Xcode 27 and choose **Integrate → Create Workflow**. Grant Xcode Cloud access to `talhabalaj/whatsrusty` only. Create the app/product record for `me.balaj.WhatsRusty` if Apple asks; it can be created without App Store distribution. Configure the workflow:
   - Name **WhatsRusty Release**. Environment: Xcode 27 (latest release), macOS latest, **Clean** builds on.
   - Start conditions: delete the default Branch Changes condition. Add **Manual Start → Tags**, any tag, so the GitHub API can start `vX.Y.Z` builds. No automatic conditions, or GitHub's start would duplicate the build.
   - Actions: one **Archive – macOS**, scheme **WhatsRusty**, Distribution Preparation **None**, Required to pass on. No Test action.
   - Post-actions: **Notarize** (Archive – macOS).
   - Save, then **Lock** the workflow (Manage Workflows → … → Restrict Editing). Keep it enabled.
   - The build number counter starts at 1 and must never decrease.
   - If Xcode commits anything (it normally only stores configuration in App Store Connect), push that commit.
   - Find the workflow ID: App Store Connect → Xcode Cloud → WhatsRusty Release; the URL ends in `/workflows/<ID>`. Or run `node scripts/ci/xcode-cloud.mjs`'s preflight through the release job. Then:
   ```sh
   gh variable set XCODE_CLOUD_WORKFLOW_ID --repo talhabalaj/whatsrusty --body '<workflow id>'
   ```
5. **Cloudflare.** Create an API token (My Profile → API Tokens). Account permissions: Workers Scripts Write, Account Settings Read. Zone permissions for `balaj.me` only: Workers Routes Write, Zone Read. Save it in 1Password as **WhatsRusty - Cloudflare CI token**, or reuse the Pufflet token if it is scoped the same way. Then:
   ```sh
   gh secret set CLOUDFLARE_API_TOKEN --repo talhabalaj/whatsrusty
   gh variable set CLOUDFLARE_ACCOUNT_ID --repo talhabalaj/whatsrusty --body 'dd0a87415ee63e34b183118e8bdd13c8'
   gh variable set CLOUDFLARE_CI_ENABLED --repo talhabalaj/whatsrusty --body true
   ```
6. **DNS and first site deploy.** `site/cloudflare.config.ts` declares the custom domain `whatsrusty.apps.balaj.me` on Worker `whatsrusty-site`. Deploying creates the proxied DNS record and certificate in the `balaj.me` zone, as for `pufflet.apps.balaj.me`; there should be no existing `whatsapp.apps` record. Run **Deploy website** (`site-deploy.yml`, workflow_dispatch) or `npm ci --prefix site && npm run deploy --prefix site` once. Check that `https://whatsrusty.apps.balaj.me/` loads. `/appcast.xml` returns 404 "The first release is being prepared." until the first release.

## Publish

1. Commit, push `main` and wait for CI to pass.
2. Tag the exact audited commit with a new, unused numeric version and push it:
   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   ```
3. `release.yml` runs `cargo test`, then starts the Xcode Cloud workflow for that tag and commit through the ASC API. It waits up to 2 hours and downloads only Apple's stapled notarized artifact. On macOS, `package-release.sh` verifies the following before packaging:
   - Developer ID authority, team, hardened runtime and timestamp, including Sparkle's helpers
   - bundle id and arm64
   - Sparkle key and feed
   - `audio-input` present; no sandbox or `get-task-allow`
   - build number equal to the Cloud counter, and version equal to the tag
   - stapled ticket and Gatekeeper
4. It then signs `WhatsRusty-<version>-<build>.zip` and `appcast.xml` with the Sparkle key, and writes `manifest.json` and the checksum.
5. `publish-release.mjs` creates a draft release for the tag and uploads the assets without overwriting (matching retries are allowed). It publishes with `--latest=false` and verifies anonymous downloads and the website's feed. It rechecks against downgrades, then marks the release latest as the final promotion. The Worker serves `/appcast.xml`, `/downloads/manifest.json`, `/downloads/latest.zip` and `/downloads/WhatsRusty-<version>-<build>.zip` from GitHub.

Publication is serialized (`concurrency: whatsrusty-app-production`). An interrupted publication can resume via workflow_dispatch with the original tag and the completed `cloud_build_id`; select `main` as the workflow ref to use repaired tooling without rebuilding the app. Only the publish job receives `contents: write` and the ephemeral `GH_TOKEN`.

Only describe notarization as complete after a successful Apple artifact and the stapler/Gatekeeper checks. Only describe automatic updating as tested after an actual Sparkle upgrade between two signed releases.

## First-run notes for the existing install

The first Release launch renames `~/Library/Application Support/WhatsApp GPUI` and `~/Library/Logs/WhatsApp GPUI` to `WhatsRusty`, keeping the linked session. Quit the old `WhatsApp GPUI.app` first and delete it afterwards. The phone most likely keeps showing the device as "WhatsApp GPUI" until it is relinked: the device name is sent when pairing.
