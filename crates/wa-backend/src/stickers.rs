//! The sticker panel: favorites, recents and the outgoing `StickerMessage`.
//!
//! Favorites are WhatsApp app state: `favoriteSticker` mutations in the `regular_low`
//! collection, indexed `["favoriteSticker", <filehash>]` (the base64 SHA-256 of the
//! decrypted sticker) with a `StickerAction` value carrying the media fields. Unfavoriting
//! is a `Set` with `isFavorite = false`. whatsapp-rust 0.7 decodes that collection but
//! drops these mutations (no event), so [`fetch_regular_low`] reads the collection itself
//! from version 0, read-only: nothing is persisted into the library's app state, its
//! keys are only looked up.
//!
//! Recents are the stickers I sent (stored sticker messages from me), newest first.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::anyhow;
use whatsapp_rust::buffa::Message as _;
use whatsapp_rust::jid_utils::server_jid;
use whatsapp_rust::prelude::{Client, wa};
use whatsapp_rust::request::{InfoQuery, InfoQueryType};
use whatsapp_rust::wacore::appstate::patch_decode::parse_patch_list_ref;
use whatsapp_rust::wacore::appstate::{ExpandedAppStateKeys, Mutation, decode_record, expand_app_state_keys};
use whatsapp_rust::wacore_binary::NodeContent;
use whatsapp_rust::NodeBuilder;

use crate::types::StickerSource;

use wa::sync_action_value::StickerAction;
use wa::syncd_mutation::SyncdOperation;

/// Most stickers the panel lists (favorites first).
pub const MAX_STICKERS: usize = 60;
/// Sticker messages scanned for recents.
pub const RECENT_SCAN: u32 = 500;
const FAVORITE_ACTION: &str = "favoriteSticker";
const REMOVE_RECENT_ACTION: &str = "removeRecentSticker";
/// Pages of patches fetched after the snapshot, at most.
const MAX_PAGES: usize = 20;

/// `<data_dir>/media/stickers/<sha256 hex>.<ext>`: the panel's own copies, keyed by the
/// sticker file's hash (`png` preview, `webp` original for re-uploads). Chat media
/// directories are named after JIDs (with an `@`), so this can't collide with one.
pub fn sticker_file(data_dir: &Path, id: &str, ext: &str) -> PathBuf {
    data_dir.join("media").join("stickers").join(format!("{}.{ext}", crate::media::file_component(id)))
}

/// Marks a favorite whose file is gone for good (expired on the CDN, and no stored message
/// has it to ask a phone for a re-upload): not shown, not fetched again, until a message
/// with it turns up.
pub fn gone_marker(data_dir: &Path, id: &str) -> PathBuf {
    sticker_file(data_dir, id, "gone")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Standard base64 (padding optional), as WhatsApp writes filehashes.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The sticker id (hex SHA-256) for a favorite's filehash.
fn filehash_id(filehash: &str) -> Option<String> {
    base64_decode(filehash).filter(|h| h.len() == 32).map(|h| hex(&h))
}

/// Milliseconds, whether the value came in seconds or milliseconds.
fn to_ms(t: i64) -> i64 {
    if t.abs() < 100_000_000_000 { t.saturating_mul(1000) } else { t }
}

/// Favorites and hidden recents, as the app state says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StickerSync {
    /// Sticker id -> (media fields, favorited at in ms).
    pub favorites: HashMap<String, (StickerAction, i64)>,
    /// Sticker id -> recents sent up to this time (ms) are hidden.
    pub removed_recents: HashMap<String, i64>,
}

/// Fold decoded mutations (snapshot first, then patches in order) into the final state.
pub fn fold_mutations(mutations: &[Mutation]) -> StickerSync {
    let mut sync = StickerSync::default();
    for m in mutations {
        let (Some(action), Some(id)) = (m.index.first(), m.index.get(1).and_then(|h| filehash_id(h))) else {
            continue;
        };
        let remove = m.operation == SyncdOperation::REMOVE;
        let value = m.action_value.as_ref();
        match action.as_str() {
            FAVORITE_ACTION => {
                let sticker = value.and_then(|v| v.sticker_action.as_option());
                match sticker {
                    Some(s) if !remove && s.is_favorite == Some(true) => {
                        let ts = value.and_then(|v| v.timestamp).map_or(0, to_ms);
                        sync.favorites.insert(id, (s.clone(), ts));
                    }
                    // Unfavorited (or removed, or no longer readable).
                    _ => {
                        sync.favorites.remove(&id);
                    }
                }
            }
            REMOVE_RECENT_ACTION if remove => {
                sync.removed_recents.remove(&id);
            }
            REMOVE_RECENT_ACTION => {
                // Without a time the entry goes whatever its age (WA Web).
                let last = value
                    .and_then(|v| v.remove_recent_sticker_action.as_option())
                    .and_then(|a| a.last_sticker_sent_ts)
                    .map_or(i64::MAX, to_ms);
                sync.removed_recents.insert(id, last);
            }
            _ => {}
        }
    }
    sync
}

/// Why reading the collection failed.
#[derive(Debug)]
pub enum FetchError {
    /// The app state key isn't here (yet): right after linking, the phone shares the
    /// keys a few seconds after connecting. Base64 key id.
    MissingKey(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::MissingKey(id) => write!(f, "app state key {id} not available yet"),
            FetchError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for FetchError {
    fn from(e: anyhow::Error) -> Self {
        FetchError::Other(e)
    }
}

/// What [`fetch_regular_low`] read.
pub struct Fetched {
    pub snapshot_version: u64,
    pub version: u64,
    pub patches: usize,
    pub mutations: Vec<Mutation>,
}

/// Read the whole `regular_low` collection (snapshot + patches) and decode it, without
/// touching the library's own app state.
pub async fn fetch_regular_low(client: &Client) -> Result<Fetched, FetchError> {
    let mut snapshot: Option<wa::SyncdSnapshot> = None;
    let mut patches: Vec<wa::SyncdPatch> = Vec::new();
    let mut version: Option<u64> = None;
    for _ in 0..MAX_PAGES {
        let mut collection = NodeBuilder::new("collection")
            .attr("name", "regular_low")
            .attr("return_snapshot", if version.is_none() { "true" } else { "false" });
        if let Some(v) = version {
            collection = collection.attr("version", v);
        }
        let iq = InfoQuery {
            namespace: "w:sync:app:state",
            query_type: InfoQueryType::Set,
            to: server_jid().clone(),
            target: None,
            id: None,
            content: Some(NodeContent::Nodes(vec![NodeBuilder::new("sync").children([collection.build()]).build()])),
            timeout: None,
        };
        let resp = client.send_iq(iq).await.map_err(|e| anyhow!("app state request: {e}"))?;
        let list = parse_patch_list_ref(resp.get())?;
        if let Some(e) = &list.error {
            return Err(anyhow!("app state error for regular_low: {e:?}").into());
        }
        if let Some(blob) = &list.snapshot_ref {
            let bytes = client.download(blob).await.map_err(|e| anyhow!("downloading app state snapshot: {e:#}"))?;
            let snap = wa::SyncdSnapshot::decode_from_slice(&bytes).map_err(|e| anyhow!("decoding snapshot: {e}"))?;
            version = Some(snap.version.version.unwrap_or(0));
            snapshot = Some(snap);
        } else if let Some(snap) = list.snapshot {
            version = Some(snap.version.version.unwrap_or(0));
            snapshot = Some(snap);
        }
        for mut patch in list.patches {
            if patch.mutations.is_empty()
                && let Some(blob) = patch.external_mutations.as_option()
            {
                let bytes =
                    client.download(blob).await.map_err(|e| anyhow!("downloading app state patch: {e:#}"))?;
                patch.mutations = wa::SyncdMutations::decode_from_slice(&bytes)
                    .map_err(|e| anyhow!("decoding patch mutations: {e}"))?
                    .mutations;
            }
            let v = patch.version.version.unwrap_or(0);
            version = Some(version.map_or(v, |cur| cur.max(v)));
            patches.push(patch);
        }
        if !list.has_more_patches {
            break;
        }
        // Without a version to continue from, asking again would return the same page.
        if version.is_none() {
            break;
        }
    }
    patches.sort_by_key(|p| p.version.version.unwrap_or(0));
    // Already in the snapshot: replaying them could undo later changes.
    let snapshot_version = snapshot.as_ref().and_then(|s| s.version.version);
    if let Some(sv) = snapshot_version {
        patches.retain(|p| p.version.version.unwrap_or(0) > sv);
    }

    // Keys for every record, from the session store.
    let backend = client.persistence_manager().backend();
    let mut keys: HashMap<Vec<u8>, Arc<ExpandedAppStateKeys>> = HashMap::new();
    for id in key_ids(snapshot.as_ref(), &patches) {
        match backend.get_sync_key(&id).await.map_err(|e| anyhow!("reading app state key: {e}"))? {
            Some(key) => {
                keys.insert(id, Arc::new(expand_app_state_keys(&key.key_data)));
            }
            None => return Err(FetchError::MissingKey(base64_encode(&id))),
        }
    }
    let mutations = decode_collection(snapshot.as_ref(), &patches, |id| keys.get(id).cloned())?;
    Ok(Fetched {
        snapshot_version: snapshot.as_ref().and_then(|s| s.version.version).unwrap_or(0),
        version: version.unwrap_or(0),
        patches: patches.len(),
        mutations,
    })
}

fn key_ids(snapshot: Option<&wa::SyncdSnapshot>, patches: &[wa::SyncdPatch]) -> HashSet<Vec<u8>> {
    let records = snapshot
        .into_iter()
        .flat_map(|s| s.records.iter())
        .chain(patches.iter().flat_map(|p| p.mutations.iter().map(|m| &*m.record)));
    records.filter_map(|r| r.key_id.id.clone()).collect()
}

/// Decode a snapshot and the patches after it (in version order) into mutations. MACs
/// aren't checked: this is a read-only view, and the library validates the same data
/// when it syncs.
pub fn decode_collection(
    snapshot: Option<&wa::SyncdSnapshot>,
    patches: &[wa::SyncdPatch],
    keys: impl Fn(&[u8]) -> Option<Arc<ExpandedAppStateKeys>>,
) -> Result<Vec<Mutation>, FetchError> {
    let decode = |op: SyncdOperation, record: &wa::SyncdRecord| -> Result<Mutation, FetchError> {
        let key_id = record.key_id.id.as_deref().ok_or_else(|| anyhow!("app state record without key id"))?;
        let k = keys(key_id).ok_or_else(|| FetchError::MissingKey(base64_encode(key_id)))?;
        let (m, _) = decode_record(op, record, &k, key_id, false).map_err(|e| anyhow!("decoding record: {e}"))?;
        Ok(m)
    };
    let mut out = Vec::new();
    for record in snapshot.iter().flat_map(|s| s.records.iter()) {
        out.push(decode(SyncdOperation::SET, record)?);
    }
    for patch in patches {
        for m in &patch.mutations {
            let op = match m.operation {
                None => SyncdOperation::SET,
                Some(op) => match op.as_known() {
                    Some(op) => op,
                    None => continue,
                },
            };
            if !m.record.is_set() {
                continue;
            }
            out.push(decode(op, &m.record)?);
        }
    }
    Ok(out)
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Zipped Lottie animations can't be shown or decoded; only their PNG thumbnail can.
pub fn is_lottie(p: &wa::message::StickerMessage) -> bool {
    p.is_lottie == Some(true) || p.mimetype.as_deref().is_some_and(|m| m.contains("was"))
}

fn png_thumbnail(p: &wa::message::StickerMessage) -> Option<&[u8]> {
    p.png_thumbnail.as_deref().filter(|t| !t.is_empty())
}

/// A favorite as a sticker proto. The app state carries no file hash in the value (it's
/// the index), no thumbnail and no key timestamp.
pub fn favorite_proto(sha256: &[u8], a: &StickerAction) -> wa::message::StickerMessage {
    wa::message::StickerMessage {
        url: a.url.clone(),
        file_sha256: Some(sha256.to_vec()),
        file_enc_sha256: a.file_enc_sha256.clone(),
        media_key: a.media_key.clone(),
        mimetype: a.mimetype.clone(),
        height: a.height,
        width: a.width,
        direct_path: a.direct_path.clone(),
        file_length: a.file_length,
        is_lottie: a.is_lottie,
        is_avatar: a.is_avatar_sticker,
        ..Default::default()
    }
}

/// The sticker message to send for a stored sticker: its media fields (the same file on
/// the CDN), without any context of the message it came from.
pub fn outgoing_sticker(p: &wa::message::StickerMessage, animated: bool) -> wa::message::StickerMessage {
    let positive = |v: Option<u32>| v.filter(|v| *v > 0);
    wa::message::StickerMessage {
        url: p.url.clone(),
        file_sha256: p.file_sha256.clone(),
        file_enc_sha256: p.file_enc_sha256.clone(),
        media_key: p.media_key.clone(),
        mimetype: Some(p.mimetype.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "image/webp".to_owned())),
        height: positive(p.height),
        width: positive(p.width),
        direct_path: p.direct_path.clone(),
        file_length: p.file_length,
        media_key_timestamp: p.media_key_timestamp,
        first_frame_length: p.first_frame_length,
        first_frame_sidecar: p.first_frame_sidecar.clone(),
        is_animated: Some(animated && !is_lottie(p)),
        png_thumbnail: png_thumbnail(p).map(<[u8]>::to_vec),
        is_avatar: p.is_avatar,
        is_ai_sticker: p.is_ai_sticker,
        is_lottie: p.is_lottie,
        accessibility_label: p.accessibility_label.clone(),
        emojis: p.emojis.clone(),
        ..Default::default()
    }
}

/// Take over another copy's CDN fields (a fresh upload of the same file): link, key and
/// hashes go together, mixing them makes the sticker undecryptable.
pub fn adopt_media(proto: &mut wa::message::StickerMessage, from: &wa::message::StickerMessage) {
    proto.url = from.url.clone();
    proto.direct_path = from.direct_path.clone();
    proto.media_key = from.media_key.clone();
    proto.file_enc_sha256 = from.file_enc_sha256.clone();
    proto.file_sha256 = from.file_sha256.clone();
    proto.file_length = from.file_length;
    proto.media_key_timestamp = from.media_key_timestamp;
}

/// Can be sent / downloaded with what we have.
pub fn has_media_fields(p: &wa::message::StickerMessage) -> bool {
    let nonempty = |b: &Option<Vec<u8>>| b.as_ref().is_some_and(|b| !b.is_empty());
    nonempty(&p.media_key)
        && nonempty(&p.file_enc_sha256)
        && nonempty(&p.file_sha256)
        && (p.direct_path.as_deref().is_some_and(|d| !d.is_empty()) || p.url.as_deref().is_some_and(|u| !u.is_empty()))
}

/// The CDN link's expiry (`oe=` query parameter: hex Unix seconds), if it has one.
pub fn cdn_expiry(p: &wa::message::StickerMessage) -> Option<i64> {
    let link = p.direct_path.as_deref().filter(|d| !d.is_empty()).or(p.url.as_deref())?;
    let query = link.split_once('?')?.1;
    query.split('&').find_map(|kv| kv.strip_prefix("oe=")).and_then(|v| i64::from_str_radix(v, 16).ok())
}

/// One sticker the panel can show, with what's needed to preview and send it.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Hex SHA-256 of the sticker file.
    pub id: String,
    pub source: StickerSource,
    pub proto: wa::message::StickerMessage,
    /// Favorited / last sent, ms.
    pub at: i64,
    /// Known from the file (or the message); `None` = not known yet.
    pub animated: Option<bool>,
    /// A stored message with this sticker (chat, id): recents: my latest send of it. Its
    /// sender's phone can re-upload expired media.
    pub anchor: Option<(String, String)>,
    /// That message's downloaded PNG, if any.
    pub message_png: Option<PathBuf>,
}

impl Candidate {
    /// Nothing showable: a Lottie sticker without a PNG thumbnail.
    fn unshowable(&self) -> bool {
        is_lottie(&self.proto) && png_thumbnail(&self.proto).is_none()
    }
}

/// The panel list: favorites (newest first), then recents (newest first) that aren't
/// favorites or hidden from recents, one per sticker file, at most [`MAX_STICKERS`].
/// Returns the list and how many Lottie stickers were left out.
pub fn assemble(
    favorites: Vec<Candidate>,
    recents: Vec<Candidate>,
    removed_recents: &HashMap<String, i64>,
) -> (Vec<Candidate>, usize) {
    let mut seen = HashSet::new();
    let mut lottie_skipped = 0;
    let mut out = Vec::new();
    let mut favorites = favorites;
    favorites.sort_by_key(|c| std::cmp::Reverse(c.at));
    let mut recents = recents;
    recents.sort_by_key(|c| std::cmp::Reverse(c.at));
    // A favorite I sent: my latest send has a message (preview, re-upload from the
    // phone) and usually a fresher link than the app state's.
    let mut latest: HashMap<&str, &Candidate> = HashMap::new();
    for r in &recents {
        latest.entry(r.id.as_str()).or_insert(r);
    }
    for f in &mut favorites {
        let Some(r) = latest.get(f.id.as_str()) else { continue };
        let favorite_fresher = matches!((cdn_expiry(&f.proto), cdn_expiry(&r.proto)), (Some(a), Some(b)) if a > b)
            || !has_media_fields(&r.proto);
        if !favorite_fresher {
            adopt_media(&mut f.proto, &r.proto);
            if f.proto.png_thumbnail.is_none() {
                f.proto.png_thumbnail = r.proto.png_thumbnail.clone();
            }
            f.anchor = r.anchor.clone();
        }
        f.message_png = f.message_png.clone().or_else(|| r.message_png.clone());
        f.animated = f.animated.or(r.animated);
    }
    let recents = recents.into_iter().filter(|c| removed_recents.get(&c.id).is_none_or(|hidden| c.at > *hidden));
    for c in favorites.into_iter().chain(recents) {
        if out.len() >= MAX_STICKERS {
            break;
        }
        if !seen.insert(c.id.clone()) {
            continue;
        }
        if c.unshowable() {
            lottie_skipped += 1;
            continue;
        }
        out.push(c);
    }
    (out, lottie_skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::prelude::MessageField;
    use whatsapp_rust::wacore::appstate::encode::encode_record;

    const FILEHASH: &str = "n4bQgYhMfWWaL+qgxVrQFaO/TxsrC4Is0V1sFbDwCgg=";

    fn action(path: &str) -> StickerAction {
        StickerAction {
            direct_path: Some(path.into()),
            media_key: Some(vec![7; 32]),
            file_enc_sha256: Some(vec![9; 32]),
            mimetype: Some("image/webp".into()),
            width: Some(512),
            height: Some(512),
            file_length: Some(1234),
            ..Default::default()
        }
    }

    fn favorite(filehash: &str, favorite: bool, ts: i64, path: &str) -> Mutation {
        let mut a = action(path);
        a.is_favorite = Some(favorite);
        Mutation {
            index: vec![FAVORITE_ACTION.into(), filehash.into()],
            operation: SyncdOperation::SET,
            action_value: Some(wa::SyncActionValue {
                sticker_action: MessageField::some(a),
                timestamp: Some(ts),
                ..Default::default()
            }),
        }
    }

    fn id_of(filehash: &str) -> String {
        filehash_id(filehash).unwrap()
    }

    #[test]
    fn filehash_is_base64_sha256() {
        let id = id_of(FILEHASH);
        assert_eq!(id.len(), 64);
        assert_eq!(unhex(&id), base64_decode(FILEHASH));
        assert_eq!(unhex("0g"), None);
        assert!(id.starts_with("9f86d081"), "{id}");
        assert_eq!(base64_encode(&base64_decode(FILEHASH).unwrap()), FILEHASH);
        assert_eq!(base64_decode("TWE="), Some(b"Ma".to_vec()));
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert!(filehash_id("c2hvcnQ=").is_none(), "not 32 bytes");
        assert!(filehash_id("not base64!").is_none());
    }

    #[test]
    fn favorites_are_added_and_removed() {
        let other = base64_encode(&[1u8; 32]);
        let sync = fold_mutations(&[
            favorite(FILEHASH, true, 1_700_000_000_000, "/a"),
            favorite(&other, true, 1_700_000_001, "/b"),
            // Updated (re-favorited with a newer path): last one wins.
            favorite(FILEHASH, true, 1_700_000_002_000, "/a2"),
        ]);
        assert_eq!(sync.favorites.len(), 2);
        let (a, ts) = &sync.favorites[&id_of(FILEHASH)];
        assert_eq!((a.direct_path.as_deref(), *ts), (Some("/a2"), 1_700_000_002_000));
        // Seconds are taken as such.
        assert_eq!(sync.favorites[&hex(&[1u8; 32])].1, 1_700_000_001_000);

        // Unfavorited with `isFavorite = false`, or by a syncd remove.
        let mut removed = favorite(&other, true, 5, "/b");
        removed.operation = SyncdOperation::REMOVE;
        let sync = fold_mutations(&[
            favorite(FILEHASH, true, 1, "/a"),
            favorite(&other, true, 2, "/b"),
            favorite(FILEHASH, false, 3, "/a"),
            removed,
        ]);
        assert!(sync.favorites.is_empty(), "{:?}", sync.favorites.keys());

        // A mutation without a usable value or hash doesn't add anything.
        let mut no_value = favorite(FILEHASH, true, 1, "/a");
        no_value.action_value = None;
        let bad_hash = favorite("???", true, 1, "/a");
        assert!(fold_mutations(&[no_value, bad_hash]).favorites.is_empty());
    }

    #[test]
    fn removed_recents() {
        let remove = |ts: Option<i64>| Mutation {
            index: vec![REMOVE_RECENT_ACTION.into(), FILEHASH.into()],
            operation: SyncdOperation::SET,
            action_value: Some(wa::SyncActionValue {
                remove_recent_sticker_action: MessageField::some(
                    wa::sync_action_value::RemoveRecentStickerAction { last_sticker_sent_ts: ts },
                ),
                ..Default::default()
            }),
        };
        let sync = fold_mutations(&[remove(Some(1_700_000_000))]);
        assert_eq!(sync.removed_recents[&id_of(FILEHASH)], 1_700_000_000_000);
        assert_eq!(fold_mutations(&[remove(None)]).removed_recents[&id_of(FILEHASH)], i64::MAX);
        let mut undo = remove(None);
        undo.operation = SyncdOperation::REMOVE;
        assert!(fold_mutations(&[remove(None), undo]).removed_recents.is_empty());
    }

    /// Records encrypted like the phone does decode back, snapshot first, then patches.
    #[test]
    fn decodes_snapshot_and_patches() {
        let key_id = b"key-1".to_vec();
        let keys = Arc::new(expand_app_state_keys(&[3u8; 32]));
        let encode = |op: SyncdOperation, m: &Mutation| {
            let index = serde_json_index(&m.index);
            encode_record(op, &index, m.action_value.as_ref().unwrap(), &keys, &key_id, &[5u8; 16], 7).0
        };
        let first = favorite(FILEHASH, true, 1, "/a");
        let snapshot = wa::SyncdSnapshot { records: vec![(*encode(SyncdOperation::SET, &first).record).clone()], ..Default::default() };
        let unfav = favorite(FILEHASH, false, 2, "/a");
        let patch = wa::SyncdPatch {
            mutations: vec![encode(SyncdOperation::SET, &unfav)],
            version: MessageField::some(wa::SyncdVersion { version: Some(2) }),
            ..Default::default()
        };
        let lookup = |id: &[u8]| (id == key_id.as_slice()).then(|| keys.clone());
        let decoded = decode_collection(Some(&snapshot), std::slice::from_ref(&patch), lookup).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].index, first.index);
        assert!(fold_mutations(&decoded[..1]).favorites.contains_key(&id_of(FILEHASH)));
        assert!(fold_mutations(&decoded).favorites.is_empty());
        // Without the key: reported as missing (to retry once the phone shares it).
        let missing = decode_collection(Some(&snapshot), &[], |_| None);
        assert!(matches!(missing, Err(FetchError::MissingKey(id)) if id == base64_encode(&key_id)));
    }

    fn serde_json_index(parts: &[String]) -> Vec<u8> {
        let quoted: Vec<String> = parts.iter().map(|p| format!("\"{p}\"")).collect();
        format!("[{}]", quoted.join(",")).into_bytes()
    }

    #[test]
    fn sticker_message_from_a_stored_sticker() {
        let stored = wa::message::StickerMessage {
            url: Some("https://mmg.whatsapp.net/v/x.enc".into()),
            direct_path: Some("/v/t62.15575-24/x.enc?ccb=11-4&oh=01_A&oe=6789ABCD&_nc_sid=5e03e0".into()),
            file_sha256: Some(vec![1; 32]),
            file_enc_sha256: Some(vec![2; 32]),
            media_key: Some(vec![3; 32]),
            file_length: Some(4096),
            media_key_timestamp: Some(1_700_000_000),
            mimetype: Some("image/webp".into()),
            width: Some(512),
            height: Some(0),
            is_animated: Some(false),
            context_info: MessageField::some(wa::ContextInfo { stanza_id: Some("Q".into()), ..Default::default() }),
            ..Default::default()
        };
        let out = outgoing_sticker(&stored, true);
        assert_eq!(out.direct_path, stored.direct_path);
        assert_eq!((out.file_sha256.clone(), out.file_enc_sha256.clone(), out.media_key.clone()), (
            stored.file_sha256.clone(),
            stored.file_enc_sha256.clone(),
            stored.media_key.clone()
        ));
        assert_eq!((out.file_length, out.media_key_timestamp), (Some(4096), Some(1_700_000_000)));
        assert_eq!((out.width, out.height), (Some(512), None));
        assert_eq!(out.is_animated, Some(true), "learnt from the file");
        assert!(out.context_info.as_option().is_none(), "the original's context doesn't travel");
        assert!(has_media_fields(&out));
        assert_eq!(cdn_expiry(&out), Some(0x6789ABCD));

        // A favorite: the hash comes from the index; default mimetype.
        let mut a = action("/v/f.enc");
        a.mimetype = None;
        let fav = outgoing_sticker(&favorite_proto(&[4; 32], &a), false);
        assert_eq!((fav.file_sha256, fav.mimetype.as_deref()), (Some(vec![4; 32]), Some("image/webp")));
        assert_eq!(cdn_expiry(&favorite_proto(&[4; 32], &a)), None);
        assert!(!has_media_fields(&favorite_proto(&[4; 32], &StickerAction::default())));

        // Lottie: never "animated" (we can't play it), its thumbnail travels.
        let lottie = wa::message::StickerMessage {
            is_lottie: Some(true),
            png_thumbnail: Some(vec![8; 10]),
            ..stored
        };
        let out = outgoing_sticker(&lottie, true);
        assert_eq!((out.is_animated, out.png_thumbnail), (Some(false), Some(vec![8; 10])));
    }

    fn candidate(id: &str, source: StickerSource, at: i64) -> Candidate {
        Candidate {
            id: id.into(),
            source,
            proto: wa::message::StickerMessage::default(),
            at,
            animated: None,
            anchor: None,
            message_png: None,
        }
    }

    #[test]
    fn panel_order_dedup_and_cap() {
        use StickerSource::{Favorite, Recent};
        let favorites = vec![candidate("f1", Favorite, 10), candidate("f2", Favorite, 20)];
        let recents = vec![
            candidate("r1", Recent, 300),
            candidate("r1", Recent, 100),
            candidate("f1", Recent, 250),
            candidate("r2", Recent, 200),
            candidate("hidden", Recent, 150),
            candidate("again", Recent, 400),
        ];
        // Hidden up to its last send; one sent again after the removal shows.
        let removed = HashMap::from([("hidden".to_owned(), 150), ("again".to_owned(), 350)]);
        let (list, _) = assemble(favorites, recents, &removed);
        let got: Vec<(&str, StickerSource)> = list.iter().map(|c| (c.id.as_str(), c.source)).collect();
        assert_eq!(got, vec![("f2", Favorite), ("f1", Favorite), ("again", Recent), ("r1", Recent), ("r2", Recent)]);
        assert_eq!(list[3].at, 300, "the latest send of a sticker sent twice");

        // Lottie without a thumbnail is left out.
        let mut lottie = candidate("l", Recent, 1);
        lottie.proto.is_lottie = Some(true);
        let mut with_thumb = candidate("lt", Recent, 2);
        with_thumb.proto.is_lottie = Some(true);
        with_thumb.proto.png_thumbnail = Some(vec![1]);
        let (list, skipped) = assemble(Vec::new(), vec![lottie, with_thumb], &HashMap::new());
        assert_eq!((list.len(), list[0].id.as_str(), skipped), (1, "lt", 1));

        let many: Vec<Candidate> = (0..100).map(|i| candidate(&format!("s{i}"), Recent, i)).collect();
        let (list, _) = assemble(vec![candidate("f", Favorite, 0)], many, &HashMap::new());
        assert_eq!(list.len(), MAX_STICKERS);
        assert_eq!((list[0].id.as_str(), list[1].id.as_str()), ("f", "s99"));
    }

    #[test]
    fn favorites_use_my_fresher_send() {
        use StickerSource::{Favorite, Recent};
        let media = |path: &str, key: u8| wa::message::StickerMessage {
            direct_path: Some(path.into()),
            url: Some(format!("https://mmg.whatsapp.net{path}")),
            media_key: Some(vec![key; 32]),
            file_enc_sha256: Some(vec![key + 1; 32]),
            file_sha256: Some(vec![1; 32]),
            file_length: Some(u64::from(key)),
            media_key_timestamp: Some(i64::from(key)),
            ..Default::default()
        };
        let mut fav = candidate("s", Favorite, 1);
        fav.proto = media("/fav?oe=10", 1);
        let mut sent = candidate("s", Recent, 5);
        sent.proto = media("/sent?oe=20", 7);
        sent.anchor = Some(("chat".into(), "M".into()));
        sent.message_png = Some("/m.png".into());
        sent.animated = Some(true);
        let (list, _) = assemble(vec![fav.clone()], vec![sent.clone()], &HashMap::new());
        assert_eq!(list.len(), 1);
        let f = &list[0];
        assert_eq!(f.source, Favorite);
        // Link, key and hashes all from the same copy.
        assert_eq!(f.proto.direct_path.as_deref(), Some("/sent?oe=20"));
        assert_eq!(f.proto.url, sent.proto.url);
        assert_eq!((f.proto.media_key.clone(), f.proto.file_enc_sha256.clone()), (Some(vec![7; 32]), Some(vec![8; 32])));
        assert_eq!((f.proto.file_length, f.proto.media_key_timestamp), (Some(7), Some(7)));
        assert_eq!(f.anchor, sent.anchor);
        assert_eq!((f.message_png.clone(), f.animated), (sent.message_png.clone(), Some(true)));

        // The app state's link lives longer: kept, with its own key.
        fav.proto = media("/fav?oe=99", 1);
        let (list, _) = assemble(vec![fav], vec![sent], &HashMap::new());
        assert_eq!(list[0].proto.direct_path.as_deref(), Some("/fav?oe=99"));
        assert_eq!(list[0].proto.media_key, Some(vec![1; 32]));
        assert_eq!(list[0].anchor, None);
    }

    #[test]
    fn preview_cache_is_keyed_by_hash_inside_the_data_dir() {
        let dir = Path::new("/data");
        let id = hex(&[0xab; 32]);
        let png = sticker_file(dir, &id, "png");
        assert_eq!(png, Path::new("/data/media/stickers").join(format!("{id}.png")));
        assert_eq!(sticker_file(dir, &id, "webp").with_extension("png"), png);
        // A hostile id can't leave the directory.
        let evil = sticker_file(dir, "../../etc/passwd", "png");
        assert_eq!(evil.parent(), Some(Path::new("/data/media/stickers")));
    }
}
