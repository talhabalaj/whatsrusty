# WhatsRusty

A native macOS WhatsApp client in Rust with an iMessage-style UI. Website and downloads:
[whatsrusty.apps.balaj.me](https://whatsrusty.apps.balaj.me).

- **UI:** [GPUI](https://www.gpui.rs/) (Zed's GPU-accelerated UI framework) via [GPUI Kit](https://gpui-kit.com) 0.8.
- **Protocol:** [whatsapp-rust](https://github.com/jlucaso1/whatsapp-rust) 0.7 (multi-device WhatsApp Web protocol, Signal E2EE).
- **Local store:** SQLite for chats, messages and contacts (whatsapp-rust keeps only the session).
- **Updates:** [Sparkle 2](https://sparkle-project.org) with an EdDSA-signed feed.

> ⚠️ Unofficial client, not affiliated with WhatsApp or Meta. Using it may violate WhatsApp's Terms
> of Service and can get the account banned. Prefer a secondary number while this is experimental.

## Layout

```
crates/
  wa-backend/   whatsapp-rust + SQLite store; talks to the UI over channels (types.rs is the contract)
  app/          GPUI app (cargo package `whatsapp-gpui`): sidebar, transcript, composer, QR linking, mock backend
packaging/      Info.plist, entitlements, icon, Sparkle package, build/release scripts (see packaging/README.md)
project.yml     XcodeGen spec for WhatsRusty.xcodeproj (wraps the cargo build into an app bundle)
ci_scripts/     Xcode Cloud hooks (Rust toolchain install, version stamping)
scripts/ci/     Release verification, Sparkle signing, GitHub Release publishing
site/           whatsrusty.apps.balaj.me (Cloudflare Worker): landing page, /appcast.xml, /downloads/*
reference/      (gitignored) Baileys + whatsapp-rust sources, for reading only
```

## Run

```sh
# UI with fake data, no WhatsApp account needed
WA_MOCK=1 cargo run -p whatsapp-gpui

# Real account (Debug: "WhatsRusty Dev"): shows a QR, scan it from WhatsApp → Settings → Linked Devices
cargo run -p whatsapp-gpui

# Backend only, QR in the terminal
cargo run -p wa-backend --example cli

# App bundle (notifications, Launch at Login and Sparkle need one)
packaging/build-app.sh [--release]
```

Debug data lives in `~/Library/Application Support/WhatsRustyDev/`, release data in
`~/Library/Application Support/WhatsRusty/`. Delete the folder to unlink and start fresh.

## Releases

Push a `vX.Y.Z` tag: Xcode Cloud archives and notarizes, GitHub Actions verifies, Sparkle-signs and
publishes. See [CI.md](CI.md) and [SIGNING.md](SIGNING.md).
