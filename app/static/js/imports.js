import { api } from './api.js';
import { openDialog, closeDialog } from './dialogs.js';
import { h, toast } from './dom.js';
import { loadDrafts, openDraft } from './drafts.js';
import { flushSave } from './editor.js';
import { dirty, S } from './state.js';
import './components/rs-import.js';

export async function showImport() {
  await flushSave();
  if (dirty.value || S.conflict) throw new Error('Сначала сохраните текущий черновик и разрешите конфликт правок.');
  const form = /** @type {import('./components/rs-import.js').RsImport} */ (document.createElement('rs-import'));
  const dialog = openDialog([h('h3', { text: 'Импорт из Telegram' }), form], [h('button', { value: 'cancel', class: 'ghost', text: 'Закрыть' })]);
  /** @type {ReturnType<typeof setTimeout> | undefined} */
  let timer;
  dialog.addEventListener('close', () => clearTimeout(timer));
  const cfg = /** @type {TelegramImportStatus} */ (await api('GET', '/api/imports/telegram'));
  form.forwardAvailable = cfg.forward_available;
  form.message = cfg.mtproto_configured ? 'Доступно чтение через Telegram.' : 'Доступен публичный веб-предпросмотр. Некоторые посты требуют подключения Telegram или пересылки боту.';

  const done = async (/** @type {TelegramImportResult} */ result) => {
    await loadDrafts();
    if (!dialog.open) { toast('Импорт сохранён в новом черновике.', 'ok'); return; }
    // Modal prevents edits during import; re-check in case another tab caused a conflict.
    if (dirty.value || S.conflict) { form.message = 'Импорт сохранён в списке черновиков. Текущие правки оставлены в редакторе.'; return; }
    await openDraft(result.draft.id);
    closeDialog(dialog);
    toast(`Импортировано блоков: ${result.block_count}, медиа: ${result.media_count}.`, 'ok');
  };
  const poll = async (id) => {
    if (!dialog.open) return;
    try {
      const ticket = /** @type {TelegramForwardImport} */ (await api('GET', '/api/imports/forward/' + encodeURIComponent(id)));
      if (ticket.state === 'done' && ticket.result) { await done(ticket.result); return; }
      if (['failed', 'expired'].includes(ticket.state)) throw new Error(ticket.error || 'Время ожидания истекло. Начните импорт заново.');
      form.message = ticket.state === 'armed' ? 'Бот готов. Перешлите выбранное сообщение.' : 'Ожидаем Start и пересылку в Telegram…';
      timer = setTimeout(() => poll(id), 2000);
    } catch (e) { form.error = e.message; form.busy = false; }
  };
  form.addEventListener('import-request', async (event) => {
    const { kind, url } = /** @type {CustomEvent} */ (event).detail;
    form.busy = true; form.error = ''; form.botUrl = ''; form.message = 'Получаем содержимое и медиа…';
    try {
      if (kind === 'forward') {
        const ticket = await api('POST', '/api/imports/forward', { url });
        form.botUrl = ticket.bot_url;
        await poll(ticket.id);
      } else {
        const result = await api('POST', '/api/imports/telegram', { url });
        await done(result);
        form.busy = false;
      }
    } catch (e) { form.error = e.message; form.busy = false; }
  });
}
