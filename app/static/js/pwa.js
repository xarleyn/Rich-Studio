import { signal } from '../vendor/signals-core.js';
import { openDialog } from './dialogs.js';
import { $, h, toast } from './dom.js';
import { tr } from './i18n.js';

/** @typedef {Event & {prompt(): Promise<void>, userChoice: Promise<{outcome: string}>}} InstallPrompt */
const display = matchMedia('(display-mode: standalone)');
const installed = signal(display.matches || !!/** @type {Navigator & {standalone?: boolean}} */ (navigator).standalone);
const busy = signal(false);
const DELAY = 45000;
const NOTICE_MS = 20000;
const SESSION_KEY = 'rs.pwa.offered';
const SNOOZE_KEY = 'rs.pwa.snoozeUntil'; // 30-day pause written by the earlier card offer; still honoured
const DECLINE_KEY = 'rs.pwa.declined';
/** @type {InstallPrompt | null} */
let prompt = null;
let initialized = false;
let remember = false;
let shownInSession = false;
let interacted = false;
let declined = false;
let snoozedUntil = 0;
let eligibleAt = 0;
/** @type {ReturnType<typeof setTimeout> | undefined} */
let timer;
/** @type {ReturnType<typeof setTimeout> | undefined} */
let noticeTimer;
/** @type {HTMLElement | null} */
let notice = null;

const iosSafari = () => (/iPad|iPhone|iPod/.test(navigator.userAgent)
  || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1))
  && /Safari/.test(navigator.userAgent) && !/CriOS|FxiOS|EdgiOS|OPiOS/.test(navigator.userAgent);

function typing() {
  let active = document.activeElement;
  while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
  return active instanceof HTMLElement && (active.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(active.tagName));
}

function hideNotice() {
  clearTimeout(noticeTimer);
  noticeTimer = undefined;
  notice?.remove();
  notice = null;
}

/** An explicit refusal is remembered for good; an ignored notice only stays quiet for this session. */
function decline() {
  hideNotice();
  try { localStorage.setItem(DECLINE_KEY, '1'); } catch { /* storage off: the session quiet already applies */ }
}

function tryOffer() {
  timer = undefined;
  if (!remember || shownInSession || installed.value || busy.value || !interacted
    || declined || snoozedUntil > Date.now() || (!prompt && !iosSafari()) || !isSecureContext) return;
  if (Date.now() < eligibleAt) { schedule(); return; }
  if (document.visibilityState !== 'visible' || document.body.classList.contains('signed-out')
    || document.querySelector('dialog[open]') || typing()) return;
  // Mark before showing: a reload must not repeat an ignored notice in the same tab/session.
  try { sessionStorage.setItem(SESSION_KEY, '1'); }
  catch { remember = false; return; }
  shownInSession = true;
  const ios = iosSafari();
  notice = h('div', { class: 'toast offer', role: 'region', 'aria-label': tr('Установка приложения') },
    h('p', { text: tr('Открывайте студию отдельным окном на телефоне или компьютере.') }),
    h('div', { class: 'row' },
      h('button', { type: 'button', class: 'primary', text: ios ? tr('Как установить') : tr('Установить'),
        onclick: () => { hideNotice(); requestInstall(); } }),
      h('button', { type: 'button', text: tr('Не предлагать'), onclick: decline })));
  $('#toasts').append(notice);
  noticeTimer = setTimeout(hideNotice, NOTICE_MS);
}

function schedule() {
  clearTimeout(timer);
  if (shownInSession || installed.value || !remember || declined) return;
  timer = setTimeout(tryOffer, Math.max(1000, eligibleAt - Date.now()));
}

function instructions() {
  openDialog([
    h('h3', { text: tr('Установить RichPost') }),
    h('p', { text: tr('Приложение открывается в отдельном окне. Для работы с постами нужен интернет.') }),
    h('p', { text: tr('На iPhone и iPad: откройте сайт в Safari, нажмите «Поделиться» → «На экран Домой». Включите «Открывать как веб-приложение», если такой переключатель показан.') }),
    h('p', { text: tr('На Android и компьютере: выберите «Установить приложение» в меню браузера или значок установки в адресной строке. В Safari на Mac — «Файл» → «Добавить в Dock».') }),
    h('p', { class: 'muted small', text: tr('Если установки нет в меню, используйте другой поддерживающий её браузер. Доступность зависит от браузера и устройства.') }),
  ]);
}

export const isPwaInstalled = () => installed.value;

/** Only an explicit click calls the browser prompt; automatic notices never consume a user gesture. */
export async function requestInstall() {
  if (busy.value || installed.value) return;
  shownInSession = true; // reading the instructions or dismissing the browser prompt also quiets this session
  hideNotice();
  if (!prompt) { instructions(); return; }
  const offer = prompt;
  prompt = null; // browsers permit each saved prompt only once
  busy.value = true;
  try {
    await offer.prompt();
    if ((await offer.userChoice).outcome === 'accepted') installed.value = true;
  } catch { instructions(); }
  finally { busy.value = false; }
}

/** Capture browser readiness early, before the account gate. Notices wait for an idle editor. */
export function initPwa() {
  if (initialized) return;
  initialized = true;
  eligibleAt = Date.now() + DELAY;
  try {
    const saved = localStorage.getItem(SNOOZE_KEY) || '0';
    snoozedUntil = Number(saved) || 0;
    // If dismissals cannot be remembered, keep automatic offers off instead of nagging after reload.
    localStorage.setItem(SNOOZE_KEY, saved);
    declined = localStorage.getItem(DECLINE_KEY) === '1';
    shownInSession = sessionStorage.getItem(SESSION_KEY) === '1';
    remember = true;
  } catch { remember = false; }
  display.addEventListener('change', () => { if (display.matches) { installed.value = true; hideNotice(); } });
  addEventListener('beforeinstallprompt', (event) => {
    event.preventDefault();
    prompt = /** @type {InstallPrompt} */ (event);
    schedule();
  });
  addEventListener('appinstalled', () => { installed.value = true; prompt = null; hideNotice(); });
  addEventListener('storage', (event) => {
    if (event.key === SNOOZE_KEY) snoozedUntil = Number(event.newValue) || 0;
    if (event.key === DECLINE_KEY) declined = event.newValue === '1';
    if ((event.key === SNOOZE_KEY || event.key === DECLINE_KEY) && (declined || snoozedUntil > Date.now())) {
      hideNotice();
      clearTimeout(timer);
    }
  });
  const interaction = () => { interacted = true; schedule(); };
  document.addEventListener('pointerdown', interaction, { passive: true });
  document.addEventListener('keydown', interaction);
  document.addEventListener('focusout', schedule);
  document.addEventListener('close', schedule, true);
  document.addEventListener('visibilitychange', schedule);
  if ('serviceWorker' in navigator && isSecureContext) {
    navigator.serviceWorker.register('/sw.js', { scope: '/', updateViaCache: 'none' })
      .catch(() => toast(tr('Не удалось подготовить приложение к установке. Обновите страницу при стабильном подключении.'), 'err'));
  }
}
