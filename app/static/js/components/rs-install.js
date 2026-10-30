import { tr } from '../i18n.js';
import { css, html, LitElement } from '../../vendor/lit-core.min.js';

// Installation is browser-owned. This view only shows availability and emits the user's intent.
export class RsInstall extends LitElement {
  static properties = { installed: { type: Boolean }, busy: { type: Boolean } };
  static styles = css`
    :host { display: inline-flex; }
    button { font: inherit; font-size: 13px; color: var(--text, #17212b); background: transparent;
      border: 1px solid var(--border, #dfe3e8); border-radius: 8px; padding: 7px 10px;
      cursor: pointer; white-space: nowrap; }
    button:hover { background: var(--soft, #eef3f8); }
    button:focus-visible { outline: 2px solid var(--accent, #3390ec); outline-offset: 2px; }
    button:disabled { cursor: wait; opacity: .6; }`;

  constructor() {
    super();
    this.installed = false;
    this.busy = false;
  }

  render() {
    return this.installed ? null : html`<button type="button" ?disabled=${this.busy}
      title=${tr('Установить RichPost на телефон или компьютер')}
      @click=${() => this.dispatchEvent(new CustomEvent('install'))}>${tr('Установить')}</button>`;
  }
}
customElements.define('rs-install', RsInstall);
