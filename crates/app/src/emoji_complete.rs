//! `:shortcode` emoji completion, like WhatsApp Web: typing `:smi` suggests 😄 😊 …,
//! and a complete `:smile:` turns into the emoji.

/// The `:query` being typed at the end of `text` (after start of text or whitespace),
/// returned without the colon. Needs at least two characters, like WhatsApp Web.
pub fn trailing_query(text: &str) -> Option<&str> {
    let start = text.rfind(':')?;
    let query = &text[start + 1..];
    let before_ok = text[..start].chars().next_back().is_none_or(char::is_whitespace);
    let valid = query.len() >= 2 && query.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-'));
    (before_ok && valid).then_some(query)
}

/// `text` ending in a complete `:shortcode:` with that shortcode replaced by its emoji.
pub fn expand_complete(text: &str) -> Option<String> {
    let body = text.strip_suffix(':')?;
    let query = trailing_query(body)?;
    let emoji = emojis::get_by_shortcode(&query.to_ascii_lowercase())?;
    Some(format!("{}{}", &body[..body.len() - query.len() - 1], emoji.as_str()))
}

/// Replace the trailing `:query` with `emoji`.
pub fn apply(text: &str, emoji: &str) -> String {
    match trailing_query(text) {
        Some(q) => format!("{}{emoji}", &text[..text.len() - q.len() - 1]),
        None => format!("{text}{emoji}"),
    }
}

#[derive(Clone)]
pub struct Suggestion {
    pub emoji: &'static str,
    pub shortcode: &'static str,
    /// The keyword that matched, when it wasn't the shortcode (shown as a hint).
    pub keyword: Option<&'static str>,
}

/// Chat slang CLDR doesn't cover but WhatsApp users type (":lol" → 😂).
const SLANG: &[(&str, &[&str])] = &[
    ("😂", &["lol", "haha", "lmao", "rofl"]),
    ("🤣", &["lol", "rofl", "lmao", "dying"]),
    ("😆", &["haha", "xd"]),
    ("👍", &["ok", "okay", "yes", "yep", "like", "agree"]),
    ("👌", &["ok", "okay", "perfect", "nice"]),
    ("👎", &["no", "nope", "dislike"]),
    ("❤️", &["love", "luv", "heart"]),
    ("😍", &["love", "crush"]),
    ("🙏", &["thanks", "thx", "ty", "please", "pls", "pray"]),
    ("😢", &["sad", "cry"]),
    ("😭", &["sad", "cry", "sob"]),
    ("😱", &["omg", "shock", "scared"]),
    ("😮", &["wow", "omg"]),
    ("😎", &["cool", "chill"]),
    ("😘", &["kiss", "muah"]),
    ("🤗", &["hug"]),
    ("😡", &["angry", "mad"]),
    ("🤔", &["hmm", "think"]),
    ("🙄", &["eyeroll", "whatever"]),
    ("😅", &["phew", "oops"]),
    ("🥳", &["party", "bday", "birthday"]),
    ("🎂", &["bday", "birthday"]),
    ("💯", &["100", "facts"]),
];

/// CLDR English keywords and name for an emoji ("laugh", "tear", "joy"… for 😂), plus
/// chat slang from [`SLANG`].
/// Each keyword comes with a tie-break rank: slang entries by their position in
/// [`SLANG`] (so ":lol" puts 😂 before 🤣), CLDR words after them by length.
fn keywords(glyph: &str) -> impl Iterator<Item = (&'static str, usize)> {
    let ann = emoji::lookup_by_glyph::lookup(glyph)
        .and_then(|e| e.annotations.iter().find(|a| a.lang == "en"));
    let slang = SLANG
        .iter()
        .enumerate()
        .filter(move |(_, (g, _))| *g == glyph)
        .flat_map(|(i, (_, words))| words.iter().map(move |w| (*w, i)));
    slang.chain(
        ann.into_iter()
            .flat_map(|a| a.tts.into_iter().chain(a.keywords.iter().copied()))
            .map(|w| (w, SLANG.len() + w.len())),
    )
}

/// Best matches for `query`, WhatsApp-Web style: every emoji is reachable by its
/// shortcodes *and* its keywords. Tiers: exact shortcode, exact keyword, shortcode prefix,
/// keyword prefix (any word), shortcode substring; shorter matches first within a tier.
pub fn suggest(query: &str, limit: usize) -> Vec<Suggestion> {
    let q = query.to_ascii_lowercase().replace('_', " ");
    let q_code = query.to_ascii_lowercase();
    let mut scored: Vec<(u8, usize, Suggestion)> = Vec::new();
    for e in emojis::iter() {
        let Some(code) = e.shortcode() else { continue };
        let by_code = e
            .shortcodes()
            .filter_map(|sc| {
                let tier = if sc == q_code {
                    0
                } else if sc.starts_with(&q_code) {
                    2
                } else if sc.contains(&q_code) {
                    4
                } else {
                    return None;
                };
                Some((tier, sc.len(), Suggestion { emoji: e.as_str(), shortcode: sc, keyword: None }))
            })
            .min_by_key(|(t, l, _)| (*t, *l));
        let by_keyword = keywords(e.as_str())
            .filter_map(|(kw, rank)| {
                let lower = kw.to_lowercase();
                let hit = lower == q || lower.starts_with(&q) || lower.split([' ', '-']).any(|w| w.starts_with(&q));
                hit.then(|| {
                    let tier = if lower == q { 1 } else { 3 };
                    (tier, rank, Suggestion { emoji: e.as_str(), shortcode: code, keyword: Some(kw) })
                })
            })
            .min_by_key(|(t, l, _)| (*t, *l));
        if let Some(best) = [by_code, by_keyword].into_iter().flatten().min_by_key(|(t, l, _)| (*t, *l)) {
            scored.push(best);
        }
    }
    scored.sort_by_key(|(tier, len, _)| (*tier, *len));
    scored.into_iter().take(limit).map(|(_, _, s)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_trailing_query() {
        assert_eq!(trailing_query("hi :smi"), Some("smi"));
        assert_eq!(trailing_query(":jo"), Some("jo"));
        assert_eq!(trailing_query("hi :s"), None);
        assert_eq!(trailing_query("time 10:30"), None);
        assert_eq!(trailing_query("hi :smile done"), None);
    }

    #[test]
    fn expands_and_suggests() {
        assert_eq!(expand_complete("ok :thumbsup:").as_deref(), Some("ok 👍"));
        assert_eq!(expand_complete("ok :nope_not_real:"), None);
        assert_eq!(apply("lol :jo", "😂"), "lol 😂");
        let s = suggest("joy", 5);
        assert_eq!(s.first().map(|s| s.emoji), Some("😂"));
        assert!(!suggest("heart", 6).is_empty());
        // Keywords reach emojis whose shortcode doesn't match.
        assert!(suggest("laugh", 8).iter().any(|s| s.emoji == "😂"));
        assert_eq!(suggest("lol", 8).first().map(|s| s.emoji), Some("😂"));
        assert!(suggest("thanks", 8).iter().any(|s| s.emoji == "🙏"));
        assert!(suggest("party", 8).iter().any(|s| s.emoji == "🎉"));
    }
}
