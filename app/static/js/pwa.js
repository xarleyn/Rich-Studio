import { effect, signal } from '../vendor/signals-core.js';
import { openDialog } from './dialogs.js';
import { h, toast } from './dom.js';
import { tr } from './i18n.js';
import './components/rs-install.js';

/** @typedef {Event & {prompt(): Promise<void>, userChoice: Promise<{outcome: string}>}} InstallPrompt */
const display = matchMedia('(display-mode: standalone)');
const installed = signal(display.matches || !!/** @type {Navigator & {standalone?: boolean}} */ (navigator).standalone);
const busy = signal(false);
/** @type {InstallPrompt | null} */
let prompt = null;

function instructions() {
  openDialog([
    h('h3', { text: tr('Установить RichPost') }),
    h('p', { text: tr('Приложение открывается в отдельном окне. Для работы с постами нужен интернет.') }),
    h('p', { text: tr('На iPhone и iPad: откройте сайт в Safari, нажмите «Поделиться» → «На экран Домой». Включите «Открывать как веб-приложение», если такой переключатель показан.') }),
    h('p', { text: tr('На Android и компьютере: выберите «Установить приложение» в меню браузера или значок установки в адресной строке. В Safari на Mac — «Файл» → «Добавить в Dock».') }),
    h('p', { class: 'muted small', text: tr('Если установки нет в меню, используйте другой поддерживающий её браузер. Доступность зависит от браузера и устройства.') }),
  ]);
}

/** Bind before the account gate, so the public sign-in page is installable too. */
export function initPwa() {
  const control = /** @type {import('./components/rs-install.js').RsInstall | null} */ (document.querySelector('#install-app'));
  if (!control) return;
  effect(() => { control.installed = installed.value; control.busy = busy.value; });
  display.addEventListener('change', () => { installed.value = display.matches; });
  addEventListener('beforeinstallprompt', (event) => {
    event.preventDefault();
    prompt = /** @type {InstallPrompt} */ (event);
  });
  addEventListener('appinstalled', () => { installed.value = true; prompt = null; });
  control.addEventListener('install', async () => {
    if (busy.value) return;
    if (!prompt) { instructions(); return; }
    const offer = prompt;
    prompt = null; // browsers permit each saved prompt only once
    busy.value = true;
    try {
      await offer.prompt();
      if ((await offer.userChoice).outcome === 'accepted') installed.value = true;
    } catch { instructions(); }
    finally { busy.value = false; }
  });
  if ('serviceWorker' in navigator && isSecureContext) {
    navigator.serviceWorker.register('/sw.js', { scope: '/', updateViaCache: 'none' })
      .catch(() => toast(tr('Не удалось подготовить приложение к установке. Обновите страницу при стабильном подключении.'), 'err'));
  }
}
