import { tr, dateLocale } from './i18n.js';
// Suggested posts from the channel's direct messages: shown above the list of posts until someone decides.
import { api } from './api.js';
import { closeDialog, openDialog } from './dialogs.js';
import { $, act, h } from './dom.js';
import { whenPicker } from './when.js';

const when = (s) => new Date(s * 1000).toLocaleString(dateLocale, { day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' });
const MEDIA = { photo: tr('фото'), video: tr('видео'), animation: 'GIF', audio: tr('аудио'), document: tr('файл'), voice: tr('голосовое'), video_note: tr('кружок'), live_photo: 'Live Photo', paid_media: tr('платное медиа') };

// open but not waiting: the decision went out with no answer, or an approval failed and can be tried again
const STATES = { approving: tr('принятие отправлено, ждём Telegram'), declining: tr('отказ отправлен, ждём Telegram'), failed: tr('одобрение не прошло') };

/** @param {SuggestedPost} s */
function price(s) {
  if (!s.price_amount) return tr('бесплатно');
  return s.price_currency === 'TON' ? `${s.price_amount / 1e9} TON` : `${s.price_amount} ⭐`;
}

/** The decision went out and Telegram did not answer: the person checks the channel's direct messages, then decides again.
 * @param {SuggestedPost} s */
function stuck(s) {
  const reopen = () => {
    if (!confirm(tr('Решение не дошло до Telegram? Если оно всё-таки дошло, Telegram может не принять второе, а итог появится сам.'))) return;
    act(() => api('POST', `/api/suggested-posts/${encodeURIComponent(s.id)}/reopen`), tr('Можно решить снова')).then((r) => { closeDialog(d); loadSuggested(); if (r) decide(r); });
  };
  const d = openDialog([
    h('h3', { text: tr('Предложенный пост') }),
    h('p', { class: 'small muted', text: `${s.author || tr('Подписчик')} · ${s.channel_title || tr('канал')} · ${price(s)}` }),
    h('div', { class: 'suggested-text' }, h('p', { text: s.text || tr('(без текста)') })),
    h('p', { text: (s.state === 'approving' ? tr('Принятие') : tr('Отказ')) + tr(' отправлено, но Telegram не ответил. Итог обычно приходит сам за несколько секунд. ')
      + tr('Если его нет, откройте личные сообщения канала: решение дошло, если пост там уже принят или отклонён. Если нет, решите снова.') })],
  [h('button', { class: 'ghost', value: 'cancel', text: tr('Закрыть') }),
    h('button', { class: 'primary', text: tr('Решить снова'), onclick: reopen })]);
}

/** @param {SuggestedPost} s */
function decide(s) {
  if (s.state === 'approving' || s.state === 'declining') { stuck(s); return; }
  const pick = whenPicker(Date.now() + 3600e3);
  const comment = /** @type {HTMLInputElement} */ (h('input', { type: 'text', maxlength: 128, placeholder: tr('Комментарий автору (необязательно)'), style: 'width:100%', 'aria-label': tr('Комментарий автору') }));
  // the list follows an error too: a decision without an answer shows as «ждём Telegram» and opens «Решить снова»
  const done = (r) => { if (r) closeDialog(d); loadSuggested(); };
  const approve = (later) => {
    const body = later ? { send_date: pick.value() } : {};
    if (!confirm((later ? tr('Опубликовать предложенный пост в выбранное время?') : tr('Опубликовать предложенный пост в канале сейчас?')) + (s.price_amount ? tr` С автора спишется ${price(s)}.` : ''))) return;
    act(() => api('POST', `/api/suggested-posts/${encodeURIComponent(s.id)}/approve`, body), tr('Пост принят: Telegram опубликует его')).then(done);
  };
  const d = openDialog([
    h('h3', { text: tr('Предложенный пост') }),
    h('p', { class: 'small muted', text: `${s.author || tr('Подписчик')} · ${s.channel_title || tr('канал')} · ${price(s)}` + (s.send_date ? tr` · автор просит ${when(s.send_date)}` : '') }),
    h('div', { class: 'suggested-text' }, s.media.length ? h('p', { class: 'small muted', text: tr('Вложения: ') + s.media.map((m) => MEDIA[m] || m).join(', ') }) : null, h('p', { text: s.text || tr('(без текста)') })),
    s.send_date ? null : h('details', {}, h('summary', { class: 'small', text: tr('Время для «В выбранное время»') }), pick.el),
    h('h4', { text: tr('Отклонить') }), comment,
    h('div', { class: 'row' }, h('button', { class: 'ghost', text: tr('Отклонить'), onclick: () => act(() => api('POST', `/api/suggested-posts/${encodeURIComponent(s.id)}/decline`, { comment: comment.value.trim() || null }), tr('Предложение отклонено')).then(done) }))],
  [h('button', { class: 'ghost', value: 'cancel', text: tr('Закрыть') }),
    s.send_date ? null : h('button', { class: 'ghost', text: tr('В выбранное время'), onclick: () => approve(true) }),
    h('button', { class: 'primary', text: s.send_date ? tr('Принять') : tr('Опубликовать сейчас'), onclick: () => approve(false) })]);
}

export async function loadSuggested() {
  const box = $('#suggested');
  const list = /** @type {SuggestedPost[]} */ (await api('GET', '/api/suggested-posts?state=open').catch(() => []));
  box.hidden = !list.length;
  box.replaceChildren(h('b', { class: 'small', text: tr`Предложенные посты: ${list.length}` }),
    h('ul', { class: 'list' }, list.map((s) => h('li', { onclick: () => decide(s), title: tr('Посмотреть, принять или отклонить') },
      h('span', { class: 'name', text: (s.text || tr('(без текста)')).split('\n')[0] }),
      h('span', { class: 'meta', text: `${s.author || tr('подписчик')} · ${price(s)}` + (s.send_date ? tr(' · к ') + when(s.send_date) : '') + (STATES[s.state] ? ' · ' + STATES[s.state] : '') })))));
}
