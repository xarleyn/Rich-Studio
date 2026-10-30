// Explicit UI translations only. Post content and API payload keys are never translated.
import english from './locale-en.js';

const requested = new URLSearchParams(globalThis.location?.search || '').get('lang');
let saved;
try { saved = globalThis.localStorage?.getItem('rs.locale'); } catch { /* storage may be disabled */ }
export const locale = requested === 'en' || requested === 'ru' ? requested : saved === 'en' ? 'en' : 'ru';
export const dateLocale = locale === 'en' ? 'en-GB' : 'ru-RU';
if (requested === 'en' || requested === 'ru') rememberLocale(locale);

export function rememberLocale(value) {
  try { globalThis.localStorage?.setItem('rs.locale', value); } catch { /* URL still preserves the choice */ }
}

/** @param {string | TemplateStringsArray} source @param {...any} values */
export function tr(source, ...values) {
  const key = typeof source === 'string' ? source : source.map((s, i) => s + (i < values.length ? `{${i}}` : '')).join('');
  const text = locale === 'en' && Object.hasOwn(english, key) ? english[key] : key;
  return values.length ? text.replace(/\{(\d+)\}/g, (match, i) => Number(i) < values.length ? String(values[Number(i)]) : match) : text;
}

/** Static markup opts in explicitly, so drafts, channel names and rich-text DOM are never traversed. */
export function translateDocument() {
  document.documentElement.lang = locale;
  for (const el of document.querySelectorAll('[data-i18n]')) el.textContent = tr(el.getAttribute('data-i18n') || '');
  for (const attr of ['title', 'placeholder', 'aria-label']) {
    for (const el of document.querySelectorAll(`[data-i18n-${attr}]`)) el.setAttribute(attr, tr(el.getAttribute(`data-i18n-${attr}`) || ''));
  }
}
