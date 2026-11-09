import { tr } from './i18n.js';
// Виджет (sendWidget): отдельное сообщение, HTML со встроенными CSS и JS. После отправки
// HTML неизменяем — меняются только данные (editMessageWidgetData). Каналы не входят в
// прямую отправку (Bot API 10.4). Лимиты: исходник 32768 байт, после zlib — 4096.
import { api } from './api.js';
import { closeDialog, openDialog } from './dialogs.js';
import { act, h, sentReport, toast } from './dom.js';
import { loadPosts } from './posts.js';
import { target, targetLabel } from './state.js';
import { whenPicker } from './when.js';

const SOURCE_MAX = 32768;
const COMPRESSED_MAX = 4096;
const DATA_MAX = 1024;

const bytes = (text) => new TextEncoder().encode(text).length;

const pill = (text, kind) => h('span', { class: kind, text });
const byteLine = (raw, compressed) =>
  h('span', { class: raw > SOURCE_MAX ? 'e' : compressed > COMPRESSED_MAX ? 'e' : 'ok', text: tr`${raw} байт · сжатие ≈ ${compressed} из ${COMPRESSED_MAX}` });

/** Куда: цель из стейта (как у опроса и медиа-постов). */
function destination() {
  return { ...target() };
}

export function showWidget() {
  const W = { html: '', caption: '', chat_data: '', public_data: '', pin: false };
  /** @type {WidgetReport | null} */
  let server = null;
  const sizes = h('span', { class: 'muted small', text: '0 байт' });
  const issues = h('div');
  let debounce = 0;
  let checkId = 0;
  let sending = false;

  const form = h('form', { method: 'POST', action: '/render/widget', target: 'rs_widget_preview', hidden: true },
    h('input', { name: 'html' }), h('input', { name: 'chat_data' }), h('input', { name: 'public_data' }));
  document.body.append(form);

  const preview = () => {
    form.elements.html.value = W.html;
    form.elements.chat_data.value = W.chat_data;
    form.elements.public_data.value = W.public_data;
    form.submit();
  };

  const field = (label, ...el) => h('div', { class: 'pf' }, h('label', { class: 'small muted', text: label }), ...el);

  const drawIssues = () => {
    const rows = [];
    for (const error of server?.errors || []) rows.push(pill(error, 'e'));
    for (const warning of server?.warnings || []) rows.push(pill(warning, 'w'));
    if (W.html.length && (server?.size_raw || 0) > SOURCE_MAX) rows.push(pill(tr('исходник больше 32768 байт'), 'e'));
    if (!rows.length && W.html.trim()) rows.push(pill(server?.ok ? tr('проверка пройдена') : '…', 'ok'));
    issues.replaceChildren(...rows.map((row) => h('div', { class: 'row' }, row)));
  };

  const check = async () => {
    const id = ++checkId;
    const widget = { html: W.html, chat_data: W.chat_data, public_data: W.public_data };
    if (!W.html.trim()) { server = null; drawIssues(); sizes.replaceChildren(h('span', { text: '0 байт' })); return null; }
    /** @type {WidgetReport} */
    let report;
    try {
      report = await api('POST', '/api/widget/validate', { widget });
    } catch (e) {
      report = { ok: false, errors: [String(e?.message ?? e)], warnings: [], size_raw: bytes(widget.html), size_compressed: 0 };
    }
    // A response for an earlier source or a closed dialog must never approve newer input.
    if (id !== checkId || !d.open) return null;
    server = report;
    sizes.replaceChildren(byteLine(report.size_raw, report.size_compressed));
    drawIssues();
    return report;
  };
  const recheck = () => {
    clearTimeout(debounce);
    ++checkId;
    server = null;
    drawIssues();
    sizes.textContent = tr`${bytes(W.html)} байт`;
    debounce = setTimeout(() => void check(), 400);
  };

  const build = () => {
    if (!W.html.trim()) { toast(tr('Пустой HTML виджета'), 'err'); return null; }
    /** @type {Json} */
    const widget = { html: W.html };
    if (W.caption.trim()) widget.caption = W.caption.trim();
    if (W.chat_data) widget.chat_data = W.chat_data;
    if (W.public_data) widget.public_data = W.public_data;
    return widget;
  };

  const go = async (scheduled) => {
    if (sending) return;
    sending = true;
    try {
      clearTimeout(debounce);
      const report = await check();
      if (!d.open) return;
      if (!report?.ok) { toast(tr('Виджет не прошёл проверку — посмотрите список под исходником'), 'err'); return; }
      const widget = build();
      if (!widget) return;
      /** @type {Json} */
      const req = { widget, pin: W.pin, ...destination() };
      if (scheduled) { const at = pick.value(); if (!at || at * 1000 < Date.now()) { toast(tr('Выберите время в будущем'), 'err'); return; } req.at = at; }
      if (!scheduled && !confirm(tr`Отправить виджет в ${targetLabel()}? HTML после отправки изменить нельзя.`)) return;
      const r = await act(() => api('POST', '/api/widget', req), scheduled ? tr('Запланировано') : (x) => sentReport(tr('Отправлено'), x));
      if (r) { closeDialog(d); loadPosts(); }
    } finally { sending = false; }
  };

  const pick = whenPicker(Date.now() + 3600e3);
  const d = openDialog([
    h('h3', { text: tr('HTML-виджет') }),
    h('p', { class: 'muted small', text: tr('HTML со встроенными CSS и JS, лимиты: исходник 32768 байт, после сжатия zlib — 4096. Куда: ') + (targetLabel() || tr('канал не указан')) + tr('. В канал виджет не уйдёт: Bot API 10.4 разрешает личный чат, обычную группу или супергруппу.') }),
    h('p', { class: 'muted small', text: tr('Автономный виджет: интерактивность работает внутри сообщения, без запросов к боту.') }),
    field(tr('Исходник (HTML/CSS/JS)'), h('textarea', {
      rows: 10, spellcheck: false, 'aria-label': tr('Исходник виджета'),
      style: 'font:13px/1.5 ui-monospace, Consolas, monospace; width:100%',
      oninput: (e) => { W.html = e.target.value; recheck(); },
    })),
    h('div', { class: 'row' }, sizes, h('button', { class: 'ghost small-btn', text: tr('Предпросмотр'), title: tr('Отдельное окно: официальный runtime WidgetSandbox'), onclick: preview })),
    issues,
    field(tr('Подпись (до 1024 символов)'), h('textarea', { rows: 2, maxlength: 1024, 'aria-label': tr('Подпись'), oninput: (e) => { W.caption = e.target.value; } })),
    field(tr('chat_data — данные для этого чата (до 1024 байт)'), h('textarea', { rows: 2, maxlength: 4096, 'aria-label': tr('chat_data'), oninput: (e) => { W.chat_data = e.target.value; recheck(); } })),
    field(tr('public_data — общие данные (до 1024 байт)'), h('textarea', { rows: 2, maxlength: 4096, 'aria-label': tr('public_data'), oninput: (e) => { W.public_data = e.target.value; recheck(); } })),
    h('label', { class: 'row', style: 'margin:2px 0' }, h('input', { type: 'checkbox', onchange: (e) => { W.pin = e.target.checked; } }), tr('Закрепить')),
    h('details', {}, h('summary', { class: 'small', text: tr('Время для «Отложить»') }), pick.el),
  ], [
    h('button', { class: 'ghost', value: 'cancel', text: tr('Закрыть') }),
    h('button', { class: 'ghost', text: tr('Отложить'), onclick: () => go(true) }),
    h('button', { class: 'primary', text: tr('Отправить'), onclick: () => go(false) }),
  ]);
  d.addEventListener('close', () => { clearTimeout(debounce); ++checkId; form.remove(); });
}

/** Правка данных отправленного или отложенного виджета (editMessageWidgetData); HTML неизменяем. */
export async function showWidgetData(p) {
  const record = await act(() => api('GET', `/api/posts/${p.id}`));
  if (!record) return;
  const payload = record.payload?.widget || {};
  const inputs = {
    chat_data: h('textarea', { rows: 3, 'aria-label': tr('chat_data') }),
    public_data: h('textarea', { rows: 3, 'aria-label': tr('public_data') }),
  };
  for (const key of ['chat_data', 'public_data']) {
    inputs[key].value = payload[key] || '';
  }
  const field = (label, el) => h('div', { class: 'pf' }, h('label', { class: 'small muted', text: label }), el);
  const d = openDialog([
    h('h3', { text: tr('Данные виджета') }),
    h('p', { class: 'muted small', text: tr('HTML виджета неизменяем: меняются только chat_data и public_data (до 1024 байт). Изменение не помечает сообщение как «изменённое».') }),
    h('p', { class: 'muted small', text: tr('Загружены текущие данные. Очистите поле, чтобы стереть его; неизменённые поля сохранятся.') }),
    field(tr('chat_data — данные для этого чата'), inputs.chat_data),
    field(tr('public_data — общие данные'), inputs.public_data),
  ], [
    h('button', { class: 'ghost', value: 'cancel', text: tr('Отмена') }),
    h('button', { class: 'primary', text: tr('Сохранить'), onclick: async () => {
      /** @type {Json} */
      const body = {};
      for (const key of ['chat_data', 'public_data']) {
        if (inputs[key].value !== (payload[key] || '')) {
          if (bytes(inputs[key].value) > DATA_MAX) { toast(tr('Данные виджета больше 1024 байт'), 'err'); return; }
          body[key] = inputs[key].value;
        }
      }
      if (!Object.keys(body).length) { closeDialog(d); return; }
      const r = await act(() => api('POST', `/api/posts/${p.id}/widget-data?post_version=${record.version}`, body), tr('Данные обновлены'));
      if (r) { closeDialog(d); loadPosts(); }
    } }),
  ]);
}

