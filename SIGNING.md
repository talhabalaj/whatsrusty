# Signing and local data

Release uses `me.balaj.WhatsRusty` ("WhatsRusty"); Debug uses `me.balaj.WhatsRusty.dev` ("WhatsRusty Dev"). The phone lists the linked device under the app name, so Debug and Release sessions are told apart there too.

Application Support and Logs use `WhatsRusty` for Release and `WhatsRustyDev` for Debug. On first launch, a Release build renames the legacy `WhatsApp GPUI` folders (`~/Library/Application Support/WhatsApp GPUI`, `~/Library/Logs/WhatsApp GPUI`) when the destination is absent. A rename keeps the contents, the 700/600 permissions and the linked session, so the phone does not need to be relinked. If the destination already exists, nothing moves. Symlinks are never followed. Mock mode (`WA_MOCK=1`) and Debug builds never migrate. Quit any old `WhatsApp GPUI.app` before the first WhatsRusty launch: the two have different bundle ids and would otherwise open the same moved databases.

The app has no Keychain items; the WhatsApp session lives in `session.db` inside the data folder (mode 600). Stable Developer ID signing is still what keeps macOS privacy grants (microphone, notifications, Launch at Login) attached to the app across updates.

`Local.xcconfig` (gitignored) may select a local signing identity, for example:

```
CODE_SIGN_STYLE[config=Release] = Manual
CODE_SIGN_IDENTITY[config=Release] = Developer ID Application: Talha Balaj (W7TT249CUU)
```

Signing and notarization are distinct. `packaging/release.sh` signs with the Developer ID identity in the keychain (the login Keychain on a Mac, a temporary keychain in CI) and notarizes with an App Store Connect API key. See CI.md for the release checks.

Entitlements (`packaging/entitlements.plist`): hardened runtime, `com.apple.security.device.audio-input` (voice messages; enforced by the hardened runtime) and `com.apple.security.network.client`. No App Sandbox, no `get-task-allow`, no library-validation or JIT exceptions. Xcode re-signs Sparkle's nested helpers (Autoupdate, Updater.app, Downloader.xpc, Installer.xpc) with the same team and hardened runtime; the release verification checks each of them.

## Sparkle keys

The EdDSA key pair was created with Sparkle 2.10.0's `generate_keys --account me.balaj.WhatsRusty`. The private key is in the login Keychain (service `https://sparkle-project.org`, account `me.balaj.WhatsRusty`); only the public key `9q0PzQIIxhsgZT877jGgyo11cQVAjhI1KsygiZExj4I=` is committed (`SUPublicEDKey` in `packaging/Info.plist`). The app requires a signed feed (`SURequireSignedFeed`) and verifies archives before extraction (`SUVerifyUpdateBeforeExtraction`).

`generate_keys` ships in the Sparkle package artifacts (after any Xcode build: `~/Library/Developer/Xcode/DerivedData/WhatsRusty-*/SourcePackages/artifacts/sparkle/Sparkle/bin/`) and in the [2.10.0 release tarball](https://github.com/sparkle-project/Sparkle/releases/tag/2.10.0).

```sh
generate_keys --account me.balaj.WhatsRusty -p      # print the public key
generate_keys --account me.balaj.WhatsRusty -x key  # export the private key (backup / CI secret)
```

Losing the private key means existing installs can no longer verify updates; rotating it requires a release signed with the old key that ships the new public key. Keep the 1Password copy.

## Credentials and their sources

No private credentials are committed to this repository. A normal local build uses the selected Apple signing identity in macOS Keychain. App builds do not need Cloudflare, ASC, or Sparkle private keys.

| Purpose | Local source | CI source |
| --- | --- | --- |
| Developer ID code signing | Certificate and private key in macOS Keychain | `DEVELOPER_ID_P12` (base64 .p12 of that one identity) and `DEVELOPER_ID_P12_PASSWORD` Actions secrets, imported into a temporary keychain per run |
| Apple notarization/API | notarytool profile `WhatsRusty`, from Personal 1Password **Pufflet - App Store Connect CI** (team-wide key, shared with Pufflet) | `ASC_PRIVATE_KEY`, `ASC_KEY_ID`, `ASC_ISSUER_ID` Actions secrets |
| Sparkle archive/feed signing | Keychain service `https://sparkle-project.org`, account `me.balaj.WhatsRusty`; recovery in Personal 1Password **WhatsRusty - Sparkle update signing** | `SPARKLE_PRIVATE_KEY` Actions secret |
| Website deployment | Personal 1Password **Pufflet - Cloudflare CI token** (account- and `balaj.me`-scoped, shared with Pufflet), or the interactive `cf` login | `CLOUDFLARE_API_TOKEN` Actions secret |
| GitHub release publishing | Existing authenticated `gh` session | Ephemeral `github.token` scoped to the release job with `contents: write` |
| WhatsApp session | `session.db` in the app's data folder | Never used by CI or bundled in artifacts |

The updater public key, bundle IDs and Cloudflare account ID are public configuration. Revoking the Developer ID certificate or rotating the shared App Store Connect key or Cloudflare token requires updating these secrets.

https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements
