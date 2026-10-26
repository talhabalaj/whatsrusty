//! In-process fake backend so the UI can be developed without a WhatsApp session.
//! Run with `WA_MOCK=1`.

use std::thread;
use std::time::Duration;

use wa_backend::*;

const ME: &str = "15550000000@s.whatsapp.net";

pub fn spawn() -> BackendHandle {
    let (cmd_tx, cmd_rx) = flume::unbounded::<Command>();
    let (ev_tx, ev_rx) = flume::unbounded::<Event>();

    thread::Builder::new()
        .name("mock-backend".into())
        .spawn(move || run(cmd_rx, ev_tx))
        .expect("spawn mock backend");

    BackendHandle { commands: cmd_tx, events: ev_rx }
}

struct Fake {
    chats: Vec<ChatSummary>,
    messages: Vec<ChatMessage>,
    /// Older messages of the long chat that only "the phone" has, newest last.
    phone_only: Vec<ChatMessage>,
}

const LONG_CHAT: &str = "15551110009@s.whatsapp.net";

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// The hiking group's members: (name, JID).
const MEMBERS: &[(&str, &str)] = &[
    ("Omar", "15551110011@s.whatsapp.net"),
    ("Sara Ali", "15551110012@s.whatsapp.net"),
    ("Ali Raza", "15551110013@s.whatsapp.net"),
];

fn member_jid(name: &str) -> String {
    MEMBERS
        .iter()
        .find(|(n, _)| n.split(' ').next() == Some(name) || *n == name)
        .map_or_else(|| format!("{name}@s.whatsapp.net"), |(_, j)| j.to_string())
}

/// What the real store does: `@<digits>` of people we know, with their names.
fn mock_mentions(text: &str) -> Vec<Mention> {
    let known = MEMBERS.iter().copied().chain([("You", ME)]);
    known
        .filter_map(|(name, jid)| {
            let user = jid.split('@').next()?;
            text.contains(&format!("@{user}")).then(|| Mention { user: user.into(), jid: jid.into(), name: name.into() })
        })
        .collect()
}

fn msg(chat: &str, id: usize, from_me: bool, sender: Option<&str>, text: &str, ts: i64) -> ChatMessage {
    ChatMessage {
        id: format!("{chat}-{id}"),
        chat_jid: chat.into(),
        sender_jid: if from_me { ME.into() } else { sender.map_or(chat.into(), member_jid) },
        sender_name: sender.map(String::from),
        from_me,
        body: MessageBody::Text(text.into()),
        timestamp: ts,
        status: if from_me { DeliveryStatus::Read } else { DeliveryStatus::Delivered },
        quoted: None,
        reactions: Vec::new(),
        edited: false,
        forwarded: false,
        deleted: false,
        mentions: mock_mentions(text),
    }
}

/// (jid, name, is_group, unread, [(text, from_me, sender, offset_secs)])
type Seed<'a> = (&'a str, &'a str, bool, bool, &'a [(&'a str, bool, Option<&'a str>, i64)]);

fn seed() -> Fake {
    let n = now();
    let h = 3600;
    let d = 86400;
    let mut messages = Vec::new();
    let convo: &[Seed] = &[
        ("15551110001@s.whatsapp.net", "Ayesha Khan", false, true, &[
            ("Are we still on for dinner tonight?", false, None, -2 * h - 600),
            ("Yes! 8pm works", true, None, -2 * h - 500),
            ("I booked the table at that new place on Main St", true, None, -2 * h - 480),
            ("Perfect 🙌", false, None, -2 * h - 300),
            ("Should I bring anything?", false, None, -2 * h - 290),
            ("Just yourself. And maybe that dessert you made last time 😄", true, None, -2 * h - 100),
            ("Haha deal. See you at 8!", false, None, -300),
        ]),
        ("120363000000000001@g.us", "Weekend Hiking 🥾", true, false, &[
            ("Trail conditions look great for Saturday", false, Some("Omar"), -d - 4 * h),
            ("I'm in. Meeting at the north parking lot?", false, Some("Sara"), -d - 3 * h),
            ("Works for me, I'll bring snacks", true, None, -d - 3 * h + 300),
            ("Don't forget sunscreen this time 😅", false, Some("Omar"), -d - 2 * h),
            ("That was ONE time", false, Some("Sara"), -d - 2 * h + 60),
            ("@15551110011 you're bringing the map, right? cc @15550000000", false, Some("Sara"), -d - 2 * h + 120),
        ]),
        ("15551110003@s.whatsapp.net", "Bilal", false, false, &[
            ("Did you push the fix?", false, None, -2 * d),
            ("Yep, it's on main now", true, None, -2 * d + 120),
            ("Legend 🚀", false, None, -2 * d + 200),
        ]),
        ("15551110004@s.whatsapp.net", "Mom", false, false, &[
            ("Call me when you're free", false, None, -4 * d),
            ("Will do after work ❤️", true, None, -4 * d + 1800),
        ]),
        ("15551110005@s.whatsapp.net", "Zain Ahmed", false, false, &[
            ("This is a much longer message to make sure wrapping works nicely inside the bubble, including multiple lines of text that keep going for a while so we can see how it looks.", false, None, -9 * d),
            ("Looks good on my side", true, None, -9 * d + 60),
        ]),
        // Links: normal, bare, email, and the suspicious kinds (see links.rs).
        ("120363000000000002@g.us", "Links 🔗", true, false, &[
            ("Docs are at https://docs.rs/url/latest/url/ (and the intro at https://en.wikipedia.org/wiki/Rust_(programming_language)).", false, Some("Omar"), -10 * 60),
            ("Or just www.example.com, or mail jane.doe+wa@example.co.uk!", true, None, -9 * 60),
            ("Your account is locked, verify at \u{430}pple.com/verify", false, Some("Sara"), -8 * 60),
            ("Login: https://google.com@evil.example/login", false, Some("Sara"), -7 * 60),
            ("Invoice: https://files.example/\u{202E}fdp.exe", false, Some("Omar"), -6 * 60),
            ("@15551110011 router is at http://192.168.1.1/admin, notes.txt has the rest", true, None, -5 * 60),
        ]),
    ];

    let mut chats = Vec::new();
    for (jid, name, is_group, unread, lines) in convo {
        for (i, (text, from_me, sender, off)) in lines.iter().enumerate() {
            messages.push(msg(jid, i, *from_me, *sender, text, n + off));
        }
        let last = lines.last().unwrap();
        chats.push(ChatSummary {
            jid: jid.to_string(),
            name: name.to_string(),
            is_group: *is_group,
            last_message: Some(MessagePreview {
                text: mock_mentions(last.0).iter().fold(last.0.to_string(), |t, m| t.replace(&format!("@{}", m.user), &format!("@{}", m.name))),
                timestamp: n + last.3,
                from_me: last.1,
                sender_name: last.2.map(String::from),
            }),
            unread_count: if *unread { 1 } else { 0 },
            pinned: false,
            archived: false,
            muted: false,
            avatar: None,
        });
    }
    // Media + a reply in Ayesha's chat.
    let ayesha = "15551110001@s.whatsapp.net";
    let mut photo = msg(ayesha, 100, false, None, "", n - 260);
    photo.body = MessageBody::Image(ImageMedia {
        path: None,
        thumbnail: None,
        width: Some(1600),
        height: Some(900),
        caption: Some("The view from the restaurant 😍".into()),
        state: MediaState::Idle,
    });
    let mut voice = msg(ayesha, 101, false, None, "", n - 250);
    voice.body = MessageBody::Voice(VoiceMedia {
        path: None,
        duration_secs: 7,
        waveform: (0..64).map(|i| ((i as f32 * 0.45).sin().abs() * 80. + 10.) as u8).collect(),
        ptt: true,
        state: MediaState::Idle,
    });
    let mut reply = msg(ayesha, 102, true, None, "Wow that looks amazing", n - 240);
    reply.quoted = Some(QuotedMessage {
        id: format!("{ayesha}-100"),
        from_me: false,
        sender_name: Some("Ayesha Khan".into()),
        preview: "📷 The view from the restaurant 😍".into(),
    });
    reply.reactions = vec![Reaction {
        emoji: "❤️".into(),
        sender_jid: ayesha.into(),
        sender_name: Some("Ayesha Khan".into()),
        from_me: false,
        timestamp: n - 230,
    }];
    messages.extend([photo, voice, reply]);
    // A few tapbacks in the group, including one of mine.
    for m in messages.iter_mut().filter(|m| m.chat_jid == "120363000000000001@g.us") {
        if m.id.ends_with("-3") {
            m.reactions = vec![
                Reaction { emoji: "😂".into(), sender_jid: "Sara@s.whatsapp.net".into(), sender_name: Some("Sara".into()), from_me: false, timestamp: n },
                Reaction { emoji: "😂".into(), sender_jid: ME.into(), sender_name: None, from_me: true, timestamp: n },
                Reaction { emoji: "👍".into(), sender_jid: "Ali@s.whatsapp.net".into(), sender_name: Some("Ali".into()), from_me: false, timestamp: n },
            ];
        }
        if m.id.ends_with("-4") {
            m.quoted = Some(QuotedMessage { id: format!("{}-3", m.chat_jid), from_me: false, sender_name: Some("Omar".into()), preview: "Don't forget sunscreen this time 😅".into() });
        }
    }

    // Every other message kind, in a chat that sorts first.
    let kinds = "15551110010@s.whatsapp.net";
    let mut k = |i: usize, from_me: bool, body: MessageBody| {
        let mut m = msg(kinds, 200 + i, from_me, None, "", n - 200 + i as i64 * 5);
        m.body = body;
        messages.push(m);
    };
    k(0, false, MessageBody::System { text: "📞 Missed voice call".into() });
    k(1, false, MessageBody::Rich(RichText {
        header: Some("Your bill is ready".into()),
        body: "Your October bill of Rs. 1,250 is due on 5 Nov. Pay now to avoid late charges.".into(),
        footer: Some("Jazz · Official account".into()),
        buttons: vec!["Pay now".into(), "View bill".into()],
    }));
    k(2, true, MessageBody::Document(DocumentMedia {
        path: None,
        file_name: "Quarterly report.pdf".into(),
        mimetype: Some("application/pdf".into()),
        size_bytes: Some(2_400_000),
        page_count: Some(14),
        thumbnail: None,
        caption: None,
        state: MediaState::Idle,
    }));
    k(3, false, MessageBody::Location(LocationInfo {
        latitude: 31.5204,
        longitude: 74.3587,
        name: Some("Liberty Market".into()),
        address: Some("Gulberg III, Lahore".into()),
        thumbnail: None,
        live: false,
    }));
    k(4, false, MessageBody::Contacts(vec![ContactCard { display_name: "Hamza Ali".into(), phones: vec!["+92 300 1234567".into()] }]));
    k(5, true, MessageBody::Poll(PollInfo {
        question: "Dinner on Friday?".into(),
        options: vec!["Pizza".into(), "BBQ".into(), "Chinese".into()],
        multiple: false,
    }));
    k(6, false, MessageBody::Video(VideoMedia {
        path: None,
        thumbnail: None,
        width: Some(1280),
        height: Some(720),
        duration_secs: 42,
        caption: None,
        gif: false,
        round: false,
        state: MediaState::Idle,
    }));
    // WA_MOCK_STICKER=<file.webp> shows that (e.g. animated) sticker instead of a failed one.
    let sticker_path = std::env::var_os("WA_MOCK_STICKER").map(std::path::PathBuf::from);
    let sticker_state = if sticker_path.is_some() { MediaState::Idle } else { MediaState::Failed };
    k(7, false, MessageBody::Sticker(StickerMedia { path: sticker_path, width: None, height: None, animated: true, state: sticker_state }));
    chats.insert(0, ChatSummary {
        jid: kinds.into(),
        name: "Message Kinds".into(),
        is_group: false,
        last_message: Some(MessagePreview { text: "Sticker".into(), timestamp: n - 165, from_me: false, sender_name: None }),
        unread_count: 0,
        pinned: true,
        archived: false,
        muted: false,
        avatar: None,
    });

    // A message that wasn't delivered, to show "Try Again".
    if let Some(m) = messages.iter_mut().find(|m| m.chat_jid == "15551110003@s.whatsapp.net" && m.from_me) {
        m.status = DeliveryStatus::Failed;
    }

    // A long thread: 300 messages stored locally, 150 more only on "the phone".
    let mut phone_only = Vec::new();
    for i in 0..450 {
        let ts = n - (450 - i as i64) * 1800;
        let m = msg(LONG_CHAT, i, i % 3 == 0, None, &format!("Message #{i}"), ts);
        if i < 150 { phone_only.push(m) } else { messages.push(m) }
    }
    chats.push(ChatSummary {
        jid: LONG_CHAT.into(),
        name: "Long Thread".into(),
        is_group: false,
        last_message: Some(MessagePreview { text: "Message #449".into(), timestamp: n - 1800, from_me: false, sender_name: None }),
        unread_count: 0,
        pinned: true,
        archived: false,
        muted: false,
        avatar: None,
    });
    // Pad with extra contacts to exercise scrolling.
    for i in 0..18 {
        let jid = format!("1555200{i:04}@s.whatsapp.net");
        let ts = n - (12 + i as i64) * d;
        messages.push(msg(&jid, 0, false, None, "See you soon!", ts));
        chats.push(ChatSummary {
            jid,
            name: ["Hamza", "Fatima Noor", "Ali Raza", "Hina", "Usman Tariq", "Mariam"][i % 6].to_string(),
            is_group: false,
            last_message: Some(MessagePreview { text: "See you soon!".into(), timestamp: ts, from_me: false, sender_name: None }),
            unread_count: 0,
            pinned: false,
            archived: false,
            muted: false,
            avatar: None,
        });
    }
    // Forwarded and deleted messages, and replies to messages far up the long thread
    // (one stored but not loaded, one only on "the phone").
    let hiking = "120363000000000001@g.us";
    let mut fwd = msg(hiking, 50, false, Some("Sara"), "Trail closed after 4pm on weekends, plan accordingly!", n - d - h);
    fwd.forwarded = true;
    let mut gone = msg(hiking, 51, false, Some("Omar"), "", n - d - h + 120);
    gone.body = MessageBody::Unsupported { label: "🚫 This message was deleted".into() };
    gone.deleted = true;
    messages.extend([fwd, gone]);
    for (i, (target, text)) in [(200, "Still relevant 😅"), (40, "Remember this one?")].into_iter().enumerate() {
        let mut m = msg(LONG_CHAT, 500 + i, true, None, text, n - 900 + i as i64 * 60);
        m.quoted = Some(QuotedMessage {
            id: format!("{LONG_CHAT}-{target}"),
            from_me: target % 3 == 0,
            sender_name: None,
            preview: format!("Message #{target}"),
        });
        messages.push(m);
    }
    // Chat-list states: a muted group, more unread, a third pin, archived chats.
    for c in chats.iter_mut() {
        match c.jid.as_str() {
            "120363000000000001@g.us" => (c.muted, c.unread_count) = (true, 4),
            "15551110003@s.whatsapp.net" => c.unread_count = 2,
            "15551110004@s.whatsapp.net" => c.pinned = true,
            "15552000016@s.whatsapp.net" | "15552000017@s.whatsapp.net" => c.archived = true,
            _ => {}
        }
    }
    sort_chats(&mut chats);
    Fake { chats, messages, phone_only }
}

impl Fake {
    fn update_chat(&mut self, jid: &str, f: impl FnOnce(&mut ChatSummary), ev: &flume::Sender<Event>) {
        if let Some(c) = self.chats.iter_mut().find(|c| c.jid == jid) {
            f(c);
        }
        sort_chats(&mut self.chats);
        let _ = ev.send(Event::Chats(self.chats.clone()));
    }
}

impl Fake {
    /// Like the backend: show my message as Pending, then Sent.
    fn send_own(&mut self, m: ChatMessage, ev: &flume::Sender<Event>) {
        self.messages.push(m.clone());
        let _ = ev.send(Event::MessageUpserted(m.clone()));
        let text = crate::state::body_text(&m.body).to_string();
        self.update_chat(
            &m.chat_jid,
            |c| c.last_message = Some(MessagePreview { text, timestamp: m.timestamp, from_me: true, sender_name: None }),
            ev,
        );
        let ev = ev.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(900));
            let _ = ev.send(Event::StatusChanged { chat_jid: m.chat_jid, message_ids: vec![m.id], status: DeliveryStatus::Sent });
        });
    }
}

/// Pinned first, then most recent activity (like the real backend).
fn sort_chats(chats: &mut [ChatSummary]) {
    chats.sort_by_key(|c| (!c.pinned, std::cmp::Reverse(c.last_message.as_ref().map_or(i64::MAX, |l| l.timestamp))));
}

/// People in "my address book" for the New Message picker: everyone with a 1:1 chat,
/// plus a few without one.
fn mock_contacts(chats: &[ChatSummary]) -> Vec<Contact> {
    let phone = |jid: &str| Some(format!("+{}", jid.split('@').next().unwrap_or("")));
    let mut list: Vec<Contact> = chats
        .iter()
        .filter(|c| !c.is_group)
        .map(|c| Contact { jid: c.jid.clone(), name: c.name.clone(), phone: phone(&c.jid), saved: true })
        .collect();
    for (user, name) in [("15553330001", "Nadia Hussain"), ("15553330002", "Imran Sheikh"), ("15553330003", "Sana Malik")] {
        let jid = format!("{user}@s.whatsapp.net");
        list.push(Contact { phone: phone(&jid), jid, name: name.into(), saved: true });
    }
    list.sort_by_key(|c| c.name.to_lowercase());
    list.dedup_by(|a, b| a.jid == b.jid);
    list
}

fn run(cmds: flume::Receiver<Command>, ev: flume::Sender<Event>) {
    let mut fake = seed();
    let _ = ev.send(Event::Connection(ConnectionState::Connecting));
    thread::sleep(Duration::from_millis(300));
    let _ = ev.send(Event::Connection(ConnectionState::Connected { own_jid: ME.into(), push_name: Some("Me".into()) }));
    let _ = ev.send(Event::Chats(fake.chats.clone()));
    // WA_MOCK_GROUP_TYPING=1: two people keep typing in the hiking group.
    if std::env::var_os("WA_MOCK_GROUP_TYPING").is_some() {
        let ev = ev.clone();
        thread::spawn(move || {
            for who in ["Omar", "Sara"] {
                thread::sleep(Duration::from_millis(1500));
                let _ = ev.send(Event::Typing {
                    chat_jid: "120363000000000001@g.us".into(),
                    sender_name: Some(who.into()),
                    typing: true,
                    sender_jid: Some(format!("{who}@s.whatsapp.net")),
                });
            }
        });
    }
    // WA_MOCK_INCOMING="<jid>|<secs>": a message from them arrives after that long.
    let (incoming_tx, incoming) = flume::unbounded::<String>();
    let _keep_open = incoming_tx.clone();
    if let Ok(spec) = std::env::var("WA_MOCK_INCOMING")
        && let Some((jid, secs)) = spec.split_once('|')
    {
        let (jid, secs) = (jid.to_string(), secs.parse().unwrap_or(5));
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(secs));
            let _ = incoming_tx.send(jid);
        });
    }

    loop {
        let next = flume::Selector::new()
            .recv(&cmds, |c| c.map(Ok))
            .recv(&incoming, |j| j.map(Err))
            .wait();
        let cmd = match next {
            Ok(Ok(cmd)) => cmd,
            Ok(Err(jid)) => {
                let text = "Are you there? 👀";
                let mut m = msg(&jid, 0, false, None, text, now());
                m.id = format!("{jid}-incoming-{}", now());
                fake.messages.push(m.clone());
                if let Some(c) = fake.chats.iter_mut().find(|c| c.jid == jid) {
                    c.unread_count += 1;
                    c.last_message = Some(MessagePreview { text: text.into(), timestamp: m.timestamp, from_me: false, sender_name: None });
                }
                sort_chats(&mut fake.chats);
                eprintln!("mock: incoming message in {jid}");
                let _ = ev.send(Event::MessageUpserted(m));
                let _ = ev.send(Event::Chats(fake.chats.clone()));
                continue;
            }
            Err(_) => break,
        };
        match cmd {
            Command::LoadMessages { chat_jid, before, limit } => {
                let older: Vec<_> = fake
                    .messages
                    .iter()
                    .filter(|m| m.chat_jid == chat_jid && before.is_none_or(|b| m.timestamp < b))
                    .cloned()
                    .collect();
                let page = older[older.len().saturating_sub(limit as usize)..].to_vec();
                let local_more = page.len() < older.len();
                let remote = chat_jid == LONG_CHAT && !fake.phone_only.is_empty();
                let _ = ev.send(Event::Messages { chat_jid: chat_jid.clone(), messages: page, has_more: local_more || remote });
                if before.is_some() && !local_more && remote {
                    // Simulate the phone answering an on-demand request with 50 messages.
                    let _ = ev.send(Event::HistoryLoading { chat_jid: chat_jid.clone(), loading: true, exhausted: false });
                    thread::sleep(Duration::from_millis(1200));
                    let batch: Vec<_> = fake.phone_only.split_off(fake.phone_only.len().saturating_sub(50));
                    fake.messages.extend(batch.iter().cloned());
                    let exhausted = fake.phone_only.is_empty();
                    let _ = ev.send(Event::Messages { chat_jid: chat_jid.clone(), messages: batch, has_more: !exhausted });
                    let _ = ev.send(Event::HistoryLoading { chat_jid, loading: false, exhausted });
                }
            }
            Command::SendText { chat_jid, text, client_id, reply_to, mentions } => {
                eprintln!("mock: SendText {text:?} mentions {mentions:?}");
                let mut m = msg(&chat_jid, 0, true, None, &text, now());
                m.id = client_id;
                m.quoted = reply_to.and_then(|id| fake.messages.iter().find(|q| q.id == id)).map(|q| QuotedMessage {
                    id: q.id.clone(),
                    from_me: q.from_me,
                    sender_name: q.sender_name.clone(),
                    preview: crate::state::body_text(&q.body).to_string(),
                });
                m.status = DeliveryStatus::Pending;
                fake.messages.push(m.clone());
                if let Some(c) = fake.chats.iter_mut().find(|c| c.jid == chat_jid) {
                    c.last_message = Some(MessagePreview { text: text.clone(), timestamp: m.timestamp, from_me: true, sender_name: None });
                }
                fake.chats.sort_by_key(|c| std::cmp::Reverse(c.last_message.as_ref().map_or(0, |l| l.timestamp)));
                let _ = ev.send(Event::MessageUpserted(m.clone()));
                let _ = ev.send(Event::Chats(fake.chats.clone()));
                let ev = ev.clone();
                thread::spawn(move || {
                    for status in [DeliveryStatus::Sent, DeliveryStatus::Delivered, DeliveryStatus::Read] {
                        thread::sleep(Duration::from_millis(700));
                        let _ = ev.send(Event::StatusChanged { chat_jid: m.chat_jid.clone(), message_ids: vec![m.id.clone()], status });
                    }
                    let _ = ev.send(Event::Typing { chat_jid: m.chat_jid.clone(), sender_name: None, typing: true, sender_jid: None });
                    thread::sleep(Duration::from_millis(1500));
                    let _ = ev.send(Event::Typing { chat_jid: m.chat_jid.clone(), sender_name: None, typing: false, sender_jid: None });
                    let mut reply = msg(&m.chat_jid, 0, false, None, "👍", now());
                    reply.id = format!("{}-reply", m.id);
                    let _ = ev.send(Event::MessageUpserted(reply));
                });
            }
            Command::MarkRead { chat_jid } => {
                eprintln!("mock: MarkRead {chat_jid}");
                if let Some(c) = fake.chats.iter_mut().find(|c| c.jid == chat_jid) {
                    c.unread_count = 0;
                }
                let _ = ev.send(Event::Chats(fake.chats.clone()));
            }
            Command::SetTyping { .. } | Command::FetchAvatar { .. } => {}
            Command::RetrySend { chat_jid, message_id } => {
                // Like the backend: a resend moves to the end with the current time.
                if let Some(m) = fake.messages.iter_mut().find(|m| m.chat_jid == chat_jid && m.id == message_id) {
                    (m.status, m.timestamp) = (DeliveryStatus::Pending, now());
                    let m = m.clone();
                    let _ = ev.send(Event::MessageUpserted(m.clone()));
                    let text = crate::state::body_text(&m.body).to_string();
                    fake.update_chat(
                        &chat_jid,
                        |c| c.last_message = Some(MessagePreview { text, timestamp: m.timestamp, from_me: true, sender_name: None }),
                        &ev,
                    );
                    thread::sleep(Duration::from_millis(600));
                    let _ = ev.send(Event::StatusChanged { chat_jid, message_ids: vec![message_id], status: DeliveryStatus::Sent });
                }
            }
            Command::EditMessage { chat_jid, message_id, text } => {
                if let Some(m) = fake.messages.iter_mut().find(|m| m.chat_jid == chat_jid && m.id == message_id) {
                    m.body = MessageBody::Text(text);
                    m.edited = true;
                    let _ = ev.send(Event::MessageUpserted(m.clone()));
                }
            }
            Command::ListStickers => {
                let preview = mock_photo();
                let list = (0..10)
                    .map(|i| StickerRef {
                        id: format!("mock-sticker-{i}"),
                        preview: preview.clone(),
                        source: if i < 4 { StickerSource::Favorite } else { StickerSource::Recent },
                        animated: false,
                    })
                    .collect();
                let _ = ev.send(Event::Stickers(list));
            }
            Command::SendSticker { chat_jid, client_id, .. } => {
                let mut m = msg(&chat_jid, 0, true, None, "", now());
                m.id = client_id;
                m.status = DeliveryStatus::Sent;
                m.body = MessageBody::Sticker(StickerMedia { path: mock_photo(), width: None, height: None, animated: false, state: MediaState::Idle });
                fake.messages.push(m.clone());
                let _ = ev.send(Event::MessageUpserted(m));
            }
            Command::FetchProfile { jid } => {
                let chat = fake.chats.iter().find(|c| c.jid == jid);
                let name = chat.map_or_else(|| jid.split('@').next().unwrap_or("").to_string(), |c| c.name.clone());
                let is_group = jid.ends_with("@g.us");
                let participant = |n: &str, admin: bool, me: bool| Participant {
                    jid: if me { ME.into() } else { member_jid(n) },
                    name: n.to_string(),
                    is_admin: admin,
                    is_me: me,
                };
                let _ = ev.send(Event::Profile(Profile {
                    jid: jid.clone(),
                    name,
                    phone: (!is_group).then(|| format!("+{}", jid.split('@').next().unwrap_or(""))),
                    about: (!is_group).then(|| "Busy building things 🛠️".to_string()),
                    photo: None,
                    is_group,
                    description: is_group.then(|| "Weekend hikes around Margalla Hills. Bring water!".to_string()),
                    participants: if is_group {
                        vec![
                            participant("Omar", true, false),
                            participant("Me", false, true),
                            participant("Ali Raza", false, false),
                            participant("Sara Ali", false, false),
                        ]
                    } else {
                        Vec::new()
                    },
                    created_at: None,
                }));
            }
            Command::React { chat_jid, message_id, emoji } => {
                let Some(m) = fake.messages.iter_mut().find(|m| m.chat_jid == chat_jid && m.id == message_id) else {
                    continue;
                };
                m.reactions.retain(|r| !r.from_me);
                if let Some(emoji) = emoji {
                    m.reactions.push(Reaction { emoji, sender_jid: ME.into(), sender_name: None, from_me: true, timestamp: now() });
                }
                let _ = ev.send(Event::MessageUpserted(m.clone()));
            }
            Command::DownloadMedia { chat_jid, message_id } => {
                let Some(m) = fake.messages.iter_mut().find(|m| m.chat_jid == chat_jid && m.id == message_id) else {
                    continue;
                };
                match &mut m.body {
                    MessageBody::Image(img) => img.path = mock_photo(),
                    MessageBody::Voice(v) => v.path = mock_voice(v.duration_secs),
                    _ => continue,
                }
                let _ = ev.send(Event::MessageUpserted(m.clone()));
            }
            Command::SendImage { chat_jid, path, client_id, caption, .. } => {
                let mut m = msg(&chat_jid, 0, true, None, "", now());
                m.id = client_id;
                m.status = DeliveryStatus::Sent;
                m.body = MessageBody::Image(ImageMedia {
                    path: Some(path),
                    thumbnail: None,
                    width: None,
                    height: None,
                    caption,
                    state: MediaState::Idle,
                });
                fake.messages.push(m.clone());
                let _ = ev.send(Event::MessageUpserted(m));
            }
            Command::Logout => {
                let _ = ev.send(Event::Connection(ConnectionState::AwaitingQr {
                    code: "2@mock-qr-payload,not-a-real-session,demo".into(),
                    valid_for_secs: 60,
                }));
            }
            Command::DeleteMessage { chat_jid, message_id, for_everyone } => {
                let Some(pos) = fake.messages.iter().position(|m| m.chat_jid == chat_jid && m.id == message_id) else {
                    continue;
                };
                if for_everyone {
                    let m = &mut fake.messages[pos];
                    m.body = MessageBody::Unsupported { label: "🚫 This message was deleted".into() };
                    (m.deleted, m.reactions, m.quoted) = (true, Vec::new(), None);
                    let _ = ev.send(Event::MessageUpserted(m.clone()));
                } else {
                    fake.messages.remove(pos);
                    let _ = ev.send(Event::MessagesRemoved { chat_jid, message_ids: vec![message_id] });
                }
            }
            Command::ForwardMessage { chat_jid, message_id, to_chat_jid, client_id } => {
                let Some(src) = fake.messages.iter().find(|m| m.chat_jid == chat_jid && m.id == message_id) else {
                    continue;
                };
                let mut m = msg(&to_chat_jid, 0, true, None, "", now());
                (m.id, m.body, m.forwarded, m.status) = (client_id, src.body.clone(), true, DeliveryStatus::Sent);
                let text = crate::state::body_text(&m.body).to_string();
                fake.messages.push(m.clone());
                eprintln!("mock: forwarded {message_id} to {to_chat_jid}");
                let _ = ev.send(Event::MessageUpserted(m.clone()));
                fake.update_chat(
                    &to_chat_jid,
                    |c| c.last_message = Some(MessagePreview { text, timestamp: m.timestamp, from_me: true, sender_name: None }),
                    &ev,
                );
            }
            Command::SearchMessages { chat_jid, query } => {
                let needle = query.trim().to_lowercase();
                let hits = fake
                    .messages
                    .iter()
                    .chain(fake.phone_only.iter())
                    .filter(|m| m.chat_jid == chat_jid && !m.deleted && !needle.is_empty())
                    .filter(|m| crate::state::body_text(&m.body).to_lowercase().contains(&needle))
                    .map(|m| SearchHit { id: m.id.clone(), timestamp: m.timestamp })
                    .collect::<Vec<_>>();
                let mut hits = hits;
                hits.sort_by_key(|h| h.timestamp);
                // Like the store, a search over the whole chat takes a moment.
                thread::sleep(Duration::from_millis(150));
                let _ = ev.send(Event::SearchResults { chat_jid, query, hits });
            }
            Command::SetPinned { chat_jid, pinned } => {
                if pinned && fake.chats.iter().filter(|c| c.pinned && c.jid != chat_jid).count() >= 3 {
                    let _ = ev.send(Event::Error("You can only pin up to 3 chats".into()));
                    continue;
                }
                fake.update_chat(&chat_jid, |c| c.pinned = pinned, &ev);
            }
            Command::SetMuted { chat_jid, until } => fake.update_chat(&chat_jid, |c| c.muted = until > now(), &ev),
            Command::SetArchived { chat_jid, archived } => fake.update_chat(
                &chat_jid,
                |c| {
                    c.archived = archived;
                    c.pinned &= !archived;
                },
                &ev,
            ),
            Command::MarkUnread { chat_jid } => {
                eprintln!("mock: MarkUnread {chat_jid}");
                fake.update_chat(&chat_jid, |c| c.unread_count = c.unread_count.max(1), &ev)
            }
            Command::ListContacts => {
                let _ = ev.send(Event::Contacts(mock_contacts(&fake.chats)));
            }
            Command::LookupPhone { phone } => {
                // Numbers ending in 0 "aren't on WhatsApp".
                let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
                let (jid, error) = match digits.len() {
                    0..8 => (None, Some("Enter the full number with country code".to_string())),
                    _ if digits.ends_with('0') => (None, None),
                    _ => (Some(format!("{digits}@s.whatsapp.net")), None),
                };
                thread::sleep(Duration::from_millis(400));
                let _ = ev.send(Event::PhoneLookup { phone, jid, error });
            }
            Command::OpenChat { jid } => {
                if !fake.chats.iter().any(|c| c.jid == jid) {
                    let name = mock_contacts(&fake.chats)
                        .into_iter()
                        .find(|c| c.jid == jid)
                        .map_or_else(|| format!("+{}", jid.split('@').next().unwrap_or("")), |c| c.name);
                    let at = fake.chats.iter().take_while(|c| c.pinned).count();
                    fake.chats.insert(at, ChatSummary {
                        jid,
                        name,
                        is_group: false,
                        last_message: None,
                        unread_count: 0,
                        pinned: false,
                        archived: false,
                        muted: false,
                        avatar: None,
                    });
                    let _ = ev.send(Event::Chats(fake.chats.clone()));
                }
            }
            Command::SendFile { chat_jid, path, caption, client_id, .. } => {
                let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                let size = std::fs::metadata(&path).map(|m| m.len()).ok();
                let video = path.extension().is_some_and(|e| ["mp4", "mov", "m4v"].contains(&e.to_string_lossy().to_lowercase().as_str()));
                eprintln!("mock: SendFile {name} ({size:?} bytes, video: {video}) caption {caption:?}");
                let mut m = msg(&chat_jid, 0, true, None, "", now());
                (m.id, m.status) = (client_id, DeliveryStatus::Pending);
                m.body = if video {
                    MessageBody::Video(VideoMedia {
                        path: Some(path),
                        thumbnail: None,
                        width: Some(1280),
                        height: Some(720),
                        duration_secs: 15,
                        caption,
                        gif: false,
                        round: false,
                        state: MediaState::Idle,
                    })
                } else {
                    MessageBody::Document(DocumentMedia {
                        path: Some(path),
                        file_name: name,
                        mimetype: None,
                        size_bytes: size,
                        page_count: None,
                        thumbnail: None,
                        caption,
                        state: MediaState::Idle,
                    })
                };
                fake.send_own(m, &ev);
            }
            Command::SendVoice { chat_jid, samples, sample_rate, client_id, .. } => {
                let secs = (samples.len() as f32 / sample_rate as f32).round().max(1.) as u32;
                eprintln!("mock: SendVoice {} samples at {sample_rate} Hz ({secs} s)", samples.len());
                let path = std::env::temp_dir().join(format!("wa-mock-sent-{client_id}.wav"));
                let _ = std::fs::write(&path, wav_bytes(&samples, sample_rate));
                let mut m = msg(&chat_jid, 0, true, None, "", now());
                (m.id, m.status) = (client_id, DeliveryStatus::Pending);
                let levels: Vec<f32> = samples
                    .chunks((samples.len() / 64).max(1))
                    .take(64)
                    .map(|c| c.iter().map(|v| f32::from(*v).abs()).sum::<f32>() / c.len() as f32)
                    .collect();
                let max = levels.iter().copied().fold(1., f32::max);
                m.body = MessageBody::Voice(VoiceMedia {
                    path: Some(path),
                    duration_secs: secs,
                    waveform: levels.iter().map(|l| (l / max * 100.) as u8).collect(),
                    ptt: true,
                    state: MediaState::Idle,
                });
                fake.send_own(m, &ev);
            }
            Command::Shutdown => break,
        }
    }
}

/// A real photo for the mock: a macOS wallpaper thumbnail converted to JPEG.
fn mock_photo() -> Option<std::path::PathBuf> {
    let out = std::env::temp_dir().join("wa-mock-photo.jpg");
    if !out.exists() {
        let src = "/System/Library/Desktop Pictures/.thumbnails/Big Sur Coastline.heic";
        let ok = std::process::Command::new("sips")
            .args(["-s", "format", "jpeg", src, "--out"])
            .arg(&out)
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            return None;
        }
    }
    Some(out)
}

fn wav_bytes(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = samples.len() as u32 * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

/// A short WAV (two-tone chirp) standing in for a decoded voice note.
fn mock_voice(secs: u32) -> Option<std::path::PathBuf> {
    let out = std::env::temp_dir().join("wa-mock-voice.wav");
    if out.exists() {
        return Some(out);
    }
    let rate = 16_000u32;
    let samples: Vec<i16> = (0..rate * secs)
        .map(|i| {
            let t = i as f32 / rate as f32;
            let f = if ((t * 2.) as u32).is_multiple_of(2) { 440. } else { 660. };
            ((t * f * std::f32::consts::TAU).sin() * 6000.) as i16
        })
        .collect();
    let data_len = samples.len() as u32 * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(&out, wav).ok()?;
    Some(out)
}
