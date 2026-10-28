//! Links in message text: URLs, bare domains and email addresses, found after the text
//! is sanitized (so hidden bidi / invisible characters can't disguise what a link shows)
//! and classified: only http, https and mailto open, and lookalikes ask first.

use std::cell::Cell;
use std::ops::Range;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;
use linkify::{LinkFinder, LinkKind};
use url::{Host, Url};


const TRAILING: &[char] = &['”', '’', '»', '›', '」', '』', '、', '。', '，', '！', '？', '：', '；', '）'];

/// Longest link text we turn into a link.
const MAX_LINK_LEN: usize = 4096;

/// Shown before a suspicious link (text presentation, not the emoji).
pub const WARNING_MARK: &str = "\u{26A0}\u{FE0E}\u{2009}";

/// Why a link may not go where it looks like it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warning {
    /// Non-ASCII (IDN) host: mixed scripts and lookalike letters (Cyrillic "а").
    Lookalike,
    /// `user@host`: the part before "@" is not the destination.
    UserInfo,
    /// Bidi controls or invisible characters were removed from it.
    HiddenChars,
    /// Plain http to a raw IP address.
    RawIp,
    /// The host it opens differs from the host it shows (`\`, percent-encoding, odd IPs).
    HostMismatch,
}

impl Warning {
    pub fn describe(self) -> &'static str {
        match self {
            Warning::Lookalike => "Its address uses non-Latin or lookalike letters.",
            Warning::UserInfo => "The text before “@” is not where it goes.",
            Warning::HiddenChars => "It contained hidden characters that can disguise it.",
            Warning::RawIp => "It goes to a bare IP address without encryption.",
            Warning::HostMismatch => "It goes somewhere other than the address shown.",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    /// The link text in [`Linked::text`].
    pub range: Range<usize>,
    /// The [`WARNING_MARK`] just before it, when suspicious.
    pub mark: Option<Range<usize>>,
    /// What opens: exactly the shown text, serialized (host in ASCII / punycode).
    pub url: Url,
    pub warnings: Vec<Warning>,
    /// Where it really goes: the ASCII host, or the address of a mailto link (domain in
    /// punycode).
    pub destination: String,
}

impl Link {
    pub fn suspicious(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Clickable span: the mark plus the link.
    pub fn hit_range(&self) -> Range<usize> {
        self.mark.as_ref().map_or(self.range.start, |m| m.start)..self.range.end
    }
}

/// Text as shown, with its links.
#[derive(Debug, Default)]
pub struct Linked {
    pub text: String,
    pub links: Vec<Link>,
    /// (input offset, bytes removed, bytes inserted), in order.
    edits: Vec<(usize, usize, usize)>,
}

impl Linked {
    /// An input offset (char boundary) in the output text.
    pub fn map(&self, x: usize) -> usize {
        let mut out = x;
        for &(pos, removed, inserted) in &self.edits {
            if pos >= x {
                break;
            }
            out = out + inserted - removed;
        }
        out
    }

    pub fn map_range(&self, r: &Range<usize>) -> Range<usize> {
        self.map(r.start)..self.map(r.end)
    }
}

/// Bidi controls with a scope (embeddings, overrides, isolates): they can reorder the
/// text after them, so they're removed everywhere.
fn is_scoped_bidi(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// `text` without scoped bidi controls, for one-line places that don't linkify (chat-list
/// previews, notification banners): an RLO there would still reorder what follows.
pub fn strip_bidi(text: &str) -> String {
    text.chars().filter(|c| !is_scoped_bidi(*c)).collect()
}

/// Invisible formatting: harmless in prose (ZWJ joins emoji, ZWNJ is needed in Persian,
/// LRM/RLM in Arabic and Hebrew), so it's only removed inside links.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{17B4}'
            | '\u{17B5}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2060}'..='\u{2064}'
            | '\u{206A}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// Find the links in `shown` (text as displayed, after @mentions became names) and
/// sanitize it. `protected` ranges (mentions: their names are sender-chosen) never
/// become links.
pub fn linkify(shown: &str, protected: &[Range<usize>]) -> Linked {
    let has_hidden = shown.chars().any(|c| is_scoped_bidi(c) || is_invisible(c));
    // Every link needs a "." (domain) or ":" (scheme).
    if !has_hidden && !shown.contains(['.', ':']) {
        return Linked { text: shown.to_string(), ..Default::default() };
    }

    // Detect on a copy with all hidden characters removed; `removed` maps it back.
    let mut clean = String::with_capacity(shown.len());
    let mut removed: Vec<(usize, usize)> = Vec::new(); // (clean pos, input pos)
    let mut removed_len: Vec<usize> = vec![0]; // bytes removed before each entry
    for (i, c) in shown.char_indices() {
        if is_scoped_bidi(c) || is_invisible(c) {
            removed.push((clean.len(), i));
            removed_len.push(removed_len.last().unwrap() + c.len_utf8());
        } else {
            clean.push(c);
        }
    }
    // A start takes the hidden characters right before it along; an end doesn't.
    let to_input = |c: usize, end: bool| c + removed_len[removed.partition_point(|r| if end { r.0 < c } else { r.0 <= c })];

    // URLs with a scheme win over emails ("https://a.com@b.com"), which win over bare
    // domains ("jane.doe" in "jane.doe+wa@x.com").
    let mut emails = LinkFinder::new();
    emails.kinds(&[LinkKind::Email]);
    let mut urls = LinkFinder::new();
    urls.url_must_have_scheme(false).kinds(&[LinkKind::Url]);
    let (schemed, bare): (Vec<_>, Vec<_>) = urls.links(&clean).partition(|l| l.as_str().contains("://"));
    let mut spans: Vec<(Range<usize>, bool)> = schemed.iter().map(|l| (l.start()..l.end(), false)).collect();
    for group in [emails.links(&clean).map(|l| (l.start()..l.end(), true)).collect::<Vec<_>>(), bare.iter().map(|l| (l.start()..l.end(), false)).collect()] {
        let kept: Vec<_> = group.into_iter().filter(|(r, _)| !spans.iter().any(|(s, _)| s.start < r.end && r.start < s.end)).collect();
        spans.extend(kept);
    }
    spans.sort_by_key(|(r, _)| r.start);

    let mut found: Vec<(Range<usize>, Link)> = Vec::new();
    for (mut range, email) in spans {
        // linkify keeps closing quotes and CJK punctuation; they're never part of a link.
        while let Some(c) = clean[range.clone()].chars().next_back().filter(|c| TRAILING.contains(c)) {
            range.end -= c.len_utf8();
        }
        let span = to_input(range.start, false)..to_input(range.end, true);
        if span.is_empty() || protected.iter().any(|p| p.start < span.end && span.start < p.end) {
            continue;
        }
        let Some(mut link) = classify(&clean[range], email) else { continue };
        let first_hidden = removed.partition_point(|r| r.1 <= span.start);
        if removed.get(first_hidden).is_some_and(|r| r.1 < span.end) {
            link.warnings.insert(0, Warning::HiddenChars);
        }
        found.push((span, link));
    }

    // Output: scoped bidi removed everywhere, invisibles removed inside links, and a
    // warning mark before each suspicious link.
    let mut out = Linked { text: String::with_capacity(shown.len() + 8), ..Default::default() };
    let mut pending = found.into_iter().peekable();
    let mut current: Option<(usize, Link)> = None; // (input end, link with its output start)
    for (i, c) in shown.char_indices() {
        if current.is_none() && pending.peek().is_some_and(|(span, _)| span.start == i) {
            let (span, mut link) = pending.next().unwrap();
            if link.suspicious() {
                let start = out.text.len();
                out.text.push_str(WARNING_MARK);
                out.edits.push((i, 0, WARNING_MARK.len()));
                link.mark = Some(start..out.text.len());
            }
            link.range = out.text.len()..out.text.len();
            current = Some((span.end, link));
        }
        if is_scoped_bidi(c) || (current.is_some() && is_invisible(c)) {
            out.edits.push((i, c.len_utf8(), 0));
        } else {
            out.text.push(c);
        }
        if current.as_ref().is_some_and(|(end, _)| i + c.len_utf8() >= *end) {
            let (_, mut link) = current.take().unwrap();
            link.range.end = out.text.len();
            out.links.push(link);
        }
    }
    out
}

/// Parse a detected link: `None` unless it's an http(s) URL / bare domain or an email.
fn classify(text: &str, email: bool) -> Option<Link> {
    if text.len() > MAX_LINK_LEN {
        return None;
    }
    let mut warnings = Vec::new();
    if email || text.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("mailto:")) {
        let address = if email { text } else { &text[7..] };
        let url = Url::parse(&format!("mailto:{address}")).ok()?;
        let (local, domain) = address.split('?').next()?.rsplit_once('@')?;
        if local.is_empty() || domain.is_empty() {
            return None;
        }
        if !domain.is_ascii() {
            warnings.push(Warning::Lookalike);
        }
        let destination = format!("{local}@{}", Host::parse(domain).ok()?);
        return Some(Link { range: 0..0, mark: None, url, warnings, destination });
    }

    let (bare, authority) = match text.find("://") {
        Some(i) => {
            if !matches!(text[..i].to_ascii_lowercase().as_str(), "http" | "https") {
                return None;
            }
            (false, &text[i + 3..])
        }
        None => (true, text),
    };
    let url = if bare { Url::parse(&format!("https://{text}")).ok()? } else { Url::parse(text).ok()? };
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    match url.host()? {
        Host::Domain(domain) => {
            let tld = domain.trim_end_matches('.').rsplit('.').next().unwrap_or("");
            if bare && !(tld.starts_with("xn--") || BARE_TLDS.contains(&tld)) {
                return None;
            }
            if domain.split('.').any(|label| label.starts_with("xn--")) {
                warnings.push(Warning::Lookalike);
            }
        }
        Host::Ipv4(_) | Host::Ipv6(_) => {
            if bare {
                return None;
            }
            if url.scheme() == "http" {
                warnings.push(Warning::RawIp);
            }
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        warnings.push(Warning::UserInfo);
    }
    // The host as the text shows it: after the last "@" of the authority, before the port.
    let shown_authority = &authority[..authority.find(['/', '?', '#']).unwrap_or(authority.len())];
    let shown_host = shown_authority.rsplit('@').next().unwrap_or("");
    let shown_host = match shown_host.strip_prefix('[') {
        Some(v6) => format!("[{}]", v6.split(']').next().unwrap_or("")),
        None => shown_host.split(':').next().unwrap_or("").to_string(),
    };
    let real_host = url.host_str().unwrap_or("");
    if !shown_host.eq_ignore_ascii_case(real_host) && !warnings.contains(&Warning::Lookalike) {
        warnings.push(Warning::HostMismatch);
    }
    let destination = real_host.to_string();
    Some(Link { range: 0..0, mark: None, url, warnings, destination })
}

/// Endings a bare domain ("example.com", no scheme) may have: country codes and common
/// generic TLDs. Others ("Node.js", "dolor.sit", "notes.txt") stay text unless written
/// with a scheme. Left out on purpose: "zip" and "mov" (real TLDs used to bait clicks on
/// what reads as a file name) and "md" / "py", which read as file names too.
const BARE_TLDS: &[&str] = &[
    "ac", "ad", "ae", "af", "ag", "ai", "al", "am", "ao", "aq", "ar", "as", "at", "au", "aw", "ax", "az", "ba", "bb",
    "bd", "be", "bf", "bg", "bh", "bi", "bj", "bm", "bn", "bo", "br", "bs", "bt", "bw", "by", "bz", "ca", "cc", "cd",
    "cf", "cg", "ch", "ci", "ck", "cl", "cm", "cn", "co", "cr", "cu", "cv", "cw", "cx", "cy", "cz", "de", "dj", "dk",
    "dm", "do", "dz", "ec", "ee", "eg", "er", "es", "et", "eu", "fi", "fj", "fk", "fm", "fo", "fr", "ga", "gd", "ge",
    "gf", "gg", "gh", "gi", "gl", "gm", "gn", "gp", "gq", "gr", "gs", "gt", "gu", "gw", "gy", "hk", "hm", "hn", "hr",
    "ht", "hu", "id", "ie", "il", "im", "in", "io", "iq", "ir", "is", "it", "je", "jm", "jo", "jp", "ke", "kg", "kh",
    "ki", "km", "kn", "kp", "kr", "kw", "ky", "kz", "la", "lb", "lc", "li", "lk", "lr", "ls", "lt", "lu", "lv", "ly",
    "ma", "mc", "me", "mg", "mh", "mk", "ml", "mm", "mn", "mo", "mp", "mq", "mr", "ms", "mt", "mu", "mv", "mw", "mx",
    "my", "mz", "na", "nc", "ne", "nf", "ng", "ni", "nl", "no", "np", "nr", "nu", "nz", "om", "pa", "pe", "pf", "pg",
    "ph", "pk", "pl", "pm", "pn", "pr", "ps", "pt", "pw", "qa", "re", "ro", "rs", "ru", "rw", "sa", "sb", "sc", "sd",
    "se", "sg", "sh", "si", "sk", "sl", "sm", "sn", "so", "sr", "ss", "st", "su", "sv", "sx", "sy", "sz", "tc", "td",
    "tf", "tg", "th", "tj", "tk", "tl", "tm", "tn", "to", "tr", "tt", "tv", "tw", "tz", "ua", "ug", "uk", "us", "uy",
    "uz", "va", "vc", "ve", "vg", "vi", "vn", "vu", "wf", "ws", "ye", "yt", "za", "zm", "zw", "com", "net", "org",
    "edu", "gov", "mil", "int", "info", "biz", "name", "pro", "mobi", "asia", "tel", "jobs", "aero", "coop", "museum",
    "travel", "app", "dev", "page", "xyz", "online", "site", "website", "store", "shop", "tech", "blog", "news",
    "live", "cloud", "wiki", "art", "club", "design", "email", "world", "today", "life", "space", "link", "top",
    "fun", "one", "ltd", "inc", "studio", "agency", "media", "network", "digital", "company", "group", "global",
    "social", "video", "photo", "photos", "music", "games", "health", "finance", "money", "bank", "law", "legal",
    "market", "academy", "school", "events", "chat", "systems", "software", "zone", "codes", "tools", "works", "run",
    "gay", "lgbt", "berlin", "london", "nyc", "paris", "tokyo", "dubai", "ninja", "guru", "rocks", "family", "work",
];

// --- UI ---

thread_local! {
    /// Whether the last mouse press was a plain left click: InteractiveText fires its
    /// click for any button, and a right-click on a link must only open the menu.
    static PRIMARY_PRESS: Cell<bool> = const { Cell::new(false) };
}

/// Open a link in the default browser / mail app (NSWorkspace, the serialized URL).
/// `WA_DEBUG_NO_OPEN=1` only logs it.
pub fn open(link: &Link, cx: &mut App) {
    if std::env::var_os("WA_DEBUG_NO_OPEN").is_some() {
        eprintln!("[links] open {}", link.url);
        return;
    }
    cx.open_url(link.url.as_str());
}

/// Bubble text with its links clickable: normal ones open, suspicious ones ask first
/// ([`confirm`]); hovering shows where a link goes.
pub fn interactive(id: SharedString, text: StyledText, links: Vec<Link>) -> Div {
    let hits = links.iter().map(Link::hit_range).collect();
    let links = std::rc::Rc::new(links);
    let tips = links.clone();
    div()
        .on_any_mouse_down(|ev, _, _| {
            PRIMARY_PRESS.set(ev.button == MouseButton::Left && !ev.modifiers.control);
        })
        .child(
            InteractiveText::new(id, text)
                .on_click(hits, move |ix, window, cx| {
                    if !PRIMARY_PRESS.get() {
                        return;
                    }
                    let link = links[ix].clone();
                    if link.suspicious() { confirm(link, window, cx) } else { open(&link, cx) }
                })
                .tooltip(move |ix, window, cx| {
                    let link = tips.iter().find(|l| l.hit_range().contains(&ix))?;
                    let tip = match (link.url.scheme(), link.suspicious()) {
                        ("mailto", false) => format!("Email {}", link.destination),
                        (_, false) => link.destination.clone(),
                        (_, true) => format!("Goes to {} (check before opening)", link.destination),
                    };
                    Some(Tooltip::new(tip).build(window, cx))
                }),
        )
}

/// "This link may not be what it looks like" (a native alert): the real destination
/// (ASCII / punycode host), why it's suspicious, and Cancel / Open Link.
pub fn confirm(link: Link, window: &mut Window, cx: &mut App) {
    let mut full = link.url.to_string();
    if full.chars().count() > 160 {
        full = full.chars().take(160).collect::<String>() + "…";
    }
    let mut reasons: Vec<&str> = link.warnings.iter().map(|w| w.describe()).collect();
    reasons.dedup();
    let detail = format!("It really goes to {}\n{full}\n\n{}", link.destination, reasons.join("\n"));
    // Cancel first: Return doesn't open it.
    let answers = [PromptButton::cancel("Cancel"), PromptButton::new("Open Link")];
    let answer = window.prompt(PromptLevel::Warning, "This link may not be what it looks like", Some(&detail), &answers, cx);
    cx.spawn(async move |cx| {
        if answer.await == Ok(1) {
            cx.update(|cx| open(&link, cx));
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    #[test]
    fn strip_bidi_keeps_everything_else() {
        assert_eq!(super::strip_bidi("invoice_\u{202E}fdp.exe"), "invoice_fdp.exe");
        assert_eq!(super::strip_bidi("\u{2067}x\u{2069} 👨\u{200D}👩 می\u{200C}خواهم"), "x 👨\u{200D}👩 می\u{200C}خواهم");
    }

    use super::{MAX_LINK_LEN, WARNING_MARK, Warning, classify, linkify};

    fn links(text: &str) -> Vec<(String, String, Vec<Warning>)> {
        let l = linkify(text, &[]);
        l.links.iter().map(|k| (l.text[k.range.clone()].to_string(), k.url.to_string(), k.warnings.clone())).collect()
    }

    fn one(text: &str) -> (String, String, Vec<Warning>) {
        let mut all = links(text);
        assert_eq!(all.len(), 1, "{text:?}: {all:?}");
        all.remove(0)
    }

    #[test]
    fn plain_urls_and_trailing_punctuation() {
        assert_eq!(one("see https://example.com/a?b=1."), ("https://example.com/a?b=1".into(), "https://example.com/a?b=1".into(), vec![]));
        assert_eq!(one("(https://example.com/x)").0, "https://example.com/x");
        assert_eq!(one("<https://example.com>").0, "https://example.com");
        assert_eq!(one("go to https://example.com!").0, "https://example.com");
        assert_eq!(one("“https://example.com/q”").0, "https://example.com/q");
        let wiki = one("read https://en.wikipedia.org/wiki/Rust_(programming_language), ok");
        assert_eq!(wiki.0, "https://en.wikipedia.org/wiki/Rust_(programming_language)");
        assert!(wiki.2.is_empty());
        assert_eq!(links("a https://a.com b http://b.org/c").len(), 2);
    }

    #[test]
    fn bare_domains_become_https() {
        assert_eq!(one("visit example.com today"), ("example.com".into(), "https://example.com/".into(), vec![]));
        assert_eq!(one("www.example.com/path.").1, "https://www.example.com/path");
        assert_eq!(one("docs.rs/url").1, "https://docs.rs/url");
        assert_eq!(one("Example.COM").1, "https://example.com/");
    }

    #[test]
    fn non_links_stay_text() {
        for text in ["notes.txt", "setup.zip", "photo.JPG", "v1.2", "3.14", "e.g. this", "Node.js", "192.168.1.1", "hello.", "a..b"] {
            assert!(links(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn emails_become_mailto() {
        let (shown, url, w) = one("mail me at jane.doe+wa@example.co.uk.");
        assert_eq!((shown.as_str(), url.as_str()), ("jane.doe+wa@example.co.uk", "mailto:jane.doe+wa@example.co.uk"));
        assert!(w.is_empty());
        let l = linkify("x@example.com", &[]);
        assert_eq!(l.links[0].destination, "x@example.com");
        // "mailto:" stays text; the address itself is the link.
        assert_eq!(one("mailto:x@example.com").0, "x@example.com");
        assert_eq!(links("ping @sara or a.b@c").len(), 0);
    }

    #[test]
    fn disallowed_schemes_are_not_links() {
        for text in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<script>alert(1)</script>",
            "ftp://example.com/x",
            "whatsapp://send?text=hi",
            "vscode://file/etc/passwd",
            "javascript://example.com/%0aalert(1)",
            "http://",
        ] {
            let found = links(text);
            assert!(found.iter().all(|(_, u, _)| u.starts_with("https://") || u.starts_with("http://")), "{text}: {found:?}");
            assert!(found.iter().all(|(s, _, _)| !s.contains(':') || s.starts_with("http")), "{text}: {found:?}");
        }
        assert!(links("javascript:alert(1)").is_empty());
        assert!(links("file:///etc/passwd").is_empty());
        assert!(links("ftp://example.com/x").is_empty());
    }

    #[test]
    fn idn_homograph_is_suspicious_and_opens_punycode() {
        // Cyrillic "а" (U+0430).
        let l = linkify("log in at \u{430}pple.com now", &[]);
        assert_eq!(l.links.len(), 1);
        let link = &l.links[0];
        assert_eq!(&l.text[link.range.clone()], "\u{430}pple.com", "whole host is the link");
        assert_eq!(link.warnings, vec![Warning::Lookalike]);
        assert!(link.url.host_str().unwrap().starts_with("xn--"));
        assert_eq!(&l.text[link.mark.clone().unwrap()], WARNING_MARK);
        assert_eq!(link.destination, link.url.host_str().unwrap());
        assert_eq!(one("https://gооgle.com/").2, vec![Warning::Lookalike]);
        assert_eq!(classify("https://пример.рф", false).unwrap().warnings, vec![Warning::Lookalike]);
        let mail = linkify("ex@g\u{43E}\u{43E}gle.com", &[]);
        assert_eq!(mail.links[0].warnings, vec![Warning::Lookalike]);
        assert!(mail.links[0].destination.starts_with("ex@xn--"), "{}", mail.links[0].destination);
        // A fully non-Latin bare domain isn't detected at all, which is the safe side.
        assert!(links("пример.рф").is_empty());
    }

    #[test]
    fn userinfo_and_backslash_tricks() {
        let (_, url, w) = one("https://google.com@evil.example/login");
        assert_eq!(url, "https://google.com@evil.example/login");
        assert!(w.contains(&Warning::UserInfo), "{w:?}");
        assert!(one("https://google.com:pw@evil.example").2.contains(&Warning::UserInfo));
        // Detection stops at "\\", so only the real host is underlined…
        assert_eq!(one("https://evil.example\\@google.com").1, "https://evil.example/");
        // …and if a "\\" did reach the parser, the shown and real hosts differ.
        let link = classify("https://evil.example\\@google.com", false).unwrap();
        assert_eq!(link.destination, "evil.example");
        assert!(link.warnings.contains(&Warning::HostMismatch), "{:?}", link.warnings);
        assert!(classify("http://2130706433/", false).unwrap().warnings.contains(&Warning::HostMismatch));
        assert!(classify("https://%67oogle.com/", false).unwrap().warnings.contains(&Warning::HostMismatch));
        assert!(one("https://example.com:8443/x").2.is_empty());
        assert!(one("https://example.com./x").2.is_empty());
    }

    #[test]
    fn raw_ip_over_http_is_suspicious() {
        assert_eq!(one("http://203.0.113.9/pay").2, vec![Warning::RawIp]);
        assert!(one("https://203.0.113.9/").2.is_empty());
        let v6 = classify("http://[2001:db8::1]:8080/", false).unwrap();
        assert_eq!(v6.warnings, vec![Warning::RawIp]);
    }

    #[test]
    fn rtlo_spoof_is_neutralized() {
        // Shows as "https://evil.example/gpj.exe" reversed into "…/exe.jpg" when RTLO is honored.
        let text = "https://evil.example/\u{202E}gpj.exe";
        let l = linkify(text, &[]);
        assert!(!l.text.contains('\u{202E}'));
        assert_eq!(l.links.len(), 1);
        let link = &l.links[0];
        assert_eq!(&l.text[link.range.clone()], "https://evil.example/gpj.exe");
        assert_eq!(link.url.as_str(), "https://evil.example/gpj.exe");
        assert_eq!(link.warnings, vec![Warning::HiddenChars]);

        // An override before a link can't flip it either: removed everywhere, so this
        // reads (and opens) as the "moc.elgoog" it really is, not "https://google.com".
        let l = linkify("hi \u{202E}moc.elgoog//:sptth", &[]);
        assert_eq!(l.text, "hi moc.elgoog//:sptth");
        assert!(l.links.iter().all(|k| k.destination == "moc.elgoog"));

        // Zero-width space inside a host: shown and opened without it, flagged.
        let (shown, url, w) = one("https://goo\u{200B}gle.com");
        assert_eq!((shown.as_str(), url.as_str()), ("https://google.com", "https://google.com/"));
        assert_eq!(w, vec![Warning::HiddenChars]);
    }

    #[test]
    fn invisible_chars_outside_links_are_kept() {
        let family = "👨\u{200D}👩\u{200D}👧 see example.com \u{200F}שלום";
        let l = linkify(family, &[]);
        assert!(l.text.contains("👨\u{200D}👩\u{200D}👧") && l.text.contains('\u{200F}'));
        assert_eq!(l.links.len(), 1);
        assert!(l.links[0].warnings.is_empty());
    }

    #[test]
    fn mentions_are_never_links_and_ranges_remap() {
        // A push name like "paypal.com" in a mention must not become a link.
        let text = "@paypal.com \u{202E}hi example.com";
        let mention = 0..11;
        let l = linkify(text, std::slice::from_ref(&mention));
        assert_eq!(l.links.len(), 1);
        assert_eq!(&l.text[l.links[0].range.clone()], "example.com");
        assert_eq!(&l.text[l.map_range(&mention)], "@paypal.com");
        // Ranges after a suspicious link's mark shift by the mark.
        let text = "\u{430}pple.com @Sara";
        let l = linkify(text, &[]);
        let sara = text.find('@').unwrap()..text.len();
        assert_eq!(&l.text[l.map_range(&sara)], "@Sara");
    }

    #[test]
    fn long_links_and_long_text() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_LINK_LEN));
        assert!(links(&long).is_empty());
        let text = "lorem ipsum https://example.com/x, dolor.sit amet ".repeat(4000);
        let started = std::time::Instant::now();
        let l = linkify(&text, &[]);
        assert_eq!(l.links.len(), 4000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        let hidden = "a\u{200B}".repeat(50_000);
        let started = std::time::Instant::now();
        linkify(&hidden, &[]);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }
}
