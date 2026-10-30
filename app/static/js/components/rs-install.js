import { tr } from '../i18n.js';
import { css, html, LitElement } from '../../vendor/lit-core.min.js';

// A quiet, nonmodal offer. It never takes focus; installation remains an explicit browser-owned action.
export class RsInstall extends LitElement {
  static properties = { shown: { type: Boolean }, busy: { type: Boolean }, ios: { type: Boolean } };
  static styles = css`
    :host { position: fixed; z-index: 90; right: max(16px, env(safe-area-inset-right));
      bottom: max(16px, env(safe-area-inset-bottom)); width: min(350px, calc(100vw - 32px)); }
    * { box-sizing: border-box; }
    section { color: var(--text, #17212b); background: var(--panel, #fff); border: 1px solid var(--border, #dfe3e8);
      border-radius: 16px; padding: 16px; box-shadow: 0 8px 32px #17212b26; }
    .heading { display: flex; align-items: center; gap: 10px; }
    .heading strong { flex: 1; font-size: 14px; }
    img { border-radius: 8px; }
    p { margin: 12px 0; font-size: 13px; line-height: 1.5; }
    .actions { display: flex; gap: 8px; }
    button { font: inherit; font-size: 13px; color: inherit; background: transparent;
      border: 1px solid var(--border, #dfe3e8); border-radius: 8px; padding: 8px 12px;
      cursor: pointer; }
    .close { border: 0; font-size: 22px; padding: 2px 8px; }
    .primary { color: white; background: var(--accent, #3390ec); border-color: var(--accent, #3390ec); }
    button:hover { background: var(--soft, #eef3f8); }
    .primary:hover { filter: brightness(.95); background: var(--accent, #3390ec); }
    button:focus-visible { outline: 2px solid var(--accent, #3390ec); outline-offset: 2px; }
    button:disabled { cursor: wait; opacity: .6; }`;

  constructor() {
    super();
    this.shown = false;
    this.busy = false;
    this.ios = false;
  }

  render() {
    return !this.shown ? null : html`<section role="region" aria-label=${tr('Установка приложения')}
      @keydown=${(event) => { if (event.key === 'Escape') this.dispatchEvent(new CustomEvent('dismiss')); }}>
      <div class="heading"><img src="/static/icons/icon-192.png" alt="" width="32" height="32">
        <strong>${tr('RichPost как приложение')}</strong>
        <button class="close" type="button" aria-label=${tr('Не предлагать установку 30 дней')}
          @click=${() => this.dispatchEvent(new CustomEvent('dismiss'))}>×</button></div>
      <p>${tr('Открывайте студию с экрана телефона или рабочего стола.')}</p>
      <div class="actions"><button id="install" class="primary" type="button" ?disabled=${this.busy}
        @click=${() => this.dispatchEvent(new CustomEvent('install'))}>${this.ios ? tr('Как установить') : tr('Установить')}</button>
        <button id="later" type="button" @click=${() => this.dispatchEvent(new CustomEvent('dismiss'))}>${tr('Позже')}</button></div>
    </section>`;
  }
}
customElements.define('rs-install', RsInstall);
