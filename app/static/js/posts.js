import { tr, dateLocale } from './i18n.js';
// Published and scheduled posts.
import { api } from './api.js';
import { setDraft } from './drafts.js';
import { $, h, toast } from './dom.js';
import { autosave, buildPost, callbacksFor, flushSave, loadPost, updatesLabel } from './editor.js';
import { resetHistory } from './history.js';
import { editPost, refreshEditing, reschedulePost } from './editing.js';
import { STATUS_LABELS } from './labels.js';
import { chatLabel, dirty, editing, kindOf, S, saveState } from './state.js';
import { loadSuggested } from './suggested.js';

// ---------- posts ----------
export async function loadPosts() {
  const only = /** @type {HTMLSelectElement} */ ($('#posts-filter')).value;
  S.posts = await api('GET', '/api/posts');
  document.dispatchEvent(new CustomEvent('rs-posts-changed'));
  S.updates = await api('GET', '/api/updates').catch(() => S.updates);
  loadSuggested();
  const ul = $('#posts');
  const u = S.updates;
  $('#updates-state').replaceChildren(h('span', { class: u && u.state === 'ok' ? 'ok' : 'w', title: (u && u.error) || '', text: tr('Приём событий: ') + updatesLabel() }),
    h('span', { class: 'muted', text: tr(' · нажатия кнопок, первые комментарии, реакции, итоги опросов, вступления, предложенные посты') }));
  const shown = S.posts.filter((p) => !only || p.status === only);
  ul.replaceChildren(...shown.map((p) => {
    const when = (p.status === 'scheduled' && p.scheduled_at) || p.sent_at || p.created_at;
    const kind = kindOf(p);
    // a poll, Live Photo or paid media is a message of its own: it does not open in the post editor
    const own = !kind.editable;
    const sent = p.status === 'sent';
    const pr = p.poll_results;
    const stats = [
      p.pinned ? tr('📌 закреплён') : null,
      p.reactions && Object.keys(p.reactions).length ? Object.entries(p.reactions).map(([k, v]) => (k.startsWith('custom:') ? '★' : k === 'paid' ? '⭐' : k) + ' ' + v).join('  ') : null,
      p.clicks && Object.keys(p.clicks).length ? tr('нажатия: ') + Object.entries(p.clicks).map(([k, v]) => ((p.buttons && p.buttons[k]) || k) + ' ' + v).join(', ') : null,
      p.comment_status ? tr('комментарий: ') + ({ waiting: tr('ждёт пересылки в обсуждение'), sent: tr('опубликован'), failed: tr('ошибка'), skipped: tr('не будет: приём событий выключен') }[p.comment_status] || p.comment_status) : null,
      pr ? tr`голосов: ${pr.total}${pr.is_closed ? tr(' (закрыт)') : ''} · ` + pr.options.map((o) => `${o.text} ${o.voter_count}`).join(' / ') : null,
    ].filter(Boolean);
    const call = async (method, path, body, ok) => { try { await api(method, path, body); toast(ok, 'ok'); } catch (e) { toast(e.message, 'err'); } loadPosts(); };
    const num = p.message_id ? '#' + p.message_id : '';
    return h('li', { class: 'post-row' + (p.status === 'failed' ? ' failed' : '') },
      h('div', { class: 'post-head' },
        p.thumb ? h('img', { class: 'thumb', src: p.thumb, alt: '', loading: 'lazy' }) : null,
        h('span', { class: 'post-title' + (p.title ? '' : ' muted'), text: p.title || tr('Без текста') })),
      h('span', { class: 'meta' }, h('span', { class: 'status ' + p.status, text: STATUS_LABELS[p.status] || p.status }), kind.badge ? h('b', { text: kind.badge }) : null, p.actor && p.actor !== 'ui' ? h('span', { class: 'actor', title: tr('Кто отправил: не редактор, а API или агент'), text: p.actor }) : null, h('span', { class: 'chan', title: tr('Канал'), text: chatLabel(p.chat_id) }),
        p.link ? h('a', { href: p.link, target: '_blank', rel: 'noopener', title: tr('Открыть в Telegram'), text: num }) : num),
      h('span', { class: 'meta', text: new Date(when * 1000).toLocaleString(dateLocale, { dateStyle: 'short', timeStyle: 'short' }) }),
      p.error ? h('span', { class: 'small post-error', text: (p.status === 'failed' ? tr('Не вышел: ') : '') + p.error }) : null,
      stats.length ? h('span', { class: 'meta stats', text: stats.join(' · ') }) : null,
      h('span', { class: 'acts' },
        !own && ['sent', 'scheduled', 'missed'].includes(p.status) ? h('button', { class: 'ghost small-btn', text: tr('Править'), title: tr('Открыть пост в редакторе; «Обновить пост» над редактором заменит его'), onclick: () => editPost(p).catch((e) => toast(e.message, 'err')) }) : null,
        !own && !['sent', 'scheduled', 'missed'].includes(p.status) ? h('button', { class: 'ghost small-btn', text: tr('Открыть'), title: tr('Копия поста новым черновиком'), onclick: () => openCopy(p) }) : null,
        ['scheduled', 'missed'].includes(p.status) ? h('button', { class: 'ghost small-btn', text: p.status === 'missed' ? tr('Отложить заново') : tr('Перенести'), onclick: () => reschedulePost(p) }) : null,
        // only for the post open for editing: the editor's buttons are that post's, not another draft's
        !own && ['sent', 'scheduled', 'missed'].includes(p.status) && editing.value && editing.value.id === p.id ? h('button', { class: 'ghost small-btn', text: tr('Кнопки'), title: tr('Заменить только кнопки под постом кнопками из редактора; текст не меняется'), onclick: () => {
          const kb = S.keyboard.filter((r) => r.length);
          if (!confirm(kb.length ? tr`Поставить под пост ${kb.flat().length} кнопок из редактора?` : tr('В редакторе нет кнопок. Убрать кнопки из поста?'))) return;
          call('POST', `/api/posts/${p.id}/buttons`, { reply_markup: kb.length ? { inline_keyboard: kb } : null, callbacks: callbacksFor(kb) }, tr('Кнопки обновлены')).then(() => refreshEditing(p.id));
        } }) : null,
        sent ? h('button', { class: 'ghost small-btn', text: p.pinned ? tr('Открепить') : tr('Закрепить'), onclick: () => p.pinned ? call('DELETE', `/api/posts/${p.id}/pin`, undefined, tr('Откреплено')) : call('POST', `/api/posts/${p.id}/pin`, { disable_notification: true }, tr('Закреплено без уведомления')) }) : null,
        kind.stoppable && sent && !(pr && pr.is_closed) ? h('button', { class: 'ghost small-btn', text: tr('Закрыть опрос'), onclick: () => { if (confirm(tr('Закрыть опрос? Голосовать больше будет нельзя.'))) call('POST', `/api/posts/${p.id}/stop-poll`, undefined, tr('Опрос закрыт')); } }) : null,
        p.status === 'failed' ? h('button', { class: 'ghost small-btn', text: tr('Убрать'), title: tr('Убрать запись из списка; в канале этого поста нет'), onclick: () => call('DELETE', '/api/posts/' + p.id, undefined, tr('Убрано из списка')) }) : null,
        ['sent', 'scheduled', 'missed'].includes(p.status) ? h('button', { class: 'danger small-btn', text: p.status === 'sent' ? tr('Удалить') : tr('Отменить'), onclick: async () => {
          if (!confirm(p.status === 'sent' ? tr('Удалить пост из канала? Это нельзя отменить.') : tr('Отменить отложенную публикацию?'))) return;
          try { await api('DELETE', '/api/posts/' + p.id); toast(tr('Готово'), 'ok'); } catch (e) { toast(e.message, 'err'); }
          loadPosts();
        } }) : null));
  }));
  if (!shown.length) ul.append(h('li', { class: 'muted small', text: only ? tr('Таких постов нет.') : tr('Здесь появятся опубликованные, отложенные и неудачные посты.') }));
}

// the post as a new draft; the open draft is saved first, so nothing typed is lost
async function openCopy(p) {
  try {
    await flushSave();
    const full = await api('GET', '/api/posts/' + p.id);
    setDraft(null, null); editing.value = null;
    $('#title').value = tr('Копия поста ') + (p.message_id || '');
    loadPost(full.payload);
    resetHistory();
    dirty.value = true; saveState.value = tr('есть изменения');
    autosave();
  } catch (e) { toast(e.message, 'err'); }
}

export async function replacePost(p) {
  if (!confirm(tr`Заменить пост ${p.message_id ? '#' + p.message_id + ' ' : ''}в ${p.chat_id} содержимым редактора?`)) return;
  try { await api('POST', `/api/posts/${p.id}/edit`, buildPost()); toast(tr('Пост обновлён'), 'ok'); loadPosts(); } catch (e) { toast(e.message, 'err'); }
}
