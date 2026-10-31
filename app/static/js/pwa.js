import { effect, signal } from '../vendor/signals-core.js';
import { openDialog } from './dialogs.js';
import { h, toast } from './dom.js';
import { tr } from './i18n.js';
import './components/rs-install.js';

/** @typedef {Event & {prompt(): Promise<void>, userChoice: Promise<{outcome: string}>}} InstallPrompt */
const display = matchMedia('(display-mode: standalone)');
const installed = signal(display.matches || !!/** @type {Navigator & {standalone?: boolean}} */ (navigator).standalone);
const busy = signal(false);
const offered = signal(false);
const DELAY = 45000;
const SNOOZE = 30 * 24 * 60 * 60 * 1000;
const SNOOZE_KEY = 'rs.pwa.snoozeUntil';
const SESSION_KEY = 'rs.pwa.offered';
/** @type {InstallPrompt | null} */
let prompt = null;
let initialized = false;
let remember = false;
let shownInSession = false;
let interacted = false;
let snoozedUntil = 0;
let eligibleAt = 0;
/** @type {ReturnType<typeof setTimeout> | undefined} */
let timer;

const iosSafari = () => (/iPad|iPhone|iPod/.test(navigator.userAgent)
  || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1))
  && /Safari/.test(navigator.userAgent) && !/CriOS|FxiOS|EdgiOS|OPiOS/.test(navigator.userAgent);

function typing() {
  let active = document.activeElement;
  while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
  return active instanceof HTMLElement && (active.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(active.tagName));
}

function snooze() {
  offered.value = false;
  shownInSession = true;
  snoozedUntil = Date.now() + SNOOZE;
  clearTimeout(timer);
  try {
    localStorage.setItem(SNOOZE_KEY, String(snoozedUntil));
    sessionStorage.setItem(SESSION_KEY, '1');
  } catch { remember = false; }
}

function tryOffer() {
  timer = undefined;
  if (!remember || shownInSession || installed.value || busy.value || !interacted
    || snoozedUntil > Date.now() || (!prompt && !iosSafari()) || !isSecureContext) return;
  if (Date.now() < eligibleAt) { schedule(); return; }
  if (document.visibilityState !== 'visible' || document.body.classList.contains('signed-out')
    || document.querySelector('dialog[open]') || typing()) return;
  // Mark before showing: a reload must not repeat an ignored offer in the same tab/session.
  try { sessionStorage.setItem(SESSION_KEY, '1'); }
  catch { remember = false; return; }
  shownInSession = true;
  offered.value = true;
}

function schedule() {
  clearTimeout(timer);
  if (shownInSession || installed.value || !remember) return;
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

/** Only an explicit click calls the browser prompt; automatic offers never consume a user gesture. */
export async function requestInstall() {
  if (busy.value || installed.value) return;
  snooze(); // reading the instructions or dismissing the browser prompt also quiets further offers
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

/** Capture browser readiness early, before the account gate. Offers wait for an idle editor. */
export function initPwa() {
  if (initialized) return;
  initialized = true;
  const control = /** @type {import('./components/rs-install.js').RsInstall | null} */ (document.querySelector('#install-app'));
  eligibleAt = Date.now() + DELAY;
  try {
    const saved = localStorage.getItem(SNOOZE_KEY) || '0';
    snoozedUntil = Number(saved) || 0;
    // If dismissals cannot be remembered, keep automatic offers off instead of nagging after reload.
    localStorage.setItem(SNOOZE_KEY, saved);
    shownInSession = sessionStorage.getItem(SESSION_KEY) === '1';
    remember = true;
  } catch { remember = false; }
  if (control) {
    effect(() => {
      control.shown = offered.value && !installed.value;
      control.busy = busy.value;
      control.ios = iosSafari();
    });
    control.addEventListener('install', requestInstall);
    control.addEventListener('dismiss', snooze);
  }
  display.addEventListener('change', () => { if (display.matches) { installed.value = true; snooze(); } });
  addEventListener('beforeinstallprompt', (event) => {
    event.preventDefault();
    prompt = /** @type {InstallPrompt} */ (event);
    schedule();
  });
  addEventListener('appinstalled', () => { installed.value = true; prompt = null; snooze(); });
  addEventListener('storage', (event) => {
    if (event.key !== SNOOZE_KEY) return;
    snoozedUntil = Number(event.newValue) || 0;
    if (snoozedUntil > Date.now()) { offered.value = false; clearTimeout(timer); }
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
