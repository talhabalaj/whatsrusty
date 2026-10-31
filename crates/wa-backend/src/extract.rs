//! Turning protocol messages into what the UI shows, plus small JID helpers.

use std::borrow::Cow;

use whatsapp_rust::buffa::Message as _;
use whatsapp_rust::download::Downloadable;
use whatsapp_rust::prelude::{MessageField, wa};
use whatsapp_rust::waproto::tags;
use whatsapp_rust::waproto::whatsapp::message::protocol_message::Type as ProtocolType;

use crate::types::{
    ContactCard, DocumentMedia, ImageMedia, LocationInfo, MediaState, MessageBody, PollInfo, RichText, StickerMedia,
    VideoMedia, ViewOnceKind, VoiceMedia,
};

/// What a received `wa::Message` means for the chat store.
#[derive(Debug, Clone, PartialEq)]
pub enum Extracted {
    /// A message to show in the chat.
    Body(MessageBody),
    /// "Delete for everyone" of an earlier message.
    Revoke { target_id: String },
    /// Edit of an earlier message.
    Edit { target_id: String, body: MessageBody },
    /// The sender's reaction to a message of the same chat; an empty `emoji` removes it.
    /// (Encrypted community-announcement reactions arrive here too: the library opens
    /// them on receipt. Copies still encrypted, in history sync, are skipped.)
    Reaction { target_id: String, emoji: String, sender_ts_ms: Option<i64> },
    /// Protocol/reaction/system traffic that should not appear in the chat.
    Skip,
}

pub const DELETED_LABEL: &str = "🚫 This message was deleted";
/// Label of messages whose type we don't know at all.
pub const UNSUPPORTED_LABEL: &str = "Unsupported message";

/// Wrappers nest (`deviceSent` > `ephemeral` > `viewOnce` > ...); bound the walk anyway.
const MAX_UNWRAP_DEPTH: usize = 8;

/// The content message inside every `FutureProofMessage`-style wrapper (in any order),
/// with what the wrappers said about it.
struct Unwrapped<'a> {
    msg: &'a wa::Message,
    /// One of the wrappers was a `lottieStickerMessage`.
    lottie: bool,
    /// One of the wrappers was a view-once wrapper.
    view_once: bool,
}

fn unwrap(msg: &wa::Message) -> Unwrapped<'_> {
    let mut m = msg;
    let mut lottie = false;
    let mut view_once = false;
    for _ in 0..MAX_UNWRAP_DEPTH {
        if let Some(inner) = m.device_sent_message.as_option().and_then(|d| d.message.as_option()) {
            m = inner;
            continue;
        }
        if let Some(inner) = m.lottie_sticker_message.as_option().and_then(|w| w.message.as_option()) {
            lottie = true;
            m = inner;
            continue;
        }
        macro_rules! peel {
            ($flag:ident: $($field:ident),* $(,)?) => {
                $(if let Some(inner) = m.$field.as_option().and_then(|w| w.message.as_option()) {
                    $flag = true;
                    m = inner;
                    continue;
                })*
            };
            ($($field:ident),* $(,)?) => {
                $(if let Some(inner) = m.$field.as_option().and_then(|w| w.message.as_option()) {
                    m = inner;
                    continue;
                })*
            };
        }
        peel!(view_once: view_once_message, view_once_message_v2, view_once_message_v2_extension);
        peel!(
            ephemeral_message,
            document_with_caption_message,
            // Newer clients wrap the MESSAGE_EDIT protocol message in this.
            edited_message,
            bot_invoke_message,
            bot_forwarded_message,
            group_mentioned_message,
            // Album children.
            associated_child_message,
            spoiler_message,
            limit_sharing_message,
            poll_creation_message_v4,
        );
        break;
    }
    Unwrapped { msg: m, lottie, view_once }
}

/// The content message inside ephemeral / view-once / album-child / ... wrappers.
pub fn unwrap_message(msg: &wa::Message) -> &wa::Message {
    unwrap(msg).msg
}

/// What a view-once message is, if it is one: a view-once wrapper or the `viewOnce` flag
/// of the photo / video / audio itself.
fn view_once_kind(u: &Unwrapped<'_>) -> Option<ViewOnceKind> {
    let m = u.msg;
    let flagged = |f: Option<bool>| u.view_once || f == Some(true);
    if let Some(x) = m.image_message.as_option() {
        return flagged(x.view_once).then_some(ViewOnceKind::Photo);
    }
    if let Some(x) = m.video_message.as_option().or(m.ptv_message.as_option()) {
        return flagged(x.view_once).then_some(ViewOnceKind::Video);
    }
    if let Some(x) = m.audio_message.as_option() {
        return flagged(x.view_once).then_some(ViewOnceKind::Voice);
    }
    None
}

/// Disappearing-message timer of a message (seconds after it was sent), from its
/// `contextInfo`.
pub fn expiration_of(msg: &wa::Message) -> Option<u32> {
    context_info(unwrap_message(msg)).and_then(|ci| ci.expiration).filter(|e| *e > 0)
}

/// The id of the message an HD copy belongs to. Phones send a photo / video twice: in
/// standard quality, then (seconds later) an HD upload linked to it. WhatsApp shows the
/// pair as one message, so the HD copy is stored under its parent's id.
pub fn hd_parent_of(msg: &wa::Message) -> Option<String> {
    use whatsapp_rust::waproto::whatsapp::message_association::AssociationType as Association;
    let mut m = msg;
    for _ in 0..MAX_UNWRAP_DEPTH {
        let association = m.message_context_info.as_option().and_then(|c| c.message_association.as_option());
        if let Some(a) = association
            && matches!(
                a.association_type,
                Some(Association::HD_IMAGE_DUAL_UPLOAD | Association::HD_VIDEO_DUAL_UPLOAD | Association::HEVC_VIDEO_DUAL_UPLOAD)
            )
        {
            return a.parent_message_key.as_option().and_then(|k| k.id.clone()).filter(|id| !id.is_empty());
        }
        // The association sits on the outer message or on one of the wrapped ones.
        let inner = m
            .device_sent_message
            .as_option()
            .and_then(|d| d.message.as_option())
            .or_else(|| m.associated_child_message.as_option().and_then(|w| w.message.as_option()))
            .or_else(|| m.ephemeral_message.as_option().and_then(|w| w.message.as_option()));
        match inner {
            Some(inner) => m = inner,
            None => break,
        }
    }
    None
}

pub fn classify(msg: &wa::Message) -> Extracted {
    let base = unwrap_message(msg);

    if let Some(r) = base.reaction_message.as_option() {
        let target_id = r.key.as_option().and_then(|k| k.id.clone()).filter(|id| !id.is_empty());
        return match target_id {
            Some(target_id) => Extracted::Reaction {
                target_id,
                emoji: r.text.clone().unwrap_or_default(),
                sender_ts_ms: r.sender_timestamp_ms.filter(|t| *t > 0),
            },
            None => Extracted::Skip,
        };
    }

    if let Some(proto) = base.protocol_message.as_option() {
        let target_id = proto.key.as_option().and_then(|k| k.id.clone());
        return match (proto.r#type, target_id) {
            (Some(ProtocolType::REVOKE), Some(target_id)) => Extracted::Revoke { target_id },
            (Some(ProtocolType::MESSAGE_EDIT), Some(target_id)) => {
                match proto.edited_message.as_option().and_then(body_of) {
                    Some(body) => Extracted::Edit { target_id, body },
                    None => Extracted::Skip,
                }
            }
            _ => Extracted::Skip,
        };
    }

    match body_of(msg) {
        Some(body) => {
            if matches!(&body, MessageBody::Unsupported { label } if label == UNSUPPORTED_LABEL) {
                // Field names only, never content: tells us which types to support next.
                log::info!("unsupported message type: {}", unknown_fields(base).join(", "));
            }
            Extracted::Body(body)
        }
        None => Extracted::Skip,
    }
}

/// The secret a poll's votes are encrypted with (its `messageContextInfo.messageSecret`).
pub fn poll_secret_of(msg: &wa::Message) -> Option<Vec<u8>> {
    let base = unwrap_message(msg);
    [base, msg]
        .into_iter()
        .find_map(|m| m.message_context_info.as_option().and_then(|c| c.message_secret.clone()))
        .filter(|s| !s.is_empty())
}

/// An encrypted vote in an earlier poll of the same chat.
#[derive(Debug, Clone, PartialEq)]
pub struct EncPollVote {
    pub poll_id: String,
    pub enc_payload: Vec<u8>,
    pub enc_iv: Vec<u8>,
    pub sender_ts_ms: Option<i64>,
}

/// The vote `msg` carries, if it's one (`pollUpdateMessage`).
pub fn poll_vote_of(msg: &wa::Message) -> Option<EncPollVote> {
    let u = unwrap_message(msg).poll_update_message.as_option()?;
    let poll_id = u.poll_creation_message_key.as_option().and_then(|k| k.id.clone()).filter(|id| !id.is_empty())?;
    let vote = u.vote.as_option()?;
    Some(EncPollVote {
        poll_id,
        enc_payload: vote.enc_payload.clone()?,
        enc_iv: vote.enc_iv.clone()?,
        sender_ts_ms: u.sender_timestamp_ms.filter(|t| *t > 0),
    })
}

/// The option names among `options` whose SHA-256 is in `hashes` (how votes name them).
pub fn poll_options_by_hash(options: &[String], hashes: &[Vec<u8>]) -> Vec<String> {
    use whatsapp_rust::wacore::poll::compute_option_hash;
    options.iter().filter(|o| hashes.iter().any(|h| h.as_slice() == compute_option_hash(o))).cloned().collect()
}

/// A pin or unpin of an earlier message of the same chat.
#[derive(Debug, Clone, PartialEq)]
pub struct PinChange {
    pub target_id: String,
    /// How long it stays pinned (`None`: an unpin).
    pub duration_secs: Option<u32>,
    /// When the sender pinned it, by their clock.
    pub sender_ts_ms: Option<i64>,
}

/// How long a pin lasts when the message doesn't say (WhatsApp's default, 7 days).
pub const DEFAULT_PIN_SECS: u32 = 7 * 86_400;

/// The pin or unpin `msg` carries, if it is one (`pinInChatMessage`; the duration is in its
/// `messageContextInfo`).
pub fn pin_of(msg: &wa::Message) -> Option<PinChange> {
    use wa::message::pin_in_chat_message::Type as PinType;
    let base = unwrap_message(msg);
    let p = base.pin_in_chat_message.as_option()?;
    let target_id = p.key.as_option().and_then(|k| k.id.clone()).filter(|id| !id.is_empty())?;
    let duration = || {
        [base, msg]
            .into_iter()
            .find_map(|m| m.message_context_info.as_option().and_then(|c| c.message_add_on_duration_in_secs))
            .filter(|d| *d > 0)
            .unwrap_or(DEFAULT_PIN_SECS)
    };
    let duration_secs = match p.r#type {
        Some(PinType::PIN_FOR_ALL) => Some(duration()),
        Some(PinType::UNPIN_FOR_ALL) => None,
        _ => return None,
    };
    Some(PinChange { target_id, duration_secs, sender_ts_ms: p.sender_timestamp_ms.filter(|t| *t > 0) })
}

/// Displayable body of a message, or `None` for messages that carry nothing to show
/// (reactions, poll votes, key distribution, album headers, ...).
pub fn body_of(msg: &wa::Message) -> Option<MessageBody> {
    let u = unwrap(msg);
    if let Some(kind) = view_once_kind(&u) {
        return Some(MessageBody::ViewOnce(kind));
    }
    let (m, lottie) = (u.msg, u.lottie);

    if let Some(text) = m.conversation.as_deref().filter(|t| !t.is_empty()) {
        return Some(MessageBody::Text(text.to_owned()));
    }
    if let Some(text) = m.extended_text_message.as_option().and_then(|e| e.text.as_deref()) {
        return Some(MessageBody::Text(text.to_owned()));
    }
    if let Some(media) = media_in(m, lottie) {
        return Some(media.body(None, MediaState::Idle));
    }
    if let Some(location) = location_of(m) {
        return Some(MessageBody::Location(location));
    }
    if let Some(cards) = contacts_of(m) {
        return Some(MessageBody::Contacts(cards));
    }
    if let Some(poll) = poll_of(m) {
        return Some(MessageBody::Poll(poll));
    }
    if let Some(rich) = rich_of(m) {
        return Some(MessageBody::Rich(rich));
    }
    if let Some(reply) = tap_reply_of(m) {
        return Some(MessageBody::Text(reply));
    }
    if let Some(text) = system_of(m) {
        return Some(MessageBody::System { text });
    }
    // The album's photos and videos arrive as their own messages.
    if m.album_message.is_set() {
        return None;
    }

    if has_other_content(m) {
        return Some(MessageBody::Unsupported { label: UNSUPPORTED_LABEL.to_owned() });
    }
    None
}

fn nonempty(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn nonempty_caption(caption: Option<&str>) -> Option<String> {
    caption.map(str::trim).filter(|c| !c.is_empty()).map(str::to_owned)
}

fn nonempty_bytes(b: &Option<Vec<u8>>) -> Option<Vec<u8>> {
    b.clone().filter(|b| !b.is_empty())
}

fn location_of(m: &wa::Message) -> Option<LocationInfo> {
    if let Some(l) = m.location_message.as_option() {
        return Some(LocationInfo {
            latitude: l.degrees_latitude.unwrap_or(0.0),
            longitude: l.degrees_longitude.unwrap_or(0.0),
            name: nonempty(&l.name),
            address: nonempty(&l.address),
            thumbnail: nonempty_bytes(&l.jpeg_thumbnail),
            live: l.is_live == Some(true),
        });
    }
    let l = m.live_location_message.as_option()?;
    Some(LocationInfo {
        latitude: l.degrees_latitude.unwrap_or(0.0),
        longitude: l.degrees_longitude.unwrap_or(0.0),
        name: nonempty(&l.caption),
        address: None,
        thumbnail: nonempty_bytes(&l.jpeg_thumbnail),
        live: true,
    })
}

fn contact_card(c: &wa::message::ContactMessage) -> ContactCard {
    let vcard = parse_vcard(c.vcard.as_deref().unwrap_or(""));
    let display_name = nonempty(&c.display_name)
        .or(vcard.name)
        .or_else(|| vcard.phones.first().cloned())
        .unwrap_or_else(|| "Contact".to_owned());
    ContactCard { display_name, phones: vcard.phones }
}

fn contacts_of(m: &wa::Message) -> Option<Vec<ContactCard>> {
    if let Some(c) = m.contact_message.as_option() {
        return Some(vec![contact_card(c)]);
    }
    let arr = m.contacts_array_message.as_option()?;
    let cards: Vec<ContactCard> = arr.contacts.iter().map(contact_card).collect();
    if cards.is_empty() {
        let display_name = nonempty(&arr.display_name).unwrap_or_else(|| "Contacts".to_owned());
        return Some(vec![ContactCard { display_name, phones: Vec::new() }]);
    }
    Some(cards)
}

/// What a contact card's vCard says.
#[derive(Debug, Default, PartialEq)]
pub struct VCard {
    /// `FN`
    pub name: Option<String>,
    /// `TEL` values (or `+<waid>` when the value is empty), deduplicated, in order.
    pub phones: Vec<String>,
}

/// Minimal vCard (2.1 / 3.0 / 4.0) reader for the fields a contact bubble shows.
pub fn parse_vcard(vcard: &str) -> VCard {
    // Unfold continuation lines (a line break followed by a space or tab).
    let mut lines: Vec<String> = Vec::new();
    for raw in vcard.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match (raw.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => lines.push(raw.to_owned()),
        }
    }
    let mut out = VCard::default();
    for line in &lines {
        let Some((head, value)) = line.split_once(':') else { continue };
        let mut params = head.split(';');
        // `item1.TEL` -> `TEL`
        let prop = params.next().unwrap_or("");
        let prop = prop.rsplit('.').next().unwrap_or(prop).trim();
        if prop.eq_ignore_ascii_case("FN") {
            out.name = out.name.or_else(|| Some(unescape_vcard(value).trim().to_owned()).filter(|n| !n.is_empty()));
        } else if prop.eq_ignore_ascii_case("TEL") {
            let waid = params.find_map(|p| {
                let (k, v) = p.split_once('=')?;
                k.trim().eq_ignore_ascii_case("waid").then(|| v.trim().trim_matches('"').to_owned())
            });
            let value = unescape_vcard(value);
            let value = value.trim();
            let value = value.get(..4).filter(|p| p.eq_ignore_ascii_case("tel:")).map_or(value, |_| &value[4..]).trim();
            let phone = match (value, waid.filter(|w| !w.is_empty())) {
                ("", Some(waid)) => format!("+{waid}"),
                ("", None) => continue,
                (value, _) => value.to_owned(),
            };
            if !out.phones.contains(&phone) {
                out.phones.push(phone);
            }
        }
    }
    out
}

fn unescape_vcard(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push(' '),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn poll_of(m: &wa::Message) -> Option<PollInfo> {
    let polls = [
        &m.poll_creation_message,
        &m.poll_creation_message_v2,
        &m.poll_creation_message_v3,
        &m.poll_creation_message_v5,
        &m.poll_creation_message_v6,
    ];
    let poll = polls.into_iter().find_map(|p| p.as_option())?;
    Some(PollInfo {
        question: nonempty(&poll.name).unwrap_or_else(|| "Poll".to_owned()),
        options: poll.options.iter().filter_map(|o| nonempty(&o.option_name)).collect(),
        // WhatsApp sends 1 for single choice and 0 for "any number" (see Baileys).
        multiple: poll.selectable_options_count.unwrap_or(0) != 1,
        votes: Vec::new(),
    })
}

/// Template / interactive / buttons / list / product messages from businesses and bots.
fn rich_of(m: &wa::Message) -> Option<RichText> {
    let rich = if let Some(t) = m.template_message.as_option() {
        template_rich(t)
    } else if let Some(i) = m.interactive_message.as_option() {
        interactive_rich(i)
    } else if let Some(b) = m.buttons_message.as_option() {
        Some(buttons_rich(b))
    } else if let Some(l) = m.list_message.as_option() {
        Some(list_rich(l))
    } else if let Some(h) = m.highly_structured_message.as_option() {
        hsm_rich(h)
    } else {
        m.product_message.as_option().map(product_rich)
    };
    let mut rich = rich?;
    if rich.body.is_empty() {
        // Never an empty bubble: promote the header, else show what kind of thing it is.
        rich.body = match rich.header.take() {
            Some(header) => header,
            None if rich.footer.is_none() && rich.buttons.is_empty() => return None,
            None => "💼 Business message".to_owned(),
        };
    }
    Some(rich)
}

fn template_rich(t: &wa::message::TemplateMessage) -> Option<RichText> {
    use wa::message::template_message::Format;
    if let Some(rich) = t.hydrated_template.as_option().and_then(hydrated_rich) {
        return Some(rich);
    }
    match &t.format {
        Some(Format::HydratedFourRowTemplate(h)) => hydrated_rich(h),
        Some(Format::InteractiveMessageTemplate(i)) => interactive_rich(i),
        Some(Format::FourRowTemplate(f)) => f.content.as_option().and_then(hsm_rich),
        None => None,
    }
}

fn hydrated_rich(h: &wa::message::template_message::HydratedFourRowTemplate) -> Option<RichText> {
    use wa::hydrated_template_button::HydratedButton;
    use wa::message::template_message::hydrated_four_row_template::Title;
    let header = match &h.title {
        Some(Title::HydratedTitleText(t)) => nonempty(&Some(t.clone())),
        Some(Title::DocumentMessage(d)) => nonempty(&d.file_name).or_else(|| nonempty(&d.title)),
        Some(Title::LocationMessage(l)) => nonempty(&l.name),
        _ => None,
    };
    let buttons = h
        .hydrated_buttons
        .iter()
        .filter_map(|b| match &b.hydrated_button {
            Some(HydratedButton::QuickReplyButton(q)) => nonempty(&q.display_text),
            Some(HydratedButton::UrlButton(u)) => nonempty(&u.display_text),
            Some(HydratedButton::CallButton(c)) => nonempty(&c.display_text),
            None => None,
        })
        .collect();
    let rich = RichText {
        header,
        body: nonempty(&h.hydrated_content_text).unwrap_or_default(),
        footer: nonempty(&h.hydrated_footer_text),
        buttons,
    };
    (!is_blank(&rich)).then_some(rich)
}

fn is_blank(r: &RichText) -> bool {
    r.header.is_none() && r.body.is_empty() && r.footer.is_none() && r.buttons.is_empty()
}

fn interactive_rich(i: &wa::message::InteractiveMessage) -> Option<RichText> {
    use wa::message::interactive_message::InteractiveMessage as Kind;
    let mut rich = RichText {
        header: i.header.as_option().and_then(|h| nonempty(&h.title).or_else(|| nonempty(&h.subtitle))),
        body: i.body.as_option().and_then(|b| nonempty(&b.text)).unwrap_or_default(),
        footer: i.footer.as_option().and_then(|f| nonempty(&f.text)),
        buttons: Vec::new(),
    };
    let fallback = match &i.interactive_message {
        Some(Kind::NativeFlowMessage(n)) => {
            rich.buttons = n
                .buttons
                .iter()
                .filter_map(|b| b.button_params_json.as_deref().and_then(json_display_text))
                .collect();
            None
        }
        Some(Kind::ShopStorefrontMessage(_)) => Some("🛍️ Shop".to_owned()),
        Some(Kind::CollectionMessage(_)) => Some("🛍️ Collection".to_owned()),
        Some(Kind::CarouselMessage(c)) => c.cards.iter().filter_map(interactive_rich).map(|r| r.body).next(),
        None => None,
    };
    if rich.body.is_empty()
        && let Some(fallback) = fallback
    {
        rich.body = fallback;
    }
    (!is_blank(&rich)).then_some(rich)
}

fn buttons_rich(b: &wa::message::ButtonsMessage) -> RichText {
    use wa::message::buttons_message::Header;
    RichText {
        header: match &b.header {
            Some(Header::Text(t)) => nonempty(&Some(t.clone())),
            Some(Header::DocumentMessage(d)) => nonempty(&d.file_name),
            Some(Header::LocationMessage(l)) => nonempty(&l.name),
            _ => None,
        },
        body: nonempty(&b.content_text).unwrap_or_default(),
        footer: nonempty(&b.footer_text),
        buttons: b
            .buttons
            .iter()
            .filter_map(|x| {
                x.button_text.as_option().and_then(|t| nonempty(&t.display_text)).or_else(|| {
                    x.native_flow_info.as_option().and_then(|n| n.params_json.as_deref()).and_then(json_display_text)
                })
            })
            .collect(),
    }
}

fn list_rich(l: &wa::message::ListMessage) -> RichText {
    let mut buttons: Vec<String> =
        l.sections.iter().flat_map(|s| s.rows.iter()).filter_map(|r| nonempty(&r.title)).collect();
    if buttons.is_empty() {
        buttons.extend(nonempty(&l.button_text));
    }
    RichText {
        header: nonempty(&l.title),
        body: nonempty(&l.description).unwrap_or_default(),
        footer: nonempty(&l.footer_text),
        buttons,
    }
}

fn hsm_rich(h: &wa::message::HighlyStructuredMessage) -> Option<RichText> {
    h.hydrated_hsm.as_option().and_then(template_rich)
}

fn product_rich(p: &wa::message::ProductMessage) -> RichText {
    let product = p.product.as_option();
    RichText {
        header: product.and_then(|s| nonempty(&s.title)),
        body: nonempty(&p.body)
            .or_else(|| product.and_then(|s| nonempty(&s.description)))
            .unwrap_or_else(|| "🛍️ Product".to_owned()),
        footer: nonempty(&p.footer),
        buttons: Vec::new(),
    }
}

/// `"display_text"` of a native-flow button's params JSON (`{"display_text":"Pay now",...}`).
fn json_display_text(json: &str) -> Option<String> {
    let key = "\"display_text\"";
    let rest = &json[json.find(key)? + key.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next()? {
            '"' => break,
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    // Surrogate pairs (emoji) need the second half.
                    let c = if (0xD800..0xDC00).contains(&code) {
                        let rest: String = chars.by_ref().take(6).collect();
                        let low = rest.strip_prefix("\\u").and_then(|h| u32::from_str_radix(h, 16).ok())?;
                        char::from_u32(0x10000 + ((code - 0xD800) << 10) + (low.checked_sub(0xDC00)?))
                    } else {
                        char::from_u32(code)
                    };
                    out.push(c.unwrap_or('\u{FFFD}'));
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_owned())
}

/// The user's tap on a business message's button / list row, shown as what they "said".
fn tap_reply_of(m: &wa::Message) -> Option<String> {
    use wa::message::buttons_response_message::Response;
    if let Some(r) = m.buttons_response_message.as_option() {
        let Some(Response::SelectedDisplayText(t)) = &r.response else { return None };
        return nonempty(&Some(t.clone()));
    }
    if let Some(r) = m.list_response_message.as_option() {
        return nonempty(&r.title).or_else(|| nonempty(&r.description));
    }
    if let Some(r) = m.template_button_reply_message.as_option() {
        return nonempty(&r.selected_display_text);
    }
    if let Some(r) = m.interactive_response_message.as_option() {
        return r.body.as_option().and_then(|b| nonempty(&b.text));
    }
    None
}

fn call_text(c: &wa::message::CallLogMessage) -> String {
    use wa::message::call_log_message::CallOutcome;
    let video = c.is_video == Some(true);
    let (icon, kind) = if video { ("📹", "video call") } else { ("📞", "voice call") };
    match c.call_outcome {
        Some(CallOutcome::MISSED | CallOutcome::SILENCED_BY_DND | CallOutcome::SILENCED_UNKNOWN_CALLER) => {
            format!("{icon} Missed {kind}")
        }
        Some(CallOutcome::REJECTED) => format!("{icon} Declined {kind}"),
        Some(CallOutcome::FAILED) => format!("{icon} Failed {kind}"),
        _ => {
            let kind = if video { "Video call" } else { "Voice call" };
            match c.duration_secs.filter(|d| *d > 0) {
                Some(d) if d >= 3600 => format!("{icon} {kind} · {}:{:02}:{:02}", d / 3600, d % 3600 / 60, d % 60),
                Some(d) => format!("{icon} {kind} · {}:{:02}", d / 60, d % 60),
                None => format!("{icon} {kind}"),
            }
        }
    }
}

/// Call logs, events, invites, pins: a short line for the middle of the chat.
fn system_of(m: &wa::Message) -> Option<String> {
    use wa::message::pin_in_chat_message::Type as PinType;
    let with_name = |prefix: &str, name: Option<String>| match name {
        Some(name) => format!("{prefix}: {name}"),
        None => prefix.to_owned(),
    };
    if let Some(c) = m.call_log_messsage.as_option() {
        return Some(call_text(c));
    }
    if m.bcall_message.is_set() {
        return Some("📞 Call".to_owned());
    }
    if let Some(s) = m.scheduled_call_creation_message.as_option() {
        return Some(with_name("📅 Scheduled call", nonempty(&s.title)));
    }
    if let Some(e) = m.event_message.as_option() {
        let text = with_name("📅 Event", nonempty(&e.name));
        return Some(if e.is_canceled == Some(true) { format!("{text} (canceled)") } else { text });
    }
    if let Some(g) = m.group_invite_message.as_option() {
        return Some(with_name("👥 Group invite", nonempty(&g.group_name)));
    }
    if let Some(p) = m.pin_in_chat_message.as_option()
        && p.r#type == Some(PinType::PIN_FOR_ALL)
    {
        return Some("📌 Pinned a message".to_owned());
    }
    if m.request_phone_number_message.is_set() {
        return Some("📱 Asked for your phone number".to_owned());
    }
    None
}

/// `m` without the fields we deliberately ignore (protocol traffic, reactions, ...).
fn without_ignored(m: &wa::Message) -> wa::Message {
    let mut rest = m.clone();
    rest.message_context_info = Default::default();
    rest.sender_key_distribution_message = Default::default();
    rest.fast_ratchet_key_sender_key_distribution_message = Default::default();
    rest.protocol_message = Default::default();
    rest.reaction_message = Default::default();
    rest.enc_reaction_message = Default::default();
    rest.poll_update_message = Default::default();
    rest.keep_in_chat_message = Default::default();
    // Pins are shown (see `system_of`); unpins aren't.
    rest.pin_in_chat_message = Default::default();
    rest.enc_event_response_message = Default::default();
    rest.enc_comment_message = Default::default();
    rest.secret_encrypted_message = Default::default();
    rest.sticker_sync_rmr_message = Default::default();
    rest.placeholder_message = Default::default();
    rest.message_history_notice = Default::default();
    rest
}

/// True when the message has content beyond the fields we deliberately ignore.
/// Keeps unknown future message types visible as "Unsupported message" instead of
/// silently dropping them, while hiding pure protocol traffic.
fn has_other_content(m: &wa::Message) -> bool {
    // Not `!= default()`: buffa compares a set-but-empty sub-message equal to an unset one.
    without_ignored(m).encoded_len() > 0
}

/// Names of the top-level `wa::Message` fields set in `m` (beyond the ignored ones),
/// e.g. `["order_message"]`. No content: safe to log.
pub fn unknown_fields(m: &wa::Message) -> Vec<String> {
    top_level_fields(&without_ignored(m).encode_to_vec())
}

macro_rules! message_field_names {
    ($($name:ident)*) => {
        /// Schema name of a top-level `wa::Message` field number (checked against
        /// waproto's generated tags at compile time).
        fn message_field_name(number: u32) -> Option<&'static str> {
            $(if number == tags::message::$name { return Some(stringify!($name)); })*
            None
        }
    };
}

message_field_names!(
    CONVERSATION SENDER_KEY_DISTRIBUTION_MESSAGE IMAGE_MESSAGE CONTACT_MESSAGE LOCATION_MESSAGE
    EXTENDED_TEXT_MESSAGE DOCUMENT_MESSAGE AUDIO_MESSAGE VIDEO_MESSAGE CALL CHAT PROTOCOL_MESSAGE
    CONTACTS_ARRAY_MESSAGE HIGHLY_STRUCTURED_MESSAGE FAST_RATCHET_KEY_SENDER_KEY_DISTRIBUTION_MESSAGE
    SEND_PAYMENT_MESSAGE LIVE_LOCATION_MESSAGE REQUEST_PAYMENT_MESSAGE DECLINE_PAYMENT_REQUEST_MESSAGE
    CANCEL_PAYMENT_REQUEST_MESSAGE TEMPLATE_MESSAGE STICKER_MESSAGE GROUP_INVITE_MESSAGE
    TEMPLATE_BUTTON_REPLY_MESSAGE PRODUCT_MESSAGE DEVICE_SENT_MESSAGE MESSAGE_CONTEXT_INFO LIST_MESSAGE
    VIEW_ONCE_MESSAGE ORDER_MESSAGE LIST_RESPONSE_MESSAGE EPHEMERAL_MESSAGE INVOICE_MESSAGE BUTTONS_MESSAGE
    BUTTONS_RESPONSE_MESSAGE PAYMENT_INVITE_MESSAGE INTERACTIVE_MESSAGE REACTION_MESSAGE
    STICKER_SYNC_RMR_MESSAGE INTERACTIVE_RESPONSE_MESSAGE POLL_CREATION_MESSAGE POLL_UPDATE_MESSAGE
    KEEP_IN_CHAT_MESSAGE DOCUMENT_WITH_CAPTION_MESSAGE REQUEST_PHONE_NUMBER_MESSAGE VIEW_ONCE_MESSAGE_V2
    ENC_REACTION_MESSAGE EDITED_MESSAGE VIEW_ONCE_MESSAGE_V2_EXTENSION POLL_CREATION_MESSAGE_V2
    SCHEDULED_CALL_CREATION_MESSAGE GROUP_MENTIONED_MESSAGE PIN_IN_CHAT_MESSAGE POLL_CREATION_MESSAGE_V3
    SCHEDULED_CALL_EDIT_MESSAGE PTV_MESSAGE BOT_INVOKE_MESSAGE CALL_LOG_MESSSAGE MESSAGE_HISTORY_BUNDLE
    ENC_COMMENT_MESSAGE BCALL_MESSAGE LOTTIE_STICKER_MESSAGE EVENT_MESSAGE ENC_EVENT_RESPONSE_MESSAGE
    COMMENT_MESSAGE NEWSLETTER_ADMIN_INVITE_MESSAGE PLACEHOLDER_MESSAGE SECRET_ENCRYPTED_MESSAGE
    ALBUM_MESSAGE EVENT_COVER_IMAGE STICKER_PACK_MESSAGE STATUS_MENTION_MESSAGE POLL_RESULT_SNAPSHOT_MESSAGE
    POLL_CREATION_OPTION_IMAGE_MESSAGE ASSOCIATED_CHILD_MESSAGE GROUP_STATUS_MENTION_MESSAGE
    POLL_CREATION_MESSAGE_V4 STATUS_ADD_YOURS GROUP_STATUS_MESSAGE RICH_RESPONSE_MESSAGE
    STATUS_NOTIFICATION_MESSAGE LIMIT_SHARING_MESSAGE BOT_TASK_MESSAGE QUESTION_MESSAGE
    MESSAGE_HISTORY_NOTICE GROUP_STATUS_MESSAGE_V2 BOT_FORWARDED_MESSAGE STATUS_QUESTION_ANSWER_MESSAGE
    QUESTION_REPLY_MESSAGE QUESTION_RESPONSE_MESSAGE STATUS_QUOTED_MESSAGE STATUS_STICKER_INTERACTION_MESSAGE
    POLL_CREATION_MESSAGE_V5 NEWSLETTER_FOLLOWER_INVITE_MESSAGE_V2 POLL_RESULT_SNAPSHOT_MESSAGE_V3
    NEWSLETTER_ADMIN_PROFILE_MESSAGE NEWSLETTER_ADMIN_PROFILE_MESSAGE_V2 SPOILER_MESSAGE
    POLL_CREATION_MESSAGE_V6 CONDITIONAL_REVEAL_MESSAGE POLL_ADD_OPTION_MESSAGE EVENT_INVITE_MESSAGE
    GROUP_ROOT_KEY_SHARE PAYMENT_REMINDER_MESSAGE SPLIT_PAYMENT_MESSAGE NEWSLETTER_ADMIN_PROFILE_STATUS_MESSAGE
    ROOT_SECRET_DISTRIBUTE_MESSAGE
);

fn read_varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        value |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Field names (or `field#N`) of the top-level fields in an encoded message, in order,
/// without repeats. Stops at anything malformed.
fn top_level_fields(bytes: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let Some(key) = read_varint(bytes, &mut pos) else { break };
        let number = (key >> 3) as u32;
        let skip = match key & 7 {
            0 => read_varint(bytes, &mut pos).map(|_| 0),
            1 => Some(8),
            2 => read_varint(bytes, &mut pos),
            5 => Some(4),
            _ => None,
        };
        let Some(skip) = skip.and_then(|s| usize::try_from(s).ok()) else { break };
        pos = pos.saturating_add(skip);
        let name = message_field_name(number).map_or_else(|| format!("field#{number}"), str::to_ascii_lowercase);
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Text shown for a body in the chat list preview, reply previews and notifications.
pub fn preview_text(body: &MessageBody) -> Cow<'_, str> {
    match body {
        MessageBody::Text(t) => Cow::Borrowed(t),
        MessageBody::Unsupported { label } => Cow::Borrowed(label),
        MessageBody::Image(i) => match &i.caption {
            Some(caption) => Cow::Owned(format!("📷 {caption}")),
            None => Cow::Borrowed("📷 Photo"),
        },
        MessageBody::Voice(v) if v.ptt => Cow::Borrowed("🎤 Voice message"),
        MessageBody::Voice(_) => Cow::Borrowed("🎵 Audio"),
        MessageBody::Video(v) => {
            let kind = if v.round { "🎥 Video message" } else if v.gif { "🎞️ GIF" } else { "🎥 Video" };
            match &v.caption {
                Some(c) => Cow::Owned(format!("{kind}: {c}")),
                None => Cow::Borrowed(kind),
            }
        }
        MessageBody::Document(d) => Cow::Owned(format!("📄 {}", d.file_name)),
        MessageBody::Sticker(_) => Cow::Borrowed("Sticker"),
        MessageBody::Location(l) => match &l.name {
            Some(n) => Cow::Owned(format!("📍 {n}")),
            None if l.live => Cow::Borrowed("📍 Live location"),
            None => Cow::Borrowed("📍 Location"),
        },
        MessageBody::Contacts(c) => match c.as_slice() {
            [one] => Cow::Owned(format!("👤 {}", one.display_name)),
            many => Cow::Owned(format!("👤 {} contacts", many.len())),
        },
        MessageBody::Poll(p) => Cow::Owned(format!("📊 {}", p.question)),
        MessageBody::Rich(r) => Cow::Borrowed(r.body.as_str()),
        MessageBody::System { text } => Cow::Borrowed(text.as_str()),
        MessageBody::ViewOnce(kind) => Cow::Borrowed(view_once_label(*kind)),
    }
}

pub fn view_once_label(kind: ViewOnceKind) -> &'static str {
    match kind {
        ViewOnceKind::Photo => "📷 View once photo",
        ViewOnceKind::Video => "🎥 View once video",
        ViewOnceKind::Voice => "🎤 View once voice message",
    }
}

/// Which proto a [`StoredMedia`] holds (the store keys its `kind` column on this).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    /// Round video note (`ptvMessage`).
    Ptv,
    Document,
    Sticker,
}

/// Downloadable media of a message, kept in the store so it can be fetched later.
/// `context_info` is stripped (the quote is stored separately).
#[derive(Clone, Debug, PartialEq)]
pub enum StoredMedia {
    Image(Box<wa::message::ImageMessage>),
    Audio(Box<wa::message::AudioMessage>),
    Video(Box<wa::message::VideoMessage>),
    Ptv(Box<wa::message::VideoMessage>),
    Document(Box<wa::message::DocumentMessage>),
    Sticker(Box<wa::message::StickerMessage>),
}

/// Apply `$body` to the proto inside any variant.
macro_rules! each_media {
    ($self:expr, $m:ident => $body:expr) => {
        match $self {
            StoredMedia::Image($m) => $body,
            StoredMedia::Audio($m) => $body,
            StoredMedia::Video($m) | StoredMedia::Ptv($m) => $body,
            StoredMedia::Document($m) => $body,
            StoredMedia::Sticker($m) => $body,
        }
    };
}

impl StoredMedia {
    pub fn kind(&self) -> MediaKind {
        match self {
            StoredMedia::Image(_) => MediaKind::Image,
            StoredMedia::Audio(_) => MediaKind::Audio,
            StoredMedia::Video(_) => MediaKind::Video,
            StoredMedia::Ptv(_) => MediaKind::Ptv,
            StoredMedia::Document(_) => MediaKind::Document,
            StoredMedia::Sticker(_) => MediaKind::Sticker,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        each_media!(self, m => m.encode_to_vec())
    }

    pub fn decode(bytes: &[u8], kind: MediaKind) -> Option<Self> {
        use wa::message as p;
        Some(match kind {
            MediaKind::Image => StoredMedia::Image(Box::new(p::ImageMessage::decode_from_slice(bytes).ok()?)),
            MediaKind::Audio => StoredMedia::Audio(Box::new(p::AudioMessage::decode_from_slice(bytes).ok()?)),
            MediaKind::Video => StoredMedia::Video(Box::new(p::VideoMessage::decode_from_slice(bytes).ok()?)),
            MediaKind::Ptv => StoredMedia::Ptv(Box::new(p::VideoMessage::decode_from_slice(bytes).ok()?)),
            MediaKind::Document => {
                StoredMedia::Document(Box::new(p::DocumentMessage::decode_from_slice(bytes).ok()?))
            }
            MediaKind::Sticker => StoredMedia::Sticker(Box::new(p::StickerMessage::decode_from_slice(bytes).ok()?)),
        })
    }

    /// The proto's own `viewOnce` flag (photos, videos, audio).
    pub fn view_once(&self) -> Option<ViewOnceKind> {
        let (flag, kind) = match self {
            StoredMedia::Image(m) => (m.view_once, ViewOnceKind::Photo),
            StoredMedia::Video(m) | StoredMedia::Ptv(m) => (m.view_once, ViewOnceKind::Video),
            StoredMedia::Audio(m) => (m.view_once, ViewOnceKind::Voice),
            StoredMedia::Document(_) | StoredMedia::Sticker(_) => return None,
        };
        (flag == Some(true)).then_some(kind)
    }

    pub fn mimetype(&self) -> Option<&str> {
        each_media!(self, m => m.mimetype.as_deref())
    }

    /// Size of the decrypted file, when the message says.
    pub fn file_length(&self) -> Option<u64> {
        each_media!(self, m => m.file_length)
    }

    pub fn media_key(&self) -> Option<&[u8]> {
        each_media!(self, m => m.media_key.as_deref())
    }

    /// After a re-upload. A static URL would take precedence over the new path when
    /// downloading, so it goes.
    pub fn set_direct_path(&mut self, path: String) {
        match self {
            StoredMedia::Image(m) => m.static_url = None,
            StoredMedia::Video(m) | StoredMedia::Ptv(m) => m.static_url = None,
            _ => {}
        }
        each_media!(self, m => m.direct_path = Some(path))
    }

    pub fn downloadable(&self) -> &dyn Downloadable {
        each_media!(self, m => m.as_ref() as &dyn Downloadable)
    }

    /// Lottie stickers are zipped vector animations we can't render; only their PNG
    /// thumbnail (if any) is usable.
    pub fn is_lottie(&self) -> bool {
        matches!(self, StoredMedia::Sticker(s)
            if s.is_lottie == Some(true) || s.mimetype.as_deref().is_some_and(|m| m.contains("was")))
    }

    /// Replace the caption (photo / video / document edits). Returns false for kinds
    /// without one.
    pub fn set_caption(&mut self, caption: Option<String>) -> bool {
        match self {
            StoredMedia::Image(m) => m.caption = caption,
            StoredMedia::Video(m) | StoredMedia::Ptv(m) => m.caption = caption,
            StoredMedia::Document(m) => m.caption = caption,
            StoredMedia::Audio(_) | StoredMedia::Sticker(_) => return false,
        }
        true
    }

    /// The body the UI shows for this media; the one mapping used both for fresh
    /// messages and stored rows.
    pub fn body(&self, path: Option<std::path::PathBuf>, state: MediaState) -> MessageBody {
        let positive = |v: Option<u32>| v.filter(|v| *v > 0);
        match self {
            StoredMedia::Image(m) => MessageBody::Image(ImageMedia {
                path,
                thumbnail: nonempty_bytes(&m.jpeg_thumbnail),
                width: positive(m.width),
                height: positive(m.height),
                caption: nonempty_caption(m.caption.as_deref()),
                state,
            }),
            StoredMedia::Audio(m) => MessageBody::Voice(VoiceMedia {
                path,
                duration_secs: m.seconds.unwrap_or(0),
                waveform: m.waveform.clone().unwrap_or_default(),
                ptt: m.ptt == Some(true),
                state,
            }),
            StoredMedia::Video(m) | StoredMedia::Ptv(m) => MessageBody::Video(VideoMedia {
                path,
                thumbnail: nonempty_bytes(&m.jpeg_thumbnail),
                width: positive(m.width),
                height: positive(m.height),
                duration_secs: m.seconds.unwrap_or(0),
                caption: nonempty_caption(m.caption.as_deref()),
                gif: m.gif_playback == Some(true),
                round: matches!(self, StoredMedia::Ptv(_)),
                size_bytes: m.file_length.filter(|l| *l > 0),
                state,
            }),
            StoredMedia::Document(m) => MessageBody::Document(DocumentMedia {
                path,
                file_name: document_name(m),
                mimetype: nonempty(&m.mimetype),
                size_bytes: m.file_length.filter(|l| *l > 0),
                page_count: positive(m.page_count),
                thumbnail: nonempty_bytes(&m.jpeg_thumbnail),
                caption: nonempty_caption(m.caption.as_deref()),
                state,
            }),
            StoredMedia::Sticker(m) => {
                let lottie = self.is_lottie();
                // Nothing we could ever show: don't offer a download.
                let state = if lottie && nonempty_bytes(&m.png_thumbnail).is_none() && path.is_none() {
                    MediaState::Failed
                } else {
                    state
                };
                MessageBody::Sticker(StickerMedia {
                    path,
                    width: positive(m.width),
                    height: positive(m.height),
                    animated: m.is_animated == Some(true) && !lottie,
                    state,
                })
            }
        }
    }

    /// As a standalone message (e.g. the `quotedMessage` of a reply), with `caption`.
    pub fn to_message(&self, caption: Option<&str>) -> wa::Message {
        let mut media = self.clone();
        media.set_caption(caption.map(str::to_owned));
        match media {
            StoredMedia::Image(m) => wa::Message { image_message: MessageField::some(*m), ..Default::default() },
            StoredMedia::Audio(m) => wa::Message { audio_message: MessageField::some(*m), ..Default::default() },
            StoredMedia::Video(m) => wa::Message { video_message: MessageField::some(*m), ..Default::default() },
            StoredMedia::Ptv(m) => wa::Message { ptv_message: MessageField::some(*m), ..Default::default() },
            StoredMedia::Document(m) => {
                wa::Message { document_message: MessageField::some(*m), ..Default::default() }
            }
            StoredMedia::Sticker(m) => wa::Message { sticker_message: MessageField::some(*m), ..Default::default() },
        }
    }
}

/// Display name of a document: its file name, else its title.
pub fn document_name(m: &wa::message::DocumentMessage) -> String {
    nonempty(&m.file_name).or_else(|| nonempty(&m.title)).unwrap_or_else(|| "Document".to_owned())
}

/// The downloadable media an (unwrapped) message carries, if any.
fn media_in(m: &wa::Message, lottie: bool) -> Option<StoredMedia> {
    macro_rules! stripped {
        ($field:ident, $variant:ident) => {
            if let Some(x) = m.$field.as_option() {
                let mut x = x.clone();
                x.context_info = Default::default();
                return Some(StoredMedia::$variant(Box::new(x)));
            }
        };
    }
    stripped!(image_message, Image);
    stripped!(video_message, Video);
    stripped!(ptv_message, Ptv);
    stripped!(audio_message, Audio);
    stripped!(document_message, Document);
    if let Some(s) = m.sticker_message.as_option() {
        let mut s = s.clone();
        s.context_info = Default::default();
        if lottie {
            s.is_lottie = Some(true);
        }
        return Some(StoredMedia::Sticker(Box::new(s)));
    }
    None
}

/// The downloadable media a message carries, if any.
/// View-once media is never kept.
pub fn media_of(msg: &wa::Message) -> Option<StoredMedia> {
    let u = unwrap(msg);
    if view_once_kind(&u).is_some() {
        return None;
    }
    media_in(u.msg, u.lottie)
}

/// What a reply points at, from the message's `contextInfo`.
#[derive(Clone, Debug, PartialEq)]
pub struct QuotedRef {
    /// Id of the quoted message.
    pub id: String,
    /// Sender of the quoted message as sent on the wire (may be a LID, may be absent).
    pub participant: Option<String>,
    /// Short text for the reply preview.
    pub preview: String,
}

fn context_info(m: &wa::Message) -> Option<&wa::ContextInfo> {
    macro_rules! ctx {
        ($($field:ident),* $(,)?) => {
            $(if let Some(ci) = m.$field.as_option().and_then(|x| x.context_info.as_option()) {
                return Some(ci);
            })*
        };
    }
    ctx!(
        extended_text_message,
        image_message,
        audio_message,
        video_message,
        ptv_message,
        document_message,
        sticker_message,
        contact_message,
        contacts_array_message,
        location_message,
        live_location_message,
        poll_creation_message,
        poll_creation_message_v2,
        poll_creation_message_v3,
        poll_creation_message_v5,
        poll_creation_message_v6,
        template_message,
        buttons_message,
        list_message,
        interactive_message,
        product_message,
        buttons_response_message,
        list_response_message,
        template_button_reply_message,
        interactive_response_message,
        event_message,
        group_invite_message,
    );
    None
}

/// The message carries WhatsApp's "Forwarded" mark.
pub fn is_forwarded(msg: &wa::Message) -> bool {
    context_info(unwrap_message(msg)).is_some_and(|ci| ci.is_forwarded == Some(true))
}

/// The quoted message of a reply, if this message is one.
pub fn quoted_of(msg: &wa::Message) -> Option<QuotedRef> {
    let ci = context_info(unwrap_message(msg))?;
    let id = ci.stanza_id.clone().filter(|id| !id.is_empty())?;
    let preview = ci
        .quoted_message
        .as_option()
        .and_then(body_of)
        .map(|b| preview_text(&b).into_owned())
        .unwrap_or_else(|| "Message".to_owned());
    Some(QuotedRef { id, participant: ci.participant.clone().filter(|p| !p.is_empty()), preview })
}

/// Chats we never show: status updates, newsletters, broadcast lists.
pub fn is_hidden_chat(jid: &str) -> bool {
    jid.ends_with("@broadcast") || jid.ends_with("@newsletter") || jid.is_empty()
}

pub fn is_group_jid(jid: &str) -> bool {
    jid.ends_with("@g.us")
}

pub fn is_lid_jid(jid: &str) -> bool {
    jid.ends_with("@lid")
}

/// Strip the agent and device parts of a JID string (`user.agent:device@server`):
/// `123:4@s.whatsapp.net` -> `123@s.whatsapp.net`.
pub fn non_ad(jid: &str) -> String {
    match jid.split_once('@') {
        Some((user, server)) => {
            let user = user.split_once(':').map_or(user, |(u, _)| u);
            let user = user.split_once('.').map_or(user, |(u, _)| u);
            format!("{user}@{server}")
        }
        None => jid.to_owned(),
    }
}

/// Last-resort display name: the phone number for PN JIDs, otherwise the user part.
pub fn fallback_name(jid: &str) -> String {
    let user = jid.split_once('@').map_or(jid, |(u, _)| u);
    if jid.ends_with("@s.whatsapp.net") && user.chars().all(|c| c.is_ascii_digit()) {
        format!("+{user}")
    } else if is_group_jid(jid) {
        "Group".to_owned()
    } else {
        user.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::prelude::MessageField;

    fn text(t: &str) -> wa::Message {
        wa::Message { conversation: Some(t.into()), ..Default::default() }
    }

    #[test]
    fn plain_and_extended_text() {
        assert_eq!(classify(&text("hi")), Extracted::Body(MessageBody::Text("hi".into())));
        let ext = wa::Message {
            extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                text: Some("link https://x".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(classify(&ext), Extracted::Body(MessageBody::Text("link https://x".into())));
    }

    #[test]
    fn wrapped_text_is_unwrapped() {
        let eph = wa::Message {
            ephemeral_message: MessageField::some(wa::message::FutureProofMessage {
                message: MessageField::some(text("secret")),
            }),
            ..Default::default()
        };
        assert_eq!(classify(&eph), Extracted::Body(MessageBody::Text("secret".into())));
    }

    fn wrap(inner: wa::Message) -> MessageField<wa::message::FutureProofMessage> {
        MessageField::some(wa::message::FutureProofMessage { message: MessageField::some(inner) })
    }

    #[test]
    fn video_gif_and_round_video() {
        let proto = wa::message::VideoMessage {
            caption: Some(" clip ".into()),
            seconds: Some(42),
            width: Some(1280),
            height: Some(720),
            jpeg_thumbnail: Some(vec![0xFF, 0xD8]),
            mimetype: Some("video/mp4".into()),
            direct_path: Some("/v/x".into()),
            ..Default::default()
        };
        let video = wa::Message { video_message: MessageField::some(proto.clone()), ..Default::default() };
        let expected = VideoMedia {
            path: None,
            thumbnail: Some(vec![0xFF, 0xD8]),
            width: Some(1280),
            height: Some(720),
            duration_secs: 42,
            caption: Some("clip".into()),
            gif: false,
            round: false,
            size_bytes: None,
            state: MediaState::Idle,
        };
        assert_eq!(body_of(&video), Some(MessageBody::Video(expected.clone())));
        assert_eq!(preview_text(&body_of(&video).unwrap()), "🎥 Video: clip");
        let media = media_of(&video).unwrap();
        assert_eq!(media.kind(), MediaKind::Video);
        assert_eq!(StoredMedia::decode(&media.encode(), MediaKind::Video), Some(media));

        let gif = wa::Message {
            video_message: MessageField::some(wa::message::VideoMessage {
                gif_playback: Some(true),
                ..proto.clone()
            }),
            ..Default::default()
        };
        assert!(matches!(body_of(&gif), Some(MessageBody::Video(v)) if v.gif && !v.round));

        // PTV: a top-level VideoMessage in its own field.
        let ptv = wa::Message { ptv_message: MessageField::some(proto.clone()), ..Default::default() };
        assert!(matches!(body_of(&ptv), Some(MessageBody::Video(v)) if v.round && v.duration_secs == 42));
        let media = media_of(&ptv).unwrap();
        assert_eq!(media.kind(), MediaKind::Ptv);
        assert!(media.to_message(None).ptv_message.is_set());
        assert_eq!(preview_text(&body_of(&ptv).unwrap()), "🎥 Video message: clip");
        // View-once wrapped (like whatsapp-rust's `is_view_once`): a placeholder, no media.
        let once = wa::Message { view_once_message_v2: wrap(ptv), ..Default::default() };
        assert_eq!(body_of(&once), Some(MessageBody::ViewOnce(ViewOnceKind::Video)));
        assert!(media_of(&once).is_none());
    }

    /// View-once photos / videos / voice notes, by wrapper or by their own flag, become
    /// placeholders: no media to download, no caption anywhere (quotes, previews).
    #[test]
    fn view_once_media_is_a_placeholder() {
        let image = wa::message::ImageMessage {
            caption: Some("private caption".into()),
            direct_path: Some("/v/x".into()),
            media_key: Some(vec![1; 32]),
            ..Default::default()
        };
        let flagged = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage { view_once: Some(true), ..image.clone() }),
            ..Default::default()
        };
        let wrapped_ephemeral = wa::Message {
            ephemeral_message: wrap(wa::Message {
                view_once_message: wrap(wa::Message { image_message: MessageField::some(image), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let voice = wa::Message {
            view_once_message_v2_extension: wrap(wa::Message {
                audio_message: MessageField::some(wa::message::AudioMessage { ptt: Some(true), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        for (m, kind) in [(&flagged, ViewOnceKind::Photo), (&wrapped_ephemeral, ViewOnceKind::Photo), (&voice, ViewOnceKind::Voice)] {
            assert_eq!(classify(m), Extracted::Body(MessageBody::ViewOnce(kind)));
            assert!(media_of(m).is_none());
            assert!(!preview_text(&body_of(m).unwrap()).contains("private"));
        }
        // Quoting one doesn't leak the caption either.
        let reply = wa::Message {
            extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                text: Some("wow".into()),
                context_info: MessageField::some(wa::ContextInfo {
                    stanza_id: Some("Q".into()),
                    quoted_message: MessageField::some(flagged),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(quoted_of(&reply).unwrap().preview, "📷 View once photo");
    }

    #[test]
    fn disappearing_timer_from_context_info() {
        let m = wa::Message {
            ephemeral_message: wrap(wa::Message {
                extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                    text: Some("poof".into()),
                    context_info: MessageField::some(wa::ContextInfo { expiration: Some(86_400), ..Default::default() }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(expiration_of(&m), Some(86_400));
        assert_eq!(expiration_of(&wa::Message { conversation: Some("x".into()), ..Default::default() }), None);
    }

    #[test]
    fn documents_with_and_without_caption_wrapper() {
        let doc = wa::message::DocumentMessage {
            file_name: Some("report.pdf".into()),
            title: Some("Report".into()),
            mimetype: Some("application/pdf".into()),
            file_length: Some(123_456),
            page_count: Some(3),
            jpeg_thumbnail: Some(vec![1, 2, 3]),
            ..Default::default()
        };
        let plain = wa::Message { document_message: MessageField::some(doc.clone()), ..Default::default() };
        assert_eq!(
            body_of(&plain),
            Some(MessageBody::Document(DocumentMedia {
                path: None,
                file_name: "report.pdf".into(),
                mimetype: Some("application/pdf".into()),
                size_bytes: Some(123_456),
                page_count: Some(3),
                thumbnail: Some(vec![1, 2, 3]),
                caption: None,
                state: MediaState::Idle,
            }))
        );
        // documentWithCaptionMessage inside an ephemeral wrapper.
        let captioned = wa::Message {
            ephemeral_message: wrap(wa::Message {
                document_with_caption_message: wrap(wa::Message {
                    document_message: MessageField::some(wa::message::DocumentMessage {
                        caption: Some("Q3 numbers".into()),
                        file_name: None,
                        ..doc
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let Some(MessageBody::Document(d)) = body_of(&captioned) else { panic!("not a document") };
        assert_eq!((d.file_name.as_str(), d.caption.as_deref()), ("Report", Some("Q3 numbers")));
        assert_eq!(media_of(&captioned).unwrap().kind(), MediaKind::Document);
        assert_eq!(preview_text(&MessageBody::Document(d)), "📄 Report");
    }

    #[test]
    fn stickers_including_lottie() {
        let sticker = wa::message::StickerMessage {
            width: Some(512),
            height: Some(512),
            is_animated: Some(true),
            mimetype: Some("image/webp".into()),
            ..Default::default()
        };
        let msg = wa::Message { sticker_message: MessageField::some(sticker.clone()), ..Default::default() };
        assert_eq!(
            body_of(&msg),
            Some(MessageBody::Sticker(StickerMedia {
                path: None,
                width: Some(512),
                height: Some(512),
                animated: true,
                state: MediaState::Idle,
            }))
        );
        assert!(!media_of(&msg).unwrap().is_lottie());

        // Lottie: wrapper marks it; nothing showable without a PNG preview.
        let lottie = wa::Message {
            lottie_sticker_message: wrap(wa::Message {
                sticker_message: MessageField::some(wa::message::StickerMessage {
                    mimetype: Some("application/was".into()),
                    ..sticker.clone()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let Some(MessageBody::Sticker(s)) = body_of(&lottie) else { panic!("not a sticker") };
        assert!(!s.animated);
        assert_eq!(s.state, MediaState::Failed);
        let media = media_of(&lottie).unwrap();
        assert!(media.is_lottie());
        // ... but with one it can be "downloaded" (the preview is used).
        let with_preview = wa::Message {
            lottie_sticker_message: wrap(wa::Message {
                sticker_message: MessageField::some(wa::message::StickerMessage {
                    png_thumbnail: Some(vec![0x89, b'P']),
                    ..sticker
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(body_of(&with_preview), Some(MessageBody::Sticker(s)) if s.state == MediaState::Idle));
        assert!(media_of(&with_preview).unwrap().is_lottie());
    }

    #[test]
    fn locations() {
        let loc = wa::Message {
            location_message: MessageField::some(wa::message::LocationMessage {
                degrees_latitude: Some(52.37),
                degrees_longitude: Some(4.89),
                name: Some("Dam Square".into()),
                address: Some("Amsterdam".into()),
                jpeg_thumbnail: Some(vec![9]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            body_of(&loc),
            Some(MessageBody::Location(LocationInfo {
                latitude: 52.37,
                longitude: 4.89,
                name: Some("Dam Square".into()),
                address: Some("Amsterdam".into()),
                thumbnail: Some(vec![9]),
                live: false,
            }))
        );
        let live = wa::Message {
            live_location_message: MessageField::some(wa::message::LiveLocationMessage {
                degrees_latitude: Some(1.5),
                degrees_longitude: Some(-2.5),
                jpeg_thumbnail: Some(vec![]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let Some(body) = body_of(&live) else { panic!("no body") };
        assert!(matches!(&body, MessageBody::Location(l) if l.live && l.thumbnail.is_none() && l.longitude == -2.5));
        assert_eq!(preview_text(&body), "📍 Live location");
    }

    #[test]
    fn vcard_parsing() {
        let vcard = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Doe;Jane;;;\r\nFN:Jane\\, \"JD\" Doe\r\n\
            item1.TEL;type=CELL;type=VOICE;waid=15551234567:+1 555-123-\r\n 4567\r\n\
            item1.X-ABLabel:Mobile\r\n\
            TEL;TYPE=work;waid=4915112345678:\r\n\
            tel;type=home:tel:+44 20 7946 0000\r\n\
            TEL:+1 555-123-4567\r\nEND:VCARD\r\n";
        assert_eq!(
            parse_vcard(vcard),
            VCard {
                name: Some("Jane, \"JD\" Doe".into()),
                phones: vec!["+1 555-123-4567".into(), "+4915112345678".into(), "+44 20 7946 0000".into()],
            }
        );
        assert_eq!(parse_vcard(""), VCard::default());
        assert_eq!(parse_vcard("garbage\nTEL;waid=:"), VCard::default());
    }

    #[test]
    fn contacts_single_and_array() {
        let card = |name: Option<&str>, vcard: &str| wa::message::ContactMessage {
            display_name: name.map(Into::into),
            vcard: Some(vcard.into()),
            ..Default::default()
        };
        let one = wa::Message {
            contact_message: MessageField::some(card(
                Some("Mom"),
                "BEGIN:VCARD\nFN:Mother\nTEL;waid=31612345678:+31 6 12345678\nEND:VCARD",
            )),
            ..Default::default()
        };
        assert_eq!(
            body_of(&one),
            Some(MessageBody::Contacts(vec![ContactCard {
                display_name: "Mom".into(),
                phones: vec!["+31 6 12345678".into()],
            }]))
        );
        let many = wa::Message {
            contacts_array_message: MessageField::some(wa::message::ContactsArrayMessage {
                display_name: Some("2 contacts".into()),
                contacts: vec![
                    card(None, "BEGIN:VCARD\nFN:From vCard\nEND:VCARD"),
                    card(Some(""), "BEGIN:VCARD\nTEL;waid=1555:\nEND:VCARD"),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let Some(body) = body_of(&many) else { panic!("no body") };
        assert_eq!(
            body,
            MessageBody::Contacts(vec![
                ContactCard { display_name: "From vCard".into(), phones: vec![] },
                ContactCard { display_name: "+1555".into(), phones: vec!["+1555".into()] },
            ])
        );
        assert_eq!(preview_text(&body), "👤 2 contacts");
    }

    #[test]
    fn polls_all_versions() {
        let poll = |count: Option<u32>| wa::message::PollCreationMessage {
            name: Some("Lunch?".into()),
            options: ["Pizza", "", "Sushi"]
                .iter()
                .map(|o| wa::message::poll_creation_message::Option {
                    option_name: Some((*o).into()),
                    ..Default::default()
                })
                .collect(),
            selectable_options_count: count,
            ..Default::default()
        };
        let expect = |multiple| {
            Some(MessageBody::Poll(PollInfo {
                question: "Lunch?".into(),
                options: vec!["Pizza".into(), "Sushi".into()],
                multiple,
                votes: Vec::new(),
            }))
        };
        let v1 = wa::Message { poll_creation_message: MessageField::some(poll(Some(0))), ..Default::default() };
        assert_eq!(body_of(&v1), expect(true));
        let v3 = wa::Message { poll_creation_message_v3: MessageField::some(poll(Some(1))), ..Default::default() };
        assert_eq!(body_of(&v3), expect(false));
        let v5 = wa::Message { poll_creation_message_v5: MessageField::some(poll(Some(2))), ..Default::default() };
        assert_eq!(body_of(&v5), expect(true));
        // v4 is a wrapper around a regular poll.
        let v4 = wa::Message {
            poll_creation_message_v4: wrap(wa::Message {
                poll_creation_message_v2: MessageField::some(poll(Some(1))),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(body_of(&v4), expect(false));
        let v6 = wa::Message { poll_creation_message_v6: MessageField::some(poll(None)), ..Default::default() };
        assert_eq!(preview_text(&body_of(&v6).unwrap()), "📊 Lunch?");
    }

    #[test]
    fn business_template_buttons_list_interactive() {
        use wa::hydrated_template_button::{HydratedButton, HydratedQuickReplyButton, HydratedURLButton};
        use wa::message::template_message::hydrated_four_row_template::Title;
        let hydrated = wa::message::template_message::HydratedFourRowTemplate {
            title: Some(Title::HydratedTitleText("Your bill".into())),
            hydrated_content_text: Some("Amount due: 20 EUR".into()),
            hydrated_footer_text: Some("Telco Inc.".into()),
            hydrated_buttons: vec![
                wa::HydratedTemplateButton {
                    hydrated_button: Some(HydratedButton::UrlButton(Box::new(HydratedURLButton {
                        display_text: Some("Pay".into()),
                        ..Default::default()
                    }))),
                    ..Default::default()
                },
                wa::HydratedTemplateButton {
                    hydrated_button: Some(HydratedButton::QuickReplyButton(Box::new(HydratedQuickReplyButton {
                        display_text: Some("Help".into()),
                        ..Default::default()
                    }))),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let expected = RichText {
            header: Some("Your bill".into()),
            body: "Amount due: 20 EUR".into(),
            footer: Some("Telco Inc.".into()),
            buttons: vec!["Pay".into(), "Help".into()],
        };
        let template = wa::Message {
            template_message: MessageField::some(wa::message::TemplateMessage {
                hydrated_template: MessageField::some(hydrated.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(body_of(&template), Some(MessageBody::Rich(expected.clone())));
        // Same content via the `format` oneof.
        let via_format = wa::Message {
            template_message: MessageField::some(wa::message::TemplateMessage {
                format: Some(wa::message::template_message::Format::HydratedFourRowTemplate(Box::new(hydrated))),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(body_of(&via_format), Some(MessageBody::Rich(expected)));

        let buttons = wa::Message {
            buttons_message: MessageField::some(wa::message::ButtonsMessage {
                header: Some(wa::message::buttons_message::Header::Text("Hi!".into())),
                content_text: Some("Choose one".into()),
                footer_text: Some("Bot".into()),
                buttons: vec![wa::message::buttons_message::Button {
                    button_text: MessageField::some(wa::message::buttons_message::button::ButtonText {
                        display_text: Some("Yes".into()),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            body_of(&buttons),
            Some(MessageBody::Rich(RichText {
                header: Some("Hi!".into()),
                body: "Choose one".into(),
                footer: Some("Bot".into()),
                buttons: vec!["Yes".into()],
            }))
        );

        let row = |t: &str| wa::message::list_message::Row { title: Some(t.into()), ..Default::default() };
        let list = wa::Message {
            list_message: MessageField::some(wa::message::ListMessage {
                title: Some("Menu".into()),
                description: Some("Pick a plan".into()),
                button_text: Some("Plans".into()),
                sections: vec![
                    wa::message::list_message::Section { title: Some("A".into()), rows: vec![row("Basic"), row("Pro")] },
                    wa::message::list_message::Section { title: None, rows: vec![row("Max")] },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let Some(MessageBody::Rich(r)) = body_of(&list) else { panic!("not rich") };
        assert_eq!((r.header.as_deref(), r.body.as_str()), (Some("Menu"), "Pick a plan"));
        assert_eq!(r.buttons, vec!["Basic", "Pro", "Max"]);

        use wa::message::interactive_message as im;
        let interactive = wa::Message {
            // Bots wrap theirs.
            bot_invoke_message: wrap(wa::Message {
                interactive_message: MessageField::some(wa::message::InteractiveMessage {
                    header: MessageField::some(im::Header { title: Some("Offer".into()), ..Default::default() }),
                    body: MessageField::some(im::Body { text: Some("10 GB for 5 EUR".into()) }),
                    footer: MessageField::some(im::Footer { text: Some("Reply STOP".into()), ..Default::default() }),
                    interactive_message: Some(im::InteractiveMessage::NativeFlowMessage(Box::new(
                        im::NativeFlowMessage {
                            buttons: vec![
                                im::native_flow_message::NativeFlowButton {
                                    name: Some("cta_url".into()),
                                    button_params_json: Some(
                                        r#"{"display_text" : "Buy \"now\" \ud83d\ude80","url":"https://x"}"#.into(),
                                    ),
                                },
                                im::native_flow_message::NativeFlowButton {
                                    name: Some("quick_reply".into()),
                                    button_params_json: Some(r#"{"id":"1"}"#.into()),
                                },
                            ],
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            body_of(&interactive),
            Some(MessageBody::Rich(RichText {
                header: Some("Offer".into()),
                body: "10 GB for 5 EUR".into(),
                footer: Some("Reply STOP".into()),
                buttons: vec!["Buy \"now\" 🚀".into()],
            }))
        );

        // Header only: promoted to the body. Shop: labelled.
        let shop = wa::Message {
            interactive_message: MessageField::some(wa::message::InteractiveMessage {
                interactive_message: Some(im::InteractiveMessage::ShopStorefrontMessage(Box::default())),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(body_of(&shop), Some(MessageBody::Rich(r)) if r.body == "🛍️ Shop"));
        let header_only = wa::Message {
            buttons_message: MessageField::some(wa::message::ButtonsMessage {
                header: Some(wa::message::buttons_message::Header::Text("Only a title".into())),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            matches!(body_of(&header_only), Some(MessageBody::Rich(r)) if r.body == "Only a title" && r.header.is_none())
        );
    }

    #[test]
    fn button_and_list_replies_are_text() {
        let buttons = wa::Message {
            buttons_response_message: MessageField::some(wa::message::ButtonsResponseMessage {
                response: Some(wa::message::buttons_response_message::Response::SelectedDisplayText("Yes".into())),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(body_of(&buttons), Some(MessageBody::Text("Yes".into())));
        let list = wa::Message {
            list_response_message: MessageField::some(wa::message::ListResponseMessage {
                title: Some("Pro plan".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(body_of(&list), Some(MessageBody::Text("Pro plan".into())));
        let template = wa::Message {
            template_button_reply_message: MessageField::some(wa::message::TemplateButtonReplyMessage {
                selected_display_text: Some("Help".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(body_of(&template), Some(MessageBody::Text("Help".into())));
        let native = wa::Message {
            interactive_response_message: MessageField::some(wa::message::InteractiveResponseMessage {
                body: MessageField::some(wa::message::interactive_response_message::Body {
                    text: Some("Buy now".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(body_of(&native), Some(MessageBody::Text("Buy now".into())));
    }

    #[test]
    fn system_lines() {
        use wa::message::call_log_message::CallOutcome;
        let call = |outcome, video, secs| wa::Message {
            call_log_messsage: MessageField::some(wa::message::CallLogMessage {
                is_video: Some(video),
                call_outcome: Some(outcome),
                duration_secs: secs,
                ..Default::default()
            }),
            ..Default::default()
        };
        let system = |m: &wa::Message| match body_of(m) {
            Some(MessageBody::System { text }) => text,
            other => panic!("not a system line: {other:?}"),
        };
        assert_eq!(system(&call(CallOutcome::MISSED, false, None)), "📞 Missed voice call");
        assert_eq!(system(&call(CallOutcome::MISSED, true, None)), "📹 Missed video call");
        assert_eq!(system(&call(CallOutcome::CONNECTED, false, Some(151))), "📞 Voice call · 2:31");
        assert_eq!(system(&call(CallOutcome::REJECTED, true, None)), "📹 Declined video call");

        let event = wa::Message {
            event_message: MessageField::some(wa::message::EventMessage {
                name: Some("Birthday".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(system(&event), "📅 Event: Birthday");
        let invite = wa::Message {
            group_invite_message: MessageField::some(wa::message::GroupInviteMessage {
                group_name: Some("Hiking".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(system(&invite), "👥 Group invite: Hiking");

        use wa::message::pin_in_chat_message::Type as PinType;
        let pin = |t| wa::Message {
            pin_in_chat_message: MessageField::some(wa::message::PinInChatMessage {
                r#type: Some(t),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(system(&pin(PinType::PIN_FOR_ALL)), "📌 Pinned a message");
        assert_eq!(classify(&pin(PinType::UNPIN_FOR_ALL)), Extracted::Skip);
        // No target: not a pin we can apply.
        assert_eq!(pin_of(&pin(PinType::PIN_FOR_ALL)), None);
    }

    #[test]
    fn pins_name_their_message_and_duration() {
        use wa::message::pin_in_chat_message::Type as PinType;
        let pin = |t, secs: Option<u32>| wa::Message {
            pin_in_chat_message: MessageField::some(wa::message::PinInChatMessage {
                key: MessageField::some(wa::MessageKey { id: Some("P1".into()), ..Default::default() }),
                r#type: Some(t),
                sender_timestamp_ms: Some(1_700_000_000_000),
            }),
            message_context_info: MessageField::some(wa::MessageContextInfo {
                message_add_on_duration_in_secs: secs,
                ..Default::default()
            }),
            ..Default::default()
        };
        let change = |duration_secs| {
            Some(PinChange { target_id: "P1".into(), duration_secs, sender_ts_ms: Some(1_700_000_000_000) })
        };
        assert_eq!(pin_of(&pin(PinType::PIN_FOR_ALL, Some(86_400))), change(Some(86_400)));
        assert_eq!(pin_of(&pin(PinType::PIN_FOR_ALL, None)), change(Some(DEFAULT_PIN_SECS)));
        assert_eq!(pin_of(&pin(PinType::UNPIN_FOR_ALL, Some(0))), change(None));
    }

    #[test]
    fn hd_copy_points_at_its_original() {
        use whatsapp_rust::waproto::whatsapp::message_association::AssociationType as Association;
        let associated = |kind| wa::MessageContextInfo {
            message_association: MessageField::some(wa::MessageAssociation {
                association_type: Some(kind),
                parent_message_key: MessageField::some(wa::MessageKey { id: Some("SD".into()), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let photo = || MessageField::some(wa::message::ImageMessage::default());
        // On the message itself, as phones send it to linked devices…
        let hd = wa::Message {
            image_message: photo(),
            message_context_info: MessageField::some(associated(Association::HD_IMAGE_DUAL_UPLOAD)),
            ..Default::default()
        };
        assert_eq!(hd_parent_of(&hd).as_deref(), Some("SD"));
        // …or inside the wrappers.
        let wrapped = wa::Message {
            device_sent_message: MessageField::some(wa::message::DeviceSentMessage {
                message: MessageField::some(wa::Message { associated_child_message: wrap(hd), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(hd_parent_of(&wrapped).as_deref(), Some("SD"));
        // Album photos are messages of their own.
        let album_photo = wa::Message {
            image_message: photo(),
            message_context_info: MessageField::some(associated(Association::MEDIA_ALBUM)),
            ..Default::default()
        };
        assert_eq!(hd_parent_of(&album_photo), None);
        assert_eq!(hd_parent_of(&wa::Message { image_message: photo(), ..Default::default() }), None);
    }

    #[test]
    fn wrappers_reach_inner_content() {
        // groupMentioned > botInvoke > ephemeral > viewOnce > text, under deviceSent.
        let nested = wa::Message {
            device_sent_message: MessageField::some(wa::message::DeviceSentMessage {
                message: MessageField::some(wa::Message {
                    group_mentioned_message: wrap(wa::Message {
                        bot_invoke_message: wrap(wa::Message {
                            ephemeral_message: wrap(wa::Message {
                                view_once_message: wrap(text("deep")),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(classify(&nested), Extracted::Body(MessageBody::Text("deep".into())));

        // An album child, wrapped as newer clients send them; the album header is skipped.
        let child = wa::Message {
            associated_child_message: wrap(wa::Message {
                image_message: MessageField::some(wa::message::ImageMessage {
                    caption: Some("1/2".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            message_context_info: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert!(matches!(classify(&child), Extracted::Body(MessageBody::Image(i)) if i.caption.as_deref() == Some("1/2")));
        assert!(matches!(media_of(&child), Some(StoredMedia::Image(_))));
        let album = wa::Message {
            album_message: MessageField::some(wa::message::AlbumMessage {
                expected_image_count: Some(2),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(classify(&album), Extracted::Skip);

        // An edit wrapped in editedMessage (newer clients).
        let edit = wa::Message {
            edited_message: wrap(wa::Message {
                protocol_message: MessageField::some(wa::message::ProtocolMessage {
                    r#type: Some(ProtocolType::MESSAGE_EDIT),
                    key: MessageField::some(wa::MessageKey { id: Some("E1".into()), ..Default::default() }),
                    edited_message: MessageField::some(text("fixed")),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(classify(&edit), Extracted::Edit { target_id: "E1".into(), body: MessageBody::Text("fixed".into()) });
    }

    #[test]
    fn quotes_of_new_kinds_get_previews() {
        let quoted_poll = wa::Message {
            poll_creation_message_v3: MessageField::some(wa::message::PollCreationMessage {
                name: Some("Lunch?".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        // A reply sent as a location, quoting a poll.
        let reply = wa::Message {
            location_message: MessageField::some(wa::message::LocationMessage {
                context_info: quote("P1", "1@s.whatsapp.net", quoted_poll),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(quoted_of(&reply).unwrap().preview, "📊 Lunch?");
        let doc = wa::Message {
            document_message: MessageField::some(wa::message::DocumentMessage {
                file_name: Some("a.pdf".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let poll_reply = wa::Message {
            poll_creation_message: MessageField::some(wa::message::PollCreationMessage {
                name: Some("q".into()),
                context_info: quote("D1", "1@s.whatsapp.net", doc),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(quoted_of(&poll_reply).unwrap().preview, "📄 a.pdf");
    }

    #[test]
    fn unknown_field_names_for_logging() {
        let order = wa::Message {
            order_message: MessageField::some(wa::message::OrderMessage {
                message: Some("secret order note".into()),
                ..Default::default()
            }),
            message_context_info: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert_eq!(body_of(&order), Some(MessageBody::Unsupported { label: UNSUPPORTED_LABEL.into() }));
        // Names only, ignored fields left out.
        assert_eq!(unknown_fields(&order), vec!["order_message"]);
        let several = wa::Message {
            invoice_message: MessageField::some(Default::default()),
            sticker_pack_message: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert_eq!(unknown_fields(&several), vec!["invoice_message", "sticker_pack_message"]);
        // Field numbers this schema doesn't know (key 999 varint, value 1; then fixed32 #1000).
        assert_eq!(top_level_fields(&[0xB8, 0x3E, 0x01, 0xC5, 0x3E, 0, 0, 0, 0]), vec!["field#999", "field#1000"]);
        assert!(top_level_fields(&[0xFF]).is_empty());
    }

    fn quote(id: &str, participant: &str, quoted: wa::Message) -> MessageField<wa::ContextInfo> {
        MessageField::some(wa::ContextInfo {
            stanza_id: Some(id.into()),
            participant: Some(participant.into()),
            quoted_message: MessageField::some(quoted),
            ..Default::default()
        })
    }

    #[test]
    fn image_and_voice_bodies() {
        let img = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("  ".into()),
                width: Some(640),
                height: Some(480),
                jpeg_thumbnail: Some(vec![0xFF, 0xD8, 1]),
                direct_path: Some("/v/t62/abc".into()),
                media_key: Some(vec![7; 32]),
                mimetype: Some("image/jpeg".into()),
                context_info: quote("Q1", "1@s.whatsapp.net", text("hi")),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            body_of(&img),
            Some(MessageBody::Image(ImageMedia {
                path: None,
                thumbnail: Some(vec![0xFF, 0xD8, 1]),
                width: Some(640),
                height: Some(480),
                caption: None,
                state: MediaState::Idle,
            }))
        );
        // Media proto keeps the download info but not the quote; it round-trips.
        let Some(StoredMedia::Image(stored)) = media_of(&img) else { panic!("no image media") };
        assert_eq!(stored.direct_path.as_deref(), Some("/v/t62/abc"));
        assert!(stored.context_info.as_option().is_none());
        let bytes = StoredMedia::Image(stored.clone()).encode();
        assert_eq!(StoredMedia::decode(&bytes, MediaKind::Image), Some(StoredMedia::Image(stored)));

        let voice = wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage {
                ptt: Some(true),
                seconds: Some(12),
                waveform: Some(vec![0, 50, 100]),
                mimetype: Some("audio/ogg; codecs=opus".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            body_of(&voice),
            Some(MessageBody::Voice(VoiceMedia {
                path: None,
                duration_secs: 12,
                waveform: vec![0, 50, 100],
                ptt: true,
                state: MediaState::Idle,
            }))
        );
        let media = media_of(&voice).unwrap();
        assert_eq!(media.mimetype(), Some("audio/ogg; codecs=opus"));
        assert_eq!(StoredMedia::decode(&media.encode(), MediaKind::Audio), Some(media));
        assert_eq!(preview_text(&body_of(&voice).unwrap()), "🎤 Voice message");
    }

    #[test]
    fn quoted_context_from_text_image_and_audio() {
        let reply = wa::Message {
            extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                text: Some("sure".into()),
                context_info: quote("ABC", "99:3@lid", text("lunch?")),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(classify(&reply), Extracted::Body(MessageBody::Text("sure".into())));
        assert_eq!(
            quoted_of(&reply),
            Some(QuotedRef { id: "ABC".into(), participant: Some("99:3@lid".into()), preview: "lunch?".into() })
        );

        let quoted_photo = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("sunset".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let photo_reply = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                context_info: quote("P1", "1@s.whatsapp.net", quoted_photo),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(quoted_of(&photo_reply).unwrap().preview, "📷 sunset");

        let quoted_voice =
            wa::Message { audio_message: MessageField::some(wa::message::AudioMessage { ptt: Some(true), ..Default::default() }), ..Default::default() };
        let voice_reply = wa::Message {
            audio_message: MessageField::some(wa::message::AudioMessage {
                ptt: Some(true),
                context_info: quote("V1", "2@s.whatsapp.net", quoted_voice),
                ..Default::default()
            }),
            ..Default::default()
        };
        let q = quoted_of(&voice_reply).unwrap();
        assert_eq!((q.id.as_str(), q.preview.as_str()), ("V1", "🎤 Voice message"));

        // Context without a stanza id (e.g. forwarded / ad context) isn't a reply.
        let forwarded = wa::Message {
            extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                text: Some("fwd".into()),
                context_info: MessageField::some(wa::ContextInfo { is_forwarded: Some(true), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(quoted_of(&forwarded), None);
        assert_eq!(quoted_of(&text("plain")), None);
        assert!(is_forwarded(&forwarded));
        assert!(!is_forwarded(&text("plain")));
        assert!(!is_forwarded(&voice_reply));
    }

    fn reaction(id: Option<&str>, text: Option<&str>, ts: Option<i64>) -> wa::Message {
        wa::Message {
            reaction_message: MessageField::some(wa::message::ReactionMessage {
                key: MessageField::some(wa::MessageKey { id: id.map(Into::into), ..Default::default() }),
                text: text.map(Into::into),
                sender_timestamp_ms: ts,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn reactions_add_and_remove() {
        assert_eq!(
            classify(&reaction(Some("T1"), Some("👍"), Some(1_700_000_000_123))),
            Extracted::Reaction { target_id: "T1".into(), emoji: "👍".into(), sender_ts_ms: Some(1_700_000_000_123) }
        );
        // Empty (or missing) text: the reaction was removed.
        assert_eq!(
            classify(&reaction(Some("T1"), Some(""), None)),
            Extracted::Reaction { target_id: "T1".into(), emoji: String::new(), sender_ts_ms: None }
        );
        assert_eq!(
            classify(&reaction(Some("T1"), None, Some(0))),
            Extracted::Reaction { target_id: "T1".into(), emoji: String::new(), sender_ts_ms: None }
        );
        // In a disappearing chat.
        let eph = wa::Message { ephemeral_message: wrap(reaction(Some("T2"), Some("❤️"), None)), ..Default::default() };
        assert!(matches!(classify(&eph), Extracted::Reaction { target_id, .. } if target_id == "T2"));
        // Nothing to attach it to.
        assert_eq!(classify(&reaction(None, Some("👍"), None)), Extracted::Skip);
    }

    #[test]
    fn reactions_and_protocol_are_skipped() {
        // Still encrypted (history copies of community-announcement reactions).
        let enc = wa::Message { enc_reaction_message: MessageField::some(Default::default()), ..Default::default() };
        assert_eq!(classify(&enc), Extracted::Skip);

        let skdm_only = wa::Message {
            sender_key_distribution_message: MessageField::some(Default::default()),
            message_context_info: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert_eq!(classify(&skdm_only), Extracted::Skip);
        assert_eq!(classify(&wa::Message::default()), Extracted::Skip);
    }

    #[test]
    fn unknown_content_is_unsupported() {
        // An empty buttons message has nothing to show either.
        let buttons = wa::Message {
            buttons_message: MessageField::some(Default::default()),
            ..Default::default()
        };
        assert_eq!(body_of(&buttons), Some(MessageBody::Unsupported { label: "Unsupported message".into() }));
    }

    #[test]
    fn revoke_and_edit() {
        let key = wa::MessageKey { id: Some("ABC".into()), ..Default::default() };
        let revoke = wa::Message {
            protocol_message: MessageField::some(wa::message::ProtocolMessage {
                r#type: Some(ProtocolType::REVOKE),
                key: MessageField::some(key.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(classify(&revoke), Extracted::Revoke { target_id: "ABC".into() });

        let edit = wa::Message {
            protocol_message: MessageField::some(wa::message::ProtocolMessage {
                r#type: Some(ProtocolType::MESSAGE_EDIT),
                key: MessageField::some(key),
                edited_message: MessageField::some(text("fixed")),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            classify(&edit),
            Extracted::Edit { target_id: "ABC".into(), body: MessageBody::Text("fixed".into()) }
        );
    }

    #[test]
    fn jid_helpers() {
        assert_eq!(non_ad("123:4@s.whatsapp.net"), "123@s.whatsapp.net");
        assert_eq!(non_ad("99@lid"), "99@lid");
        assert_eq!(non_ad("99.1:2@lid"), "99@lid");
        assert_eq!(non_ad("120363-1@g.us"), "120363-1@g.us");
        assert_eq!(fallback_name("15551234567@s.whatsapp.net"), "+15551234567");
        assert!(is_hidden_chat("status@broadcast"));
        assert!(is_hidden_chat("1@newsletter"));
        assert!(!is_hidden_chat("1@g.us"));
    }
}
