# WhatsApp GPUI

A native macOS WhatsApp client in Rust with an iMessage-style UI.

- **UI:** [GPUI](https://www.gpui.rs/) (Zed's GPU-accelerated UI framework) via [GPUI Kit](https://gpui-kit.com) 0.8.
- **Protocol:** [whatsapp-rust](https://github.com/jlucaso1/whatsapp-rust) 0.7 (multi-device WhatsApp Web protocol, Signal E2EE).
- **Local store:** SQLite for chats, messages and contacts (whatsapp-rust keeps only the session).

> ⚠️ Unofficial client. Using it may violate WhatsApp's Terms of Service and can get the account banned.
> Prefer a secondary number while this is experimental.

## Layout

```
crates/
  wa-backend/   whatsapp-rust + SQLite store; talks to the UI over channels (types.rs is the contract)
  app/          GPUI app: sidebar, transcript, composer, QR linking screen, mock backend
reference/      (gitignored) Baileys + whatsapp-rust sources, for reading only
```

## Run

```sh
# UI with fake data, no WhatsApp account needed
WA_MOCK=1 cargo run -p whatsapp-gpui

# Real account: shows a QR, scan it from WhatsApp → Settings → Linked Devices
cargo run -p whatsapp-gpui

# Backend only, QR in the terminal
cargo run -p wa-backend --example cli
```

Data lives in `~/Library/Application Support/WhatsApp GPUI/`. Delete that folder to unlink and start fresh.
