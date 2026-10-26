//! Pair and watch the backend from a terminal.
//!
//! ```sh
//! cargo run -p wa-backend --example cli
//! WA_CLI_SECONDS=20 cargo run -p wa-backend --example cli   # exit (gracefully) after 20 s
//! ```
//!
//! Data lives in `/tmp/wa-backend-test`; delete it to pair from scratch.

use std::time::{Duration, Instant};

use qrcode::QrCode;
use qrcode::render::unicode;
use wa_backend::{BackendConfig, Command, ConnectionState, Event};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,wa_backend=info"))
        .init();
    let handle = wa_backend::spawn(BackendConfig {
        data_dir: "/tmp/wa-backend-test".into(),
        device_name: "WhatsApp GPUI (CLI)".into(),
    })?;

    let deadline = std::env::var("WA_CLI_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(|secs| Instant::now() + Duration::from_secs(secs));
    let mut shutdown_sent = false;

    loop {
        let timeout = match deadline {
            Some(d) if !shutdown_sent => d.saturating_duration_since(Instant::now()),
            _ => Duration::from_secs(3600),
        };
        let event = match handle.events.recv_timeout(timeout) {
            Ok(event) => event,
            Err(flume::RecvTimeoutError::Timeout) => {
                println!("== time is up, sending Shutdown");
                handle.commands.send(Command::Shutdown)?;
                shutdown_sent = true;
                continue;
            }
            // The backend thread exited and dropped its sender.
            Err(flume::RecvTimeoutError::Disconnected) => break,
        };
        match event {
            Event::Connection(ConnectionState::AwaitingQr { code, valid_for_secs }) => {
                let qr = QrCode::new(code.as_bytes())?;
                let art = qr.render::<unicode::Dense1x2>().quiet_zone(true).build();
                println!("\n{art}\nScan with WhatsApp > Linked devices (valid for {valid_for_secs} s)\n");
            }
            Event::Chats(chats) => {
                println!("== {} chats", chats.len());
                for c in chats.iter().take(15) {
                    let preview = c.last_message.as_ref().map_or("", |m| m.text.as_str());
                    let preview: String = preview.chars().take(50).collect();
                    let flags = format!(
                        "{}{}{}",
                        if c.pinned { "📌" } else { "" },
                        if c.muted { "🔇" } else { "" },
                        if c.archived { "🗄" } else { "" }
                    );
                    println!("  [{:>3}] {flags}{:<30} {preview}", c.unread_count, c.name);
                }
            }
            Event::MessageUpserted(m) => println!("== message {m:?}"),
            other => println!("== {other:?}"),
        }
    }
    println!("== backend stopped");
    Ok(())
}
