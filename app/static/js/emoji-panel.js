// Кастомные эмодзи: плавающая панель вставки в тексте и настройка паков в меню аккаунта.
// Паки хранятся на сервере (мета учётки, /api/emoji/packs), картинки — /api/emoji/img/<id>.

import { api } from './api.js';
import { act, h } from './dom.js';
import { openDialog } from './dialogs.js';
import { tr } from './i18n.js';

let packs = null;  // кэш списка паков: [{short_name, title, stickers: [{custom_emoji_id, emoji, kind}]}]

export async function loadPacks(force = false) {
  if (packs && !force) return packs;
  try {
    packs = (await api('GET', '/api/emoji/packs')).packs;
  } catch {
    packs = [];
  }
  return packs;
}

/** Карта id → 'video' | 'tgs' для анимированного предпросмотра; для статики ключа нет. */
export function emojiKinds(list = packs || []) {
  const kinds = {};
  for (const pack of list) {
    for (const st of pack.stickers || []) {
      if (st.kind === 'video' || st.kind === 'tgs') kinds[st.custom_emoji_id] = st.kind;
    }
  }
  return kinds;
}

let openPanel = null;  // текущая панель, чтобы закрыть при открытии новой

/** Плавающая панель выбора эмодзи рядом с кнопкой тулбара; onInsert получает сущность custom_emoji. */
export async function showEmojiPanel(anchor, onInsert) {
  closeEmojiPanel();
  const list = await loadPacks();
  const panel = h('div', { class: 'panel-emoji', role: 'dialog', 'aria-label': tr('Кастомные эмодзи') });
  const grid = h('div', { class: 'emoji-grid' });
  const tabs = h('div', { class: 'emoji-tabs' });
  const search = h('input', { type: 'search', placeholder: tr('Поиск по эмодзи'), 'aria-label': tr('Поиск по эмодзи') });
  let active = list[0] ? list[0].short_name : '';
  const drawGrid = () => {
    const query = search.value.trim().toLowerCase();
    const pack = list.find((p) => p.short_name === active) || list[0];
    grid.replaceChildren(
      ...((pack ? pack.stickers : []) || [])
        .filter((st) => !query || (st.emoji || '').toLowerCase().includes(query))
        .map((st) => h('button', {
          type: 'button', class: 'emoji-cell', title: st.emoji || st.custom_emoji_id,
          onclick: () => {
            onInsert({ type: 'custom_emoji', custom_emoji_id: st.custom_emoji_id, text: st.emoji || '⭐' });
            closeEmojiPanel();
          },
        }, h('img', { src: '/api/emoji/img/' + st.custom_emoji_id, alt: st.emoji || '', loading: 'lazy' }))),
      ...(list.length && !grid.children.length ? [h('p', { class: 'muted small', text: tr('Ничего не нашлось') })] : []),
    );
  };
  const drawTabs = () => {
    tabs.replaceChildren(
      ...list.map((p) => h('button', {
        type: 'button', class: p.short_name === active ? 'on' : '', title: p.title,
        text: p.title, onclick: () => { active = p.short_name; drawTabs(); drawGrid(); },
      })),
    );
  };
  if (!list.length) {
    panel.append(
      h('p', { class: 'muted', text: tr('Паки эмодзи ещё не настроены') }),
      h('button', { type: 'button', class: 'primary', text: tr('Настроить паки'), onclick: () => { closeEmojiPanel(); showPacksSettings(); } }),
    );
  } else {
    panel.append(tabs, search, grid);
    drawTabs();
    drawGrid();
    search.addEventListener('input', drawGrid);
  }
  document.body.append(panel);
  openPanel = panel;
  const r = anchor.getBoundingClientRect();
  panel.style.top = Math.min(innerHeight - panel.offsetHeight - 8, r.bottom + 6) + 'px';
  panel.style.left = Math.max(4, Math.min(r.left, innerWidth - panel.offsetWidth - 8)) + 'px';
  search.focus();
  setTimeout(() => {
    document.addEventListener('pointerdown', outside);
    document.addEventListener('keydown', escape);
  });
  function outside(ev) { if (!panel.contains(ev.target)) closeEmojiPanel(); }
  function escape(ev) { if (ev.key === 'Escape') closeEmojiPanel(); }
  function closeEmojiPanel() {
    if (!openPanel) return;
    document.removeEventListener('pointerdown', outside);
    document.removeEventListener('keydown', escape);
    openPanel.remove();
    openPanel = null;
  }
}

/** Диалог настройки паков: пункт меню аккаунта. Добавление и удаление идут на сервер сразу. */
export async function showPacksSettings() {
  const input = h('input', {
    type: 'text', placeholder: tr('t.me/addemoji/<код> или короткий код'), 'aria-label': tr('Набор эмодзи'),
  });
  const err = h('p', { class: 'err small' });
  const list = h('div', { class: 'packs-list' });
  const draw = async () => {
    const packs = await loadPacks(true);
    list.replaceChildren(
      ...packs.map((p) => h('div', { class: 'packs-row' },
        h('span', { class: 'packs-title', text: p.title }),
        h('span', { class: 'muted small', text: tr('эмодзи: ') + (p.stickers || []).length }),
        h('button', {
          type: 'button', class: 'danger small-btn', text: tr('Убрать'),
          onclick: () => act(async () => {
            await api('DELETE', '/api/emoji/packs/' + encodeURIComponent(p.short_name));
            await draw();
          }),
        }),
      )),
      ...(packs.length ? [] : [h('p', { class: 'muted', text: tr('Паков пока нет. Добавьте набор кастомных эмодзи — он появится в панели редактора.') })]),
    );
  };
  const submit = () => act(async () => {
    try {
      await api('POST', '/api/emoji/packs', { name: input.value });
      err.textContent = '';
      input.value = '';
      await draw();
    } catch (e) {
      err.textContent = e.message;
    }
  });
  input.addEventListener('keydown', (e) => { if (e.key === 'Enter') submit(); });
  openDialog(
    [h('h3', { text: tr('Паки эмодзи') }),
      h('p', { class: 'muted small', text: tr('Наборы кастомных эмодзи из Telegram. Короткий код — то, что в ссылке t.me/addemoji/<код>.') }),
      list, err,
      h('div', { class: 'row' }, input, h('button', { type: 'button', class: 'primary', text: tr('Добавить'), onclick: submit }))],
    [h('button', { class: 'ghost', value: 'cancel', text: tr('Готово') })],
  );
  await draw();
  input.focus();
}
