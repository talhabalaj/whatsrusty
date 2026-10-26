// A live, clickable replica of the app window: messages arrive, the "user" replies,
// and visitors can pick any conversation or type their own message.

const chats = [
  { id: 'ayesha', name: 'Ayesha Khan', time: '7:26 PM', msgs: [
    { text: 'Are we still on for tonight?' },
    { out: true, text: 'Yes! 8pm at the usual place 🍝' },
    { text: 'Haha deal. See you at 8!' },
  ] },
  { id: 'hiking', name: 'Weekend Hiking 🥾', group: true, time: 'Yesterday', msgs: [
    { from: 'Sara', text: 'Who is bringing snacks this time?' },
    { from: 'Bilal', text: 'I got chips. Last time I forgot the water…' },
    { from: 'Sara', text: 'That was ONE time' },
  ] },
  { id: 'bilal', name: 'Bilal', time: 'Saturday', msgs: [
    { out: true, text: 'Shipped it 🎉' },
    { text: 'Legend 🚀' },
  ] },
  { id: 'mom', name: 'Mom', time: 'Thursday', msgs: [
    { text: 'Call me when you are free' },
    { out: true, text: 'Will do after work ❤️' },
  ] },
  { id: 'zain', name: 'Zain Ahmed', time: '10/17/26', msgs: [
    { out: true, text: 'Can you review the deck?' },
    { text: 'Looks good on my side' },
  ] },
  { id: 'design', name: 'Design Team', group: true, time: '10/16/26', msgs: [
    { from: 'Hamza', text: 'New icons are in Figma' },
    { from: 'Fatima', text: 'Love the bubbles 💬' },
  ] },
  { id: 'fatima', name: 'Fatima Noor', time: '10/13/26', msgs: [{ text: 'See you soon!' }] },
  { id: 'hamza', name: 'Hamza', time: '10/14/26', msgs: [{ text: 'Send me the pics 📸' }] },
];

// What happens next. `select` and `reply` stand down while the visitor is driving.
const script = [
  ['wait', 1400], ['in', 'ayesha', ['Running 5 min late 🙈', 'Parking now, 2 min!', 'Grabbing a table by the window']],
  ['wait', 600], ['reply', 'ayesha', ['No rush, I just got here', 'Perfect, see you inside', 'Ooh nice, coming']],
  ['wait', 1300], ['react', 'ayesha', ['❤️', '👍', '😂']],
  ['wait', 1700], ['in', 'hiking', ['Trail starts 7am sharp ⛰️', 'Forecast says sunny ☀️', 'Who has the first-aid kit?'], 'Bilal'],
  ['wait', 1700], ['select', 'hiking'],
  ['wait', 800], ['in', 'hiking', ['Bringing coffee for everyone ☕️', 'I will bring the speaker 🔊', 'Meet at the gas station?'], 'Sara'],
  ['wait', 500], ['reply', 'hiking', ["I'll drive 🚗", 'Count me in!', 'I have the kit 🩹']],
  ['wait', 1900], ['in', 'mom', ['Did you eat?', 'Are you home yet?', 'Don’t forget your jacket']],
  ['wait', 1500], ['select', 'mom'],
  ['wait', 600], ['reply', 'mom', ['Yes mom, biryani 😋', 'Almost home!', 'Wearing it 🧥']],
  ['wait', 1300], ['react', 'mom', ['❤️', '❤️', '👍']],
  ['wait', 1800], ['in', 'zain', ['Pushed the fix, can you check?', 'Standup moved to 10', 'Lunch tomorrow?']],
  ['wait', 1600], ['select', 'ayesha'],
  ['wait', 2600],
];

const visitorReplies = [
  'Haha, I see you found the demo 😄',
  'This is a preview. The real thing runs on your Mac 💻',
  'Download it and say hi from WhatsRusty!',
  '😂😂',
];

export function startDemo(root) {
  const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
  const $ = selector => root.querySelector(selector);
  const list = $('.d-list'), thread = $('.d-thread'), title = $('.d-head strong'), titleAvatar = $('.d-head .d-av');
  const form = $('.d-composer'), input = $('.d-input'), banner = $('.d-banner');
  const byId = new Map(chats.map(chat => [chat.id, chat]));
  let order = chats.map(chat => chat.id);
  let selected = byId.get('ayesha');
  let clock = 7 * 60 + 28;
  let userUntil = 0;
  let visible = false, wake = null;

  const now = () => `${Math.floor(clock / 60) % 12 || 12}:${String(clock % 60).padStart(2, '0')} PM`;
  const pick = (options, round) => options[round % options.length];
  const userActive = () => Date.now() < userUntil;
  const waitVisible = () => (visible && !document.hidden ? null : new Promise(resolve => { wake = resolve; }));
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms)).then(waitVisible);
  const wakeUp = () => { if (visible && !document.hidden && wake) { wake(); wake = null; } };

  new IntersectionObserver(([entry]) => { visible = entry.isIntersecting; wakeUp(); }, { threshold: 0.3 }).observe(root);
  document.addEventListener('visibilitychange', wakeUp);

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }
  function initials(name) {
    const words = name.split(/\s+/).filter(word => /^\p{L}/u.test(word));
    return (words.length > 1 ? words[0][0] + words[1][0] : (words[0] ?? '?')[0]).toUpperCase();
  }
  function avatar(name, className = 'd-av') {
    return el('span', className, initials(name));
  }
  function preview(chat) {
    const last = chat.msgs.at(-1);
    if (!last) return '';
    return chat.group && !last.out && last.from ? `${last.from}: ${last.text}` : last.text;
  }

  // Sidebar rows are reused so a focused row keeps focus when the list reorders.
  const rows = new Map();
  function rowFor(chat) {
    let li = rows.get(chat.id);
    if (!li) {
      li = el('li');
      li.dataset.id = chat.id;
      const button = el('button', 'd-row');
      button.type = 'button';
      button.append(el('span', 'd-dot'), avatar(chat.name), el('span', 'd-meta'));
      button.addEventListener('click', () => { touch(); select(chat); });
      li.append(button);
      rows.set(chat.id, li);
    }
    const button = li.firstChild;
    button.classList.toggle('is-selected', chat === selected);
    button.classList.toggle('is-unread', !!chat.unread);
    button.setAttribute('aria-current', chat === selected ? 'true' : 'false');
    button.setAttribute('aria-label', `${chat.name}${chat.unread ? ', unread' : ''}: ${preview(chat)}`);
    const meta = button.querySelector('.d-meta');
    const top = el('span', 'd-top');
    top.append(el('strong', null, chat.name), el('time', null, chat.time));
    meta.replaceChildren(top, el('span', 'd-preview', preview(chat)));
    return li;
  }
  function renderList() {
    const before = new Map([...list.children].map(li => [li.dataset.id, li.getBoundingClientRect().top]));
    const focused = list.contains(document.activeElement) ? document.activeElement : null;
    for (const id of order) list.append(rowFor(byId.get(id)));
    if (focused) focused.focus({ preventScroll: true });
    if (reduce) return;
    for (const li of list.children) {
      const top = before.get(li.dataset.id);
      const dy = top == null ? 0 : top - li.getBoundingClientRect().top;
      if (dy) li.animate([{ transform: `translateY(${dy}px)` }, { transform: 'none' }], { duration: 380, easing: 'cubic-bezier(0.23, 1, 0.32, 1)' });
    }
  }

  function renderThread({ animateLast = false, popReaction = false } = {}) {
    const chat = selected;
    title.textContent = chat.name;
    titleAvatar.textContent = initials(chat.name);
    const nodes = [el('div', 'd-stamp', 'Today 7:20 PM')];
    const lastOut = chat.msgs.findLastIndex(msg => msg.out);
    chat.msgs.forEach((msg, i) => {
      const prev = chat.msgs[i - 1], next = chat.msgs[i + 1];
      const sameAsPrev = prev && !!prev.out === !!msg.out && prev.from === msg.from;
      const sameAsNext = next && !!next.out === !!msg.out && next.from === msg.from;
      if (chat.group && !msg.out && !sameAsPrev) nodes.push(el('div', 'd-sender', msg.from));
      const line = el('div', `d-line ${msg.out ? 'out' : 'in'}${sameAsPrev ? '' : ' first'}`);
      if (chat.group && !msg.out) line.append(sameAsNext ? el('span', 'd-av-slot') : avatar(msg.from, 'd-av d-av-sm'));
      const bubble = el('div', `d-b${sameAsNext ? '' : ' tail'}`, msg.text);
      if (msg.react) {
        const tap = el('span', 'd-tap', msg.react);
        if (popReaction && i === lastOut) tap.classList.add('pop');
        bubble.append(tap);
      }
      line.append(bubble);
      if (animateLast && i === chat.msgs.length - 1 && !reduce) line.classList.add('new');
      nodes.push(line);
      if (i === lastOut && msg.status) nodes.push(el('div', 'd-receipt', msg.status));
    });
    if (chat.typing) {
      const line = el('div', 'd-line in first');
      if (chat.group) line.append(avatar(chat.typing, 'd-av d-av-sm'));
      const typing = el('div', 'd-b tail d-typing');
      typing.setAttribute('aria-label', `${chat.typing} is typing`);
      typing.append(el('i'), el('i'), el('i'));
      line.append(typing);
      if (!reduce) line.classList.add('new');
      nodes.push(line);
    }
    thread.replaceChildren(...nodes);
    thread.scrollTo({ top: thread.scrollHeight, behavior: reduce || !animateLast ? 'auto' : 'smooth' });
  }

  function select(chat) {
    if (chat === selected) return;
    selected = chat;
    chat.unread = false;
    input.value = '';
    form.classList.remove('has-text');
    renderList();
    renderThread();
  }
  function bump(chat) {
    clock += 1;
    chat.time = now();
    order = [chat.id, ...order.filter(id => id !== chat.id)];
    if (chat.msgs.length > 14) chat.msgs.splice(0, chat.msgs.length - 14);
  }
  function showBanner(chat, msg) {
    const text = el('div');
    banner.replaceChildren(avatar(chat.name), text);
    text.append(el('strong', null, chat.name), el('span', null, chat.group ? `${msg.from}: ${msg.text}` : msg.text));
    banner.classList.remove('show');
    void banner.offsetWidth;
    banner.classList.add('show');
  }

  async function incoming(chat, text, from) {
    const sender = from ?? chat.name.split(' ')[0];
    if (chat === selected) {
      chat.typing = sender;
      renderThread({ animateLast: true });
      await sleep(1500);
      chat.typing = null;
    }
    const msg = { text, from: chat.group ? sender : undefined };
    chat.msgs.push(msg);
    bump(chat);
    if (chat !== selected) {
      chat.unread = true;
      showBanner(chat, msg);
    } else {
      // Our last message has now been seen.
      const lastOut = chat.msgs.findLast(m => m.out);
      if (lastOut?.status) lastOut.status = `Read ${now()}`;
      renderThread({ animateLast: true });
    }
    renderList();
  }
  async function send(chat, text) {
    chat.msgs.push({ out: true, text, status: 'Delivered' });
    bump(chat);
    renderList();
    renderThread({ animateLast: true });
    await sleep(1200);
    const msg = chat.msgs.findLast(m => m.out);
    if (msg && !chat.group) { msg.status = `Read ${now()}`; if (chat === selected) renderThread(); }
  }
  async function typeAndSend(chat, text) {
    if (reduce) { await send(chat, text); return; }
    for (const ch of text) {
      if (userActive() || selected !== chat) { input.value = ''; form.classList.remove('has-text'); return; }
      input.value += ch;
      form.classList.add('has-text');
      await sleep(35 + Math.random() * 55);
    }
    await sleep(250);
    input.value = '';
    form.classList.remove('has-text');
    await send(chat, text);
  }
  function react(chat, emoji) {
    const msg = chat.msgs.findLast(m => m.out);
    if (!msg) return;
    msg.react = emoji;
    if (chat === selected) renderThread({ popReaction: true });
  }

  // The visitor can take over: click any chat, or type and send a message.
  function touch() { userUntil = Date.now() + 20000; }
  root.addEventListener('pointerdown', touch);
  root.addEventListener('keydown', touch);
  input.addEventListener('input', () => form.classList.toggle('has-text', input.value.trim() !== ''));
  let replyIndex = 0;
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const text = input.value.trim();
    if (!text) return;
    touch();
    const chat = selected;
    input.value = '';
    form.classList.remove('has-text');
    await send(chat, text.slice(0, 200));
    await sleep(500);
    const member = chat.group ? chat.msgs.findLast(m => !m.out)?.from ?? 'Sara' : undefined;
    await incoming(chat, visitorReplies[replyIndex++ % visitorReplies.length], member);
  });

  renderList();
  renderThread();
  if (reduce) return;

  (async () => {
    await waitVisible();
    for (let round = 0; ; round++) {
      for (const [kind, id, value, from] of script) {
        if (kind === 'wait') { await sleep(id); continue; }
        const chat = byId.get(id);
        if (kind === 'in') await incoming(chat, pick(value, round), from);
        else if (kind === 'select' && !userActive()) select(chat);
        else if (kind === 'reply' && !userActive() && selected === chat) await typeAndSend(chat, pick(value, round));
        else if (kind === 'react') react(chat, pick(value, round));
      }
    }
  })();
}
