//! Pure helpers for `Command::FetchProfile`: phone formatting and group member lists.

use crate::types::Participant;

/// Country calling codes that are 1 or 2 digits long (ITU-T E.164); the rest are 3.
const CC_1: &[&str] = &["1", "7"];
const CC_2: &[&str] = &[
    "20", "27", "30", "31", "32", "33", "34", "36", "39", "40", "41", "43", "44", "45", "46", "47", "48", "49", "51",
    "52", "53", "54", "55", "56", "57", "58", "60", "61", "62", "63", "64", "65", "66", "81", "82", "84", "86", "90",
    "91", "92", "93", "94", "95", "98",
];

/// "+92 300 1234567" from the digits of an international number ("923001234567").
/// North American numbers get the usual 3-3-4 grouping; elsewhere the national number
/// is split after its first 3 digits (operator / area code), which reads right for most
/// countries without a full numbering-plan database.
pub fn format_phone(digits: &str) -> String {
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return digits.to_owned();
    }
    let cc_len = if CC_1.iter().any(|cc| digits.starts_with(cc)) {
        1
    } else if digits.len() >= 2 && CC_2.contains(&&digits[..2]) {
        2
    } else {
        3
    };
    if digits.len() <= cc_len + 4 {
        return format!("+{digits}");
    }
    let (cc, national) = digits.split_at(cc_len);
    let groups: Vec<&str> = if cc == "1" && national.len() == 10 {
        vec![&national[..3], &national[3..6], &national[6..]]
    } else if national.len() > 7 {
        vec![&national[..3], &national[3..]]
    } else {
        vec![national]
    };
    format!("+{cc} {}", groups.join(" "))
}

/// The digits of a phone-number JID (`923001234567@s.whatsapp.net`, device parts
/// ignored); `None` for LIDs, groups etc.
pub fn pn_digits(jid: &str) -> Option<&str> {
    let user = jid.strip_suffix("@s.whatsapp.net")?;
    let user = user.split([':', '.']).next()?;
    (!user.is_empty() && user.chars().all(|c| c.is_ascii_digit())).then_some(user)
}

/// Display phone for a JID, if it is (or is known to be) a phone number.
pub fn display_phone(jid: &str) -> Option<String> {
    pn_digits(jid).map(format_phone)
}

/// Admins first, then by name (case-insensitive), then by JID for a stable order.
pub fn sort_participants(participants: &mut [Participant]) {
    participants.sort_by(|a, b| {
        b.is_admin
            .cmp(&a.is_admin)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.jid.cmp(&b.jid))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_phones() {
        assert_eq!(format_phone("923001234567"), "+92 300 1234567");
        assert_eq!(format_phone("15551234567"), "+1 555 123 4567");
        assert_eq!(format_phone("447911123456"), "+44 791 1123456");
        assert_eq!(format_phone("971501234567"), "+971 501 234567");
        assert_eq!(format_phone("79161234567"), "+7 916 1234567");
        assert_eq!(format_phone("3531234567"), "+353 1234567");
        assert_eq!(format_phone("12345"), "+12345");
        assert_eq!(format_phone(""), "");
        assert_eq!(format_phone("12ab"), "12ab");
    }

    #[test]
    fn phones_only_for_pn_jids() {
        assert_eq!(display_phone("923001234567@s.whatsapp.net").as_deref(), Some("+92 300 1234567"));
        assert_eq!(display_phone("923001234567:12@s.whatsapp.net").as_deref(), Some("+92 300 1234567"));
        assert_eq!(display_phone("100000012345678@lid"), None);
        assert_eq!(display_phone("120363040237990503@g.us"), None);
    }

    fn p(jid: &str, name: &str, is_admin: bool) -> Participant {
        Participant { jid: jid.into(), name: name.into(), is_admin, is_me: false }
    }

    #[test]
    fn admins_first_then_by_name() {
        let mut list = vec![
            p("1@s.whatsapp.net", "zed", false),
            p("2@s.whatsapp.net", "Bob", true),
            p("3@s.whatsapp.net", "alice", false),
            p("4@s.whatsapp.net", "Amy", true),
            p("5@s.whatsapp.net", "Alice", false),
        ];
        sort_participants(&mut list);
        let order: Vec<&str> = list.iter().map(|p| p.jid.as_str()).collect();
        assert_eq!(order, ["4@s.whatsapp.net", "2@s.whatsapp.net", "3@s.whatsapp.net", "5@s.whatsapp.net", "1@s.whatsapp.net"]);
    }
}
