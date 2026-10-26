//! Asking the phone to re-upload media whose CDN link expired ("media retry").
//!
//! whatsapp-rust 0.7.0 has `Client::media_reupload()`, but it addresses the
//! `<receipt type="server-error">` to `device.pn`, which is our own *device* JID
//! (`user:N@s.whatsapp.net`, from pair-success). The server then routes the request to
//! this companion device instead of the phone, so no `mediaretry` notification ever
//! comes back. WA Web (and Baileys' `jidNormalizedUser(me.id)`) address the account
//! itself; current WA Web uses the account LID. We build the stanza ourselves with
//! whatsapp-rust's crypto / node helpers, addressed like WA Web.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use whatsapp_rust::wacore::media_retry::{
    MediaRetryResult, build_media_retry_receipt, encrypt_media_retry_receipt, parse_media_retry_notification,
};
use whatsapp_rust::wacore_binary::{JidExt as _, NodeRef};
use whatsapp_rust::{Client, Jid, NodeFilter};

/// Covers the receipt's server ack and the phone's answer (WA Web waits about as long).
const RETRY_TIMEOUT: Duration = Duration::from_secs(30);

/// Media of one stored message to re-upload.
pub(crate) struct Request<'a> {
    pub msg_id: &'a str,
    /// The chat as it was addressed on the wire (`RawMessage::wire_chat`).
    pub chat: &'a Jid,
    /// Author of a group / broadcast message (`RawMessage::wire_sender`).
    pub participant: Option<&'a Jid>,
    pub from_me: bool,
    pub media_key: &'a [u8],
}

/// Where the receipt goes and how it names the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Addressing {
    /// Our account (never a device JID): LID if known, else PN.
    pub to: Jid,
    pub chat: Jid,
    pub participant: Option<Jid>,
}

/// `chat_lid`: the LID of a PN 1:1 chat, when the account is LID-migrated and the
/// mapping is known (the phone files such chats under the LID, like WA Web's
/// `WAWebSendServerErrorReceiptJob` assumes). `None` when not logged in.
pub(crate) fn addressing(
    own_pn: Option<&Jid>,
    own_lid: Option<&Jid>,
    chat: &Jid,
    chat_lid: Option<Jid>,
    participant: Option<&Jid>,
) -> Option<Addressing> {
    let to = own_lid.or(own_pn)?.to_non_ad();
    let chat = chat.to_non_ad();
    let multi = chat.is_group() || chat.is_broadcast_list() || chat.is_status_broadcast();
    let chat = match chat_lid {
        Some(lid) if chat.is_pn() => lid.to_non_ad(),
        _ => chat,
    };
    let participant = participant.filter(|_| multi).map(Jid::to_non_ad);
    Some(Addressing { to, chat, participant })
}

/// How a re-upload request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// New direct path of the re-uploaded media.
    Success(String),
    /// The phone doesn't have the message (or its media) anymore.
    NotFound,
    /// The phone couldn't decrypt our request (wrong media key).
    DecryptionError,
    GeneralError,
    /// The server refused the receipt (its ack carried this error code).
    Rejected(Option<u16>),
    /// No answer within [`RETRY_TIMEOUT`] (phone offline, or the request went nowhere).
    Timeout,
}

impl Outcome {
    fn from_result(r: MediaRetryResult) -> Self {
        match r {
            MediaRetryResult::Success { direct_path } => Outcome::Success(direct_path),
            MediaRetryResult::NotFound => Outcome::NotFound,
            MediaRetryResult::DecryptionError => Outcome::DecryptionError,
            MediaRetryResult::GeneralError => Outcome::GeneralError,
        }
    }

    /// For logs and errors. HTTP-ish codes as in Baileys' `MEDIA_RETRY_STATUS_MAP`.
    pub fn code(&self) -> String {
        match self {
            Outcome::Success(_) => "success (200)".into(),
            Outcome::NotFound => "not-found (404)".into(),
            Outcome::DecryptionError => "decryption-error (412)".into(),
            Outcome::GeneralError => "general-error (418)".into(),
            Outcome::Rejected(Some(code)) => format!("rejected ({code})"),
            Outcome::Rejected(None) => "rejected".into(),
            Outcome::Timeout => "timeout".into(),
        }
    }
}

/// Decrypt / interpret a `<notification type="mediaretry">`.
pub(crate) fn parse_notification(node: &NodeRef<'_>, media_key: &[u8]) -> Result<Outcome> {
    parse_media_retry_notification(node, media_key).map(Outcome::from_result)
}

/// `Some(code)` if the server's `<ack>` for our receipt reports an error.
pub(crate) fn ack_error(node: &NodeRef<'_>) -> Option<Option<u16>> {
    if let Some(code) = node.get_attr("error") {
        return Some(code.as_str().parse().ok());
    }
    node.get_optional_child_by_tag(&["error"])
        .map(|e| e.get_attr("code").and_then(|c| c.as_str().parse().ok()))
}

/// Send the re-upload request and wait for the phone's answer.
pub(crate) async fn request(client: &Client, req: &Request<'_>) -> Result<Outcome> {
    let id = req.msg_id;
    if req.chat.is_newsletter() {
        return Err(anyhow!("newsletter media can't be re-uploaded"));
    }
    let snapshot = client.persistence_manager().get_device_snapshot();
    let chat_lid = if req.chat.is_pn() && client.is_lid_migrated().await {
        client.get_lid_pn_entry(req.chat).await.ok().flatten().map(|e| Jid::lid(e.lid.as_ref()))
    } else {
        None
    };
    let addr = addressing(snapshot.pn.as_ref(), snapshot.lid.as_ref(), req.chat, chat_lid, req.participant)
        .context("not logged in")?;
    let (ciphertext, iv) = encrypt_media_retry_receipt(req.media_key, id)?;
    let node =
        build_media_retry_receipt(&addr.to, id, &addr.chat, req.from_me, addr.participant.as_ref(), &ciphertext, &iv);

    // Both waiters exist before the send, so neither answer can slip past.
    let mut notification =
        client.wait_for_node(NodeFilter::tag("notification").attr("type", "mediaretry").attr("id", id));
    let mut ack =
        client.wait_for_node(NodeFilter::tag("ack").attr("id", id).attr("class", "receipt").attr("type", "server-error"));
    client.send_node(node).await?;
    log::info!(
        "media re-upload requested for {id} (to={}, chat={}, from_me={}, participant={})",
        addr.to,
        addr.chat,
        req.from_me,
        addr.participant.as_ref().map_or_else(|| "-".to_owned(), Jid::to_string),
    );

    let deadline = tokio::time::sleep(RETRY_TIMEOUT);
    tokio::pin!(deadline);
    let mut acked = false;
    let outcome = loop {
        tokio::select! {
            node = &mut notification => {
                let node = node.map_err(|_| anyhow!("media retry waiter dropped"))?;
                break parse_notification(node.get(), req.media_key)
                    .inspect_err(|e| log::info!("media re-upload result for {id}: unreadable ({e:#})"))?;
            }
            // A missing ack isn't fatal; an error ack means no answer is coming.
            node = &mut ack, if !acked => {
                acked = true;
                let Ok(node) = node else { continue };
                if let Some(code) = ack_error(node.get()) {
                    break Outcome::Rejected(code);
                }
                log::info!("media re-upload request for {id} accepted by the server, waiting for the phone");
            }
            () = &mut deadline => break Outcome::Timeout,
        }
    };
    log::info!("media re-upload result for {id}: {}", outcome.code());
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::NodeBuilder;
    use whatsapp_rust::wacore::media_retry::decrypt_media_retry_notification;

    // Vectors made with Node's crypto exactly like Baileys' `encryptMediaRetryRequest` /
    // `decryptMediaRetryData`: HKDF-SHA256(mediaKey, salt = empty,
    // info = "WhatsApp Media Retry Notification", 32), AES-256-GCM, AAD = message id.
    const KEY: [u8; 32] = {
        let mut k = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            k[i] = i as u8 + 1;
            i += 1;
        }
        k
    };
    const ID: &str = "3EB0C0FFEE1234567890ABCD";
    const IV: &str = "000102030405060708090a0b";
    /// MediaRetryNotification { stanzaId: ID, directPath: "/v/t62.7118-24/fresh.enc?ccb=11-4", result: SUCCESS }
    const SUCCESS: &str = "c390d11c7e36a611cbe282e1fd40c9b5aaaf7665be840f658b41e8080e0eff0630eb5ff227752abb82f64cda1a249d01df8ffa362e07b05a5d150dc8dc473e71197cff4327d1b201eb25e1a18260e4";
    /// MediaRetryNotification { stanzaId: ID, result: NOT_FOUND }
    const NOT_FOUND: &str =
        "c390d11c7e36a611cbe282e1fd40c9b5aaaf7665be840f658b41e22b50f4f27dc3ce58c47235a4f5f7b18a6c";
    /// ServerErrorReceipt { stanzaId: ID } (what we send)
    const RECEIPT: &str = "c390d11c7e36a611cbe282e1fd40c9b5aaaf7665be840f658b4191a09752a538d323e19a2f3734adea36";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn encrypted_notification(ciphertext: &str) -> whatsapp_rust::wacore_binary::Node {
        NodeBuilder::new("notification")
            .attr("type", "mediaretry")
            .attr("id", ID)
            .children([
                NodeBuilder::new("encrypt")
                    .children([
                        NodeBuilder::new("enc_p").bytes(hex(ciphertext)).build(),
                        NodeBuilder::new("enc_iv").bytes(hex(IV)).build(),
                    ])
                    .build(),
                NodeBuilder::new("rmr").attr("jid", "923001234567@s.whatsapp.net").attr("from_me", "true").build(),
            ])
            .build()
    }

    #[test]
    fn decrypts_a_baileys_style_success() {
        let node = encrypted_notification(SUCCESS);
        let outcome = parse_notification(&node.as_node_ref(), &KEY).unwrap();
        assert_eq!(outcome, Outcome::Success("/v/t62.7118-24/fresh.enc?ccb=11-4".into()));
    }

    #[test]
    fn decrypts_an_encrypted_not_found() {
        let node = encrypted_notification(NOT_FOUND);
        assert_eq!(parse_notification(&node.as_node_ref(), &KEY).unwrap(), Outcome::NotFound);
    }

    #[test]
    fn wrong_media_key_is_an_error() {
        let node = encrypted_notification(SUCCESS);
        assert!(parse_notification(&node.as_node_ref(), &[9u8; 32]).is_err());
    }

    #[test]
    fn request_payload_matches_baileys() {
        // Same plaintext, key and IV as Baileys -> the same ciphertext decrypts here.
        let plain = decrypt_media_retry_notification(&KEY, ID, &hex(IV), &hex(RECEIPT)).unwrap();
        let mut expected = vec![0x0a, ID.len() as u8];
        expected.extend_from_slice(ID.as_bytes());
        assert_eq!(plain, expected);
        // And what we encrypt (random IV) is the same protobuf.
        let (ciphertext, iv) = encrypt_media_retry_receipt(&KEY, ID).unwrap();
        assert_eq!(decrypt_media_retry_notification(&KEY, ID, &iv, &ciphertext).unwrap(), expected);
    }

    #[test]
    fn plaintext_error_codes() {
        for (code, expected) in [(2, Outcome::NotFound), (3, Outcome::DecryptionError), (1, Outcome::GeneralError), (99, Outcome::GeneralError)] {
            let node = NodeBuilder::new("notification")
                .attr("type", "mediaretry")
                .attr("id", ID)
                .children([NodeBuilder::new("error").attr("code", code.to_string()).build()])
                .build();
            assert_eq!(parse_notification(&node.as_node_ref(), &KEY).unwrap(), expected, "code {code}");
        }
    }

    #[test]
    fn outcome_codes() {
        assert_eq!(Outcome::Success("/x".into()).code(), "success (200)");
        assert_eq!(Outcome::NotFound.code(), "not-found (404)");
        assert_eq!(Outcome::DecryptionError.code(), "decryption-error (412)");
        assert_eq!(Outcome::GeneralError.code(), "general-error (418)");
        assert_eq!(Outcome::Rejected(Some(400)).code(), "rejected (400)");
        assert_eq!(Outcome::Timeout.code(), "timeout");
    }

    #[test]
    fn error_acks() {
        let ok = NodeBuilder::new("ack").attr("id", ID).attr("class", "receipt").build();
        assert_eq!(ack_error(&ok.as_node_ref()), None);
        let attr = NodeBuilder::new("ack").attr("id", ID).attr("error", "400").build();
        assert_eq!(ack_error(&attr.as_node_ref()), Some(Some(400)));
        let child = NodeBuilder::new("ack")
            .attr("id", ID)
            .children([NodeBuilder::new("error").attr("code", "403").build()])
            .build();
        assert_eq!(ack_error(&child.as_node_ref()), Some(Some(403)));
    }

    fn jid(s: &str) -> Jid {
        s.parse().unwrap()
    }

    #[test]
    fn addresses_the_account_not_this_device() {
        let pn = jid("923001234567:33@s.whatsapp.net");
        let lid = jid("100000012345678:33@lid");
        let chat = jid("923331112222@s.whatsapp.net");
        let a = addressing(Some(&pn), Some(&lid), &chat, None, None).unwrap();
        assert_eq!(a.to.to_string(), "100000012345678@lid");
        assert_eq!(a.chat.to_string(), "923331112222@s.whatsapp.net");
        assert_eq!(a.participant, None);
        // No LID yet: the PN, still without the device.
        let a = addressing(Some(&pn), None, &chat, None, None).unwrap();
        assert_eq!(a.to.to_string(), "923001234567@s.whatsapp.net");
        assert!(addressing(None, None, &chat, None, None).is_none());
    }

    #[test]
    fn migrated_pn_chats_use_the_lid() {
        let pn = jid("923001234567:33@s.whatsapp.net");
        let chat = jid("923331112222@s.whatsapp.net");
        let a = addressing(Some(&pn), None, &chat, Some(jid("200000099999999@lid")), None).unwrap();
        assert_eq!(a.chat.to_string(), "200000099999999@lid");
    }

    #[test]
    fn participant_only_for_groups() {
        let pn = jid("923001234567:33@s.whatsapp.net");
        let sender = jid("923331112222:4@s.whatsapp.net");
        let dm = jid("923331112222@s.whatsapp.net");
        assert_eq!(addressing(Some(&pn), None, &dm, None, Some(&sender)).unwrap().participant, None);
        let group = jid("120363040237990503@g.us");
        let a = addressing(Some(&pn), None, &group, Some(jid("1@lid")), Some(&sender)).unwrap();
        assert_eq!(a.chat, group);
        assert_eq!(a.participant.unwrap().to_string(), "923331112222@s.whatsapp.net");
    }

    #[test]
    fn receipt_shape() {
        let pn = jid("923001234567:33@s.whatsapp.net");
        let group = jid("120363040237990503@g.us");
        let sender = jid("923331112222@s.whatsapp.net");
        let a = addressing(Some(&pn), None, &group, None, Some(&sender)).unwrap();
        let (ciphertext, iv) = encrypt_media_retry_receipt(&KEY, ID).unwrap();
        let node = build_media_retry_receipt(&a.to, ID, &a.chat, true, a.participant.as_ref(), &ciphertext, &iv);
        let node = node.as_node_ref();
        let attr = |n: &NodeRef<'_>, k: &str| n.get_attr(k).map(|v| v.as_str().into_owned());
        assert_eq!(node.tag.as_ref(), "receipt");
        assert_eq!(attr(&node, "type").as_deref(), Some("server-error"));
        assert_eq!(attr(&node, "id").as_deref(), Some(ID));
        assert_eq!(attr(&node, "to").as_deref(), Some("923001234567@s.whatsapp.net"));
        let rmr = node.get_optional_child_by_tag(&["rmr"]).unwrap();
        assert_eq!(attr(rmr, "jid").as_deref(), Some("120363040237990503@g.us"));
        assert_eq!(attr(rmr, "from_me").as_deref(), Some("true"));
        assert_eq!(attr(rmr, "participant").as_deref(), Some("923331112222@s.whatsapp.net"));
        let encrypt = node.get_optional_child_by_tag(&["encrypt"]).unwrap();
        assert!(encrypt.get_optional_child_by_tag(&["enc_p"]).is_some());
        assert!(encrypt.get_optional_child_by_tag(&["enc_iv"]).is_some());
    }
}
