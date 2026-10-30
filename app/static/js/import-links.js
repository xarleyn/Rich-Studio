import { tr } from './i18n.js';
// Same contract and shared fixtures as services/import_links.py. Server validation is authoritative.
const reserved = new Set(['c', 's', 'share', 'joinchat', 'addstickers', 'proxy', 'socks', 'login', 'addlist', 'boost']);
const id = (value) => /^[1-9][0-9]{0,9}$/.test(value) && Number(value) <= 2147483647;

export function parseTelegramLink(raw) {
  const error = tr('Нужна ссылка https://t.me/канал/сообщение или https://t.me/канал/тема/сообщение.');
  if (typeof raw !== 'string' || raw.length > 2048 || [...raw.trim()].some((c) => c.charCodeAt(0) <= 32 || c.charCodeAt(0) === 127 || c === '\\' || c === '%')) throw new Error(error);
  let input = raw.trim();
  if (input.startsWith('t.me/')) input = 'https://' + input;
  // URL normalizes backslashes, explicit ports and dot segments; check the raw authority/path first.
  if (!/^https:\/\/t\.me\//i.test(input) || /\/(?:\.|\.\.)(?:\/|[?#]|$)/.test(input)) throw new Error(error);
  const url = new URL(input);
  if (url.protocol !== 'https:' || url.host.toLowerCase() !== 't.me' || url.username || url.password || url.hash) throw new Error(error);
  const parts = url.pathname.slice(1).replace(/\/$/, '').split('/');
  /** @type {string | null} */
  let username = null;
  /** @type {number | null} */
  let channel_id = null;
  let ids;
  if (parts[0] === 'c') {
    if (![3, 4].includes(parts.length) || !/^[1-9][0-9]{0,12}$/.test(parts[1])) throw new Error(error);
    channel_id = Number(parts[1]); ids = parts.slice(2);
  } else {
    if (![2, 3].includes(parts.length) || !/^[A-Za-z][A-Za-z0-9_]{3,31}$/.test(parts[0]) || reserved.has(parts[0].toLowerCase())) throw new Error(error);
    username = parts[0].toLowerCase(); ids = parts.slice(1);
  }
  if (!ids.every(id)) throw new Error(error);
  let topic_id = ids.length === 2 ? Number(ids[0]) : null;
  let single = false;
  const seen = new Set();
  for (const [key, value] of url.searchParams) {
    if (seen.has(key)) throw new Error(error);
    seen.add(key);
    if (key === 'single' && ['', '1'].includes(value)) single = true;
    else if (key === 'thread' && id(value)) {
      if (topic_id !== null && topic_id !== Number(value)) throw new Error(error);
      topic_id = Number(value);
    } else if (!(key === 't' && /^[0-9hms:]{1,20}$/.test(value))) throw new Error(error + tr(' Параметр ссылки не поддерживается.'));
  }
  const message_id = Number(ids.at(-1));
  const canonical = `https://t.me/${username || 'c/' + channel_id}/${topic_id ? topic_id + '/' : ''}${message_id}${single ? '?single' : ''}`;
  return { username, channel_id, message_id, topic_id, single, url: canonical };
}
