// Обвязка предпросмотра виджета поверх вендоренного WidgetSandbox (см. /render/widget).
// Trusted-код: сам виджет исполняется в QuickJS-worker рантайма, до этой страницы не доходит.
// Документ открывается отдельным окном с COOP/COEP: рантайму нужен SharedArrayBuffer.
import { WidgetHost } from '@telegram/widget-sandbox/host/widget-host';
import { WIDGET_MESSAGE_KINDS } from '@telegram/widget-sandbox/shared/sdk-policy';

const block = document.getElementById('widget-source');
if (!(block instanceof HTMLScriptElement)) throw new Error('нет блока widget-source');
const source = JSON.parse(block.textContent || '{}');

const status = document.getElementById('status');
const show = (text) => {
  if (status) status.textContent = text;
};
const log = (level, text) => {
  const line = `[${level}] ${text}`;
  if (level === 'error' || level === 'fatal') console.error(line);
  else if (level === 'warn') console.warn(line);
  else console.log(line);
};

const host = new WidgetHost(document.getElementById('host'), String(source.html || ''), {
  policy: 'strict',
  features: ['webAnimations'],
  theme: 'light',
  locale: navigator.language || 'ru',
  // Личный чат с ботом: у оригинала запросы к боту разрешены; в предпросмотре они
  // отвечаются пустым результатом, чтобы поток запроса был виден без сервера.
  context: WIDGET_MESSAGE_KINDS.private,
  data: { chatData: source.chat_data ?? null, publicData: source.public_data ?? null },
  onRequest: (request) => {
    console.log('запрос виджета к боту (предпросмотр):', request.data);
    show('запрос к боту — в Telegram ответит бот');
    return { result: '' };
  },
  onLog: (level, text) => log(level, text),
  onDiag: (kind, detail) => log('warn', `diag ${kind}: ${detail}`),
  onStats: (stats) => log('info', `stats: tasks=${stats.tasks} ops=${stats.ops ?? '?'} nodes=${stats.nodes}`),
  onError: (text) => {
    console.error(text);
    show(`ошибка: ${text}`);
  },
  onDead: (reason) => show(`виджет остановлен: ${reason}`),
  onResize: (height) => show(`${Math.round(height)} px`),
});
window.__widgetPreview = { host };
