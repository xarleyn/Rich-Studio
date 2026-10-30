import { tr, locale } from './i18n.js';
// Cosmetic Russian names for spec types and fields; unknown names fall back to the spec.

// ---------- labels (cosmetic) ----------
export const BLOCK_LABELS = {
  paragraph: tr('Абзац'), heading: tr('Заголовок'), pre: tr('Код'), footer: tr('Подвал'), divider: tr('Разделитель'),
  mathematical_expression: tr('Формула'), anchor: tr('Якорь'), list: tr('Список'), blockquote: tr('Цитата'),
  expandable_blockquote: tr('Свёрнутая цитата'), pullquote: tr('Акцентная цитата'), collage: tr('Коллаж'),
  slideshow: tr('Слайдшоу'), table: tr('Таблица'), details: tr('Раскрывашка'), map: tr('Карта'), buttons: tr('Кнопки'),
  animation: 'GIF', audio: tr('Аудио'), document: tr('Файл'), photo: tr('Фото'), video: tr('Видео'),
  voice_note: tr('Голосовое'), thinking: tr('Думаю… (только личка)'),
};
// groups of the block palette; blocks the spec adds later land in «Другое»
export const BLOCK_GROUPS = {
  [tr('Текст')]: ['paragraph', 'heading', 'list', 'blockquote', 'expandable_blockquote', 'pullquote', 'pre', 'mathematical_expression', 'footer'],
  [tr('Медиа')]: ['photo', 'video', 'animation', 'collage', 'slideshow', 'audio', 'voice_note', 'document', 'map'],
  [tr('Структура')]: ['table', 'details', 'divider', 'anchor'],
  [tr('Кнопки')]: ['buttons'],
};
export const RT_LABELS = {
  bold: tr('Жирный'), italic: tr('Курсив'), underline: tr('Подчёркнутый'), strikethrough: tr('Зачёркнутый'), spoiler: tr('Спойлер'),
  date_time: tr('Дата и время'), text_mention: tr('Упоминание по id'), subscript: tr('Нижний индекс'), superscript: tr('Верхний индекс'),
  marked: tr('Маркер'), code: tr('Код'), custom_emoji: tr('Кастомный эмодзи'), mathematical_expression: tr('Формула'), url: tr('Ссылка'),
  email_address: 'E-mail', phone_number: tr('Телефон'), bank_card_number: tr('Номер карты'), mention: tr('@упоминание'),
  hashtag: tr('#хэштег'), cashtag: tr('$кэштег'), bot_command: tr('/команда'), button: tr('Кнопка в тексте'), anchor: tr('Якорь'),
  anchor_link: tr('Ссылка на якорь'), reference: tr('Сноска'), reference_link: tr('Ссылка на сноску'),
};
export const FIELD_LABELS = {
  text: tr('Текст'), size: tr('Размер (1–6)'), language: tr('Язык'), expression: 'LaTeX', name: tr('Имя'), items: tr('Пункты'),
  blocks: tr('Содержимое'), credit: tr('Автор'), caption: tr('Подпись'), cells: tr('Ячейки'), is_bordered: tr('Рамки'),
  is_striped: tr('Полосы'), is_compact: tr('Компактно'), summary: tr('Заголовок'), is_open: tr('Открыто'), location: tr('Точка'),
  zoom: tr('Масштаб'), width: tr('Ширина'), height: tr('Высота'), buttons: tr('Кнопки'), align: tr('Выравнивание'), url: 'URL',
  style: tr('Стиль'), callback_data: tr('Данные для бота'), copy_text: tr('Копировать текст'), disabled: tr('Неактивна'),
  has_checkbox: tr('Чекбокс'), is_checked: tr('Отмечен'), value: tr('Номер'), latitude: tr('Широта'), longitude: tr('Долгота'),
  media: tr('Файл'), has_spoiler: tr('Под спойлером'), unix_time: tr('Unix-время'), date_time_format: tr('Формат'),
  custom_emoji_id: tr('id эмодзи'), alternative_text: tr('Замена'), user: tr('Пользователь'), anchor_name: tr('Якорь'),
  reference_name: tr('Сноска'), username: 'username', hashtag: tr('Хэштег'),
  photo: tr('Фото'), video: tr('Видео'), animation: 'GIF', audio: tr('Аудио'), document: tr('Файл'), voice_note: tr('Голосовое'),
};
// Placeholders in Russian; the English description from the spec stays in the field's tooltip
export const FIELD_HINTS = {
  text: tr('Текст'), summary: tr('Виден всегда, остальное раскрывается по нажатию'), caption: tr('Подпись под медиа'),
  credit: tr('Автор или источник'), url: 'https://example.com', callback_data: tr('Что получит бот при нажатии, до 64 байт'),
  copy_text: tr('Что скопируется в буфер'), expression: 'E = mc^2', language: 'python', name: tr('латиницей, без пробелов'),
  alternative_text: tr('Эмодзи, если кастомный не загрузится'), username: tr('без @'), hashtag: tr('без #'),
  anchor_name: tr('Имя якоря'), reference_name: tr('Имя сноски'), switch_inline_query: tr('Текст запроса к боту'),
};
export const STATUS_LABELS = { sent: tr('опубликован'), scheduled: tr('отложен'), failed: tr('ошибка'), cancelled: tr('отменён'), deleted: tr('удалён'), sending: tr('отправляется'), missed: tr('пропущен') };
/** «1 блок», «2 блока», «5 блоков» */
export function plural(n, one, few, many) {
  if (locale === 'en') return `${n} ${n === 1 ? one : many}`;
  const m10 = n % 10, m100 = n % 100;
  const w = m10 === 1 && m100 !== 11 ? one : m10 >= 2 && m10 <= 4 && (m100 < 12 || m100 > 14) ? few : many;
  return `${n} ${w}`;
}
// Inline types the rich text field edits as formatting (others become chips)
export const RT_WRAP = { bold: 'b', italic: 'i', underline: 'u', strikethrough: 's', spoiler: 'span.rt-spoiler', marked: 'mark', code: 'code', subscript: 'sub', superscript: 'sup' };
export const IGNORED_MEDIA_FIELDS = new Set(['caption', 'parse_mode', 'caption_entities', 'show_caption_above_media']);
