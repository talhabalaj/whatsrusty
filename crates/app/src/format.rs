//! Date/time formatting in the style of Messages.app.

use chrono::{DateTime, Datelike, Local, TimeZone};
use wa_backend::Timestamp;

fn local(ts: Timestamp) -> DateTime<Local> {
    Local.timestamp_opt(ts, 0).single().unwrap_or_else(Local::now)
}

/// Sidebar timestamp: "9:41 AM", "Yesterday", "Tuesday", "10/12/26".
pub fn sidebar_time(ts: Timestamp) -> String {
    let t = local(ts);
    let now = Local::now();
    let days = (now.date_naive() - t.date_naive()).num_days();
    match days {
        0 => t.format("%-I:%M %p").to_string(),
        1 => "Yesterday".into(),
        2..=6 => t.format("%A").to_string(),
        _ => t.format("%-m/%-d/%y").to_string(),
    }
}

/// Centered separator in the transcript: "Today 9:41 AM", "Yesterday 8:02 PM",
/// "Tuesday 7:15 AM", "Mon, Oct 12 at 9:41 AM".
pub fn transcript_time(ts: Timestamp) -> String {
    let t = local(ts);
    let now = Local::now();
    let days = (now.date_naive() - t.date_naive()).num_days();
    let time = t.format("%-I:%M %p");
    match days {
        0 => format!("Today {time}"),
        1 => format!("Yesterday {time}"),
        2..=6 => format!("{} {time}", t.format("%A")),
        _ if t.year() == now.year() => format!("{} at {time}", t.format("%a, %b %-d")),
        _ => format!("{} at {time}", t.format("%b %-d, %Y")),
    }
}

pub fn short_time(ts: Timestamp) -> String {
    local(ts).format("%-I:%M %p").to_string()
}

/// Up to two initials for avatars ("Jane Appleseed" → "JA", "+1 555…" → "#").
pub fn initials(name: &str) -> String {
    let letters: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphabetic()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() { "#".into() } else { letters }
}
