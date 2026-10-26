//! @mentions: the `@query` being typed in a group, the participant list it suggests,
//! and the two forms of a mention: `@Name` in the composer and bubbles, `@<digits>` (plus
//! the JID in `mentionedJid`) on the wire.

use std::ops::Range;

use wa_backend::{Mention, Participant};

/// The `@query` being typed at the end of `text` (after start of text or whitespace),
/// without the `@`. Empty right after typing `@`.
pub fn trailing_query(text: &str) -> Option<&str> {
    let start = text.rfind('@')?;
    let query = &text[start + 1..];
    let before_ok = text[..start].chars().next_back().is_none_or(char::is_whitespace);
    let valid = query.chars().count() <= 30 && !query.contains(char::is_whitespace);
    (before_ok && valid).then_some(query)
}

/// Participants matching `query`: a word of their name starts with it, or their number
/// contains it. Keeps the given order (admins first, then by name).
pub fn suggest(participants: &[Participant], query: &str, limit: usize) -> Vec<Participant> {
    let q = query.to_lowercase();
    participants
        .iter()
        .filter(|p| {
            q.is_empty()
                || p.name.to_lowercase().split_whitespace().any(|w| w.starts_with(&q))
                || p.name.to_lowercase().starts_with(&q)
                || (q.chars().all(|c| c.is_ascii_digit()) && user_of(&p.jid).contains(&q))
        })
        .take(limit)
        .cloned()
        .collect()
}

/// Replace the trailing `@query` with `@Name `.
pub fn apply(text: &str, name: &str) -> String {
    let base = match trailing_query(text) {
        Some(q) => &text[..text.len() - q.len() - 1],
        None => text,
    };
    format!("{base}@{name} ")
}

/// The digits (user part) of a JID.
pub fn user_of(jid: &str) -> &str {
    jid.split_once('@').map_or(jid, |(u, _)| u)
}

/// The text as sent: each `@Name` still in it becomes `@<digits>`; returns the JIDs
/// mentioned. Longer names first, so "@Sara Ali" wins over "@Sara".
pub fn to_wire(text: &str, mentions: &[(String, String)]) -> (String, Vec<String>) {
    let mut sorted: Vec<&(String, String)> = mentions.iter().collect();
    sorted.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    let mut out = text.to_string();
    let mut jids = Vec::new();
    for (name, jid) in sorted {
        let tag = format!("@{name}");
        if out.contains(&tag) {
            out = out.replace(&tag, &format!("@{}", user_of(jid)));
            if !jids.contains(jid) {
                jids.push(jid.clone());
            }
        }
    }
    (out, jids)
}

/// The text as shown: known `@<digits>` replaced by `@Name`, and where those are.
pub fn display(text: &str, mentions: &[Mention]) -> (String, Vec<Range<usize>>) {
    if mentions.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let mut out = String::with_capacity(text.len());
    let mut ranges = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        let (before, tail) = rest.split_at(at);
        out.push_str(before);
        let digits = tail[1..].find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len() - 1);
        let token = &tail[1..1 + digits];
        let word_before = out.chars().next_back().is_some_and(char::is_alphanumeric);
        match mentions.iter().find(|m| m.user == token).filter(|_| !word_before && !token.is_empty()) {
            Some(m) => {
                let start = out.len();
                out.push('@');
                out.push_str(&m.name);
                ranges.push(start..out.len());
                rest = &tail[1 + digits..];
            }
            None => {
                out.push('@');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    (out, ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, jid: &str) -> Participant {
        Participant { jid: jid.into(), name: name.into(), is_admin: false, is_me: false }
    }

    #[test]
    fn query_after_at() {
        assert_eq!(trailing_query("hey @"), Some(""));
        assert_eq!(trailing_query("@sa"), Some("sa"));
        assert_eq!(trailing_query("mail me@home"), None);
        assert_eq!(trailing_query("@sara ali"), None);
        assert_eq!(trailing_query("no at"), None);
    }

    #[test]
    fn suggests_by_word_or_number() {
        let people = [p("Omar Farooq", "15551110011@s.whatsapp.net"), p("Sara", "15551110012@s.whatsapp.net")];
        let names = |q| suggest(&people, q, 8).into_iter().map(|p| p.name).collect::<Vec<_>>();
        assert_eq!(names(""), ["Omar Farooq", "Sara"]);
        assert_eq!(names("far"), ["Omar Farooq"]);
        assert_eq!(names("S"), ["Sara"]);
        assert_eq!(names("1110012"), ["Sara"]);
        assert!(names("zz").is_empty());
    }

    #[test]
    fn wire_and_display_round_trip() {
        assert_eq!(apply("hi @sa", "Sara Ali"), "hi @Sara Ali ");
        let mentions = vec![
            ("Sara".to_string(), "15551110012@s.whatsapp.net".to_string()),
            ("Sara Ali".to_string(), "900000000013@lid".to_string()),
        ];
        let (wire, jids) = to_wire("@Sara Ali and @Sara, @Sara!", &mentions);
        assert_eq!(wire, "@900000000013 and @15551110012, @15551110012!");
        assert_eq!(jids, ["900000000013@lid", "15551110012@s.whatsapp.net"]);

        let resolved = [
            Mention { user: "900000000013".into(), jid: "900000000013@lid".into(), name: "Sara Ali".into() },
            Mention { user: "15551110012".into(), jid: "15551110012@s.whatsapp.net".into(), name: "Sara".into() },
        ];
        let (shown, ranges) = display(&wire, &resolved);
        assert_eq!(shown, "@Sara Ali and @Sara, @Sara!");
        assert_eq!(ranges.iter().map(|r| &shown[r.clone()]).collect::<Vec<_>>(), ["@Sara Ali", "@Sara", "@Sara"]);
        // Unknown numbers and e-mail addresses stay as they are.
        assert_eq!(display("a@15551110012 @123", &resolved), ("a@15551110012 @123".to_string(), Vec::new()));
    }
}
