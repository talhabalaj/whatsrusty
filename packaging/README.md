# Packaging (macOS)

| File | Purpose |
| --- | --- |
| `icon/make-icon-composer.py` | Writes `icon/AppIcon.icon`, the Icon Composer document (our bubble + checks as layers; background, squircle, glass and shadows are Icon Composer's). `build-app.sh` compiles it with `actool` into `Assets.car` + `AppIcon.icns`. Preview: `icon/preview/composer-*.png` |
| `icon/render.sh` | Flat fallback icon (used only when Xcode's `actool` is missing): regenerates `icon/AppIcon.svg` → `icon/png/*.png` → `icon/AppIcon.icns` (needs `rsvg-convert`: `brew install librsvg`) |
| `build-app.sh` | `cargo build --release` and assembles an unsigned `dist/WhatsApp GPUI.app` (`--universal` for arm64 + x86_64) |
| `entitlements.plist` | Hardened-runtime entitlements (network client, audio input; no sandbox, see the comment inside) |
| `release.sh` | Build → Developer ID codesign → verify → notarize + staple app → DMG → sign, notarize + staple DMG |

## One-time setup: notarization credentials

```sh
# Apple ID + app-specific password (https://account.apple.com > Sign-In and Security > App-Specific Passwords)
xcrun notarytool store-credentials whatsapp-gpui-notary \
  --apple-id "<Apple ID email>" --team-id W7TT249CUU --password "<app-specific password>"
```

(Or use an App Store Connect API key: `--key AuthKey_XXX.p8 --key-id XXX --issuer <uuid>`.)

## Release

```sh
packaging/release.sh                 # full pipeline → dist/WhatsApp-GPUI-<version>.dmg
packaging/release.sh --no-notarize   # sign + DMG only (local testing)
packaging/release.sh --skip-build    # re-sign/re-package the existing dist/ app
```

Env overrides: `SIGN_IDENTITY` (default: SHA-1 of the first `Developer ID Application … (W7TT249CUU)` identity),
`NOTARY_PROFILE` (default `whatsapp-gpui-notary`), `BUILD_NUMBER` (`CFBundleVersion`, default: Cargo version).

## Notes

- Bundle id `com.talhabalaj.whatsapp-gpui`. Running from the bundle is what enables GPUI system
  notifications (`UNUserNotificationCenter` needs a bundle identifier); `cargo run` builds log
  "system notifications disabled".
- Mock mode from the bundle: `WA_MOCK=1 "dist/WhatsApp GPUI.app/Contents/MacOS/whatsapp-gpui"`
  (env vars don't pass through `open`).
- Data lives in `~/Library/Application Support/WhatsApp GPUI` for both dev and bundled builds.
- Hardened runtime blocks the microphone unless `com.apple.security.device.audio-input` is present
  *and* `NSMicrophoneUsageDescription` is in Info.plist; both are already set.
